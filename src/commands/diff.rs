use anyhow::{Context, Result};
use std::collections::HashMap;

use crate::db::Database;
use crate::errors::NoIndexError;
use crate::git;
use crate::indexer;
use crate::parsers;

#[derive(Debug, serde::Serialize)]
struct AddedSymbol {
    file: String,
    name: String,
    kind: String,
    line: i64,
}

#[derive(Debug, serde::Serialize)]
struct RemovedSymbol {
    /// DB row id, so `--impact` can join straight to `callers_of` without a
    /// second per-symbol lookup. Not part of the public JSON shape.
    #[serde(skip)]
    id: i64,
    file: String,
    name: String,
    kind: String,
    line: i64,
}

#[derive(Debug, serde::Serialize)]
struct ModifiedSymbol {
    /// DB row id, see `RemovedSymbol::id`.
    #[serde(skip)]
    id: i64,
    file: String,
    name: String,
    kind: String,
    old_line: i64,
    new_line: i64,
    /// "signature" when kind/visibility/params/returns changed, "body" when
    /// only the line range moved.
    change: &'static str,
}

/// One changed symbol that triggered a dependent's appearance in an
/// `--impact` report.
#[derive(Debug, serde::Serialize)]
struct ImpactTrigger {
    name: String,
    file: String,
    /// "removed" | "signature" | "body", the same vocabulary as
    /// `ModifiedSymbol::change` plus "removed".
    change: &'static str,
    /// Used to render `[removed] fn gone` in the text report; not part of
    /// the JSON shape (kept minimal there, matching the top-level arrays).
    #[serde(skip)]
    kind: String,
}

/// One dependent of the changed symbols, deduped so a symbol touched by
/// several changed symbols appears once with every trigger listed.
#[derive(Debug, serde::Serialize)]
struct ImpactDependent {
    name: String,
    kind: String,
    line: i64,
    /// Max severity over `triggers`: "removed" > "signature" > "body".
    severity: &'static str,
    triggers: Vec<ImpactTrigger>,
}

#[derive(Debug, serde::Serialize)]
struct ImpactFileGroup {
    file: String,
    dependents: Vec<ImpactDependent>,
}

/// A real usage of a changed symbol that could not be attributed to a
/// containing symbol, so it is absent from `files` above.
#[derive(Debug, serde::Serialize)]
struct UnattributedUsageOut {
    file: String,
    line: i64,
    symbol: String,
}

/// An unresolved import that names a changed file's stem — a possible
/// dependent the index could not confirm.
#[derive(Debug, serde::Serialize)]
struct UnresolvedImportOut {
    file: String,
    import_path: String,
}

/// An added symbol: new since the last index, so the index has no
/// dependents recorded for it yet.
#[derive(Debug, serde::Serialize)]
struct AddedWithoutDependents {
    file: String,
    name: String,
}

#[derive(Debug, serde::Serialize)]
struct Impact {
    dependent_count: usize,
    file_count: usize,
    files: Vec<ImpactFileGroup>,
    unattributed_usages: Vec<UnattributedUsageOut>,
    unresolved_imports: Vec<UnresolvedImportOut>,
    added_symbols_without_dependents: Vec<AddedWithoutDependents>,
}

/// Severity rank for sorting/max: `removed` breaks the caller outright,
/// `signature` may not compile, `body` is a no-op for the caller.
fn severity_rank(change: &str) -> u8 {
    match change {
        "removed" => 2,
        "signature" => 1,
        _ => 0, // "body"
    }
}

