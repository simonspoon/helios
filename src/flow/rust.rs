//! The Rust statement mapping for `helios flow`.

use anyhow::{Context, Result, anyhow};
use tree_sitter::{Node, Parser};

use super::{
    Breakable, Builder, ERR_EXIT, FlowEdge, FlowGraph, FunctionInfo, Pending, label_of, line_of,
    qualified,
};
use crate::parsers::rust_parser::RustParser;

/// Node kinds that hold a Rust function body, in definition order.
const RUST_FN_KINDS: &[&str] = &["function_item", "function_signature_item"];

/// Locate the function whose name matches `name` and whose body contains
/// `line`. The index records the *name* position, so the innermost enclosing
/// function that also carries that name is the definition the user asked for.
///
/// `scope` — the impl block, trait or module the index recorded — is a hard
/// filter, not a hint. Without it a file edited since the last `helios init`
/// would resolve `B::go` to whichever function now sits on that line.
fn find_rust_function<'t>(
    root: Node<'t>,
    source: &[u8],
    name: &str,
    scope: Option<&str>,
    line: i64,
) -> Option<Node<'t>> {
    let mut named: Vec<Node> = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if RUST_FN_KINDS.contains(&node.kind())
            && let Some(name_node) = node.child_by_field_name("name")
            && std::str::from_utf8(&source[name_node.byte_range()]).unwrap_or("") == name
            && scope.is_none_or(|s| RustParser::find_scope(source, name_node).as_deref() == Some(s))
        {
            named.push(node);
        }
        for i in 0..node.named_child_count() as u32 {
            if let Some(child) = node.named_child(i) {
                stack.push(child);
            }
        }
    }

    // Innermost wins: a nested fn shadowing the name is the tighter match.
    let at_line = named
        .iter()
        .filter(|n| (line_of(**n)..=(n.end_position().row as i64 + 1)).contains(&line))
        .min_by_key(|n| n.byte_range().len());

    match at_line {
        Some(node) => Some(*node),
        // The file has moved on since it was indexed, so the recorded line
        // points somewhere else now. One function still owns the name, so it is
        // the one meant; several, and there is nothing to disambiguate with.
        None => match named.as_slice() {
            [only] => Some(*only),
            _ => None,
        },
    }
}

/// The `'name` on a labelled loop, or on the `break`/`continue` naming it.
fn loop_label(source: &[u8], node: Node) -> Option<String> {
    (0..node.named_child_count() as u32)
        .filter_map(|i| node.named_child(i))
        .find(|c| c.kind() == "label")
        .map(|c| label_of(source, c))
}

