use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::{Context, Result};

use crate::db::{CallEdge, Database, SymbolDefinition, TypeEdge, UsageKind};
use crate::errors::NoIndexError;
use crate::parsers::detect_language;

/// A symbol target: the name to look up plus the narrowing the user asked for.
pub struct SymbolTarget {
    pub name: String,
    pub scope: Option<String>,
    pub file: Option<String>,
    /// Whether an empty result should be retried as a file target. Only a bare
    /// dotted target sets this — it may be a module path rather than a
    /// qualified name.
    pub may_be_file: bool,
}

/// Which definition(s) a target names, and how it was spelled.
///
/// `deps` answers two different questions depending on its target, and a bare
/// string has to be sorted into one of them. A target is a *file* when it names
/// one — it has a path separator or a source extension — and a *symbol*
/// otherwise. `--scope` / `--file` force symbol mode, since they only narrow a
/// definition.
pub fn parse_target(target: &str, scope: Option<&str>, file: Option<&str>) -> Option<SymbolTarget> {
    // `path/to/file.ts:name` — a file and a name, so unambiguously a symbol.
    if let Some((path, name)) = target.rsplit_once(':')
        && !name.is_empty()
        && !path.is_empty()
    {
        return Some(SymbolTarget {
            name: name.to_string(),
            scope: scope.map(str::to_string),
            file: Some(path.to_string()),
            may_be_file: false,
        });
    }

    if scope.is_some() || file.is_some() {
        return Some(SymbolTarget {
            name: target.to_string(),
            scope: scope.map(str::to_string),
            file: file.map(str::to_string),
            may_be_file: false,
        });
    }

    if target.contains('/') || detect_language(target).is_some() {
        return None;
    }

    // `Class.Method`: the last segment is the name, everything before it the
    // scope. A dotted target that matches no definition is retried as a file
    // (`pkg.util.money` is how Python and C# name a module or namespace).
    match target.rsplit_once('.') {
        Some((qualifier, name)) if !qualifier.is_empty() && !name.is_empty() => {
            Some(SymbolTarget {
                name: name.to_string(),
                scope: Some(qualifier.to_string()),
                file: None,
                may_be_file: true,
            })
        }
        // Any other dotted spelling (`.`, `..money`) is not a qualified name,
        // but it is still how a module can be written, so keep the retry.
        _ => Some(SymbolTarget {
            name: target.to_string(),
            scope: None,
            file: None,
            may_be_file: target.contains('.'),
        }),
    }
}

/// BFS traversal result: (path, depth_level)
struct BfsResult {
    entries: Vec<(String, u32)>,
}

/// BFS over file dependencies or dependents up to max_depth.
/// Returns entries with their depth level (1-indexed).
fn bfs_file_deps(
    db: &Database,
    start: &str,
    max_depth: u32,
    get_neighbors: impl Fn(&Database, &str) -> Result<Vec<String>>,
) -> Result<BfsResult> {
    let mut visited = HashSet::new();
    visited.insert(start.to_string());

    let mut queue: VecDeque<(String, u32)> = VecDeque::new();
    queue.push_back((start.to_string(), 0));

    let mut entries: Vec<(String, u32)> = Vec::new();

    while let Some((current, current_depth)) = queue.pop_front() {
        if current_depth >= max_depth {
            continue;
        }

        let neighbors = get_neighbors(db, &current)?;
        for neighbor in neighbors {
            if visited.insert(neighbor.clone()) {
                let depth_level = current_depth + 1;
                entries.push((neighbor.clone(), depth_level));
                queue.push_back((neighbor, depth_level));
            }
        }
    }

    Ok(BfsResult { entries })
}

/// Which direction a call-graph BFS walks: outward to what a symbol calls,
/// or inward to what calls it.
#[derive(Clone, Copy)]
enum CallDirection {
    Callees,
    Callers,
}