pub fn run(impact: bool, json: bool, compact: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let db_path = cwd.join(".helios/index.db");

    if !db_path.exists() {
        return Err(NoIndexError.into());
    }

    if !git::is_git_repo() {
        if json {
            let output = serde_json::json!({"error": "Not a git repository."});
            let formatted = if compact {
                serde_json::to_string(&output)?
            } else {
                serde_json::to_string_pretty(&output)?
            };
            println!("{}", formatted);
        } else {
            println!("Not a git repository. Diff requires git.");
        }
        return Ok(());
    }

    let db = Database::open(&db_path).context("opening database")?;

    let last_commit = db.get_metadata("last_indexed_commit")?;
    let last_commit = match last_commit {
        Some(c) => c,
        None => {
            if json {
                let output = serde_json::json!({"error": "No indexed commit found. Run `helios init` first."});
                let formatted = if compact {
                    serde_json::to_string(&output)?
                } else {
                    serde_json::to_string_pretty(&output)?
                };
                println!("{}", formatted);
            } else {
                println!("No indexed commit found. Run `helios init` first.");
            }
            return Ok(());
        }
    };

    // The same staleness `status` and `update` report, so the three never
    // disagree about what changed: paths relative to the index root, filtered
    // to what the index would actually re-read.
    let (modified_files, deleted_files) = indexer::stale_files(&db, &cwd, &last_commit)?;

    // Unlike the later "no *symbol* changes" gate below, this return is safe
    // even under --impact: an empty changed-file set means an empty stem set
    // for `unresolved_imports_touching` and no removed/modified symbol ids to
    // query callers for, so the impact report would be empty too — there is
    // nothing here for --impact to lose by returning early.
    if modified_files.is_empty() && deleted_files.is_empty() {
        if json {
            let output = serde_json::json!({
                "added": [],
                "removed": [],
                "modified": [],
            });
            let formatted = if compact {
                serde_json::to_string(&output)?
            } else {
                serde_json::to_string_pretty(&output)?
            };
            println!("{}", formatted);
        } else {
            println!("No symbol changes detected.");
        }
        return Ok(());
    }

    // Build a set of all indexed file paths for quick lookup
    let all_db_files: std::collections::HashSet<String> =
        db.all_files()?.into_iter().map(|f| f.path).collect();

    let mut added: Vec<AddedSymbol> = Vec::new();
    let mut removed: Vec<RemovedSymbol> = Vec::new();
    let mut modified: Vec<ModifiedSymbol> = Vec::new();

    // Process modified/added files
    for file_path in &modified_files {
        let full_path = cwd.join(file_path);
        if !full_path.exists() {
            continue;
        }

        let content = match std::fs::read_to_string(&full_path) {
            Ok(c) => c,
            Err(_) => continue, // skip binary or unreadable files
        };

        // Parse current file
        let current_symbols = match parsers::parse_file(file_path, &content)? {
            Some((_lang, result)) => result.symbols,
            None => continue, // unsupported language
        };

        // Get DB symbols for this file
        let db_symbols = get_exact_file_symbols(&db, file_path)?;

        let is_new_file = !all_db_files.contains(file_path);

        if is_new_file {
            // All symbols are added
            for sym in &current_symbols {
                added.push(AddedSymbol {
                    file: file_path.clone(),
                    name: sym.name.clone(),
                    kind: sym.kind.clone(),
                    line: sym.line,
                });
            }
        } else {
            // Compare by name. NOTE: if a file declares two symbols with the
            // same name in different scopes, this collapses them to one
            // HashMap entry (last write wins) rather than disambiguating by
            // scope — so `--impact` can end up joining against the wrong
            // same-named symbol's id in that case. Pre-existing limitation of
            // diff's matching, not something `--impact` introduces or fixes.
            let current_by_name: HashMap<&str, &crate::db::ParsedSymbol> = current_symbols
                .iter()
                .map(|s| (s.name.as_str(), s))
                .collect();
            let db_by_name: HashMap<&str, &crate::db::SymbolRecord> =
                db_symbols.iter().map(|s| (s.name.as_str(), s)).collect();

            // Added: in current but not in DB
            for (name, sym) in &current_by_name {
                if !db_by_name.contains_key(name) {
                    added.push(AddedSymbol {
                        file: file_path.clone(),
                        name: sym.name.clone(),
                        kind: sym.kind.clone(),
                        line: sym.line,
                    });
                }
            }

            // Removed: in DB but not in current
            for (name, sym) in &db_by_name {
                if !current_by_name.contains_key(name) {
                    removed.push(RemovedSymbol {
                        id: sym.id,
                        file: file_path.clone(),
                        name: sym.name.clone(),
                        kind: sym.kind.clone(),
                        line: sym.line,
                    });
                }
            }

            // Modified: same name, different line/end_line/kind/visibility/signature
            for (name, current_sym) in &current_by_name {
                if let Some(db_sym) = db_by_name.get(name)
                    && (current_sym.line != db_sym.line
                        || current_sym.end_line != db_sym.end_line
                        || current_sym.kind != db_sym.kind
                        || current_sym.visibility != db_sym.visibility
                        || signature_changed(current_sym, db_sym))
                {
                    let change = if current_sym.kind != db_sym.kind
                        || current_sym.visibility != db_sym.visibility
                        || signature_changed(current_sym, db_sym)
                    {
                        "signature"
                    } else {
                        "body"
                    };
                    modified.push(ModifiedSymbol {
                        id: db_sym.id,
                        file: file_path.clone(),
                        name: current_sym.name.clone(),
                        kind: current_sym.kind.clone(),
                        old_line: db_sym.line,
                        new_line: current_sym.line,
                        change,
                    });
                }
            }
        }
    }

    // Process deleted files: all DB symbols are removed
    for file_path in &deleted_files {
        let db_symbols = get_exact_file_symbols(&db, file_path)?;
        for sym in &db_symbols {
            removed.push(RemovedSymbol {
                id: sym.id,
                file: file_path.clone(),
                name: sym.name.clone(),
                kind: sym.kind.clone(),
                line: sym.line,
            });
        }
    }

    // --impact: join the changed symbol set to its dependents in one pass.
    let impact_report = if impact {
        let changed_files: std::collections::HashSet<String> = modified_files
            .iter()
            .chain(deleted_files.iter())
            .cloned()
            .collect();
        Some(compute_impact(
            &db,
            &removed,
            &modified,
            &added,
            &changed_files,
        )?)
    } else {
        None
    };

    // Output
    if json {
        let mut output = serde_json::json!({
            "added": added,
            "removed": removed,
            "modified": modified,
        });
        if let Some(report) = &impact_report {
            output["impact"] = serde_json::to_value(report)?;
        }
        let formatted = if compact {
            serde_json::to_string(&output)?
        } else {
            serde_json::to_string_pretty(&output)?
        };
        println!("{}", formatted);
    } else {
        let no_symbol_changes = added.is_empty() && removed.is_empty() && modified.is_empty();
        if no_symbol_changes {
            // Accurate as far as it goes — there is no *symbol*-level change
            // — but must not `return` here: with --impact, an outstanding
            // honesty-gate finding (e.g. an unresolved import naming a
            // changed file's stem) can still exist even when nothing was
            // classified as added/removed/modified, and silently dropping it
            // is exactly what the honesty gate exists to prevent.
            println!("No symbol changes detected.");
        } else {
            for sym in &added {
                println!("+ {} {} ({}:{})", sym.kind, sym.name, sym.file, sym.line);
            }
            for sym in &removed {
                println!("- {} {} ({}:{})", sym.kind, sym.name, sym.file, sym.line);
            }
            for sym in &modified {
                println!(
                    "~ {} {} ({}:{} -> {}) [{}]",
                    sym.kind, sym.name, sym.file, sym.old_line, sym.new_line, sym.change
                );
            }
        }

        match &impact_report {
            Some(report) => {
                println!();
                print_impact_text(report);
            }
            // No --impact: byte-identical to before this flag existed.
            None if no_symbol_changes => return Ok(()),
            None => {}
        }
    }

    Ok(())
}

