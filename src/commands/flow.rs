use std::collections::HashSet;

use anyhow::{Context, Result, bail};

use crate::commands::deps::{SymbolTarget, parse_target};
use crate::db::Database;
use crate::errors::NoIndexError;
use crate::flow::{ERR_EXIT, FlowGraph, StoredSignature, build};
use crate::parsers::detect_language;

/// Labels carry arbitrary source text. A `"` would close the label early and a
/// `|` would close an edge label early, so both go in as mermaid entity codes.
fn escape_mermaid(label: &str) -> String {
    label.replace('"', "#quot;").replace('|', "#124;")
}

/// Nodes drawn as a decision: everything else is a step.
fn mermaid_shape(kind: &str, label: &str) -> String {
    let text = escape_mermaid(label);
    match kind {
        "entry" | "exit" => format!("([\"{text}\"])"),
        "branch" | "match" => format!("{{\"{text}\"}}"),
        "loop" => format!("{{{{\"{text}\"}}}}"),
        // A throw ends the path just as a return does, so it gets the same
        // shape rather than reading as an ordinary step.
        "return" | "throw" => format!("[/\"{text}\"/]"),
        _ => format!("[\"{text}\"]"),
    }
}

fn render_mermaid(graph: &FlowGraph) -> String {
    let mut out = String::from("flowchart TD\n");
    for node in &graph.nodes {
        out.push_str(&format!(
            "  n{}{}\n",
            node.id,
            mermaid_shape(&node.kind, &node.label)
        ));
    }
    for edge in &graph.edges {
        match &edge.label {
            Some(label) => out.push_str(&format!(
                "  n{} -->|\"{}\"| n{}\n",
                edge.from,
                escape_mermaid(label),
                edge.to
            )),
            None => out.push_str(&format!("  n{} --> n{}\n", edge.from, edge.to)),
        }
    }
    out
}

/// Walk the graph from the entry node, indenting each step. A node already
/// printed is shown as a back-reference so loops and joins terminate.
fn render_tree(graph: &FlowGraph) -> String {
    let mut out = String::new();
    let mut visited = HashSet::new();
    walk(graph, 0, 0, &mut visited, &mut out);
    out
}

fn walk(
    graph: &FlowGraph,
    id: usize,
    depth: usize,
    visited: &mut HashSet<usize>,
    out: &mut String,
) {
    let indent = "  ".repeat(depth);
    let Some(node) = graph.nodes.get(id) else {
        return;
    };

    if !visited.insert(id) {
        out.push_str(&format!(
            "{indent}-> #{id} {} ({})\n",
            node.kind, node.label
        ));
        return;
    }

    out.push_str(&format!(
        "{indent}#{id} {} {} :{}\n",
        node.kind, node.label, node.line
    ));

    // An early `?` exit annotates the call it happens on. Indenting the rest of
    // the function under it would bury the main path, so it stays a side note.
    let (aborts, flow): (Vec<_>, Vec<_>) = graph
        .edges
        .iter()
        .filter(|e| e.from == id)
        .partition(|e| e.label.as_deref() == Some(ERR_EXIT));
    for edge in aborts {
        out.push_str(&format!("{indent}  ! {ERR_EXIT} -> #{}\n", edge.to));
    }

    // A straight line of steps stays at one indent level; only a real fork nests.
    if let [edge] = flow.as_slice()
        && edge.label.is_none()
    {
        walk(graph, edge.to, depth, visited, out);
        return;
    }

    for edge in flow {
        match &edge.label {
            Some(label) => {
                out.push_str(&format!("{indent}  [{label}]\n"));
                walk(graph, edge.to, depth + 2, visited, out);
            }
            None => walk(graph, edge.to, depth + 1, visited, out),
        }
    }
}

pub fn run(
    target: &str,
    json: bool,
    compact: bool,
    mermaid: bool,
    scope: Option<&str>,
    file: Option<&str>,
    line: Option<i64>,
) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let db_path = cwd.join(".helios/index.db");

    if !db_path.exists() {
        return Err(NoIndexError.into());
    }

    let db = Database::open(&db_path).context("opening database")?;

    let symbol = match parse_target(target, scope, file) {
        Some(t) => t,
        // `parse_target` reads a bare word as a file when it looks like an
        // extension, which is right for `deps` (it accepts files) and wrong
        // here: `flow go` means the method `go`. A path still has a separator
        // or a dot, so only the bare word is reclaimed.
        None if !target.contains('/') && !target.contains('.') => SymbolTarget {
            name: target.to_string(),
            scope: None,
            file: None,
            may_be_file: false,
        },
        None => bail!("flow needs a function or method, not a file: {target}"),
    };

    // A LIKE search on the name, narrowed to functions and then to an exact
    // name match: `query_symbols` is the only lookup that carries the kind.
    let matches: Vec<_> = db
        .query_symbols(
            symbol.file.as_deref(),
            Some("fn"),
            Some(&symbol.name),
            symbol.scope.as_deref(),
            None,
            None,
            None,
            None,
            None,
        )?
        .into_iter()
        .filter(|(sym, _)| sym.name == symbol.name)
        // Overloads differ only by their parameters. The index records those
        // now, but there is no `--params` flag to disambiguate by them, so
        // `--line` remains the only way to pick one.
        .filter(|(sym, _)| line.is_none_or(|l| sym.line == l))
        .collect();

    let (sym, path) = match matches.len() {
        0 => match line {
            Some(l) => bail!("no function named {target} declared on line {l}"),
            None => bail!("no function named {target} in the index"),
        },
        1 => matches.into_iter().next().expect("one match"),
        _ => {
            let candidates: Vec<String> = matches
                .iter()
                .map(|(s, p)| match &s.scope {
                    Some(scope) if !scope.is_empty() => format!("{p}:{} ({scope})", s.line),
                    _ => format!("{p}:{}", s.line),
                })
                .collect();
            bail!(
                "{target} matches {} definitions; narrow with --file, --scope or --line:\n  {}",
                candidates.len(),
                candidates.join("\n  ")
            );
        }
    };

    let language =
        detect_language(&path).ok_or_else(|| anyhow::anyhow!("unknown language for {path}"))?;
    let source =
        std::fs::read_to_string(cwd.join(&path)).with_context(|| format!("reading {path}"))?;

    let stored = StoredSignature {
        params: sym.params.clone(),
        returns: sym.returns.clone(),
    };
    let graph = build(
        language,
        &source,
        &path,
        &sym.name,
        sym.scope.as_deref(),
        sym.line,
        Some(&stored),
    )?;

    if json {
        let formatted = if compact {
            serde_json::to_string(&graph)?
        } else {
            serde_json::to_string_pretty(&graph)?
        };
        println!("{}", formatted);
    } else if mermaid {
        print!("{}", render_mermaid(&graph));
    } else {
        println!(
            "{}:{}-{} {}",
            graph.function.file, graph.function.line, graph.function.end_line, graph.function.name
        );
        print!("{}", render_tree(&graph));
    }

    Ok(())
}
