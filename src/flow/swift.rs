//! The Swift statement mapping for `helios flow`.
//!
//! A target is a `func` (the only Swift callable the indexer records as `fn`).
//! `defer` blocks are not drawn: tree-sitter-swift has no node for them (a
//! `defer { }` parses as a call to a function named `defer`), so they are
//! skipped rather than shown as a fake call. Closures and nested functions
//! are never expanded, and `fallthrough` ends its switch entry like a `break`.

use anyhow::{Context, Result, anyhow};
use tree_sitter::{Node, Parser};

use super::{
    Breakable, Builder, FlowEdge, FlowGraph, FunctionInfo, LABEL_WIDTH, Pending, StoredSignature,
    collapsed, label_of, line_of, qualified,
};
use crate::parsers::swift::find_scope;

/// Locate the `func` named `name` whose body contains `line`, exactly as the
/// C# side does: `scope` is a hard filter, the innermost match at the line
/// wins, and a line that has moved on since indexing falls back to a sole
/// namesake.
fn find_swift_function<'t>(
    root: Node<'t>,
    source: &[u8],
    name: &str,
    scope: Option<&str>,
    line: i64,
) -> Option<Node<'t>> {
    let mut named: Vec<Node> = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "function_declaration"
            && let Some(name_node) = node.child_by_field_name("name")
            && std::str::from_utf8(&source[name_node.byte_range()]).unwrap_or("") == name
            && scope.is_none_or(|s| find_scope(source, name_node).as_deref() == Some(s))
        {
            named.push(node);
        }
        for i in 0..node.named_child_count() as u32 {
            if let Some(child) = node.named_child(i) {
                stack.push(child);
            }
        }
    }

    let at_line = named
        .iter()
        .filter(|n| (line_of(**n)..=(n.end_position().row as i64 + 1)).contains(&line))
        .min_by_key(|n| n.byte_range().len());

    match at_line {
        Some(node) => Some(*node),
        None => match named.as_slice() {
            [only] => Some(*only),
            _ => None,
        },
    }
}

/// The Swift walker; see the C# one for why the graph state is wrapped.
struct SwiftBuilder<'a> {
    b: Builder<'a>,
    /// Each switch node and its arm labels in source order, for [`Self::reorder_arms`].
    arm_order: Vec<(usize, Vec<String>)>,
}

impl<'t> SwiftBuilder<'_> {
    /// The statements of a `statements` node (or the empty list when a body
    /// has none). Returns the tails that fall out of it.
    fn block(&mut self, node: Option<Node<'t>>, tails: Vec<Pending>) -> Vec<Pending> {
        let Some(node) = node else {
            return tails;
        };
        let statements: Vec<Node> = (0..node.named_child_count() as u32)
            .filter_map(|i| node.named_child(i))
            .filter(|c| !c.is_extra())
            .collect();
        self.sequence(&statements, tails)
    }