/// Join the changed symbol set (`removed` + `modified` — the only ones the
/// index still has row ids for; `added` symbols are new since the last
/// index and cannot have recorded dependents) to their dependents in one
/// batched query, then roll the result up by dependent file and symbol so a
/// symbol touched by several changed symbols is reported once, not once per
/// trigger.
fn compute_impact(
    db: &Database,
    removed: &[RemovedSymbol],
    modified: &[ModifiedSymbol],
    added: &[AddedSymbol],
    changed_files: &std::collections::HashSet<String>,
) -> Result<Impact> {
    // symbol id -> (name, file, kind, severity) for every changed symbol
    // still in the index.
    let mut targets: HashMap<i64, (String, String, String, &'static str)> = HashMap::new();
    for r in removed {
        targets.insert(
            r.id,
            (r.name.clone(), r.file.clone(), r.kind.clone(), "removed"),
        );
    }
    for m in modified {
        targets.insert(
            m.id,
            (m.name.clone(), m.file.clone(), m.kind.clone(), m.change),
        );
    }
    let target_ids: Vec<i64> = targets.keys().copied().collect();

    // One batched query for every dependent, each row attributed to which
    // changed symbol it reaches.
    let edges = db.callers_of_with_targets(&target_ids)?;

    struct DependentAgg {
        name: String,
        file: String,
        kind: String,
        line: i64,
        triggers: Vec<ImpactTrigger>,
    }
    let mut by_caller: HashMap<i64, DependentAgg> = HashMap::new();
    for edge in &edges {
        let Some((t_name, t_file, t_kind, t_change)) = targets.get(&edge.target_symbol_id) else {
            continue;
        };
        let agg = by_caller
            .entry(edge.caller_id)
            .or_insert_with(|| DependentAgg {
                name: edge.caller_name.clone(),
                file: edge.caller_path.clone(),
                kind: edge.caller_kind.clone(),
                line: edge.caller_line,
                triggers: Vec::new(),
            });
        agg.triggers.push(ImpactTrigger {
            name: t_name.clone(),
            file: t_file.clone(),
            change: t_change,
            kind: t_kind.clone(),
        });
    }

    let mut dependent_count = 0usize;
    let mut files_map: HashMap<String, Vec<ImpactDependent>> = HashMap::new();
    for agg in by_caller.into_values() {
        let mut triggers = agg.triggers;
        triggers.sort_by(|a, b| {
            severity_rank(b.change)
                .cmp(&severity_rank(a.change))
                .then_with(|| a.name.cmp(&b.name))
        });
        let severity = match triggers.iter().map(|t| severity_rank(t.change)).max() {
            Some(2) => "removed",
            Some(1) => "signature",
            _ => "body",
        };
        dependent_count += 1;
        files_map
            .entry(agg.file.clone())
            .or_default()
            .push(ImpactDependent {
                name: agg.name,
                kind: agg.kind,
                line: agg.line,
                severity,
                triggers,
            });
    }

    let mut files: Vec<ImpactFileGroup> = files_map
        .into_iter()
        .map(|(file, mut dependents)| {
            dependents.sort_by(|a, b| {
                severity_rank(b.severity)
                    .cmp(&severity_rank(a.severity))
                    .then_with(|| a.line.cmp(&b.line))
            });
            ImpactFileGroup { file, dependents }
        })
        .collect();
    // File groups ordered by their worst dependent first, so a reviewer
    // reads breakage before moved-line noise; ties broken by path.
    files.sort_by(|a, b| {
        let a_sev = a
            .dependents
            .iter()
            .map(|d| severity_rank(d.severity))
            .max()
            .unwrap_or(0);
        let b_sev = b
            .dependents
            .iter()
            .map(|d| severity_rank(d.severity))
            .max()
            .unwrap_or(0);
        b_sev.cmp(&a_sev).then_with(|| a.file.cmp(&b.file))
    });
    let file_count = files.len();

    let unattributed_usages = db
        .unattributed_usages(&target_ids)?
        .into_iter()
        .map(|u| UnattributedUsageOut {
            file: u.file,
            line: u.line,
            symbol: u.symbol_name,
        })
        .collect();

    let unresolved_imports = unresolved_imports_touching(db, changed_files)?;

    let added_symbols_without_dependents = added
        .iter()
        .map(|a| AddedWithoutDependents {
            file: a.file.clone(),
            name: a.name.clone(),
        })
        .collect();

    Ok(Impact {
        dependent_count,
        file_count,
        files,
        unattributed_usages,
        unresolved_imports,
        added_symbols_without_dependents,
    })
}

/// The module stem a source path or an import specifier both reduce to, so
/// `unresolved_imports_touching` can compare them on equal footing: the last
/// `/`-separated segment, with its extension stripped only when that
/// extension names a language the indexer knows (`detect_language`). A
/// source filename may legitimately contain dots of its own (`lib.utils.ts`,
/// `user.model.ts`), so blindly stripping "the last dotted component" (as
/// `Path::file_stem` does) both under-strips real source paths and
/// over-strips import specifiers that merely echo one of those dots —
/// asking `detect_language` first is what tells a real `.ts` extension apart
/// from a `.utils` that only looks like one.
fn module_stem(s: &str) -> String {
    let segment = s.rsplit('/').next().unwrap_or(s);
    if parsers::detect_language(segment).is_some() {
        std::path::Path::new(segment)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| segment.to_string())
    } else {
        segment.to_string()
    }
}