impl Builder<'_> {
    /// Which enclosing loop a `break`/`continue` leaves: the one it names, or
    /// the innermost when it names none.
    fn target_loop(&self, node: Node) -> Option<usize> {
        match loop_label(self.source, node) {
            Some(name) => self
                .breakables
                .iter()
                .rposition(|ctx| ctx.label.as_deref() == Some(name.as_str())),
            None => self.breakables.len().checked_sub(1),
        }
    }

    /// Statements of a block, in order. Returns the tails that fall out of it;
    /// empty means every path returned, broke, or continued.
    ///
    /// `is_tail` says this block's value is the function's return value, so its
    /// trailing expression is an implicit `return`. A block that is not in tail
    /// position — a loop body, an inner `{ }` — just falls through to whatever
    /// comes after it.
    fn block(&mut self, node: Node, mut tails: Vec<Pending>, is_tail: bool) -> Vec<Pending> {
        let count = node.named_child_count() as u32;
        for i in 0..count {
            let Some(child) = node.named_child(i) else {
                continue;
            };
            if child.is_extra() {
                continue; // comments
            }
            let child_is_tail = is_tail && i + 1 == count && is_expression(child);
            tails = self.statement(child, tails, child_is_tail);
            if tails.is_empty() {
                break; // unreachable from here on
            }
        }
        tails
    }

    /// One statement. `is_tail` marks an expression whose value the function
    /// returns, so it ends in a `return` node rather than falling through.
    fn statement(&mut self, node: Node, tails: Vec<Pending>, is_tail: bool) -> Vec<Pending> {
        match node.kind() {
            "expression_statement" => match node.named_child(0) {
                Some(inner) => self.statement(inner, tails, is_tail),
                None => tails,
            },
            "block" => self.block(node, tails, is_tail),
            "unsafe_block" | "async_block" | "try_block" | "const_block" => {
                match node.named_child(0) {
                    Some(inner) if inner.kind() == "block" => self.block(inner, tails, is_tail),
                    _ => tails,
                }
            }
            "if_expression" => self.if_expr(node, tails, is_tail),
            "match_expression" => self.match_expr(node, tails, is_tail),
            "for_expression" | "while_expression" | "loop_expression" => {
                self.loop_expr(node, tails)
            }
            "return_expression" => {
                // `return compute(1)?` calls before it returns.
                let tails = self.emit_calls(node, tails);
                let id = self.chain(tails, "return", label_of(self.source, node), line_of(node));
                self.edges.push(FlowEdge {
                    from: id,
                    to: self.exit,
                    label: None,
                });
                Vec::new()
            }
            "break_expression" => {
                let tails = self.emit_calls(node, tails);
                let id = self.chain(tails, "break", label_of(self.source, node), line_of(node));
                let target = self.target_loop(node);
                match target.and_then(|i| self.breakables.get_mut(i)) {
                    Some(ctx) => ctx.breaks.push((id, None)),
                    // A stray break outside a loop only happens in code that
                    // does not compile; treat it as an exit rather than lose it.
                    None => self.edges.push(FlowEdge {
                        from: id,
                        to: self.exit,
                        label: None,
                    }),
                }
                Vec::new()
            }
            "continue_expression" => {
                let id = self.chain(
                    tails,
                    "continue",
                    label_of(self.source, node),
                    line_of(node),
                );
                if let Some(header) = self.target_loop(node).map(|i| self.breakables[i].header) {
                    self.edges.push(FlowEdge {
                        from: id,
                        to: header,
                        label: None,
                    });
                }
                Vec::new()
            }
            "let_declaration" => {
                let value = node.child_by_field_name("value");
                match node.child_by_field_name("alternative") {
                    // `let Some(x) = o else { ... };` branches.
                    Some(alt) => self.let_else(node, value, alt, tails),
                    // `let x = if .. {} else {};` / `let x = match .. {}` put
                    // real control flow in value position.
                    None => match value {
                        Some(v) if is_control_flow(v) => self.statement(v, tails, false),
                        _ => self.plain(node, tails, false),
                    },
                }
            }
            _ => self.plain(node, tails, is_tail),
        }
    }

    /// `let PATTERN = value else { ... };` — a branch, not a statement. The
    /// else block has to diverge, so nothing flows out of it into the code
    /// after the `let`; only the bound path carries on.
    fn let_else(
        &mut self,
        node: Node,
        value: Option<Node>,
        alt: Node,
        tails: Vec<Pending>,
    ) -> Vec<Pending> {
        // The value is evaluated before the pattern is tested. Its calls are
        // taken from the value alone: the else block is separate control flow,
        // and inlining its calls here would claim they always run.
        let tails = match value {
            Some(v) => self.emit_calls(v, tails),
            None => tails,
        };

        let label = match (node.child_by_field_name("pattern"), value) {
            (Some(p), Some(v)) => {
                format!(
                    "let {} = {}",
                    label_of(self.source, p),
                    label_of(self.source, v)
                )
            }
            (Some(p), None) => format!("let {}", label_of(self.source, p)),
            _ => "let else".to_string(),
        };
        let id = self.chain(tails, "branch", label, line_of(node));

        // Code that compiles never falls out of the else block; wire any tail
        // that does to the exit rather than to the statement after the `let`.
        let else_tails = self.block(alt, vec![(id, Some("else".into()))], false);
        self.connect(&else_tails, self.exit);

        vec![(id, Some("bound".into()))]
    }

    fn if_expr(&mut self, node: Node, tails: Vec<Pending>, is_tail: bool) -> Vec<Pending> {
        let condition = node.child_by_field_name("condition");
        // The condition runs before the branch is taken, `?` and all.
        let tails = match condition {
            Some(c) => self.emit_calls(c, tails),
            None => tails,
        };
        let label = condition
            .map(|c| label_of(self.source, c))
            .unwrap_or_default();
        let id = self.chain(tails, "branch", label, line_of(node));

        let mut out = Vec::new();
        match node.child_by_field_name("consequence") {
            Some(body) => out.extend(self.block(body, vec![(id, Some("true".into()))], is_tail)),
            None => out.push((id, Some("true".into()))),
        }

        match node.child_by_field_name("alternative") {
            Some(alt) => {
                // else_clause wraps either a block or a chained `else if`.
                let inner = if alt.kind() == "else_clause" {
                    alt.named_child(0)
                } else {
                    Some(alt)
                };
                match inner {
                    Some(b) => {
                        out.extend(self.statement(b, vec![(id, Some("false".into()))], is_tail))
                    }
                    None => out.push((id, Some("false".into()))),
                }
            }
            None => out.push((id, Some("false".into()))),
        }
        out
    }

    fn match_expr(&mut self, node: Node, tails: Vec<Pending>, is_tail: bool) -> Vec<Pending> {
        let scrutinee = node.child_by_field_name("value");
        // The scrutinee is evaluated before any arm is chosen.
        let tails = match scrutinee {
            Some(v) => self.emit_calls(v, tails),
            None => tails,
        };
        let value = scrutinee
            .map(|v| label_of(self.source, v))
            .unwrap_or_default();
        let id = self.chain(tails, "match", format!("match {value}"), line_of(node));

        let Some(body) = node.child_by_field_name("body") else {
            return vec![(id, None)];
        };

        let mut out = Vec::new();
        let mut arms = 0;
        for i in 0..body.named_child_count() as u32 {
            let Some(arm) = body.named_child(i) else {
                continue;
            };
            if arm.kind() != "match_arm" {
                continue;
            }
            arms += 1;
            let pattern_node = arm.child_by_field_name("pattern");
            let pattern = pattern_node
                .map(|p| label_of(self.source, p))
                .unwrap_or_else(|| "_".into());

            // `Some(n) if big(n) =>` runs `big` on the way into the arm.
            let arm_tails = vec![(id, Some(pattern))];
            let arm_tails = match pattern_node.and_then(|p| p.child_by_field_name("condition")) {
                Some(guard) => self.emit_calls(guard, arm_tails),
                None => arm_tails,
            };

            match arm.child_by_field_name("value") {
                Some(value) => out.extend(self.statement(value, arm_tails, is_tail)),
                None => out.extend(arm_tails),
            }
        }
        if arms == 0 {
            out.push((id, None));
        }
        out
    }

    fn loop_expr(&mut self, node: Node, tails: Vec<Pending>) -> Vec<Pending> {
        // Calls in the iterator or the condition sit before the header. That is
        // exact for `for`, whose iterator is evaluated once, and an
        // approximation for `while`, which re-tests every time round.
        let tails = match node
            .child_by_field_name("value")
            .or_else(|| node.child_by_field_name("condition"))
        {
            Some(head) => self.emit_calls(head, tails),
            None => tails,
        };

        let label = match node.kind() {
            "for_expression" => {
                let pattern = node
                    .child_by_field_name("pattern")
                    .map(|p| label_of(self.source, p))
                    .unwrap_or_default();
                let value = node
                    .child_by_field_name("value")
                    .map(|v| label_of(self.source, v))
                    .unwrap_or_default();
                format!("for {pattern} in {value}")
            }
            "while_expression" => {
                let condition = node
                    .child_by_field_name("condition")
                    .map(|c| label_of(self.source, c))
                    .unwrap_or_default();
                format!("while {condition}")
            }
            _ => "loop".to_string(),
        };
        let id = self.chain(tails, "loop", label, line_of(node));

        self.breakables.push(Breakable {
            header: id,
            label: loop_label(self.source, node),
            is_loop: true,
            finally_depth: 0,
            breaks: Vec::new(),
        });
        // A loop body is never in tail position: the value of a `loop` comes
        // out of its `break`, and `for`/`while` evaluate to `()`.
        let body_tails = match node.child_by_field_name("body") {
            Some(body) => self.block(body, vec![(id, Some("body".into()))], false),
            None => Vec::new(),
        };
        let ctx = self.breakables.pop().expect("loop context pushed above");

        // Falling off the end of the body goes back round. A tail that already
        // carries a label ("false" off a trailing `if`) keeps it — the edge
        // pointing back at the header is what marks it as the repeat.
        for (from, label) in body_tails {
            self.edges.push(FlowEdge {
                from,
                to: id,
                label: Some(label.unwrap_or_else(|| "repeat".into())),
            });
        }

        let mut out = ctx.breaks;
        // `loop` only leaves through a break; for/while can also finish.
        if node.kind() != "loop_expression" {
            out.push((id, Some("done".into())));
        }
        out
    }

    /// One node per call `node` makes, chained in evaluation order. An
    /// expression that calls nothing adds nothing and passes its tails through.
    fn emit_calls(&mut self, node: Node, tails: Vec<Pending>) -> Vec<Pending> {
        let mut calls = Vec::new();
        collect_calls(node, &mut calls);

        let mut tails = tails;
        for call in &calls {
            let (label, fallible) = call_label(self.source, *call);
            let id = self.chain(tails, "call", label, line_of(*call));
            if fallible {
                self.edges.push(FlowEdge {
                    from: id,
                    to: self.exit,
                    label: Some(ERR_EXIT.into()),
                });
            }
            tails = vec![(id, None)];
        }
        tails
    }

    /// A statement with no control flow of its own: its calls, plus a `return`
    /// node when its value is what the function hands back.
    fn plain(&mut self, node: Node, tails: Vec<Pending>, is_tail: bool) -> Vec<Pending> {
        let tails = self.emit_calls(node, tails);

        if is_tail {
            // The block's value: for a function body, the implicit return.
            let id = self.chain(tails, "return", label_of(self.source, node), line_of(node));
            self.edges.push(FlowEdge {
                from: id,
                to: self.exit,
                label: None,
            });
            return Vec::new();
        }
        tails
    }
}