/// One edge discovered while walking the call graph — the far end's
/// definition, plus how the edge was derived. A static edge (`inferred:
/// false`) always carries a `call_site`: it came from an actual
/// `references_` row. An inferred edge (dynamic dispatch, `--follow-impls`)
/// never does — there is no reference row for a call that only *might* reach
/// an override at runtime, only a `type_relations` row saying the override
/// exists.
struct CallStep {
    symbol_id: i64,
    name: String,
    scope: Option<String>,
    path: String,
    line: i64,
    call_site: Option<(String, i64, i64)>,
    inferred: bool,
    /// `type_relations.kind` ("extends"/"implements") or "overrides" when
    /// `inferred` — the answer to "inferred *how*?" `None` for a static edge.
    edge_kind: Option<String>,
    /// True for an inferred `Callers` edge (`Database::supertype_member_ids`)
    /// — the far end here is the *base* member, not an override of the
    /// symbol being expanded. Human formatting needs this to word the edge
    /// correctly: "implements"/"extends"/"overrides" describes how an
    /// override relates to its base, which reads backwards on a `Callers`
    /// hop where the direction of the inference is reversed (see
    /// `call_steps`'s doc comment). Never set on a static edge or on an
    /// inferred `Callees` edge.
    via_supertype: bool,
}

impl From<CallEdge> for CallStep {
    fn from(e: CallEdge) -> Self {
        CallStep {
            symbol_id: e.symbol_id,
            name: e.name,
            scope: e.scope,
            path: e.path,
            line: e.line,
            call_site: Some((e.call_file, e.call_line, e.call_column)),
            inferred: false,
            edge_kind: None,
            via_supertype: false,
        }
    }
}

/// `{scope}.{name}` when the symbol has a scope, `{name}` otherwise — the
/// same convention `format_override_edge` already uses for a member.
fn qualified_name(name: &str, scope: Option<&str>) -> String {
    match scope {
        Some(s) => format!("{s}.{name}"),
        None => name.to_string(),
    }
}

/// Every out-edge from `id` (a member named `name` in `scope`) in the given
/// direction: the static edges `references_` records, plus — only when
/// `follow_impls` is set and the symbol has a scope (it is a member) — the
/// dynamic-dispatch edge dynamic dispatch implies.
///
/// The two directions are *not* the same rule pointed the other way: dispatch
/// only ever widens outward from a base member to its overrides, never the
/// reverse, so which db method supplies the inferred edge depends on which
/// way the graph is being walked.
/// - `Callees`: a call to `scope.name` may, at runtime, dispatch to any
///   override of it — `Database::override_impl_ids` (the id-returning
///   sibling of `overrides_of`) widens outward to those overrides.
/// - `Callers`: the question is reversed — not "what can this call reach"
///   but "what can reach this". A caller of `scope.name` (an override) may
///   in fact be a caller of the base member it overrides, since all it knows
///   about is that base member's name — so the inferred edge here has to
///   step to the *supertype*'s member (`Database::supertype_member_ids`),
///   a bridge node whose own callers surface at the next depth. Stepping to
///   a sibling override instead (re-using `override_impl_ids` here) would be
///   wrong: an override is not a caller of the thing it overrides.
///
/// `type_relations` — and so both of these — is populated by the Rust,
/// TypeScript, Python and C# parsers only. Go and Swift record no type
/// relations at all, so `--follow-impls` is silently a no-op for those two
/// languages: there is nothing to add, not an error.
fn call_steps(
    db: &Database,
    direction: CallDirection,
    id: i64,
    name: &str,
    scope: Option<&str>,
    follow_impls: bool,
) -> Result<Vec<CallStep>> {
    let edges = match direction {
        CallDirection::Callees => db.callees_of(&[id])?,
        CallDirection::Callers => db.callers_of(&[id])?,
    };
    let mut steps: Vec<CallStep> = edges.into_iter().map(CallStep::from).collect();

    if follow_impls
        && let Some(scope) = scope
    {
        let via_supertype = matches!(direction, CallDirection::Callers);
        let inferred_targets = if via_supertype {
            db.supertype_member_ids(name, scope)?
        } else {
            db.override_impl_ids(name, scope)?
        };
        for (target_id, kind) in inferred_targets {
            // Both id-returning queries only ever return ids that came from
            // `symbols`, so this should always resolve — but a miss is
            // treated as nothing to add rather than an error, the same
            // caution `implementors_of`'s raw-name fallback already applies
            // to a supertype that never resolved.
            if let Some(def) = db.symbol_by_id(target_id)? {
                steps.push(CallStep {
                    symbol_id: target_id,
                    name: name.to_string(),
                    scope: def.scope,
                    path: def.path,
                    line: def.line,
                    call_site: None,
                    inferred: true,
                    edge_kind: Some(kind),
                    via_supertype,
                });
            }
        }
    }

    Ok(steps)
}