/// Unresolved imports (see `Database::all_imports_with_source`) whose final
/// path segment names the stem of one of the files this diff touched.
/// Rationale: a plain package import (`lodash`) is deliberately left
/// unresolved by the resolver and would be pure noise here, but an
/// unresolved import naming a changed file's stem means some file may depend
/// on the change and the index simply could not confirm it. Residual
/// limitation: an aliased or path-mapped import that does not literally name
/// the changed file's stem is still invisible to this check.
fn unresolved_imports_touching(
    db: &Database,
    changed_files: &std::collections::HashSet<String>,
) -> Result<Vec<UnresolvedImportOut>> {
    let stems: std::collections::HashSet<String> = changed_files
        .iter()
        .map(|f| module_stem(f.as_str()))
        .collect();

    let mut out: Vec<UnresolvedImportOut> = db
        .all_imports_with_source()?
        .into_iter()
        .filter(|imp| imp.resolved_file_id.is_none())
        .filter(|imp| stems.contains(&module_stem(&imp.import_path)))
        .map(|imp| UnresolvedImportOut {
            file: imp.source_path,
            import_path: imp.import_path,
        })
        .collect();
    out.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then_with(|| a.import_path.cmp(&b.import_path))
    });
    Ok(out)
}

/// Text rendering of an `--impact` report, printed after the plain
/// added/removed/modified lines.
fn print_impact_text(report: &Impact) {
    if report.dependent_count == 0 {
        println!("Impact: no recorded dependents.");
    } else {
        println!(
            "Impact: {} dependent{} across {} file{}",
            report.dependent_count,
            if report.dependent_count == 1 { "" } else { "s" },
            report.file_count,
            if report.file_count == 1 { "" } else { "s" },
        );
        for group in &report.files {
            println!();
            println!("{}", group.file);
            for dep in &group.dependents {
                let triggers: Vec<String> = dep
                    .triggers
                    .iter()
                    .map(|t| format!("[{}] {} {}", t.change, t.kind, t.name))
                    .collect();
                println!(
                    "  {} {} ({}) <- {}",
                    dep.kind,
                    dep.name,
                    dep.line,
                    triggers.join(", ")
                );
            }
        }
    }

    if !report.unattributed_usages.is_empty() {
        println!();
        println!("Unattributed usages (not attributable to a containing symbol):");
        for u in &report.unattributed_usages {
            println!("  {}:{} -> {}", u.file, u.line, u.symbol);
        }
    }

    if !report.unresolved_imports.is_empty() {
        println!();
        println!("Unresolved imports that may hide dependents:");
        for i in &report.unresolved_imports {
            println!("  {} -> {}", i.file, i.import_path);
        }
    }

    if !report.added_symbols_without_dependents.is_empty() {
        println!();
        println!("Added symbols (new since last index, dependents not yet recorded):");
        for a in &report.added_symbols_without_dependents {
            println!("  {} -> {}", a.file, a.name);
        }
    }
}