fn is_control_flow(node: Node) -> bool {
    matches!(
        node.kind(),
        "if_expression"
            | "match_expression"
            | "for_expression"
            | "while_expression"
            | "loop_expression"
    )
}

/// Whether a block child can be the block's value.
///
/// A block-like expression in statement position (`if`, `match`, `loop`) is
/// wrapped in `expression_statement` even with no semicolon, so the wrapper
/// says nothing; the semicolon is what discards the value.
fn is_expression(node: Node) -> bool {
    if node.is_extra() || node.kind().ends_with("_item") || node.kind() == "let_declaration" {
        return false;
    }
    if node.kind() == "expression_statement" {
        let last = (node.child_count() as u32).saturating_sub(1);
        return node.child(last).map(|c| c.kind()) != Some(";");
    }
    true
}

/// Calls made by a statement, in evaluation order: receiver and arguments
/// before the call they feed, so `a.b().c()` reads `b` then `c`.
///
/// Nothing inside another body is collected — not a nested `fn`, and not a
/// closure, whose calls run when someone else invokes it rather than here.
fn collect_calls<'t>(node: Node<'t>, out: &mut Vec<Node<'t>>) {
    if RUST_FN_KINDS.contains(&node.kind()) || node.kind() == "closure_expression" {
        return;
    }
    for i in 0..node.named_child_count() as u32 {
        if let Some(child) = node.named_child(i) {
            collect_calls(child, out);
        }
    }
    if matches!(node.kind(), "call_expression" | "macro_invocation") {
        out.push(node);
    }
}