/// One symbol reached during a call-graph BFS: the edge that reached it, the
/// depth it was reached at, and the id it was reached *from* — the last is
/// what lets `--to` reconstruct the shortest path once a target is found.
struct CallHop {
    step: CallStep,
    depth: u32,
    from: i64,
}

/// Result of walking the call graph from a set of starting symbols: every
/// hop discovered, in breadth-first (shortest-path-first) order, plus enough
/// about how the search ended to tell a bounded answer from a complete one
/// — a bounded answer must never read as a complete one (see the "no path"
/// messages built from this in `run_to_query`, and the truncation line built
/// from it in `run`).
struct CallGraphWalk {
    hops: Vec<CallHop>,
    /// True when the depth limit stopped the search with symbols still
    /// queued and unexplored.
    truncated: bool,
    /// How many of those unexplored symbols there were. Zero when
    /// `truncated` is false.
    unexplored: usize,
    /// Total distinct symbols visited, including the starting symbols.
    explored: usize,
    /// The deepest level any visited symbol sits at.
    max_depth_reached: u32,
}

/// BFS over the call graph, outward (`Callees`) or inward (`Callers`), up to
/// `max_depth` levels from `starts`.
///
/// Every edge here is name-resolved, not type-resolved: an ambiguous callee
/// name fans out to one `references_` row per candidate definition (see
/// `Database::symbol_references`), so a single ambiguous call in the source
/// is walked as if it reached every candidate. A hop existing in this graph
/// is evidence a call *could* resolve there, never proof that it does.
///
/// "The call graph" is also only as complete as `references_` itself (see
/// `Database::callees_of`): on the tree-sitter path every row already is a
/// real call site, but a C# row indexed by the Roslyn semantic helper can be
/// a field read/write rather than a call — Roslyn sends no is-call flag over
/// the wire, so the index cannot tell those apart for C#. And a call written
/// outside every symbol's line range (top-level/module-scope code) has no
/// `container_symbol_id`, so it never enters this graph at all — a `Callers`
/// walk can miss a real caller for that reason alone.
///
/// `visited` starts pre-seeded with every starting id, so a symbol that
/// calls itself (direct recursion) is not re-expanded, and every other
/// symbol is expanded at most once no matter how many paths reach it — this
/// is what makes a cyclic call graph, including mutual recursion, terminate.
fn bfs_call_graph(
    db: &Database,
    starts: &[SymbolDefinition],
    start_name: &str,
    max_depth: u32,
    follow_impls: bool,
    direction: CallDirection,
) -> Result<CallGraphWalk> {
    let mut visited: HashSet<i64> = starts.iter().map(|d| d.id).collect();
    let mut queue: VecDeque<(i64, String, Option<String>, u32)> = starts
        .iter()
        .map(|d| (d.id, start_name.to_string(), d.scope.clone(), 0))
        .collect();

    let mut hops = Vec::new();
    let mut truncated = false;
    let mut unexplored = 0usize;
    let mut max_depth_reached = 0u32;

    while let Some((id, name, scope, depth)) = queue.pop_front() {
        max_depth_reached = max_depth_reached.max(depth);

        if depth >= max_depth {
            // Reachable (it was queued) but the depth limit stops it from
            // being expanded — the search is bounded here, not exhausted.
            truncated = true;
            unexplored += 1;
            continue;
        }

        for step in call_steps(db, direction, id, &name, scope.as_deref(), follow_impls)? {
            if visited.insert(step.symbol_id) {
                let next_depth = depth + 1;
                let next_name = step.name.clone();
                let next_scope = step.scope.clone();
                let next_id = step.symbol_id;
                hops.push(CallHop {
                    step,
                    depth: next_depth,
                    from: id,
                });
                queue.push_back((next_id, next_name, next_scope, next_depth));
            }
        }
    }

    Ok(CallGraphWalk {
        hops,
        truncated,
        unexplored,
        explored: visited.len(),
        max_depth_reached,
    })
}