/// Whether a symbol's params or return/declared type changed between the
/// stored record and the freshly parsed one.
///
/// A stored `None` means "not recorded" (a legacy index, or a row indexed
/// before this feature existed), not "empty" — comparing it against a fresh
/// `Some(..)` would flag every symbol in every legacy index as an API change
/// on the first diff. So only compare a field when the stored side is `Some`.
fn signature_changed(current: &crate::db::ParsedSymbol, stored: &crate::db::SymbolRecord) -> bool {
    let params_changed = match &stored.params {
        Some(stored_params) => current.params.as_deref() != Some(stored_params.as_slice()),
        None => false,
    };
    let returns_changed = match &stored.returns {
        Some(stored_returns) => current.returns.as_ref() != Some(stored_returns),
        None => false,
    };
    params_changed || returns_changed
}

/// Get symbols for an exact file path from the DB.
/// query_symbols uses LIKE with substring match, so we query then filter for exact path.
fn get_exact_file_symbols(db: &Database, file_path: &str) -> Result<Vec<crate::db::SymbolRecord>> {
    let results = db.query_symbols(
        Some(file_path),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )?;
    Ok(results
        .into_iter()
        .filter(|(_, path)| path == file_path)
        .map(|(sym, _)| sym)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_stem_strips_a_real_source_extension() {
        assert_eq!(module_stem("src/lib.utils.ts"), "lib.utils");
    }

    /// Pins the under-report bug: naively running BOTH sides through
    /// `Path::file_stem` stemmed `./lib.utils` again, down to `lib`, so it no
    /// longer matched `src/lib.utils.ts`'s (correct) stem `lib.utils` and the
    /// unresolved import silently dropped out of the report.
    #[test]
    fn module_stem_leaves_a_dotted_import_specifier_alone() {
        assert_eq!(module_stem("./lib.utils"), "lib.utils");
    }

    /// Pins the over-match bug: `Path::file_stem` on `lib.otherthing` also
    /// stripped `.otherthing` as if it were an extension, colliding with an
    /// unrelated changed file whose stem is `lib`.
    #[test]
    fn module_stem_does_not_treat_a_non_language_suffix_as_an_extension() {
        assert_eq!(module_stem("./lib.otherthing"), "lib.otherthing");
    }

    #[test]
    fn module_stem_strips_an_explicit_source_extension_on_an_import() {
        assert_eq!(module_stem("./lib.ts"), "lib");
    }

    #[test]
    fn module_stem_handles_a_bare_relative_import_with_no_extension() {
        assert_eq!(module_stem("../missing/lib"), "lib");
    }

    #[test]
    fn module_stem_leaves_a_package_import_alone() {
        assert_eq!(module_stem("lodash"), "lodash");
    }
}