    fn sequence(&mut self, statements: &[Node<'t>], mut tails: Vec<Pending>) -> Vec<Pending> {
        // `outer: for ...` is a `statement_label` sibling ahead of the loop.
        let mut label: Option<String> = None;
        for statement in statements {
            // Unreachable code contributes no graph.
            if tails.is_empty() {
                break;
            }
            if statement.kind() == "statement_label" {
                label = Some(
                    collapsed(self.b.source, *statement)
                        .trim_end_matches(':')
                        .trim()
                        .to_string(),
                );
                continue;
            }
            tails = self.statement(*statement, label.take(), tails);
        }
        tails
    }

    fn statement(
        &mut self,
        node: Node<'t>,
        label: Option<String>,
        tails: Vec<Pending>,
    ) -> Vec<Pending> {
        match node.kind() {
            "if_statement" => self.if_stmt(node, tails),
            "guard_statement" => self.guard_stmt(node, tails),
            "switch_statement" => self.switch_stmt(node, label, tails),
            "for_statement" | "while_statement" | "repeat_while_statement" => {
                self.loop_stmt(node, label, tails)
            }
            "do_statement" => self.do_stmt(node, tails),
            "control_transfer_statement" => self.transfer(node, tails),
            // A `defer { }` parses as a call named `defer`; see the module docs.
            "call_expression" if is_defer(self.b.source, node) => tails,
            _ => self.emit_calls(node, tails),
        }
    }

    /// `return`, `throw`, `break`, `continue` and `fallthrough`.
    fn transfer(&mut self, node: Node<'t>, tails: Vec<Pending>) -> Vec<Pending> {
        let keyword = node
            .child(0)
            .map(|c| collapsed(self.b.source, c))
            .unwrap_or_default();
        // The operand of a `break`/`continue` is a loop label, not a value.
        let target_label = node
            .child_by_field_name("result")
            .filter(|_| keyword == "break" || keyword == "continue")
            .map(|r| collapsed(self.b.source, r));
        match keyword.as_str() {
            "return" => {
                let tails = self.emit_calls(node, tails);
                self.exits(node, "return", tails)
            }
            "throw" => {
                let tails = self.emit_calls(node, tails);
                self.exits(node, "throw", tails)
            }
            "break" => {
                let target = self.target(target_label.as_deref(), false);
                let id = self
                    .b
                    .chain(tails, "break", label_of(self.b.source, node), line_of(node));
                match target.and_then(|i| self.b.breakables.get_mut(i)) {
                    Some(ctx) => ctx.breaks.push((id, None)),
                    // Only in code that does not compile; keep the node.
                    None => self.b.edges.push(FlowEdge {
                        from: id,
                        to: self.b.exit,
                        label: None,
                    }),
                }
                Vec::new()
            }
            "continue" => {
                let target = self.target(target_label.as_deref(), true);
                let id = self.b.chain(
                    tails,
                    "continue",
                    label_of(self.b.source, node),
                    line_of(node),
                );
                if let Some(header) = target.map(|i| self.b.breakables[i].header) {
                    self.b.edges.push(FlowEdge {
                        from: id,
                        to: header,
                        label: None,
                    });
                }
                Vec::new()
            }
            // `fallthrough` runs the next entry's body, which this graph does
            // not link; the entry just ends here and joins the switch's exit.
            _ => tails,
        }
    }

    /// Index of the breakable a `break`/`continue` leaves: the named one when a
    /// label is given, else the innermost (loops only, for `continue`; a
    /// `switch` is breakable but never continuable).
    fn target(&self, label: Option<&str>, loops_only: bool) -> Option<usize> {
        self.b.breakables.iter().rposition(|c| match label {
            Some(l) => c.label.as_deref() == Some(l),
            None => !loops_only || c.is_loop,
        })
    }

    /// A node that ends the function: its edge goes to the exit.
    fn exits(&mut self, node: Node<'t>, kind: &str, tails: Vec<Pending>) -> Vec<Pending> {
        let id = self
            .b
            .chain(tails, kind, label_of(self.b.source, node), line_of(node));
        self.b.edges.push(FlowEdge {
            from: id,
            to: self.b.exit,
            label: None,
        });
        Vec::new()
    }

    /// The condition nodes of an `if`/`guard`/`while`: the `condition:` and
    /// `bound_identifier:` children, which tree-sitter-swift repeats as a
    /// flat list (`if let a = b, c > 1`).
    fn conditions(node: Node<'t>) -> Vec<Node<'t>> {
        let (mut c1, mut c2) = (node.walk(), node.walk());
        let mut out: Vec<Node> = node
            .children_by_field_name("condition", &mut c1)
            .chain(node.children_by_field_name("bound_identifier", &mut c2))
            .collect();
        out.sort_by_key(|n| n.start_byte());
        out
    }

    /// The conditions as one label, from the first to the last in the source.
    fn condition_label(&self, conditions: &[Node<'t>]) -> String {
        let (Some(first), Some(last)) = (conditions.first(), conditions.last()) else {
            return String::new();
        };
        let text =
            std::str::from_utf8(&self.b.source[first.start_byte()..last.end_byte()]).unwrap_or("");
        elide(&collapse(text))
    }

    /// Run the calls in every condition, in order, before the node they feed.
    fn emit_condition_calls(
        &mut self,
        conditions: &[Node<'t>],
        mut tails: Vec<Pending>,
    ) -> Vec<Pending> {
        for c in conditions {
            tails = self.emit_calls(*c, tails);
        }
        tails
    }

    /// The `statements` children of a node split around its `else` token: the
    /// bodies before it and after it.
    fn split_else(node: Node<'t>) -> (Option<Node<'t>>, Option<Node<'t>>) {
        let mut then = None;
        let mut other = None;
        let mut seen_else = false;
        for i in 0..node.child_count() as u32 {
            let Some(child) = node.child(i) else {
                continue;
            };
            match child.kind() {
                "else" => seen_else = true,
                "statements" | "if_statement" if seen_else => other = Some(child),
                "statements" => then = Some(child),
                _ => {}
            }
        }
        (then, other)
    }

    fn if_stmt(&mut self, node: Node<'t>, tails: Vec<Pending>) -> Vec<Pending> {
        let conditions = Self::conditions(node);
        let tails = self.emit_condition_calls(&conditions, tails);
        let label = self.condition_label(&conditions);
        let id = self.b.chain(tails, "branch", label, line_of(node));

        let (then, other) = Self::split_else(node);
        let mut out = Vec::new();
        out.extend(self.block(then, vec![(id, Some("true".into()))]));
        match other {
            // `else if` nests another `if_statement`; `else { }` is a block.
            Some(alt) if alt.kind() == "if_statement" => {
                out.extend(self.if_stmt(alt, vec![(id, Some("false".into()))]));
            }
            Some(alt) => out.extend(self.block(Some(alt), vec![(id, Some("false".into()))])),
            // `else { }` with nothing in it, or no else at all.
            None => out.push((id, Some("false".into()))),
        }
        out
    }

    /// `guard cond else { ... }`: the else body must leave, so only the true
    /// edge carries on past the statement.
    fn guard_stmt(&mut self, node: Node<'t>, tails: Vec<Pending>) -> Vec<Pending> {
        let conditions = Self::conditions(node);
        let tails = self.emit_condition_calls(&conditions, tails);
        let label = self.condition_label(&conditions);
        let id = self.b.chain(tails, "branch", label, line_of(node));

        let (_, body) = Self::split_else(node);
        let mut out = vec![(id, Some("true".into()))];
        out.extend(self.block(body, vec![(id, Some("false".into()))]));
        out
    }

    /// `switch v { case ...: }`. Swift entries do not fall through, so each
    /// entry's body is entered only by its own patterns.
    fn switch_stmt(
        &mut self,
        node: Node<'t>,
        label: Option<String>,
        tails: Vec<Pending>,
    ) -> Vec<Pending> {
        let value = node.child_by_field_name("expr");
        let tails = match value {
            Some(v) => self.emit_calls(v, tails),
            None => tails,
        };
        let text = value
            .map(|v| label_of(self.b.source, v))
            .unwrap_or_default();
        let id = self
            .b
            .chain(tails, "match", format!("switch {text}"), line_of(node));

        self.b.breakables.push(Breakable {
            header: id,
            label,
            is_loop: false,
            finally_depth: 0,
            breaks: Vec::new(),
        });

        let mut out = Vec::new();
        let mut order: Vec<String> = Vec::new();
        for i in 0..node.named_child_count() as u32 {
            let Some(entry) = node.named_child(i) else {
                continue;
            };
            if entry.kind() != "switch_entry" {
                continue;
            }
            let label = self.entry_label(entry);
            order.push(label.clone());
            let mut entry_tails = vec![(id, Some(label))];
            // A `where` guard is evaluated before the body, after the match.
            if let Some(guard) = self.entry_guard(entry) {
                entry_tails = self.emit_calls(guard, entry_tails);
            }
            let body = (0..entry.named_child_count() as u32)
                .filter_map(|i| entry.named_child(i))
                .find(|c| c.kind() == "statements");
            out.extend(self.block(body, entry_tails));
        }

        let ctx = self.b.breakables.pop().expect("switch pushed above");
        out.extend(ctx.breaks);
        if order.is_empty() {
            out.push((id, None));
        }
        self.arm_order.push((id, order));
        out
    }

    /// The `where` expression of a switch entry, if it has one.
    fn entry_guard(&self, entry: Node<'t>) -> Option<Node<'t>> {
        let mut seen = false;
        for i in 0..entry.named_child_count() as u32 {
            let child = entry.named_child(i)?;
            if child.kind() == "where_keyword" {
                seen = true;
            } else if seen && child.kind() != "statements" {
                return Some(child);
            }
        }
        None
    }

    /// `.a, .b` / `default` / `let .c(n) where n > 1` for an entry's edge.
    fn entry_label(&self, entry: Node<'t>) -> String {
        let mut patterns = Vec::new();
        let mut is_default = false;
        for i in 0..entry.named_child_count() as u32 {
            let Some(child) = entry.named_child(i) else {
                continue;
            };
            match child.kind() {
                "switch_pattern" => patterns.push(label_of(self.b.source, child)),
                "default_keyword" => is_default = true,
                _ => {}
            }
        }
        let mut label = if is_default {
            "default".to_string()
        } else {
            patterns.join(", ")
        };
        if let Some(guard) = self.entry_guard(entry) {
            label = format!("{label} where {}", label_of(self.b.source, guard));
        }
        label
    }

    /// Put each switch node's outgoing edges back into source order; an arm
    /// whose body makes no call gets its edge only when the arms join.
    fn reorder_arms(&mut self) {
        for (from, labels) in &self.arm_order {
            let slots: Vec<usize> = (0..self.b.edges.len())
                .filter(|i| self.b.edges[*i].from == *from)
                .collect();
            let mut edges: Vec<FlowEdge> = slots.iter().map(|i| self.b.edges[*i].clone()).collect();
            edges.sort_by_key(|e| {
                labels
                    .iter()
                    .position(|l| Some(l.as_str()) == e.label.as_deref())
                    .unwrap_or(usize::MAX)
            });
            for (slot, edge) in slots.into_iter().zip(edges) {
                self.b.edges[slot] = edge;
            }
        }
    }

    fn loop_stmt(
        &mut self,
        node: Node<'t>,
        label: Option<String>,
        tails: Vec<Pending>,
    ) -> Vec<Pending> {
        // Calls in the header sit before it: exact for a `for` collection,
        // an approximation for a condition that is re-tested every time round.
        let mut tails = tails;
        let mut cursor = node.walk();
        let heads: Vec<Node> = if node.kind() == "for_statement" {
            node.children_by_field_name("collection", &mut cursor)
                .collect()
        } else {
            Self::conditions(node)
        };
        for head in heads {
            tails = self.emit_calls(head, tails);
        }
        let id = self
            .b
            .chain(tails, "loop", self.loop_header_label(node), line_of(node));

        self.b.breakables.push(Breakable {
            header: id,
            label,
            is_loop: true,
            finally_depth: 0,
            breaks: Vec::new(),
        });
        let body = (0..node.named_child_count() as u32)
            .filter_map(|i| node.named_child(i))
            .find(|c| c.kind() == "statements");
        let body_tails = self.block(body, vec![(id, Some("body".into()))]);
        let ctx = self.b.breakables.pop().expect("loop pushed above");

        for (from, label) in body_tails {
            self.b.edges.push(FlowEdge {
                from,
                to: id,
                label: Some(label.unwrap_or_else(|| "repeat".into())),
            });
        }

        let mut out = ctx.breaks;
        out.push((id, Some("done".into())));
        out
    }

    /// `for x in xs where ...`, `while c`, `repeat while c`: the header's own
    /// source up to the body's `{`.
    fn loop_header_label(&self, node: Node<'t>) -> String {
        if node.kind() == "repeat_while_statement" {
            let conditions = Self::conditions(node);
            return format!("repeat while {}", self.condition_label(&conditions));
        }
        header_text(self.b.source, node)
    }

    /// `do { } catch { }`. Every catch hangs off the entry of the `do`, because
    /// the statement that threw is not known here. A `do` with no `catch` is
    /// just a scope and adds no node.
    fn do_stmt(&mut self, node: Node<'t>, tails: Vec<Pending>) -> Vec<Pending> {
        let body = (0..node.named_child_count() as u32)
            .filter_map(|i| node.named_child(i))
            .find(|c| c.kind() == "statements");
        let catches: Vec<Node> = (0..node.named_child_count() as u32)
            .filter_map(|i| node.named_child(i))
            .filter(|c| c.kind() == "catch_block")
            .collect();
        if catches.is_empty() {
            return self.block(body, tails);
        }

        let id = self
            .b
            .chain(tails, "branch", "do".to_string(), line_of(node));
        let mut joins = self.block(body, vec![(id, Some("do".into()))]);
        for clause in catches {
            let entry = vec![(id, Some(header_text(self.b.source, clause)))];
            let body = (0..clause.named_child_count() as u32)
                .filter_map(|i| clause.named_child(i))
                .find(|c| c.kind() == "statements");
            joins.extend(self.block(body, entry));
        }
        joins
    }

    /// One node per call `node` makes, chained in evaluation order. An
    /// expression that calls nothing adds nothing and passes its tails through.
    fn emit_calls(&mut self, node: Node<'t>, tails: Vec<Pending>) -> Vec<Pending> {
        let mut calls = Vec::new();
        collect_calls(self.b.source, node, &mut calls);

        let mut tails = tails;
        for call in &calls {
            let id = self.b.chain(
                tails,
                "call",
                call_label(self.b.source, *call),
                line_of(*call),
            );
            tails = vec![(id, None)];
        }
        tails
    }
}

/// `defer { ... }`, which tree-sitter-swift reads as a trailing-closure call.
fn is_defer(source: &[u8], node: Node) -> bool {
    node.named_child(0)
        .is_some_and(|c| c.kind() == "simple_identifier" && collapsed(source, c) == "defer")
}

/// Collapse runs of whitespace in a string, as [`collapsed`] does for a node.
fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn elide(text: &str) -> String {
    if text.chars().count() > LABEL_WIDTH {
        return text.chars().take(LABEL_WIDTH - 1).collect::<String>() + "…";
    }
    text.to_string()
}

/// A statement's header: its source from the start up to the body's `{`.
fn header_text(source: &[u8], node: Node) -> String {
    let end = (0..node.child_count() as u32)
        .filter_map(|i| node.child(i))
        .find(|c| c.kind() == "{")
        .map(|c| c.start_byte())
        .unwrap_or(node.end_byte());
    let text = std::str::from_utf8(&source[node.start_byte()..end]).unwrap_or("");
    elide(&collapse(text))
}

/// Calls made by a statement, in evaluation order: receiver and arguments
/// before the call they feed, so `a.b().c()` reads `b` then `c`.
///
/// Nothing inside another body is collected: not a closure, whose calls run
/// when someone else invokes it, and not a nested function or type.
fn collect_calls<'t>(source: &[u8], node: Node<'t>, out: &mut Vec<Node<'t>>) {
    if matches!(
        node.kind(),
        "lambda_literal" | "function_declaration" | "class_declaration" | "protocol_declaration"
    ) || (node.kind() == "call_expression" && is_defer(source, node))
    {
        return;
    }
    for i in 0..node.named_child_count() as u32 {
        if let Some(child) = node.named_child(i) {
            collect_calls(source, child, out);
        }
    }
    if node.kind() == "call_expression" {
        out.push(node);
    }
}

/// Label for a call node: the callee, not the whole expression.
fn call_label(source: &[u8], node: Node) -> String {
    // `!f()` reads as a call whose callee is a prefix expression around `f`.
    let callee = node.named_child(0).map(|c| match c.kind() {
        "prefix_expression" => c.child_by_field_name("target").unwrap_or(c),
        _ => c,
    });
    match callee {
        Some(f) => format!("{}(…)", callee_label(source, f)),
        None => label_of(source, node),
    }
}

/// The callee of a call. A member access is elided from the *left*: the method
/// being called is the last segment, and a long chain truncated the usual way
/// leaves several calls spelled identically.
fn callee_label(source: &[u8], callee: Node) -> String {
    if callee.kind() != "navigation_expression" {
        return label_of(source, callee);
    }
    let text = collapsed(source, callee);
    if text.chars().count() <= LABEL_WIDTH {
        return text;
    }
    let keep = text.chars().count() - (LABEL_WIDTH - 1);
    "…".to_string() + &text.chars().skip(keep).collect::<String>()
}

/// Build the flow graph for the Swift function named `name` at `line`.
pub(super) fn build(
    source: &str,
    file: &str,
    name: &str,
    scope: Option<&str>,
    line: i64,
    stored: Option<&StoredSignature>,
) -> Result<FlowGraph> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_swift::LANGUAGE.into())
        .context("setting Swift language")?;
    let tree = parser.parse(source, None).context("parsing Swift source")?;
    let src = source.as_bytes();

    let func = find_swift_function(tree.root_node(), src, name, scope, line).ok_or_else(|| {
        anyhow!(
            "no function body for {} at {file}:{line} \
             (the index may be stale — run `helios update`)",
            qualified(scope, name)
        )
    })?;

    let params = stored.and_then(|s| s.params.clone()).unwrap_or_else(|| {
        (0..func.child_count() as u32)
            .filter_map(|i| func.child(i))
            .filter(|c| c.kind() == "parameter")
            .map(|c| label_of(src, c))
            .collect()
    });
    let returns = stored.and_then(|s| s.returns.clone()).or_else(|| {
        func.child_by_field_name("return_type")
            .map(|r| label_of(src, r))
    });

    let function = FunctionInfo {
        name: name.to_string(),
        scope: scope.map(str::to_string),
        file: file.to_string(),
        line: line_of(func),
        end_line: func.end_position().row as i64 + 1,
        language: "swift".to_string(),
        params,
        returns,
    };

    let mut sw = SwiftBuilder {
        b: Builder::start(src, &function),
        arm_order: Vec::new(),
    };
    let exit = sw.b.exit;

    // A protocol requirement has no body to draw.
    let body = func
        .child_by_field_name("body")
        .ok_or_else(|| anyhow!("{name} has no body at {file}:{line}"))?;
    let statements = (0..body.named_child_count() as u32)
        .filter_map(|i| body.named_child(i))
        .find(|c| c.kind() == "statements");
    let tails = sw.block(statements, vec![(0, None)]);
    sw.b.connect(&tails, exit);

    sw.reorder_arms();
    Ok(sw.b.finish(function))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build the graph for `name`, taking the line from the first mention of
    /// the name in the source.
    fn graph(source: &str, name: &str) -> FlowGraph {
        let line = source
            .lines()
            .position(|l| l.contains(name))
            .map(|i| i as i64 + 1)
            .unwrap_or(1);
        build(source, "Test.swift", name, None, line, None).unwrap()
    }

    /// Wrap statements in a function, so a test only spells the body.
    fn func(body: &str) -> String {
        format!("func run(n: Int) {{\n{body}\n}}\n")
    }

    fn node(g: &FlowGraph, label: &str) -> usize {
        g.nodes
            .iter()
            .find(|n| n.label == label)
            .unwrap_or_else(|| panic!("no node {label:?}: {:?}", g.nodes))
            .id
    }

    fn of_kind<'g>(g: &'g FlowGraph, kind: &str) -> Vec<&'g super::super::FlowNode> {
        g.nodes.iter().filter(|n| n.kind == kind).collect()
    }