/// The `[inferred: ...]` human label for a `CallStep`, or `None` for a
/// static edge. `edge_kind` ("implements"/"extends"/"overrides") reads
/// correctly on its own only for a `Callees` inferred edge, where the far
/// end really is an override of the symbol being expanded. A `Callers`
/// inferred edge (`via_supertype`) points the other way — the far end is the
/// *base* member, reached only because *its* callers may also be callers of
/// the override — so `edge_kind` alone would misdescribe it as "this
/// implements the target" when the opposite relationship holds; spell out
/// what the edge means instead of naming the underlying relation.
fn inferred_label(step: &CallStep) -> Option<String> {
    if !step.inferred {
        return None;
    }
    if step.via_supertype {
        let base = qualified_name(&step.name, step.scope.as_deref());
        Some(format!("[inferred: callers of {base} may dispatch here]"))
    } else {
        step.edge_kind.as_deref().map(|kind| format!("[inferred: {kind}]"))
    }
}

/// One `Calls`/`Callers` line (`deps --depth N>1`): indented by depth,
/// matching the existing file-mode style (`"  ".repeat(depth)`).
fn format_call_hop(hop: &CallHop) -> String {
    let inferred = match inferred_label(&hop.step) {
        Some(label) => format!(" {label}"),
        None => String::new(),
    };
    format!(
        "{}-> {}:{} {} (depth {}){}",
        "  ".repeat(hop.depth as usize),
        hop.step.path,
        hop.step.line,
        qualified_name(&hop.step.name, hop.step.scope.as_deref()),
        hop.depth,
        inferred
    )
}

/// One `Calls`/`Callers` JSON entry — no `call_site`, since this listing
/// answers "is X reachable, and how far away", not "where was each call
/// written" (that's what `--to`'s path entries are for).
///
/// `via_supertype` disambiguates `inferred`/`edge_kind` for a machine reader
/// the same way `inferred_label` does for a human one: `edge_kind` alone
/// reads as "the far end is an override of the target" (true on `Calls`),
/// but on `Callers` the far end is the *base* member the target overrides —
/// the opposite relationship. Without this field a consumer can't tell the
/// two apart from `inferred`/`edge_kind` alone.
fn call_hop_json(hop: &CallHop) -> serde_json::Value {
    serde_json::json!({
        "name": hop.step.name,
        "scope": hop.step.scope,
        "path": hop.step.path,
        "line": hop.step.line,
        "depth": hop.depth,
        "inferred": hop.step.inferred,
        "edge_kind": hop.step.edge_kind,
        "via_supertype": hop.step.via_supertype,
    })
}

/// One `--to` path-query JSON entry — same shape as `call_hop_json`, plus
/// `call_site` (`null` for an inferred hop, which has no reference row).
fn call_hop_json_with_site(hop: &CallHop) -> serde_json::Value {
    serde_json::json!({
        "name": hop.step.name,
        "scope": hop.step.scope,
        "path": hop.step.path,
        "line": hop.step.line,
        "depth": hop.depth,
        "call_site": hop.step.call_site.as_ref().map(|(file, line, column)| {
            serde_json::json!({ "file": file, "line": line, "column": column })
        }),
        "inferred": hop.step.inferred,
        "edge_kind": hop.step.edge_kind,
        "via_supertype": hop.step.via_supertype,
    })
}

/// One line of a `--to` path: the callee's definition site and name, plus
/// either where the call is written (a static edge) or why it was inferred
/// (dynamic dispatch, which has no call site to show).
fn format_path_hop(hop: &CallHop) -> String {
    let name = qualified_name(&hop.step.name, hop.step.scope.as_deref());
    match (&hop.step.call_site, inferred_label(&hop.step)) {
        (Some((file, line, _column)), _) => format!(
            "-> {}:{} {} (call at {}:{})",
            hop.step.path, hop.step.line, name, file, line
        ),
        (None, Some(label)) => {
            format!("-> {}:{} {} {}", hop.step.path, hop.step.line, name, label)
        }
        (None, None) => format!("-> {}:{} {}", hop.step.path, hop.step.line, name),
    }
}

