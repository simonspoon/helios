//! Control-flow graph of a single function body.
//!
//! `helios flow <symbol>` answers "what does this method do?" — the branches it
//! takes, the loops it runs, the calls it makes, and where it returns. The graph
//! stops at the function boundary: a call is a labelled node, never an expansion
//! of the callee's body.
//!
//! The graph shape is shared; the statement mapping is per-language and lives in
//! a submodule ([`rust`], [`csharp`]). A language with no mapping parses fine but
//! is rejected by [`build`] with a clear message.

mod csharp;
mod rust;

use anyhow::{Result, bail};
use tree_sitter::Node;

/// Edge label for the early return a `?` operator can take. Rust only: no other
/// mapped language has an operator that returns from the middle of a call.
pub const ERR_EXIT: &str = "Err ?";

/// How wide a label may get before it is elided.
pub(crate) const LABEL_WIDTH: usize = 60;

#[derive(Debug, Clone, serde::Serialize)]
pub struct FunctionInfo {
    pub name: String,
    pub scope: Option<String>,
    pub file: String,
    pub line: i64,
    pub end_line: i64,
    pub language: String,
    pub params: Vec<String>,
    pub returns: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct FlowNode {
    pub id: usize,
    /// entry, exit, branch, match, loop, call, return, break, continue, throw,
    /// yield
    pub kind: String,
    pub label: String,
    pub line: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct FlowEdge {
    pub from: usize,
    pub to: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct FlowGraph {
    pub function: FunctionInfo,
    pub nodes: Vec<FlowNode>,
    pub edges: Vec<FlowEdge>,
}

/// A dangling predecessor: the node the next statement should hang off, and the
/// label the connecting edge carries ("true", an arm pattern, ...).
pub(crate) type Pending = (usize, Option<String>);

/// A node's source text as one line, with runs of whitespace collapsed.
pub(crate) fn collapsed(source: &[u8], node: Node) -> String {
    let text = std::str::from_utf8(&source[node.byte_range()]).unwrap_or("");
    let mut out = String::new();
    let mut space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            space = !out.is_empty();
        } else {
            if space {
                out.push(' ');
            }
            space = false;
            out.push(ch);
        }
    }
    out
}

/// Collapse whitespace and elide, so a label stays one readable line.
pub(crate) fn label_of(source: &[u8], node: Node) -> String {
    let out = collapsed(source, node);
    if out.chars().count() > LABEL_WIDTH {
        return out.chars().take(LABEL_WIDTH - 1).collect::<String>() + "…";
    }
    out
}

pub(crate) fn line_of(node: Node) -> i64 {
    node.start_position().row as i64 + 1
}

/// What a `break` leaves. A loop in either language, or a C# `switch`, which is
/// breakable but is never what a `continue` targets.
pub(crate) struct Breakable {
    pub(crate) header: usize,
    /// The `'outer` on a labelled Rust loop, so `break 'outer` finds the right
    /// one. C# has no labelled loops, so it is always `None` there.
    pub(crate) label: Option<String>,
    /// False only for a C# `switch`: a `continue` inside one skips past it to
    /// the enclosing loop.
    pub(crate) is_loop: bool,
    /// How many C# `finally` blocks were already open when this was pushed. A
    /// jump out of here runs the ones opened since, and no others. Rust has no
    /// `finally`, so it is always 0 there.
    pub(crate) finally_depth: usize,
    /// `break`s that leave it, to be joined onto whatever follows.
    pub(crate) breaks: Vec<Pending>,
}

/// The graph under construction. The statement walkers that fill it are
/// per-language; what they share is the node and edge bookkeeping below.
pub(crate) struct Builder<'a> {
    pub(crate) source: &'a [u8],
    pub(crate) nodes: Vec<FlowNode>,
    pub(crate) edges: Vec<FlowEdge>,
    pub(crate) exit: usize,
    pub(crate) breakables: Vec<Breakable>,
}

impl<'a> Builder<'a> {
    /// A graph opened with its entry and exit nodes, in that order.
    pub(crate) fn start(source: &'a [u8], info: &FunctionInfo) -> Builder<'a> {
        let mut builder = Builder {
            source,
            nodes: Vec::new(),
            edges: Vec::new(),
            exit: 0,
            breakables: Vec::new(),
        };
        let entry = builder.add("entry", signature_of(info), info.line);
        let exit = builder.add("exit", "end".to_string(), info.end_line);
        builder.exit = exit;
        debug_assert_eq!(entry, 0);
        builder
    }

    /// The graph as the command sees it, once the body has been walked.
    pub(crate) fn finish(self, function: FunctionInfo) -> FlowGraph {
        FlowGraph {
            function,
            nodes: self.nodes,
            edges: self.edges,
        }
    }

    pub(crate) fn add(&mut self, kind: &str, label: String, line: i64) -> usize {
        let id = self.nodes.len();
        self.nodes.push(FlowNode {
            id,
            kind: kind.to_string(),
            label,
            line,
        });
        id
    }

    pub(crate) fn connect(&mut self, tails: &[Pending], to: usize) {
        for (from, label) in tails {
            self.edges.push(FlowEdge {
                from: *from,
                to,
                label: label.clone(),
            });
        }
    }

    /// Chain a node onto the pending predecessors and become the new tail.
    pub(crate) fn chain(
        &mut self,
        tails: Vec<Pending>,
        kind: &str,
        label: String,
        line: i64,
    ) -> usize {
        let id = self.add(kind, label, line);
        self.connect(&tails, id);
        id
    }
}

/// The entry node's label: how the user would write the target back.
fn signature_of(info: &FunctionInfo) -> String {
    let params = info.params.join(", ");
    let name = &info.name;
    match (&info.scope, info.returns.as_deref()) {
        (Some(s), Some(r)) => format!("{s}.{name}({params}) -> {r}"),
        (Some(s), None) => format!("{s}.{name}({params})"),
        (None, Some(r)) => format!("{name}({params}) -> {r}"),
        (None, None) => format!("{name}({params})"),
    }
}

/// How the user named the target, for an error message.
pub(crate) fn qualified(scope: Option<&str>, name: &str) -> String {
    match scope {
        Some(s) => format!("{s}.{name}"),
        None => name.to_string(),
    }
}

/// Build the flow graph for the function named `name` at `line` in `source`.
pub fn build(
    language: &str,
    source: &str,
    file: &str,
    name: &str,
    scope: Option<&str>,
    line: i64,
) -> Result<FlowGraph> {
    match language {
        "rust" => rust::build(source, file, name, scope, line),
        "csharp" => csharp::build(source, file, name, scope, line),
        _ => bail!(
            "flow does not support {language} yet (supported: rust, csharp); \
             the graph builder is per-language and only those two are mapped so far"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_language_is_rejected() {
        let err = build("python", "def f(): pass", "a.py", "f", None, 1).unwrap_err();
        assert!(err.to_string().contains("does not support python"));
    }
}