    fn edge(g: &FlowGraph, from: usize, label: &str) -> Option<usize> {
        g.edges
            .iter()
            .find(|e| e.from == from && e.label.as_deref() == Some(label))
            .map(|e| e.to)
    }

    fn has_edge(g: &FlowGraph, from: usize, to: usize) -> bool {
        g.edges.iter().any(|e| e.from == from && e.to == to)
    }

    #[test]
    fn signature_comes_from_the_declaration() {
        let g = graph(
            "func add(_ a: Int, b: Int = 1) -> Int {\n    return a + b\n}\n",
            "add",
        );
        assert_eq!(g.function.language, "swift");
        assert_eq!(g.function.params, vec!["_ a: Int", "b: Int"]);
        assert_eq!(g.function.returns.as_deref(), Some("Int"));
    }

    #[test]
    fn stored_signature_overrides_derivation() {
        let stored = StoredSignature {
            params: Some(vec!["x: Int".to_string()]),
            returns: Some("Bool".to_string()),
        };
        let g = build(
            "func run() {\n    go()\n}\n",
            "T.swift",
            "run",
            None,
            1,
            Some(&stored),
        )
        .unwrap();
        assert_eq!(g.nodes[0].label, "run(x: Int) -> Bool");
    }

    #[test]
    fn calls_are_nodes_in_evaluation_order() {
        let g = graph(
            &func(
                "    if !ready() {}\n    engine.start(a: prepare())\n    Widget(size: 2)\n    plain()",
            ),
            "run",
        );
        let calls: Vec<_> = of_kind(&g, "call")
            .iter()
            .map(|n| n.label.clone())
            .collect();
        assert_eq!(
            calls,
            vec![
                "ready(…)",
                "prepare(…)",
                "engine.start(…)",
                "Widget(…)",
                "plain(…)"
            ]
        );
    }