/// Label for a call node plus whether `?` makes it an early exit. The label is
/// the callee, not the whole expression, so nested calls stay legible.
fn call_label(source: &[u8], node: Node) -> (String, bool) {
    // `fetch(p).await?` and `(read()?)` still `?` the call, so look past the
    // wrappers that pass a value straight through.
    let mut outer = node;
    while let Some(parent) = outer.parent() {
        match parent.kind() {
            "await_expression" | "parenthesized_expression" | "reference_expression" => {
                outer = parent
            }
            _ => break,
        }
    }
    let fallible = outer
        .parent()
        .map(|p| p.kind() == "try_expression")
        .unwrap_or(false);

    let label = if node.kind() == "macro_invocation" {
        match node.child_by_field_name("macro") {
            Some(m) => format!("{}!", label_of(source, m)),
            None => label_of(source, node),
        }
    } else {
        match node.child_by_field_name("function") {
            Some(f) => format!("{}(…)", label_of(source, f)),
            None => label_of(source, node),
        }
    };
    (label, fallible)
}

/// Build the flow graph for the Rust function named `name` at `line`.
pub(super) fn build(
    source: &str,
    file: &str,
    name: &str,
    scope: Option<&str>,
    line: i64,
) -> Result<FlowGraph> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .context("setting Rust language")?;
    let tree = parser.parse(source, None).context("parsing Rust source")?;
    let src = source.as_bytes();

    let func = find_rust_function(tree.root_node(), src, name, scope, line).ok_or_else(|| {
        anyhow!(
            "no function body for {} at {file}:{line} \
             (the index may be stale — run `helios update`)",
            qualified(scope, name)
        )
    })?;

    let params = func
        .child_by_field_name("parameters")
        .map(|p| {
            (0..p.named_child_count() as u32)
                .filter_map(|i| p.named_child(i))
                .filter(|c| !c.is_extra())
                .map(|c| label_of(src, c))
                .collect()
        })
        .unwrap_or_default();

    let function = FunctionInfo {
        name: name.to_string(),
        scope: scope.map(str::to_string),
        file: file.to_string(),
        line: line_of(func),
        end_line: func.end_position().row as i64 + 1,
        language: "rust".to_string(),
        params,
        returns: func
            .child_by_field_name("return_type")
            .map(|r| label_of(src, r)),
    };

    let mut builder = Builder::start(src, &function);
    let exit = builder.exit;

    let body = func
        .child_by_field_name("body")
        .ok_or_else(|| anyhow!("{name} has no body at {file}:{line}"))?;
    let tails = builder.block(body, vec![(0, None)], true);
    builder.connect(&tails, exit);

    Ok(builder.finish(function))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(source: &str, name: &str) -> FlowGraph {
        let line = source
            .lines()
            .position(|l| l.contains(&format!("fn {name}")))
            .map(|i| i as i64 + 1)
            .unwrap_or(1);
        build(source, "test.rs", name, None, line).unwrap()
    }

    fn kinds(g: &FlowGraph, kind: &str) -> Vec<String> {
        g.nodes
            .iter()
            .filter(|n| n.kind == kind)
            .map(|n| n.label.clone())
            .collect()
    }

    fn edge(g: &FlowGraph, from: usize, label: &str) -> Option<usize> {
        g.edges
            .iter()
            .find(|e| e.from == from && e.label.as_deref() == Some(label))
            .map(|e| e.to)
    }

    #[test]
    fn entry_carries_signature_and_params() {
        let g = graph("fn add(a: i32, b: i32) -> i32 { a + b }", "add");
        assert_eq!(g.nodes[0].kind, "entry");
        assert_eq!(g.nodes[0].label, "add(a: i32, b: i32) -> i32");
        assert_eq!(g.function.params, vec!["a: i32", "b: i32"]);
        assert_eq!(g.function.returns.as_deref(), Some("i32"));
        assert_eq!(kinds(&g, "return"), vec!["a + b"]);
    }

    #[test]
    fn if_else_branches_both_ways_and_rejoins() {
        let g = graph(
            r#"
fn pick(a: bool) {
    if a {
        left();
    } else {
        right();
    }
    after();
}
"#,
            "pick",
        );
        let branch = g.nodes.iter().find(|n| n.kind == "branch").unwrap();
        assert_eq!(branch.label, "a");
        let t = edge(&g, branch.id, "true").unwrap();
        let f = edge(&g, branch.id, "false").unwrap();
        assert_eq!(g.nodes[t].label, "left(…)");
        assert_eq!(g.nodes[f].label, "right(…)");
        // Both arms converge on the statement after the if.
        let after = g.nodes.iter().find(|n| n.label == "after(…)").unwrap();
        for arm in [t, f] {
            assert!(g.edges.iter().any(|e| e.from == arm && e.to == after.id));
        }
    }

    #[test]
    fn missing_else_falls_through_to_the_join() {
        let g = graph(
            r#"
fn maybe(a: bool) {
    if a {
        work();
    }
    after();
}
"#,
            "maybe",
        );
        let branch = g.nodes.iter().find(|n| n.kind == "branch").unwrap();
        let after = g.nodes.iter().find(|n| n.label == "after(…)").unwrap();
        assert_eq!(edge(&g, branch.id, "false"), Some(after.id));
    }

    #[test]
    fn loop_body_repeats_and_break_leaves() {
        let g = graph(
            r#"
fn scan(items: Vec<u32>) {
    for item in items {
        if done(item) {
            break;
        }
        handle(item);
    }
    finish();
}
"#,
            "scan",
        );
        let header = g.nodes.iter().find(|n| n.kind == "loop").unwrap();
        assert_eq!(header.label, "for item in items");
        let handle = g.nodes.iter().find(|n| n.label == "handle(…)").unwrap();
        assert_eq!(edge(&g, handle.id, "repeat"), Some(header.id));

        let brk = g.nodes.iter().find(|n| n.kind == "break").unwrap();
        let finish = g.nodes.iter().find(|n| n.label == "finish(…)").unwrap();
        assert!(
            g.edges
                .iter()
                .any(|e| e.from == brk.id && e.to == finish.id)
        );
        assert_eq!(edge(&g, header.id, "done"), Some(finish.id));
    }

    #[test]
    fn continue_goes_back_to_the_header() {
        let g = graph(
            r#"
fn skip(items: Vec<u32>) {
    for item in items {
        if odd(item) {
            continue;
        }
        handle(item);
    }
}
"#,
            "skip",
        );
        let header = g.nodes.iter().find(|n| n.kind == "loop").unwrap();
        let cont = g.nodes.iter().find(|n| n.kind == "continue").unwrap();
        assert!(
            g.edges
                .iter()
                .any(|e| e.from == cont.id && e.to == header.id)
        );
    }

    #[test]
    fn labelled_break_and_continue_target_the_named_loop() {
        let g = graph(
            r#"
fn nested(rows: Vec<Vec<u32>>) {
    'outer: for row in rows {
        for cell in row {
            if bad(cell) {
                break 'outer;
            }
            if skip(cell) {
                continue 'outer;
            }
        }
        tidy(row);
    }
    finish();
}
"#,
            "nested",
        );
        let loops: Vec<_> = g.nodes.iter().filter(|n| n.kind == "loop").collect();
        assert_eq!(loops.len(), 2, "{:?}", g.nodes);
        let outer = loops[0].id;

        let brk = g.nodes.iter().find(|n| n.kind == "break").unwrap();
        let finish = g.nodes.iter().find(|n| n.label == "finish(…)").unwrap();
        assert!(
            g.edges
                .iter()
                .any(|e| e.from == brk.id && e.to == finish.id),
            "break 'outer leaves the outer loop, not the inner one"
        );

        let cont = g.nodes.iter().find(|n| n.kind == "continue").unwrap();
        assert!(
            g.edges.iter().any(|e| e.from == cont.id && e.to == outer),
            "continue 'outer goes back to the outer header"
        );
    }

    #[test]
    fn match_arms_are_labelled_edges() {
        let g = graph(
            r#"
fn route(v: Op) {
    match v {
        Op::Add => add(),
        Op::Sub => sub(),
        _ => fallback(),
    }
}
"#,
            "route",
        );
        let m = g.nodes.iter().find(|n| n.kind == "match").unwrap();
        assert_eq!(m.label, "match v");
        for (pattern, callee) in [
            ("Op::Add", "add(…)"),
            ("Op::Sub", "sub(…)"),
            ("_", "fallback(…)"),
        ] {
            let to = edge(&g, m.id, pattern).unwrap();
            assert_eq!(g.nodes[to].label, callee);
        }
    }

    #[test]
    fn returns_reach_the_exit_node() {
        let g = graph(
            r#"
fn early(a: bool) -> u32 {
    if a {
        return 1;
    }
    2
}
"#,
            "early",
        );
        let exit = g.nodes.iter().find(|n| n.kind == "exit").unwrap();
        let returns: Vec<_> = g.nodes.iter().filter(|n| n.kind == "return").collect();
        assert_eq!(returns.len(), 2);
        for r in returns {
            assert!(g.edges.iter().any(|e| e.from == r.id && e.to == exit.id));
        }
    }

    #[test]
    fn question_mark_adds_an_error_exit() {
        let g = graph(
            r#"
fn load(p: &str) -> Result<u32> {
    let raw = read(p)?;
    Ok(raw)
}
"#,
            "load",
        );
        let read = g.nodes.iter().find(|n| n.label == "read(…)").unwrap();
        let exit = g.nodes.iter().find(|n| n.kind == "exit").unwrap();
        assert!(
            g.edges.iter().any(|e| e.from == read.id
                && e.to == exit.id
                && e.label.as_deref() == Some(ERR_EXIT))
        );
    }

    #[test]
    fn calls_are_leaves_not_expansions() {
        let g = graph(
            r#"
fn caller() {
    helper();
}

fn helper() {
    deep_inside();
}
"#,
            "caller",
        );
        assert_eq!(kinds(&g, "call"), vec!["helper(…)"]);
    }

    #[test]
    fn nested_calls_are_listed_in_evaluation_order_and_skip_closures() {
        let g = graph(
            r#"
fn pipeline(items: Vec<u32>) {
    let out = items.iter().map(|i| double(i)).collect();
    send(out);
}
"#,
            "pipeline",
        );
        assert_eq!(
            kinds(&g, "call"),
            vec![
                "items.iter(…)",
                "items.iter().map(…)",
                "items.iter().map(|i| double(i)).collect(…)",
                "send(…)"
            ],
            "receiver before call, and `double` belongs to the closure's body"
        );
    }

    #[test]
    fn statements_without_calls_do_not_add_nodes() {
        let g = graph(
            r#"
fn quiet() {
    let a = 1;
    let b = a + 2;
    let _ = b;
}
"#,
            "quiet",
        );
        assert_eq!(g.nodes.len(), 2, "only entry and exit: {:?}", g.nodes);
        assert_eq!(g.edges.len(), 1);
    }

    #[test]
    fn value_position_control_flow_is_a_branch() {
        let g = graph(
            r#"
fn choose(a: bool) {
    let v = if a { one() } else { two() };
    use_it(v);
}
"#,
            "choose",
        );
        assert_eq!(kinds(&g, "branch"), vec!["a"]);
        assert!(kinds(&g, "call").contains(&"one(…)".to_string()));
    }

    #[test]
    fn macros_count_as_calls() {
        let g = graph(
            r#"
fn shout() {
    println!("hi");
}
"#,
            "shout",
        );
        assert_eq!(kinds(&g, "call"), vec!["println!"]);
    }

    #[test]
    fn method_uses_the_innermost_matching_function() {
        let source = r#"
fn outer() {
    fn helper() {
        inner_call();
    }
    helper();
}
"#;
        let line = source
            .lines()
            .position(|l| l.contains("fn helper"))
            .map(|i| i as i64 + 1)
            .unwrap();
        let g = build(source, "test.rs", "helper", None, line).unwrap();
        assert_eq!(kinds(&g, "call"), vec!["inner_call(…)"]);
    }

    #[test]
    fn let_else_branches_and_its_else_arm_diverges() {
        let g = graph(
            r#"
fn let_else_tail(o: Option<u32>) -> u32 {
    let Some(x) = o else {
        return bail_out();
    };
    use_x(x)
}
"#,
            "let_else_tail",
        );
        let branch = g.nodes.iter().find(|n| n.kind == "branch").unwrap();
        assert_eq!(branch.label, "let Some(x) = o");

        let bail = g.nodes.iter().find(|n| n.label == "bail_out(…)").unwrap();
        let use_x = g.nodes.iter().find(|n| n.label == "use_x(…)").unwrap();
        assert_eq!(edge(&g, branch.id, "else"), Some(bail.id));
        assert_eq!(edge(&g, branch.id, "bound"), Some(use_x.id));
        assert!(
            !g.edges
                .iter()
                .any(|e| e.from == bail.id && e.to == use_x.id),
            "the else arm diverges; it cannot flow into the bound path"
        );
        // The else block's `return` is a real exit of the function.
        let exit = g.nodes.iter().find(|n| n.kind == "exit").unwrap();
        let ret = g
            .nodes
            .iter()
            .find(|n| n.kind == "return" && n.label.contains("bail_out"))
            .expect("the else arm returns");
        assert!(g.edges.iter().any(|e| e.from == ret.id && e.to == exit.id));
    }

    #[test]
    fn calls_in_return_and_break_operands_are_nodes() {
        let g = graph(
            r#"
fn explicit_return_call(a: bool) -> u32 {
    if a {
        return compute(1);
    }
    0
}
"#,
            "explicit_return_call",
        );
        assert_eq!(kinds(&g, "call"), vec!["compute(…)"]);

        let g = graph(
            r#"
fn loop_break_err(p: &str, a: bool) -> Result<u32> {
    let v = loop {
        if a {
            break fetch(p)?;
        }
        tick();
    };
    Ok(v)
}
"#,
            "loop_break_err",
        );
        let fetch = g.nodes.iter().find(|n| n.label == "fetch(…)").unwrap();
        let exit = g.nodes.iter().find(|n| n.kind == "exit").unwrap();
        assert!(
            g.edges.iter().any(|e| e.from == fetch.id
                && e.to == exit.id
                && e.label.as_deref() == Some("Err ?")),
            "`break fetch(p)?` can leave the function on the error path"
        );
        let brk = g.nodes.iter().find(|n| n.kind == "break").unwrap();
        assert!(g.edges.iter().any(|e| e.from == fetch.id && e.to == brk.id));
    }

    #[test]
    fn a_trailing_if_returns_from_both_arms() {
        // A block-like tail expression is wrapped in `expression_statement`
        // even with no semicolon, which must not hide that it is the value.
        let g = graph(
            r#"
fn compute(units: i32) -> i32 {
    if units > 10 {
        discount(units)
    } else {
        units * 2
    }
}
"#,
            "compute",
        );
        assert_eq!(kinds(&g, "return"), vec!["discount(units)", "units * 2"]);
        let exit = g.nodes.iter().find(|n| n.kind == "exit").unwrap();
        for r in g.nodes.iter().filter(|n| n.kind == "return") {
            assert!(g.edges.iter().any(|e| e.from == r.id && e.to == exit.id));
        }
    }

    #[test]
    fn a_loop_body_ending_in_an_expression_still_repeats() {
        let g = graph(
            r#"
fn loop_body_tail(items: Vec<u32>) {
    for i in items {
        process(i)
    }
    finish();
}
"#,
            "loop_body_tail",
        );
        let header = g.nodes.iter().find(|n| n.kind == "loop").unwrap();
        let process = g.nodes.iter().find(|n| n.label == "process(…)").unwrap();
        assert_eq!(edge(&g, process.id, "repeat"), Some(header.id));
        assert!(
            kinds(&g, "return").is_empty(),
            "a loop body is not the function's value: {:?}",
            g.nodes
        );
        assert!(g.nodes.iter().any(|n| n.label == "finish(…)"));
    }

    #[test]
    fn a_value_position_if_rejoins_instead_of_returning() {
        let g = graph(
            r#"
fn value_pos_if(a: bool) -> u32 {
    let v = if a { compute() } else { 0 };
    use_it(v);
    v
}
"#,
            "value_pos_if",
        );
        let compute = g.nodes.iter().find(|n| n.label == "compute(…)").unwrap();
        let use_it = g.nodes.iter().find(|n| n.label == "use_it(…)").unwrap();
        assert!(
            g.edges
                .iter()
                .any(|e| e.from == compute.id && e.to == use_it.id),
            "the true arm flows on to the next statement: {:?}",
            g.edges
        );
        assert_eq!(kinds(&g, "return"), vec!["v"]);
    }

    #[test]
    fn an_inner_block_falls_through_to_the_next_statement() {
        let g = graph(
            r#"
fn blocky() {
    {
        inner_one()
    }
    inner_two();
}
"#,
            "blocky",
        );
        assert_eq!(kinds(&g, "call"), vec!["inner_one(…)", "inner_two(…)"]);
        assert!(kinds(&g, "return").is_empty());
    }

    #[test]
    fn a_call_in_a_condition_is_a_node_and_keeps_its_error_exit() {
        let g = graph(
            r#"
fn q_in_cond(p: &str) -> Result<u32> {
    if check(p)? {
        Ok(1)
    } else {
        Ok(2)
    }
}
"#,
            "q_in_cond",
        );
        let check = g.nodes.iter().find(|n| n.label == "check(…)").unwrap();
        let branch = g.nodes.iter().find(|n| n.kind == "branch").unwrap();
        let exit = g.nodes.iter().find(|n| n.kind == "exit").unwrap();
        assert!(
            g.edges
                .iter()
                .any(|e| e.from == check.id && e.to == branch.id)
        );
        assert!(
            g.edges.iter().any(|e| e.from == check.id
                && e.to == exit.id
                && e.label.as_deref() == Some("Err ?"))
        );
    }

    #[test]
    fn calls_in_a_scrutinee_a_guard_and_an_iterator_are_nodes() {
        let g = graph(
            r#"
fn heads(items: Vec<u32>) {
    match classify(items) {
        n if big(n) => wide(n),
        _ => narrow(),
    }
    for item in fetch_all() {
        touch(item);
    }
}
"#,
            "heads",
        );
        let calls = kinds(&g, "call");
        for expected in ["classify(…)", "big(…)", "fetch_all(…)"] {
            assert!(
                calls.contains(&expected.to_string()),
                "missing {expected}: {calls:?}"
            );
        }
        // The scrutinee runs before the match decides.
        let classify = g.nodes.iter().find(|n| n.label == "classify(…)").unwrap();
        let m = g.nodes.iter().find(|n| n.kind == "match").unwrap();
        assert!(
            g.edges
                .iter()
                .any(|e| e.from == classify.id && e.to == m.id)
        );
    }

    #[test]
    fn question_mark_after_await_still_exits() {
        let g = graph(
            r#"
async fn chain_q(p: &str) -> Result<u32> {
    let v = fetch(p).await?;
    Ok(v)
}
"#,
            "chain_q",
        );
        let fetch = g.nodes.iter().find(|n| n.label == "fetch(…)").unwrap();
        let exit = g.nodes.iter().find(|n| n.kind == "exit").unwrap();
        assert!(
            g.edges.iter().any(|e| e.from == fetch.id
                && e.to == exit.id
                && e.label.as_deref() == Some("Err ?"))
        );
    }

    #[test]
    fn scope_picks_the_right_impl_when_the_line_has_moved() {
        // Two `go`s; the recorded line now points inside A::go, as it would
        // after the file was edited without reindexing.
        let source = r#"
struct A;
struct B;

impl A {
    fn go(&self) {
        a_go();
    }
}

impl B {
    fn go(&self) {
        b_go();
    }
}
"#;
        let g = build(source, "test.rs", "go", Some("B"), 7).unwrap();
        assert_eq!(kinds(&g, "call"), vec!["b_go(…)"]);

        let g = build(source, "test.rs", "go", Some("A"), 13).unwrap();
        assert_eq!(kinds(&g, "call"), vec!["a_go(…)"]);
    }

    #[test]
    fn a_stale_line_still_finds_the_only_function_with_that_name() {
        let source = "fn moved() {\n    work();\n}\n";
        let g = build(source, "test.rs", "moved", None, 97).unwrap();
        assert_eq!(kinds(&g, "call"), vec!["work(…)"]);
        assert_eq!(g.function.line, 1, "the graph reports where it really is");
    }
}