/// `deps --to`: find the shortest call path from `from_defs` to any
/// definition `to_target` resolves to, walking callee edges up to `depth`
/// levels. Prints the path when one is found, or — when it isn't — explains
/// *why* in a way that can never be mistaken for the other case: hitting the
/// depth limit with symbols still unexplored ("a longer path may exist") is
/// a fundamentally different answer from exhausting the whole reachable set
/// ("no static call path exists"). Conflating the two would let a bounded
/// search masquerade as a complete one.
#[allow(clippy::too_many_arguments)]
fn run_to_query(
    db: &Database,
    from_target: &str,
    from_name: &str,
    from_defs: &[SymbolDefinition],
    to_target: &str,
    depth: u32,
    follow_impls: bool,
    json: bool,
    compact: bool,
) -> Result<()> {
    let to_parsed = parse_target(to_target, None, None);
    let to_defs: Vec<SymbolDefinition> = match &to_parsed {
        Some(t) => db.find_definitions(&t.name, t.scope.as_deref(), t.file.as_deref())?,
        None => Vec::new(),
    };
    if to_defs.is_empty() {
        anyhow::bail!("--to target '{to_target}' resolves to no definition");
    }
    let to_ids: HashSet<i64> = to_defs.iter().map(|d| d.id).collect();

    // A start symbol that `--to` also names is a zero-hop path — worth
    // reporting as found rather than walking straight past it.
    let trivial = from_defs.iter().find(|d| to_ids.contains(&d.id));

    let walk = bfs_call_graph(db, from_defs, from_name, depth, follow_impls, CallDirection::Callees)?;
    let by_id: HashMap<i64, &CallHop> =
        walk.hops.iter().map(|h| (h.step.symbol_id, h)).collect();
    // Hops are appended in BFS (shortest-path-first) order, so the first
    // match by iteration order is already the shortest.
    let found_hop = walk.hops.iter().find(|h| to_ids.contains(&h.step.symbol_id));

    let path: Option<Vec<&CallHop>> = if trivial.is_some() {
        Some(Vec::new())
    } else {
        found_hop.map(|target_hop| {
            let mut chain = vec![target_hop];
            let mut current = target_hop;
            while let Some(prev) = by_id.get(&current.from) {
                chain.push(prev);
                current = prev;
            }
            chain.reverse();
            chain
        })
    };
    let found = path.is_some();

    let start_def = trivial.or_else(|| {
        path.as_ref()
            .and_then(|chain| chain.first())
            .and_then(|first| from_defs.iter().find(|d| d.id == first.from))
    });

    if json {
        let output = serde_json::json!({
            "target": from_target,
            "to": to_target,
            "depth": depth,
            "path": path.as_ref().map(|chain| {
                chain.iter().map(|h| call_hop_json_with_site(h)).collect::<Vec<_>>()
            }),
            "found": found,
            "truncated": if found { false } else { walk.truncated },
            "explored": walk.explored,
            "max_depth_reached": walk.max_depth_reached,
        });
        let formatted = if compact {
            serde_json::to_string(&output)?
        } else {
            serde_json::to_string_pretty(&output)?
        };
        println!("{}", formatted);
        return Ok(());
    }

    if let Some(chain) = &path {
        println!(
            "Path from {} to {} ({} calls, depth limit {}):",
            from_target,
            to_target,
            chain.len(),
            depth
        );
        if let Some(def) = start_def {
            println!("  {}:{} {}", def.path, def.line, qualified_name(from_name, def.scope.as_deref()));
        }
        for hop in chain {
            println!("  {}", format_path_hop(hop));
        }
    } else if walk.truncated {
        println!("No path from {} to {} within depth {}.", from_target, to_target, depth);
        println!(
            "The search was cut off at depth {} with {} symbols still unexplored — a longer path may exist. Re-run with a larger --depth.",
            depth, walk.unexplored
        );
    } else {
        println!("No path from {} to {}.", from_target, to_target);
        println!(
            "The whole reachable set from {} was searched ({} symbols, max depth {}) — no static call path exists in the index.",
            from_target, walk.explored, walk.max_depth_reached
        );
        if !follow_impls {
            println!(
                "Calls through an interface or trait object are not static edges; re-run with --follow-impls to also follow implementors."
            );
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    target: &str,
    json: bool,
    compact: bool,
    depth: Option<u32>,
    scope: Option<&str>,
    file: Option<&str>,
    reads: bool,
    writes: bool,
    to: Option<&str>,
    follow_impls: bool,
) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let db_path = cwd.join(".helios/index.db");

    if !db_path.exists() {
        return Err(NoIndexError.into());
    }

    let db = Database::open(&db_path).context("opening database")?;

    // Depth 1 is useless for a path query — it could never find a call more
    // than one hop away — so `--to` gets a much larger default. Symbol/file
    // targets without `--to` keep the depth-1 default they always had, so an
    // explicit `--depth` is the only thing that ever changes that behavior.
    let depth = depth.unwrap_or(if to.is_some() { 10 } else { 1 });

    // `--reads`/`--writes` narrow `symbol_references` to usage_kind — both
    // together, or neither, means no filtering. `unknown` rows never satisfy
    // either flag: an unclassified usage is not evidence of a read or a write.
    let usage_kinds: Option<Vec<UsageKind>> = match (reads, writes) {
        (true, false) => Some(vec![UsageKind::Read, UsageKind::ReadWrite]),
        (false, true) => Some(vec![UsageKind::Write, UsageKind::ReadWrite]),
        _ => None,
    };

    // Resolve a symbol target to the definitions it selects. Only those
    // definitions' ids are queried, so `--scope`/`--file`/`Class.Method`
    // exclude the same-named definitions the user did not mean.
    let symbol = parse_target(target, scope, file);
    let defs: Vec<SymbolDefinition> = match &symbol {
        Some(t) => db.find_definitions(&t.name, t.scope.as_deref(), t.file.as_deref())?,
        None => Vec::new(),
    };
    let is_file = match &symbol {
        None => true,
        Some(t) => t.may_be_file && defs.is_empty(),
    };
    let symbol_ids: Vec<i64> = defs.iter().map(|d| d.id).collect();
    let target_name = symbol.as_ref().map(|t| t.name.as_str()).unwrap_or(target);

    // `--to` answers a wholly different question (a path between two
    // symbols, not one symbol's own deps/refs/type-edges) and has its own
    // output shape (see `run_to_query`'s doc comment and the CLI-surface
    // spec), so it short-circuits everything below.
    if let Some(to_target) = to {
        return run_to_query(&db, target, target_name, &defs, to_target, depth, follow_impls, json, compact);
    }

    // Type edges (supertypes/implementors/overrides) — symbol mode only, a
    // file target has no type relations of its own. Gathered once and shared
    // between the human and `--json` branches below.
    let (supertypes, implementors, overrides, edge_languages) = if is_file {
        (Vec::new(), Vec::new(), Vec::new(), Vec::new())
    } else {
        let supertypes = db.supertypes_of(&symbol_ids)?;
        let implementors = db.implementors_of(&symbol_ids, target_name)?;
        // Overrides only make sense for a member (a definition with a scope)
        // — a bare type has nothing to override. Query once per distinct
        // scope among the resolved definitions, since an ambiguous name can
        // resolve to definitions in more than one scope.
        let scopes: HashSet<&str> = defs.iter().filter_map(|d| d.scope.as_deref()).collect();
        let mut overrides = Vec::new();
        for scope in scopes {
            overrides.extend(db.overrides_of(target_name, scope)?);
        }
        // Provenance: which languages' files actually contributed an edge to
        // THIS answer, so a partial-coverage answer never reads as complete.
        let mut edge_languages: Vec<String> = supertypes
            .iter()
            .chain(&implementors)
            .chain(&overrides)
            .map(|e| e.language.clone())
            .collect();
        edge_languages.sort();
        edge_languages.dedup();
        (supertypes, implementors, overrides, edge_languages)
    };

    // Transitive call-graph reachability (`--depth N>1`, symbol targets
    // only) — additive, and gathered once here so depth-1 output (both
    // human and JSON) is untouched: neither branch below even looks at
    // these unless `depth > 1`.
    let (calls_walk, callers_walk) = if !is_file && depth > 1 {
        let calls = bfs_call_graph(&db, &defs, target_name, depth, follow_impls, CallDirection::Callees)?;
        let callers = bfs_call_graph(&db, &defs, target_name, depth, follow_impls, CallDirection::Callers)?;
        (Some(calls), Some(callers))
    } else {
        (None, None)
    };

    if json {
        if is_file {
            let deps_result =
                bfs_file_deps(&db, target, depth, |db, path| db.file_dependencies(path))?;
            let dependents_result =
                bfs_file_deps(&db, target, depth, |db, path| db.file_dependents(path))?;

            let deps_json: Vec<serde_json::Value> = deps_result
                .entries
                .iter()
                .map(|(path, d)| {
                    serde_json::json!({
                        "path": path,
                        "depth": d,
                    })
                })
                .collect();

            let dependents_json: Vec<serde_json::Value> = dependents_result
                .entries
                .iter()
                .map(|(path, d)| {
                    serde_json::json!({
                        "path": path,
                        "depth": d,
                    })
                })
                .collect();

            let output = serde_json::json!({
                "target": target,
                "depth": depth,
                "dependencies": deps_json,
                "dependents": dependents_json,
            });

            let formatted = if compact {
                serde_json::to_string(&output)?
            } else {
                serde_json::to_string_pretty(&output)?
            };
            println!("{}", formatted);
        } else {
            // Symbol mode: ignore depth, keep depth=1 behavior
            let deps = db.symbol_dependencies(&symbol_ids)?;
            let refs = db.symbol_references(&symbol_ids, usage_kinds.as_deref())?;

            let mut output = serde_json::json!({
                "target": target,
                "definitions": defs,
                "supertypes": supertypes,
                "implementors": implementors,
                "overrides": overrides,
                "edge_languages": edge_languages,
                "dependencies": deps,
                "dependents": refs.iter()
                    .map(|site| {
                        serde_json::json!({
                            "file": site.path,
                            "line": site.line,
                            "column": site.column,
                            "container": site.container,
                            "usage_kind": site.usage_kind.as_str(),
                        })
                    })
                    .collect::<Vec<_>>(),
            });

            // Additive: only present when `--depth` asked for more than the
            // direct references above, so depth==1 JSON is byte-identical
            // to before this field existed.
            if let (Some(calls), Some(callers)) = (&calls_walk, &callers_walk) {
                let obj = output.as_object_mut().expect("json! object");
                obj.insert(
                    "calls".to_string(),
                    serde_json::Value::Array(calls.hops.iter().map(call_hop_json).collect()),
                );
                obj.insert("calls_truncated".to_string(), serde_json::json!(calls.truncated));
                obj.insert(
                    "callers".to_string(),
                    serde_json::Value::Array(callers.hops.iter().map(call_hop_json).collect()),
                );
                obj.insert("callers_truncated".to_string(), serde_json::json!(callers.truncated));
            }

            let formatted = if compact {
                serde_json::to_string(&output)?
            } else {
                serde_json::to_string_pretty(&output)?
            };
            println!("{}", formatted);
        }
    } else {
        if is_file {
            let deps_result =
                bfs_file_deps(&db, target, depth, |db, path| db.file_dependencies(path))?;
            let dependents_result =
                bfs_file_deps(&db, target, depth, |db, path| db.file_dependents(path))?;

            if !deps_result.entries.is_empty() {
                println!("Dependencies (what {} imports):", target);
                for (dep, d) in &deps_result.entries {
                    let indent = "  ".repeat(*d as usize);
                    println!("{}-> {} (depth {})", indent, dep, d);
                }
            }

            if !dependents_result.entries.is_empty() {
                println!("Dependents (what imports {}):", target);
                for (dep, d) in &dependents_result.entries {
                    let indent = "  ".repeat(*d as usize);
                    println!("{}-> {} (depth {})", indent, dep, d);
                }
            }

            if deps_result.entries.is_empty() && dependents_result.entries.is_empty() {
                println!("No dependencies found for {}", target);
            }
        } else {
            // Symbol mode: ignore depth, keep depth=1 behavior
            let deps = db.symbol_dependencies(&symbol_ids)?;
            let refs = db.symbol_references(&symbol_ids, usage_kinds.as_deref())?;

            if !defs.is_empty() {
                println!("Definitions of {}:", target);
                for def in &defs {
                    match &def.scope {
                        Some(s) => println!("  {}:{} (scope {})", def.path, def.line, s),
                        None => println!("  {}:{}", def.path, def.line),
                    }
                }
            }

            if !supertypes.is_empty() {
                println!("Supertypes (what {} extends/implements):", target);
                for edge in &supertypes {
                    println!("  {}", format_type_edge(edge));
                }
            }

            if !implementors.is_empty() {
                println!("Implementors (what extends/implements {}):", target);
                for edge in &implementors {
                    println!("  {}", format_type_edge(edge));
                }
            }

            if !overrides.is_empty() {
                println!("Overrides (what overrides {}):", target);
                for edge in &overrides {
                    println!("  {}", format_override_edge(edge));
                }
            }

            if !deps.is_empty() {
                println!("Dependencies (imports in files defining {}):", target);
                for dep in &deps {
                    println!("  {} -> {} (import)", target, dep);
                }
            }

            if !refs.is_empty() {
                println!("References (where {} is used):", target);
                for site in &refs {
                    // `read` and `unknown` print nothing extra, so today's
                    // output is unchanged for the common case.
                    let suffix = match site.usage_kind {
                        UsageKind::Write => " [write]",
                        UsageKind::ReadWrite => " [readwrite]",
                        UsageKind::Read | UsageKind::Unknown => "",
                    };
                    match &site.container {
                        Some(c) => println!(
                            "  {}:{}:{} in {} -> {} (reference){}",
                            site.path, site.line, site.column, c, target, suffix
                        ),
                        None => println!(
                            "  {}:{}:{} -> {} (reference){}",
                            site.path, site.line, site.column, target, suffix
                        ),
                    }
                }
            }

            // Additive: only printed for `--depth N>1`, so depth-1 output is
            // unchanged from before this existed.
            if let (Some(calls), Some(callers)) = (&calls_walk, &callers_walk) {
                if !calls.hops.is_empty() {
                    println!("Calls (what {} reaches, transitively):", target);
                    for hop in &calls.hops {
                        println!("{}", format_call_hop(hop));
                    }
                }
                if !callers.hops.is_empty() {
                    println!("Callers (what reaches {}, transitively):", target);
                    for hop in &callers.hops {
                        println!("{}", format_call_hop(hop));
                    }
                }
                // Printed per direction, and only for a direction that
                // actually truncated — a direction whose BFS ran to
                // exhaustion has nothing bounded to disclose.
                if calls.truncated {
                    println!(
                        "Calls truncated at depth {}: {} symbols were still unexplored.",
                        depth, calls.unexplored
                    );
                }
                if callers.truncated {
                    println!(
                        "Callers truncated at depth {}: {} symbols were still unexplored.",
                        depth, callers.unexplored
                    );
                }
            }

            let calls_empty = calls_walk.as_ref().is_none_or(|w| w.hops.is_empty());
            let callers_empty = callers_walk.as_ref().is_none_or(|w| w.hops.is_empty());
            if deps.is_empty()
                && refs.is_empty()
                && supertypes.is_empty()
                && implementors.is_empty()
                && overrides.is_empty()
                && calls_empty
                && callers_empty
            {
                println!("No dependencies found for {}", target);
            }

            // Ends the answer, not just this section — a reader who sees only
            // "csharp" here knows a same-named Python edge could exist but was
            // not part of what was actually returned (see the raw-name
            // fallback in `implementors_of`).
            if !edge_languages.is_empty() {
                println!("Type edges from: {}", edge_languages.join(", "));
            }
        }
    }

    Ok(())
}

/// One `Supertypes`/`Implementors` line: `<file>:<line> <sub> -> <super>
/// (<kind>[, external])`. The leading `file:line` matches the convention
/// every other `deps` section already uses (Definitions' `path:line`,
/// References' `path:line:col`) — it names the symbol's declaring location,
/// not merely its name, and it is the load-bearing half of the mitigation for
/// `implementors_of`'s raw-name fallback: that query has no language or file
/// scoping, so an `external` hit can come from a same-named type in a wholly
/// different language. Printing the declaring file on every row (together
/// with the `Type edges from:` provenance line) is what lets a reader notice.
fn format_type_edge(edge: &TypeEdge) -> String {
    if edge.external {
        format!(
            "{}:{} {} -> {} ({}, external)",
            edge.file, edge.line, edge.sub_name, edge.super_name, edge.kind
        )
    } else {
        format!(
            "{}:{} {} -> {} ({})",
            edge.file, edge.line, edge.sub_name, edge.super_name, edge.kind
        )
    }
}

/// One `Overrides` line: the overriding member's own file:line, scope and
/// name, against the base scope it overrides — `overrides_of` always returns
/// kind "overrides", so unlike `format_type_edge` there is no external case.
fn format_override_edge(edge: &TypeEdge) -> String {
    match &edge.sub_scope {
        Some(scope) => format!(
            "{}:{} {}.{} overrides {}.{}",
            edge.file, edge.line, scope, edge.sub_name, edge.super_name, edge.sub_name
        ),
        None => format!(
            "{}:{} {} overrides {}.{}",
            edge.file, edge.line, edge.sub_name, edge.super_name, edge.sub_name
        ),
    }
}