    #[test]
    fn closures_and_nested_functions_are_not_expanded() {
        let g = graph(
            &func(
                "    items.forEach { visit($0) }\n    func helper() { inner() }\n    defer { cleanup() }",
            ),
            "run",
        );
        let calls: Vec<_> = of_kind(&g, "call")
            .iter()
            .map(|n| n.label.clone())
            .collect();
        assert_eq!(calls, vec!["items.forEach(…)"]);
    }

    #[test]
    fn if_else_branches_both_ways_and_rejoins() {
        let g = graph(
            &func(
                "    if n > 0 {\n        left()\n    } else {\n        right()\n    }\n    after()",
            ),
            "run",
        );
        let branch = of_kind(&g, "branch")[0];
        assert_eq!(branch.label, "n > 0");
        let t = edge(&g, branch.id, "true").unwrap();
        let f = edge(&g, branch.id, "false").unwrap();
        assert_eq!(g.nodes[t].label, "left(…)");
        assert_eq!(g.nodes[f].label, "right(…)");
        let after = node(&g, "after(…)");
        assert!(has_edge(&g, t, after) && has_edge(&g, f, after));
    }

    #[test]
    fn if_let_condition_and_else_if_chain() {
        let g = graph(
            &func(
                "    if let v = lookup(n), v > 1 {\n        a()\n    } else if n == 0 {\n        b()\n    }",
            ),
            "run",
        );
        let branches = of_kind(&g, "branch");
        assert_eq!(branches.len(), 2);
        assert_eq!(branches[0].label, "let v = lookup(n), v > 1");
        // The lookup runs before the branch it feeds.
        assert!(has_edge(&g, node(&g, "lookup(…)"), branches[0].id));
        // The else-if hangs off the first branch's false edge.
        assert_eq!(edge(&g, branches[0].id, "false"), Some(branches[1].id));
    }

    #[test]
    fn guard_continues_on_true_and_leaves_on_false() {
        let g = graph(
            &func("    guard let v = lookup(n) else {\n        return\n    }\n    use(v)"),
            "run",
        );
        let branch = of_kind(&g, "branch")[0];
        let ret = of_kind(&g, "return")[0];
        assert_eq!(edge(&g, branch.id, "false"), Some(ret.id));
        assert_eq!(edge(&g, branch.id, "true"), Some(node(&g, "use(…)")));
        let exit = of_kind(&g, "exit")[0].id;
        assert!(has_edge(&g, ret.id, exit));
    }

    #[test]
    fn for_in_repeats_and_break_and_continue_find_the_loop() {
        let g = graph(
            &func(
                "    for item in items() {\n        if skip(item) {\n            continue\n        }\n        if done(item) {\n            break\n        }\n        handle(item)\n    }\n    finish()",
            ),
            "run",
        );
        let header = of_kind(&g, "loop")[0];
        assert_eq!(header.label, "for item in items()");
        assert!(has_edge(&g, node(&g, "items(…)"), header.id));
        assert_eq!(edge(&g, node(&g, "handle(…)"), "repeat"), Some(header.id));
        let cont = of_kind(&g, "continue")[0];
        assert!(has_edge(&g, cont.id, header.id));
        let brk = of_kind(&g, "break")[0];
        let finish = node(&g, "finish(…)");
        assert!(has_edge(&g, brk.id, finish));
        assert_eq!(edge(&g, header.id, "done"), Some(finish));
    }

    #[test]
    fn while_and_repeat_while_are_loops() {
        let g = graph(&func("    while ready() {\n        step()\n    }"), "run");
        assert_eq!(of_kind(&g, "loop")[0].label, "while ready()");
        let g = graph(
            &func("    repeat {\n        step()\n    } while ready()"),
            "run",
        );
        assert_eq!(of_kind(&g, "loop")[0].label, "repeat while ready()");
        assert_eq!(
            edge(&g, node(&g, "step(…)"), "repeat"),
            Some(of_kind(&g, "loop")[0].id)
        );
    }

    #[test]
    fn labeled_break_and_continue_target_the_named_loop() {
        let g = graph(
            &func(
                "    outer: for a in xs {\n        for b in ys {\n            if hit(b) {\n                continue outer\n            }\n            break outer\n        }\n    }\n    after()",
            ),
            "run",
        );
        let loops = of_kind(&g, "loop");
        assert_eq!(loops.len(), 2);
        let cont = of_kind(&g, "continue")[0];
        assert!(has_edge(&g, cont.id, loops[0].id), "continue outer");
        let brk = of_kind(&g, "break")[0];
        assert!(has_edge(&g, brk.id, node(&g, "after(…)")), "break outer");
    }

    #[test]
    fn switch_labels_every_entry_and_breaks_leave_it() {
        let g = graph(
            &func(
                "    switch n {\n    case 0, 1:\n        small()\n    case let k where k > 5:\n        big(k)\n        break\n    default:\n        other()\n    }\n    after()",
            ),
            "run",
        );
        let m = of_kind(&g, "match")[0];
        assert_eq!(m.label, "switch n");
        assert_eq!(edge(&g, m.id, "0, 1"), Some(node(&g, "small(…)")));
        assert!(edge(&g, m.id, "let k where k > 5").is_some());
        assert_eq!(edge(&g, m.id, "default"), Some(node(&g, "other(…)")));
        // No fallthrough between entries: each body joins `after`.
        let after = node(&g, "after(…)");
        assert!(has_edge(&g, node(&g, "small(…)"), after));
        assert!(has_edge(&g, of_kind(&g, "break")[0].id, after));
    }

    #[test]
    fn do_catch_branches_and_throw_leaves() {
        let g = graph(
            &func(
                "    do {\n        try risky()\n    } catch let e as Failure {\n        report(e)\n    } catch {\n        throw error\n    }\n    after()",
            ),
            "run",
        );
        let branch = of_kind(&g, "branch")[0];
        assert_eq!(branch.label, "do");
        assert_eq!(edge(&g, branch.id, "do"), Some(node(&g, "risky(…)")));
        assert_eq!(
            edge(&g, branch.id, "catch let e as Failure"),
            Some(node(&g, "report(…)"))
        );
        let throw = of_kind(&g, "throw")[0];
        assert!(has_edge(&g, throw.id, of_kind(&g, "exit")[0].id));
        let after = node(&g, "after(…)");
        assert!(has_edge(&g, node(&g, "risky(…)"), after));
        assert!(!has_edge(&g, throw.id, after));
    }

    #[test]
    fn return_calls_before_it_returns() {
        let g = graph(&func("    return compute(n)"), "run");
        let ret = of_kind(&g, "return")[0];
        assert!(has_edge(&g, node(&g, "compute(…)"), ret.id));
        assert!(has_edge(&g, ret.id, of_kind(&g, "exit")[0].id));
    }

    #[test]
    fn scope_filters_namesakes() {
        let source =
            "class A {\n    func go() { one() }\n}\nclass B {\n    func go() { two() }\n}\n";
        let g = build(source, "T.swift", "go", Some("B"), 5, None).unwrap();
        assert_eq!(of_kind(&g, "call")[0].label, "two(…)");
    }
}
