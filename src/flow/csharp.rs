//! The C# statement mapping for `helios flow`.

use anyhow::{Context, Result, anyhow, bail};
use tree_sitter::{Node, Parser};

use super::{
    Breakable, Builder, FlowEdge, FlowGraph, FunctionInfo, LABEL_WIDTH, Pending, collapsed,
    label_of, line_of, qualified,
};
use crate::parsers::csharp::find_scope;

/// Node kinds a target can name. These are exactly what the indexer records as
/// `fn`; an operator, a destructor and a local function have bodies too, but
/// nothing puts them in the index, so nothing can ask for one.
const CSHARP_FN_KINDS: &[&str] = &[
    "method_declaration",
    "constructor_declaration",
    "property_declaration",
];

/// Locate the declaration named `name` whose body contains `line`, exactly as
/// the Rust side does: `scope` — the class, struct, record or interface the
/// index recorded — is a hard filter, the innermost match at the line wins, and
/// a line that has moved on since indexing falls back to a sole namesake.
fn find_csharp_function<'t>(
    root: Node<'t>,
    source: &[u8],
    name: &str,
    scope: Option<&str>,
    line: i64,
) -> Option<Node<'t>> {
    let mut named: Vec<Node> = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if CSHARP_FN_KINDS.contains(&node.kind())
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

/// The C# walker. The graph state is shared with the Rust mapping; the wrapper
/// exists so both languages can name their statement walkers `block`,
/// `statement` and so on without colliding on one inherent impl.
struct CsBuilder<'a, 't> {
    b: Builder<'a>,
    /// The `finally` blocks currently open, outermost first. A jump out of one
    /// runs them on the way, so they are walked again on that path.
    finallys: Vec<Node<'t>>,
    /// Each match node and its arm labels in source order, for [`Self::reorder_arms`].
    arm_order: Vec<(usize, Vec<String>)>,
}

impl<'t> CsBuilder<'_, 't> {
    /// The named children of a block, in order. Returns the tails that fall out
    /// of it; empty means every path returned, threw, broke, or continued.
    fn block(&mut self, node: Node<'t>, tails: Vec<Pending>) -> Vec<Pending> {
        let statements: Vec<Node> = (0..node.named_child_count() as u32)
            .filter_map(|i| node.named_child(i))
            .filter(|c| !c.is_extra())
            .collect();
        self.sequence(&statements, tails)
    }

    /// Statements run one after another. A switch section is a run of
    /// statements with no block of its own, which is why this is separate.
    fn sequence(&mut self, statements: &[Node<'t>], mut tails: Vec<Pending>) -> Vec<Pending> {
        for statement in statements {
            // Nothing reaches here, so walking on would add nodes no edge
            // points at. Unreachable code contributes no graph.
            if tails.is_empty() {
                break;
            }
            tails = self.statement(*statement, tails);
        }
        tails
    }

    fn statement(&mut self, node: Node<'t>, tails: Vec<Pending>) -> Vec<Pending> {
        match node.kind() {
            "block" => self.block(node, tails),
            "if_statement" => self.if_stmt(node, tails),
            "switch_statement" => self.switch_stmt(node, tails),
            "for_statement" | "foreach_statement" | "while_statement" | "do_statement" => {
                self.loop_stmt(node, tails)
            }
            "try_statement" => self.try_stmt(node, tails),
            // A header that binds or locks something runs its calls first, then
            // the body. `label: stmt` has no header call to make, but the same
            // first-child/last-child shape.
            "using_statement" | "lock_statement" | "checked_statement" | "unsafe_statement"
            | "fixed_statement" | "labeled_statement" => self.wrapper(node, tails),
            // `goto` jumps somewhere this graph does not track, so the path
            // ends here rather than joining onto the next statement.
            "goto_statement" => {
                self.b
                    .chain(tails, "goto", label_of(self.b.source, node), line_of(node));
                Vec::new()
            }
            "return_statement" => {
                // `return Compute(1);` calls before it returns, and
                // `return n switch { ... }` branches to choose the value.
                let tails = match switch_in_value(node) {
                    Some(switch) => {
                        let tails = self.emit_calls_outside(node, Some(switch), tails);
                        self.switch_expr(switch, tails)
                    }
                    None => self.emit_calls(node, tails),
                };
                self.exits(node, "return", tails)
            }
            // A throw leaves the function as far as this graph is concerned;
            // see the module docs for why a catch does not intercept it.
            "throw_statement" => {
                let tails = self.emit_calls(node, tails);
                self.exits(node, "throw", tails)
            }
            // `yield return x;` hands a value out and carries on; `yield break;`
            // ends the iterator, which is a return.
            "yield_statement" => {
                let tails = self.emit_calls(node, tails);
                if yields_break(node) {
                    return self.exits(node, "return", tails);
                }
                let id = self
                    .b
                    .chain(tails, "yield", label_of(self.b.source, node), line_of(node));
                vec![(id, None)]
            }
            "break_statement" => {
                let target = self.b.breakables.len().checked_sub(1);
                // Leaving the breakable runs every `finally` opened inside it.
                let tails = self.unwind(self.depth_of(target), tails);
                let id = self
                    .b
                    .chain(tails, "break", label_of(self.b.source, node), line_of(node));
                match target.and_then(|i| self.b.breakables.get_mut(i)) {
                    Some(ctx) => ctx.breaks.push((id, None)),
                    // A stray break only happens in code that does not compile;
                    // treat it as an exit rather than lose it.
                    None => self.b.edges.push(FlowEdge {
                        from: id,
                        to: self.b.exit,
                        label: None,
                    }),
                }
                Vec::new()
            }
            "continue_statement" => {
                // A `switch` is breakable but not continuable, so the enclosing
                // loop is the target even when a switch sits in between.
                let target = self.b.breakables.iter().rposition(|c| c.is_loop);
                let tails = self.unwind(self.depth_of(target), tails);
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
            // `var x = y switch { ... };` puts real control flow in value
            // position; anything else is a plain statement.
            _ => match switch_in_value(node) {
                Some(switch) => {
                    // `Target()[Index()] = v switch { ... };` evaluates its
                    // left-hand side before the switch picks the value.
                    let tails = self.emit_calls_outside(node, Some(switch), tails);
                    self.switch_expr(switch, tails)
                }
                None => self.plain(node, tails),
            },
        }
    }

    /// Put each match node's outgoing edges back into source order.
    ///
    /// An arm whose body makes no call adds no node, so its edge is not created
    /// when the arm is walked but later, when the arms join — which lands it
    /// after arms that come after it in the source. Overlapping patterns are
    /// tried in order, so the order the graph shows has to be the real one.
    fn reorder_arms(&mut self) {
        for (from, labels) in &self.arm_order {
            let slots: Vec<usize> = (0..self.b.edges.len())
                .filter(|i| self.b.edges[*i].from == *from)
                .collect();
            let mut edges: Vec<FlowEdge> = slots.iter().map(|i| self.b.edges[*i].clone()).collect();
            // A label the arms do not account for — the `no match` edge — sorts
            // last and keeps its relative position, the sort being stable.
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

    /// How many `finally` blocks were open when the target breakable was
    /// entered — the ones a jump out of it does *not* run. A jump with no
    /// target leaves the function, which runs all of them.
    fn depth_of(&self, target: Option<usize>) -> usize {
        match target {
            Some(i) => self.b.breakables[i].finally_depth,
            None => 0,
        }
    }

    /// Run a fresh copy of every `finally` open past `from`, innermost first,
    /// the way a jump out of a `try` really does. The block appears in the
    /// graph once per path that runs it, which is the truth of the construct.
    fn unwind(&mut self, from: usize, mut tails: Vec<Pending>) -> Vec<Pending> {
        let open = self.finallys.clone();
        for i in (from..open.len()).rev() {
            // A jump inside this copy runs only what encloses the copy, so the
            // stack shrinks as we walk outward.
            self.finallys.truncate(i);
            tails = self.block(open[i], tails);
            if tails.is_empty() {
                break; // the finally itself diverged
            }
        }
        self.finallys = open;
        tails
    }

    /// A node that ends the function: its edge goes to the exit and nothing
    /// flows on from it. Every open `finally` runs first.
    fn exits(&mut self, node: Node<'t>, kind: &str, tails: Vec<Pending>) -> Vec<Pending> {
        let tails = self.unwind(0, tails);
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

    /// `using`, `lock`, `fixed`, `checked`, `unsafe`, `label:` — a header that
    /// may make calls, wrapped round a body the flow passes straight through.
    fn wrapper(&mut self, node: Node<'t>, tails: Vec<Pending>) -> Vec<Pending> {
        let count = node.named_child_count() as u32;
        let Some(body) = node.named_child(count.saturating_sub(1)) else {
            return tails;
        };
        let header = node.named_child(0).filter(|h| *h != body);
        let tails = match header {
            Some(h) => self.emit_calls(h, tails),
            None => tails,
        };
        self.statement(body, tails)
    }

    fn if_stmt(&mut self, node: Node<'t>, tails: Vec<Pending>) -> Vec<Pending> {
        let condition = node.child_by_field_name("condition");
        // The condition runs before the branch is taken.
        let tails = match condition {
            Some(c) => self.emit_calls(c, tails),
            None => tails,
        };
        let label = condition
            .map(|c| label_of(self.b.source, c))
            .unwrap_or_default();
        let id = self.b.chain(tails, "branch", label, line_of(node));

        let mut out = Vec::new();
        match node.child_by_field_name("consequence") {
            Some(body) => out.extend(self.statement(body, vec![(id, Some("true".into()))])),
            None => out.push((id, Some("true".into()))),
        }
        // The alternative is the `else` body, or the next `if` of an else-if
        // chain; either way it is one statement.
        match node.child_by_field_name("alternative") {
            Some(alt) => out.extend(self.statement(alt, vec![(id, Some("false".into()))])),
            None => out.push((id, Some("false".into()))),
        }
        out
    }

    /// `switch (v) { case ...: }`. A section is entered by every label that
    /// leads to it, including the labels of the empty sections above it, which
    /// is how the grammar spells `case 1: case 2: body`.
    fn switch_stmt(&mut self, node: Node<'t>, tails: Vec<Pending>) -> Vec<Pending> {
        let value = node.child_by_field_name("value");
        // The value is evaluated before any section is chosen.
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
            label: None,
            is_loop: false,
            finally_depth: self.finallys.len(),
            breaks: Vec::new(),
        });

        let mut out = Vec::new();
        let mut pending: Vec<String> = Vec::new();
        let mut order: Vec<String> = Vec::new();
        let mut has_default = false;
        if let Some(body) = node.child_by_field_name("body") {
            for i in 0..body.named_child_count() as u32 {
                let Some(section) = body.named_child(i) else {
                    continue;
                };
                if section.kind() != "switch_section" {
                    continue;
                }
                let (labels, statements) = split_section(self.b.source, section);
                has_default |= labels.iter().any(|l| l == "default");
                order.extend(labels.iter().cloned());
                pending.extend(labels);
                if statements.is_empty() {
                    continue; // the labels fall into the next section's body
                }
                let entry: Vec<Pending> =
                    pending.drain(..).map(|label| (id, Some(label))).collect();
                // A section that still has tails fell out of the bottom, which
                // compiling code does not do — a `goto case` ends its path at
                // the `goto` node. Join whatever is left onto the switch's exit.
                out.extend(self.sequence(&statements, entry));
            }
        }

        let ctx = self.b.breakables.pop().expect("switch pushed above");
        out.extend(ctx.breaks);
        // A label with no section after it leads nowhere, which only happens in
        // code that does not compile; show it leaving the switch rather than
        // dropping it.
        out.extend(pending.into_iter().map(|label| (id, Some(label))));
        // Without a `default` the value can match nothing, and the switch is
        // skipped whole. That is a real path, so it gets a real edge.
        if !has_default {
            out.push((id, Some("no match".into())));
        }
        self.arm_order.push((id, order));
        out
    }

    /// `v switch { pattern => value, ... }` in expression position.
    fn switch_expr(&mut self, node: Node<'t>, tails: Vec<Pending>) -> Vec<Pending> {
        let value = node
            .named_child(0)
            .filter(|c| c.kind() != "switch_expression_arm");
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

        let mut out = Vec::new();
        let mut order: Vec<String> = Vec::new();
        for i in 0..node.named_child_count() as u32 {
            let Some(arm) = node.named_child(i) else {
                continue;
            };
            if arm.kind() != "switch_expression_arm" {
                continue;
            }
            let label = arm_label(self.b.source, arm);
            order.push(label.clone());
            let arm_tails = vec![(id, Some(label))];
            // The arm's value is the last child: the pattern and any `when`
            // clause are already in the edge label. That value can be another
            // switch, which branches again rather than running in sequence.
            match arm.named_child((arm.named_child_count() as u32).saturating_sub(1)) {
                Some(v) => match tail_switch(v) {
                    Some(inner) => out.extend(self.switch_expr(inner, arm_tails)),
                    None => out.extend(self.plain(v, arm_tails)),
                },
                None => out.extend(arm_tails),
            }
        }
        if order.is_empty() {
            out.push((id, None));
        }
        self.arm_order.push((id, order));
        out
    }

    fn loop_stmt(&mut self, node: Node<'t>, tails: Vec<Pending>) -> Vec<Pending> {
        // Calls in the header sit before it. That is exact for `foreach`, whose
        // collection is evaluated once, and an approximation for a condition,
        // which is re-tested every time round.
        let mut tails = tails;
        for head in header_expressions(node) {
            tails = self.emit_calls(head, tails);
        }
        let id = self.b.chain(
            tails,
            "loop",
            loop_header_label(self.b.source, node),
            line_of(node),
        );

        self.b.breakables.push(Breakable {
            header: id,
            label: None,
            is_loop: true,
            finally_depth: self.finallys.len(),
            breaks: Vec::new(),
        });
        let body_tails = match node.child_by_field_name("body") {
            Some(body) => self.statement(body, vec![(id, Some("body".into()))]),
            None => Vec::new(),
        };
        let ctx = self.b.breakables.pop().expect("loop pushed above");

        // Falling off the end of the body goes back round. A tail that already
        // carries a label ("false" off a trailing `if`) keeps it — the edge
        // pointing back at the header is what marks it as the repeat.
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

    /// `try { } catch { } finally { }`. Every catch hangs off the entry of the
    /// try, because the statement that threw is not known here; the tails of
    /// the try and of each catch join, through the finally when there is one.
    fn try_stmt(&mut self, node: Node<'t>, tails: Vec<Pending>) -> Vec<Pending> {
        let id = self
            .b
            .chain(tails, "branch", "try".to_string(), line_of(node));

        // The finally has to be known before the try body is walked: a `return`
        // in there runs it on the way out.
        let finally = child_of_kind(node, "finally_clause").and_then(|c| c.named_child(0));
        if let Some(block) = finally {
            self.finallys.push(block);
        }

        let mut joins = match node.child_by_field_name("body") {
            Some(body) => self.block(body, vec![(id, Some("try".into()))]),
            None => vec![(id, Some("try".into()))],
        };

        for i in 0..node.named_child_count() as u32 {
            let Some(clause) = node.named_child(i) else {
                continue;
            };
            if clause.kind() != "catch_clause" {
                continue;
            }
            let declaration = child_of_kind(clause, "catch_declaration");
            let filter = child_of_kind(clause, "catch_filter_clause");
            let label = match (declaration, filter) {
                (Some(d), Some(f)) => format!(
                    "catch {} {}",
                    label_of(self.b.source, d),
                    label_of(self.b.source, f)
                ),
                (Some(d), None) => format!("catch {}", label_of(self.b.source, d)),
                (None, Some(f)) => format!("catch {}", label_of(self.b.source, f)),
                (None, None) => "catch".to_string(),
            };
            // The filter is evaluated to decide whether this catch runs at all,
            // so its calls sit on the way in.
            let entry = vec![(id, Some(label))];
            let entry = match filter {
                Some(f) => self.emit_calls(f, entry),
                None => entry,
            };
            match clause.child_by_field_name("body") {
                Some(body) => joins.extend(self.block(body, entry)),
                None => joins.extend(entry),
            }
        }

        if finally.is_some() {
            self.finallys.pop();
        }

        // A finally block runs on every path out of the try, so everything
        // joins there first and its own tails are what carry on. When nothing
        // falls out, every path already ran its own inlined copy.
        match finally {
            Some(block) => self.block(block, joins),
            None => joins,
        }
    }

    /// One node per call `node` makes, chained in evaluation order. An
    /// expression that calls nothing adds nothing and passes its tails through.
    fn emit_calls(&mut self, node: Node<'t>, tails: Vec<Pending>) -> Vec<Pending> {
        self.emit_calls_outside(node, None, tails)
    }

    /// The same, minus the calls inside `skip`. A statement whose value comes
    /// from a switch still evaluates the rest of itself — the receiver, an
    /// index, the left-hand side — and those calls belong before the branch.
    fn emit_calls_outside(
        &mut self,
        node: Node<'t>,
        skip: Option<Node<'t>>,
        tails: Vec<Pending>,
    ) -> Vec<Pending> {
        let mut calls = Vec::new();
        collect_calls(node, skip, &mut calls);

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

    /// A statement with no control flow of its own: just its calls.
    fn plain(&mut self, node: Node<'t>, tails: Vec<Pending>) -> Vec<Pending> {
        self.emit_calls(node, tails)
    }
}

/// True for `yield break;`, false for `yield return x;`.
fn yields_break(node: Node) -> bool {
    (0..node.child_count() as u32)
        .filter_map(|i| node.child(i))
        .any(|c| c.kind() == "break")
}

/// The `switch_expression` a statement's value comes from, if that is what
/// chooses it: `return v switch {…}`, `x = v switch {…}`, `var x = v switch
/// {…}`, or the expression statement on its own. Anywhere else in an
/// expression a switch is a subexpression, and its arms are drawn as calls.
fn switch_in_value(node: Node) -> Option<Node> {
    let value = match node.kind() {
        "expression_statement" | "return_statement" => node.named_child(0)?,
        "local_declaration_statement" => {
            let declaration = node.named_child(0)?;
            let declarator = (0..declaration.named_child_count() as u32)
                .filter_map(|i| declaration.named_child(i))
                .find(|c| c.kind() == "variable_declarator")?;
            declarator.named_child((declarator.named_child_count() as u32).saturating_sub(1))?
        }
        _ => return None,
    };
    tail_switch(value)
}

/// The same, for an expression already in value position — an arrow body, or
/// the right-hand side an assignment hands on.
fn tail_switch(node: Node) -> Option<Node> {
    match node.kind() {
        "switch_expression" => Some(node),
        "assignment_expression" => tail_switch(node.child_by_field_name("right")?),
        _ => None,
    }
}

/// The first named child of `kind`, for the clauses the grammar hangs off a
/// node without giving them a field name.
fn child_of_kind<'t>(node: Node<'t>, kind: &str) -> Option<Node<'t>> {
    (0..node.named_child_count() as u32)
        .filter_map(|i| node.named_child(i))
        .find(|c| c.kind() == kind)
}

/// The labels of a switch section and the statements they lead to. The grammar
/// spells a label as the `case`/`default` keyword followed by patterns and an
/// optional `when` clause, all before the `:`; everything after is a statement.
fn split_section<'t>(source: &[u8], section: Node<'t>) -> (Vec<String>, Vec<Node<'t>>) {
    let mut labels = Vec::new();
    let mut statements = Vec::new();
    let mut parts: Option<Vec<String>> = None;

    for i in 0..section.child_count() as u32 {
        let Some(child) = section.child(i) else {
            continue;
        };
        match child.kind() {
            "case" => parts = Some(Vec::new()),
            "default" => labels.push("default".to_string()),
            ":" => {
                if let Some(parts) = parts.take() {
                    labels.push(parts.join(" "));
                }
            }
            _ if child.is_extra() => {}
            _ => match &mut parts {
                Some(parts) => parts.push(label_of(source, child)),
                None if child.is_named() => statements.push(child),
                None => {}
            },
        }
    }
    (labels, statements)
}

/// `pattern` or `pattern when guard`, for a switch-expression arm's edge.
fn arm_label(source: &[u8], arm: Node) -> String {
    let mut parts = Vec::new();
    for i in 0..arm.named_child_count() as u32 {
        let Some(child) = arm.named_child(i) else {
            continue;
        };
        // The last child is the arm's value; everything before it — the pattern
        // and any `when` clause — is what selects the arm.
        if i + 1 == arm.named_child_count() as u32 {
            break;
        }
        parts.push(label_of(source, child));
    }
    if parts.is_empty() {
        "_".to_string()
    } else {
        parts.join(" ")
    }
}

/// The parts of a loop header that are evaluated as expressions, in order.
fn header_expressions(node: Node) -> Vec<Node> {
    let fields: &[&str] = match node.kind() {
        "for_statement" => &["initializer", "condition"],
        "foreach_statement" => &["right"],
        _ => &["condition"],
    };
    fields
        .iter()
        .filter_map(|f| node.child_by_field_name(f))
        .collect()
}

fn loop_header_label(source: &[u8], node: Node) -> String {
    let field = |name: &str| {
        node.child_by_field_name(name)
            .map(|n| label_of(source, n))
            .unwrap_or_default()
    };
    match node.kind() {
        "for_statement" => {
            let updates: Vec<String> = {
                let mut cursor = node.walk();
                node.children_by_field_name("update", &mut cursor)
                    .map(|n| label_of(source, n))
                    .collect()
            };
            format!(
                "for ({}; {}; {})",
                field("initializer"),
                field("condition"),
                updates.join(", ")
            )
        }
        "foreach_statement" => format!(
            "foreach ({} {} in {})",
            field("type"),
            field("left"),
            field("right")
        ),
        "do_statement" => format!("do while ({})", field("condition")),
        _ => format!("while ({})", field("condition")),
    }
}

/// Calls made by a statement, in evaluation order: receiver and arguments
/// before the call they feed, so `a.B().C()` reads `B` then `C`.
///
/// Nothing inside another body is collected — not a local function, and not a
/// lambda, whose calls run when someone else invokes it rather than here.
/// `skip` is a subtree whose calls someone else is placing — the switch a
/// statement takes its value from, whose arms are walked separately.
fn collect_calls<'t>(node: Node<'t>, skip: Option<Node<'t>>, out: &mut Vec<Node<'t>>) {
    if Some(node) == skip {
        return;
    }
    if matches!(
        node.kind(),
        "lambda_expression" | "anonymous_method_expression" | "local_function_statement"
    ) {
        return;
    }
    for i in 0..node.named_child_count() as u32 {
        if let Some(child) = node.named_child(i) {
            collect_calls(child, skip, out);
        }
    }
    if matches!(
        node.kind(),
        "invocation_expression" | "object_creation_expression"
    ) {
        out.push(node);
    }
}

/// Label for a call node: the callee, not the whole expression, so nested calls
/// stay legible.
fn call_label(source: &[u8], node: Node) -> String {
    if node.kind() == "object_creation_expression" {
        return match node.child_by_field_name("type") {
            Some(t) => format!("new {}(…)", label_of(source, t)),
            None => label_of(source, node),
        };
    }
    match node.child_by_field_name("function") {
        Some(f) => format!("{}(…)", callee_label(source, f)),
        None => label_of(source, node),
    }
}

/// The callee of an invocation. A member access is elided from the *left*: the
/// method being called is the last segment, and a long LINQ chain truncated the
/// usual way leaves several calls spelled identically.
fn callee_label(source: &[u8], callee: Node) -> String {
    if !matches!(
        callee.kind(),
        "member_access_expression" | "conditional_access_expression"
    ) {
        return label_of(source, callee);
    }
    let text = collapsed(source, callee);
    if text.chars().count() <= LABEL_WIDTH {
        return text;
    }
    let keep = text.chars().count() - (LABEL_WIDTH - 1);
    "…".to_string() + &text.chars().skip(keep).collect::<String>()
}

/// Build the flow graph for the C# member named `name` at `line`.
pub(super) fn build(
    source: &str,
    file: &str,
    name: &str,
    scope: Option<&str>,
    line: i64,
) -> Result<FlowGraph> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_c_sharp::LANGUAGE.into())
        .context("setting C# language")?;
    let tree = parser.parse(source, None).context("parsing C# source")?;
    let src = source.as_bytes();

    let func = find_csharp_function(tree.root_node(), src, name, scope, line).ok_or_else(|| {
        anyhow!(
            "no function body for {} at {file}:{line} \
             (the index may be stale — run `helios update`)",
            qualified(scope, name)
        )
    })?;

    // A property with `get`/`set` bodies is several flows, not one; an
    // arrow-bodied property is a single expression and works like a method.
    if func.child_by_field_name("accessors").is_some() {
        bail!(
            "{} is a property with accessors; flow needs a method body",
            qualified(scope, name)
        );
    }

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
        language: "csharp".to_string(),
        params,
        // A method spells its result `returns`; a property and a local function
        // both spell theirs `type`.
        returns: func
            .child_by_field_name("returns")
            .or_else(|| func.child_by_field_name("type"))
            .map(|r| label_of(src, r)),
    };

    let mut cs = CsBuilder {
        b: Builder::start(src, &function),
        finallys: Vec::new(),
        arm_order: Vec::new(),
    };
    let exit = cs.b.exit;

    // A property's arrow body is its `value`; everything else calls it `body`.
    let body = func
        .child_by_field_name("body")
        .or_else(|| func.child_by_field_name("value"))
        .ok_or_else(|| anyhow!("{name} has no body at {file}:{line}"))?;

    match body.kind() {
        // `=> expr;` — the expression is the return value.
        "arrow_expression_clause" => {
            let value = body
                .named_child(0)
                .ok_or_else(|| anyhow!("{name} has an empty body at {file}:{line}"))?;
            // `=> v switch { ... }` chooses the returned value by branching.
            let tails = match tail_switch(value) {
                Some(switch) => cs.switch_expr(switch, vec![(0, None)]),
                None => cs.emit_calls(value, vec![(0, None)]),
            };
            cs.exits(value, "return", tails);
        }
        // A block body never returns its last statement implicitly.
        _ => {
            let tails = cs.block(body, vec![(0, None)]);
            cs.b.connect(&tails, exit);
        }
    }

    cs.reorder_arms();
    Ok(cs.b.finish(function))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build the graph for `name`, taking the line from the first mention of
    /// the name in the source — near enough to what the index would record.
    fn graph(source: &str, name: &str) -> FlowGraph {
        let line = source
            .lines()
            .position(|l| l.contains(name))
            .map(|i| i as i64 + 1)
            .unwrap_or(1);
        build(source, "Test.cs", name, None, line).unwrap()
    }

    /// Wrap statements in a class and a method, so a test only spells the body.
    fn method(body: &str) -> String {
        format!("class C {{\n    void Run(int n) {{\n{body}\n    }}\n}}\n")
    }

    fn kinds(g: &FlowGraph, kind: &str) -> Vec<String> {
        g.nodes
            .iter()
            .filter(|n| n.kind == kind)
            .map(|n| n.label.clone())
            .collect()
    }

    fn node(g: &FlowGraph, label: &str) -> usize {
        g.nodes
            .iter()
            .find(|n| n.label == label)
            .unwrap_or_else(|| panic!("no node labelled {label}: {:?}", g.nodes))
            .id
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
    fn if_else_branches_both_ways_and_rejoins() {
        let g = graph(
            &method(
                "        if (n > 0) {\n            Left();\n        } else {\n            Right();\n        }\n        After();",
            ),
            "Run",
        );
        let branch = g.nodes.iter().find(|n| n.kind == "branch").unwrap();
        assert_eq!(branch.label, "n > 0");
        let t = edge(&g, branch.id, "true").unwrap();
        let f = edge(&g, branch.id, "false").unwrap();
        assert_eq!(g.nodes[t].label, "Left(…)");
        assert_eq!(g.nodes[f].label, "Right(…)");
        let after = node(&g, "After(…)");
        for arm in [t, f] {
            assert!(has_edge(&g, arm, after), "both arms converge on After");
        }
    }

    #[test]
    fn foreach_repeats_and_break_and_continue_find_the_loop() {
        let g = graph(
            &method(
                "        foreach (var item in Items()) {\n            if (Skip(item)) {\n                continue;\n            }\n            if (Done(item)) {\n                break;\n            }\n            Handle(item);\n        }\n        Finish();",
            ),
            "Run",
        );
        let header = g.nodes.iter().find(|n| n.kind == "loop").unwrap();
        assert_eq!(header.label, "foreach (var item in Items())");
        // The collection is evaluated before the header it feeds.
        assert!(has_edge(&g, node(&g, "Items(…)"), header.id));

        assert_eq!(edge(&g, node(&g, "Handle(…)"), "repeat"), Some(header.id));

        let cont = g.nodes.iter().find(|n| n.kind == "continue").unwrap();
        assert!(has_edge(&g, cont.id, header.id));

        let brk = g.nodes.iter().find(|n| n.kind == "break").unwrap();
        let finish = node(&g, "Finish(…)");
        assert!(has_edge(&g, brk.id, finish));
        assert_eq!(edge(&g, header.id, "done"), Some(finish));
    }

    #[test]
    fn switch_labels_every_section_including_the_default() {
        let g = graph(
            &method(
                "        switch (n) {\n            case 0:\n            case 1:\n                Small();\n                break;\n            default:\n                Large();\n                break;\n        }\n        After();",
            ),
            "Run",
        );
        let m = g.nodes.iter().find(|n| n.kind == "match").unwrap();
        assert_eq!(m.label, "switch n");

        // Labels stacked on one body all lead to that body's first statement.
        let small = node(&g, "Small(…)");
        assert_eq!(edge(&g, m.id, "0"), Some(small));
        assert_eq!(edge(&g, m.id, "1"), Some(small));
        assert_eq!(edge(&g, m.id, "default"), Some(node(&g, "Large(…)")));

        // Each section's `break` leaves the switch for the statement after it.
        let after = node(&g, "After(…)");
        for brk in g.nodes.iter().filter(|n| n.kind == "break") {
            assert!(
                has_edge(&g, brk.id, after),
                "break {} leaves the switch",
                brk.id
            );
        }
    }

    #[test]
    fn a_switch_with_no_default_can_match_nothing() {
        let g = graph(
            &method(
                "        switch (n) {\n            case 0:\n                Small();\n                break;\n        }\n        After();",
            ),
            "Run",
        );
        let m = g.nodes.iter().find(|n| n.kind == "match").unwrap();
        let after = node(&g, "After(…)");
        assert_eq!(
            edge(&g, m.id, "no match"),
            Some(after),
            "an unmatched value skips the switch whole: {:?}",
            g.edges
        );

        // With a default there is no such path.
        let g = graph(
            &method(
                "        switch (n) {\n            case 0:\n                Small();\n                break;\n            default:\n                Large();\n                break;\n        }\n        After();",
            ),
            "Run",
        );
        let m = g.nodes.iter().find(|n| n.kind == "match").unwrap();
        assert_eq!(edge(&g, m.id, "no match"), None);
    }

    #[test]
    fn a_section_that_returns_does_not_leave_the_switch() {
        let g = graph(
            &method(
                "        switch (n) {\n            case 0:\n                return;\n            default:\n                Large();\n                break;\n        }\n        After();",
            ),
            "Run",
        );
        let exit = g.nodes.iter().find(|n| n.kind == "exit").unwrap().id;
        let ret = g.nodes.iter().find(|n| n.kind == "return").unwrap();
        assert!(has_edge(&g, ret.id, exit));
        assert!(
            !has_edge(&g, ret.id, node(&g, "After(…)")),
            "a returning section diverges"
        );
    }

    #[test]
    fn a_break_in_a_switch_leaves_the_switch_not_the_loop() {
        let g = graph(
            &method(
                "        while (More()) {\n            switch (n) {\n                case 0:\n                    break;\n            }\n            Tick();\n        }\n        Finish();",
            ),
            "Run",
        );
        let header = g.nodes.iter().find(|n| n.kind == "loop").unwrap();
        assert_eq!(header.label, "while (More())");
        let brk = g.nodes.iter().find(|n| n.kind == "break").unwrap();
        let tick = node(&g, "Tick(…)");
        assert!(
            has_edge(&g, brk.id, tick),
            "the break resumes after the switch, inside the loop: {:?}",
            g.edges
        );
        assert!(
            !has_edge(&g, brk.id, node(&g, "Finish(…)")),
            "the break must not leave the loop"
        );
    }

    #[test]
    fn a_continue_inside_a_switch_still_targets_the_loop() {
        let g = graph(
            &method(
                "        while (More()) {\n            switch (n) {\n                case 0:\n                    continue;\n            }\n            Tick();\n        }",
            ),
            "Run",
        );
        let header = g.nodes.iter().find(|n| n.kind == "loop").unwrap();
        let cont = g.nodes.iter().find(|n| n.kind == "continue").unwrap();
        assert!(has_edge(&g, cont.id, header.id));
    }

    #[test]
    fn try_and_catch_join_through_the_finally() {
        let g = graph(
            &method(
                "        try {\n            Commit();\n        } catch (IOException e) {\n            Rollback();\n        } catch {\n            Report();\n        } finally {\n            Close();\n        }\n        After();",
            ),
            "Run",
        );
        let t = g.nodes.iter().find(|n| n.kind == "branch").unwrap();
        assert_eq!(t.label, "try");
        assert_eq!(edge(&g, t.id, "try"), Some(node(&g, "Commit(…)")));
        assert_eq!(
            edge(&g, t.id, "catch (IOException e)"),
            Some(node(&g, "Rollback(…)"))
        );
        assert_eq!(edge(&g, t.id, "catch"), Some(node(&g, "Report(…)")));

        // Everything runs the finally, and only the finally carries on.
        let close = node(&g, "Close(…)");
        for label in ["Commit(…)", "Rollback(…)", "Report(…)"] {
            assert!(has_edge(&g, node(&g, label), close), "{label} runs Close");
        }
        assert!(has_edge(&g, close, node(&g, "After(…)")));
    }

    #[test]
    fn a_throw_ends_the_function() {
        let g = graph(
            &method("        throw new ArgumentException(\"bad\");"),
            "Run",
        );
        let exit = g.nodes.iter().find(|n| n.kind == "exit").unwrap().id;
        let throw = g.nodes.iter().find(|n| n.kind == "throw").unwrap();
        assert_eq!(throw.label, "throw new ArgumentException(\"bad\");");
        assert!(has_edge(&g, throw.id, exit));
        // The exception is constructed on the way.
        assert!(has_edge(&g, node(&g, "new ArgumentException(…)"), throw.id));
    }

    #[test]
    fn returns_reach_the_exit_node() {
        let source = "class C {\n    int Run(int n) {\n        if (n > 0) {\n            return Compute(n);\n        }\n        return 0;\n    }\n}\n";
        let g = graph(source, "Run");
        let exit = g.nodes.iter().find(|n| n.kind == "exit").unwrap().id;
        let returns: Vec<_> = g.nodes.iter().filter(|n| n.kind == "return").collect();
        assert_eq!(returns.len(), 2);
        for r in &returns {
            assert!(has_edge(&g, r.id, exit));
        }
        assert_eq!(kinds(&g, "call"), vec!["Compute(…)"]);
    }

    #[test]
    fn the_last_statement_of_a_block_is_not_an_implicit_return() {
        let g = graph(&method("        Work();"), "Run");
        assert!(
            kinds(&g, "return").is_empty(),
            "C# has no trailing-expression return: {:?}",
            g.nodes
        );
        let exit = g.nodes.iter().find(|n| n.kind == "exit").unwrap().id;
        assert!(has_edge(&g, node(&g, "Work(…)"), exit));
    }

    #[test]
    fn an_arrow_body_returns_its_expression() {
        let source = "class C {\n    int Run(int n) => Compute(n) + 1;\n}\n";
        let g = graph(source, "Run");
        assert_eq!(kinds(&g, "call"), vec!["Compute(…)"]);
        assert_eq!(kinds(&g, "return"), vec!["Compute(n) + 1"]);
        let exit = g.nodes.iter().find(|n| n.kind == "exit").unwrap().id;
        assert!(has_edge(&g, node(&g, "Compute(n) + 1"), exit));
    }

    #[test]
    fn calls_are_labelled_by_callee_and_new_by_type() {
        let g = graph(
            &method("        var c = new List<int>();\n        Log.Write(c.Count());"),
            "Run",
        );
        assert_eq!(
            kinds(&g, "call"),
            vec!["new List<int>(…)", "c.Count(…)", "Log.Write(…)"],
            "arguments are evaluated before the call they feed"
        );
    }

    #[test]
    fn lambdas_and_local_functions_are_not_descended_into() {
        let g = graph(
            &method(
                "        var f = () => Deferred();\n        void Inner() { AlsoDeferred(); }\n        Send(f);",
            ),
            "Run",
        );
        assert_eq!(kinds(&g, "call"), vec!["Send(…)"]);
    }

    #[test]
    fn a_switch_expression_is_a_match_in_value_position() {
        let g = graph(
            &method(
                "        var label = n switch { 0 => Zero(), var x when x > 2 => Many(x), _ => Some() };\n        Use(label);",
            ),
            "Run",
        );
        let m = g.nodes.iter().find(|n| n.kind == "match").unwrap();
        assert_eq!(m.label, "switch n");
        assert_eq!(edge(&g, m.id, "0"), Some(node(&g, "Zero(…)")));
        assert_eq!(
            edge(&g, m.id, "var x when x > 2"),
            Some(node(&g, "Many(…)"))
        );
        assert_eq!(edge(&g, m.id, "_"), Some(node(&g, "Some(…)")));
        // Every arm flows on to the statement that uses the value.
        let use_it = node(&g, "Use(…)");
        for label in ["Zero(…)", "Many(…)", "Some(…)"] {
            assert!(has_edge(&g, node(&g, label), use_it), "{label} rejoins");
        }
    }

    #[test]
    fn yield_return_carries_on_and_yield_break_exits() {
        let source = "class C {\n    IEnumerable<int> Run() {\n        yield return First();\n        yield break;\n    }\n}\n";
        let g = graph(source, "Run");
        let y = g.nodes.iter().find(|n| n.kind == "yield").unwrap();
        assert_eq!(y.label, "yield return First();");
        assert!(has_edge(&g, node(&g, "First(…)"), y.id));

        let exit = g.nodes.iter().find(|n| n.kind == "exit").unwrap().id;
        let ret = g.nodes.iter().find(|n| n.kind == "return").unwrap();
        assert_eq!(ret.label, "yield break;");
        assert!(has_edge(&g, y.id, ret.id));
        assert!(has_edge(&g, ret.id, exit));
    }

    #[test]
    fn a_using_body_is_walked_after_its_header() {
        let g = graph(
            &method("        using (var s = Open()) {\n            Read(s);\n        }"),
            "Run",
        );
        assert_eq!(kinds(&g, "call"), vec!["Open(…)", "Read(…)"]);
    }

    #[test]
    fn scope_picks_the_right_class_when_the_line_has_moved() {
        // Two `Go`s; the recorded line now points inside A.Go, as it would
        // after the file was edited without reindexing.
        let source = "class A {\n    void Go() {\n        AGo();\n    }\n}\n\nclass B {\n    void Go() {\n        BGo();\n    }\n}\n";
        let g = build(source, "Test.cs", "Go", Some("B"), 2).unwrap();
        assert_eq!(kinds(&g, "call"), vec!["BGo(…)"]);

        let g = build(source, "Test.cs", "Go", Some("A"), 8).unwrap();
        assert_eq!(kinds(&g, "call"), vec!["AGo(…)"]);
    }

    /// Every node other than the entry must be reachable: a node with no
    /// incoming edge is one the renderer silently drops, which is how a
    /// `return` inside a `try` once made its `finally` disappear.
    #[test]
    fn no_fixture_leaves_an_unreachable_node() {
        let fixtures: Vec<String> = vec![
            method("        try {\n            return;\n        } finally {\n            Close();\n        }"),
            method("        try {\n            Commit();\n        } catch (IOException e) when (Bad(e)) {\n            Rollback();\n        } finally {\n            Close();\n        }\n        After();"),
            method("        try {\n            Commit();\n        } catch (IOException e) {\n            return;\n        } finally {\n            Close();\n        }\n        After();"),
            method("        var label = n switch { 0 => \"new\", 1 => Pay(), _ => \"?\" };\n        Use(label);"),
            method("        Target()[Index()] = n switch { 0 => Zero(), _ => Other() };"),
            method("        var v = n switch { 0 => n switch { 1 => A(), _ => B() }, _ => C2() };\n        Use(v);"),
            method("        while (More()) {\n            try {\n                break;\n            } finally {\n                Close();\n            }\n        }\n        After();"),
            method("        foreach (var x in Items()) {\n            if (Skip(x)) {\n                continue;\n            }\n            Handle(x);\n        }"),
            method("        switch (n) {\n            case 0:\n            case 1:\n                Small();\n                break;\n            default:\n                return;\n        }\n        After();"),
            method("        switch (n) {\n            case 0:\n                Only();\n                break;\n        }\n        After();"),
            method("        if (n > 0) {\n            goto done;\n        }\n        A();\n        done: B();"),
            method("        do {\n            Tick();\n        } while (More());\n        After();"),
            method("        var v = n switch { 0 => Zero(), _ => Other() };\n        Use(v);"),
            method("        lock (gate) {\n            Guarded();\n        }\n        After();"),
            method("        throw new ArgumentException(\"bad\");"),
            "class C {\n    int Run(int n) => n switch { 0 => Zero(), _ => Other() };\n}\n".to_string(),
            "class C {\n    IEnumerable<int> Run() {\n        yield return First();\n        yield break;\n    }\n}\n".to_string(),
        ];

        for source in &fixtures {
            let g = graph(source, "Run");
            for node in g.nodes.iter().skip(1) {
                assert!(
                    g.edges.iter().any(|e| e.to == node.id),
                    "#{} {} ({}) has no incoming edge in:\n{source}\nedges: {:?}",
                    node.id,
                    node.kind,
                    node.label,
                    g.edges
                );
            }
        }
    }

    #[test]
    fn a_return_inside_a_try_runs_the_finally_on_its_way_out() {
        let g = graph(
            &method(
                "        try {\n            return Compute();\n        } finally {\n            Close();\n        }",
            ),
            "Run",
        );
        let compute = node(&g, "Compute(…)");
        let close = node(&g, "Close(…)");
        let ret = g.nodes.iter().find(|n| n.kind == "return").unwrap();
        let exit = g.nodes.iter().find(|n| n.kind == "exit").unwrap().id;

        // The value is computed, then the finally, then the jump.
        assert!(has_edge(&g, compute, close), "{:?}", g.edges);
        assert!(has_edge(&g, close, ret.id), "{:?}", g.edges);
        assert!(has_edge(&g, ret.id, exit));
        assert_eq!(
            g.nodes.iter().filter(|n| n.label == "Close(…)").count(),
            1,
            "only the returning path runs it, so only one copy: {:?}",
            g.nodes
        );
    }

    #[test]
    fn a_break_out_of_a_try_runs_the_finally_but_a_break_inside_one_does_not() {
        // The try is inside the loop, so leaving the loop leaves the try.
        let g = graph(
            &method(
                "        while (More()) {\n            try {\n                break;\n            } finally {\n                Close();\n            }\n        }\n        After();",
            ),
            "Run",
        );
        let close = node(&g, "Close(…)");
        let brk = g.nodes.iter().find(|n| n.kind == "break").unwrap();
        assert!(has_edge(&g, close, brk.id), "{:?}", g.edges);
        assert!(has_edge(&g, brk.id, node(&g, "After(…)")));

        // The loop is inside the try, so the break never leaves the try and the
        // finally does not run on that path.
        let g = graph(
            &method(
                "        try {\n            while (More()) {\n                break;\n            }\n            Done();\n        } finally {\n            Close();\n        }",
            ),
            "Run",
        );
        let brk = g.nodes.iter().find(|n| n.kind == "break").unwrap();
        assert!(
            has_edge(&g, brk.id, node(&g, "Done(…)")),
            "the break resumes inside the try: {:?}",
            g.edges
        );
        assert!(
            !has_edge(&g, brk.id, node(&g, "Close(…)")),
            "the finally is not on the break's path: {:?}",
            g.edges
        );
    }

    #[test]
    fn a_diverging_path_in_a_catch_also_runs_the_finally() {
        // A `return` in a catch leaves the try just as one in the body does.
        let g = graph(
            &method(
                "        try {\n            Commit();\n        } catch (IOException e) {\n            return;\n        } finally {\n            Close();\n        }\n        After();",
            ),
            "Run",
        );
        let closes: Vec<usize> = g
            .nodes
            .iter()
            .filter(|n| n.label == "Close(…)")
            .map(|n| n.id)
            .collect();
        assert_eq!(
            closes.len(),
            2,
            "one copy for the catch's return, one for the path that falls out: {:?}",
            g.nodes
        );
        let ret = g.nodes.iter().find(|n| n.kind == "return").unwrap();
        assert!(
            closes.iter().any(|c| has_edge(&g, *c, ret.id)),
            "the returning path runs a finally first: {:?}",
            g.edges
        );

        // A bare `throw;` rethrow is the same kind of exit.
        let g = graph(
            &method(
                "        try {\n            Commit();\n        } catch (IOException e) {\n            throw;\n        } finally {\n            Close();\n        }",
            ),
            "Run",
        );
        let throw = g.nodes.iter().find(|n| n.kind == "throw").unwrap();
        assert!(
            g.nodes
                .iter()
                .filter(|n| n.label == "Close(…)")
                .any(|c| has_edge(&g, c.id, throw.id)),
            "the rethrow runs the finally on its way out: {:?}",
            g.edges
        );
    }

    #[test]
    fn switch_arms_are_edges_in_source_order() {
        // Only the second arm makes a call, so only it creates its edge while
        // the arms are walked; the rest are created when the arms join.
        let g = graph(
            &method(
                "        var label = n switch { 0 => \"new\", 1 => Pay(), 2 => \"void\", _ => \"?\" };\n        Use(label);",
            ),
            "Run",
        );
        let m = g.nodes.iter().find(|n| n.kind == "match").unwrap();
        let order: Vec<&str> = g
            .edges
            .iter()
            .filter(|e| e.from == m.id)
            .filter_map(|e| e.label.as_deref())
            .collect();
        assert_eq!(order, vec!["0", "1", "2", "_"], "{:?}", g.edges);

        // The same for a switch statement, whose `no match` edge sorts last.
        let g = graph(
            &method(
                "        switch (n) {\n            case 0:\n                break;\n            case 1:\n                One();\n                break;\n            case 2:\n                break;\n        }\n        After();",
            ),
            "Run",
        );
        let m = g.nodes.iter().find(|n| n.kind == "match").unwrap();
        let order: Vec<&str> = g
            .edges
            .iter()
            .filter(|e| e.from == m.id)
            .filter_map(|e| e.label.as_deref())
            .collect();
        assert_eq!(order, vec!["0", "1", "2", "no match"], "{:?}", g.edges);
    }

    #[test]
    fn a_long_member_chain_keeps_the_method_being_called() {
        let g = graph(
            &method(
                "        var top = all.Where(x => x.Ok).Select(x => x.Name).OrderBy(x => x).ToList();",
            ),
            "Run",
        );
        let calls = kinds(&g, "call");
        assert_eq!(calls.len(), 4, "one node per link in the chain: {calls:?}");
        // The last two are long enough to be elided; the method name is what
        // has to survive, so the elision eats the receiver instead.
        for (call, method) in calls.iter().zip(["Where", "Select", "OrderBy", "ToList"]) {
            assert!(
                call.contains(method),
                "{call} should name {method}: {calls:?}"
            );
            assert!(
                call.chars().count() <= LABEL_WIDTH + 3,
                "{call} is too wide"
            );
        }
        assert!(
            calls[3].starts_with('…'),
            "a chain too long to print is cut from the left: {calls:?}"
        );
    }

    #[test]
    fn a_switch_expression_is_recognised_in_return_and_assignment_position() {
        let g = graph(
            "class C {\n    int Run(int n) {\n        return n switch { 0 => Zero(), _ => Other() };\n    }\n}\n",
            "Run",
        );
        let m = g.nodes.iter().find(|n| n.kind == "match").unwrap();
        assert_eq!(edge(&g, m.id, "0"), Some(node(&g, "Zero(…)")));
        assert_eq!(edge(&g, m.id, "_"), Some(node(&g, "Other(…)")));
        let ret = g.nodes.iter().find(|n| n.kind == "return").unwrap();
        for arm in ["Zero(…)", "Other(…)"] {
            assert!(has_edge(&g, node(&g, arm), ret.id), "{arm} returns");
        }

        let g = graph(
            &method("        v = n switch { 0 => Zero(), _ => Other() };\n        Use(v);"),
            "Run",
        );
        let m = g.nodes.iter().find(|n| n.kind == "match").unwrap();
        assert_eq!(edge(&g, m.id, "0"), Some(node(&g, "Zero(…)")));
        assert!(has_edge(&g, node(&g, "Other(…)"), node(&g, "Use(…)")));

        let g = graph(
            "class C {\n    int Run(int n) => n switch { 0 => Zero(), _ => Other() };\n}\n",
            "Run",
        );
        assert!(g.nodes.iter().any(|n| n.kind == "match"), "{:?}", g.nodes);
        assert_eq!(kinds(&g, "return").len(), 1);
    }

    #[test]
    fn calls_outside_a_switch_in_value_position_still_run() {
        // The left-hand side is evaluated before the switch picks the value,
        // so those calls belong in the graph and belong before the match.
        let g = graph(
            &method("        Target()[Index()] = n switch { 0 => Zero(), _ => Other() };"),
            "Run",
        );
        let m = g.nodes.iter().find(|n| n.kind == "match").unwrap();
        let target = node(&g, "Target(…)");
        let index = node(&g, "Index(…)");
        assert!(has_edge(&g, target, index), "{:?}", g.edges);
        assert!(
            has_edge(&g, index, m.id),
            "the switch runs after the target it assigns to: {:?}",
            g.edges
        );
        assert_eq!(edge(&g, m.id, "0"), Some(node(&g, "Zero(…)")));

        // The same for a member path and for an indexer alone.
        let g = graph(
            &method("        Obj().Field = n switch { 0 => Zero(), _ => Other() };"),
            "Run",
        );
        assert!(has_edge(
            &g,
            node(&g, "Obj(…)"),
            g.nodes.iter().find(|n| n.kind == "match").unwrap().id
        ));

        let g = graph(
            &method("        var v = arr[Index()] + (n switch { 0 => Zero(), _ => Other() });"),
            "Run",
        );
        assert!(
            g.nodes.iter().any(|n| n.label == "Index(…)"),
            "an index beside the switch is still a call: {:?}",
            g.nodes
        );
    }

    #[test]
    fn a_switch_expression_nested_in_an_arm_branches_again() {
        let g = graph(
            &method(
                "        var v = n switch { 0 => n switch { 1 => A(), _ => B() }, _ => C2() };\n        Use(v);",
            ),
            "Run",
        );
        let matches: Vec<usize> = g
            .nodes
            .iter()
            .filter(|n| n.kind == "match")
            .map(|n| n.id)
            .collect();
        assert_eq!(
            matches.len(),
            2,
            "the inner switch is a decision too: {:?}",
            g.nodes
        );

        let outer = matches[0];
        let inner = matches[1];
        assert_eq!(edge(&g, outer, "0"), Some(inner));
        assert_eq!(edge(&g, inner, "1"), Some(node(&g, "A(…)")));
        assert_eq!(edge(&g, inner, "_"), Some(node(&g, "B(…)")));
        assert_eq!(edge(&g, outer, "_"), Some(node(&g, "C2(…)")));

        // Both levels rejoin on the statement that uses the value.
        let use_it = node(&g, "Use(…)");
        for arm in ["A(…)", "B(…)", "C2(…)"] {
            assert!(has_edge(&g, node(&g, arm), use_it), "{arm} rejoins");
        }
    }

    #[test]
    fn goto_ends_the_path_instead_of_joining_the_next_statement() {
        let g = graph(
            &method(
                "        if (n > 0) {\n            goto done;\n        }\n        A();\n        done: B();",
            ),
            "Run",
        );
        let jump = g.nodes.iter().find(|n| n.kind == "goto").unwrap();
        assert_eq!(jump.label, "goto done;");
        assert!(
            !g.edges.iter().any(|e| e.from == jump.id),
            "a goto leads somewhere the graph does not track: {:?}",
            g.edges
        );
        let branch = g.nodes.iter().find(|n| n.kind == "branch").unwrap();
        assert_eq!(edge(&g, branch.id, "true"), Some(jump.id));
        assert_eq!(edge(&g, branch.id, "false"), Some(node(&g, "A(…)")));

        // `goto case` must not be joined onto whatever follows the switch.
        let g = graph(
            &method(
                "        switch (n) {\n            case 1:\n                goto case 2;\n            case 2:\n                A();\n                break;\n        }\n        After();",
            ),
            "Run",
        );
        let jump = g.nodes.iter().find(|n| n.kind == "goto").unwrap();
        assert_eq!(jump.label, "goto case 2;");
        assert!(
            !has_edge(&g, jump.id, node(&g, "After(…)")),
            "the section does not fall out of the switch: {:?}",
            g.edges
        );
    }

    #[test]
    fn a_do_loop_repeats_and_leaves() {
        let g = graph(
            &method(
                "        do {\n            Tick();\n        } while (More());\n        After();",
            ),
            "Run",
        );
        let header = g.nodes.iter().find(|n| n.kind == "loop").unwrap();
        assert_eq!(header.label, "do while (More())");
        assert_eq!(edge(&g, header.id, "body"), Some(node(&g, "Tick(…)")));
        assert_eq!(edge(&g, node(&g, "Tick(…)"), "repeat"), Some(header.id));
        assert_eq!(edge(&g, header.id, "done"), Some(node(&g, "After(…)")));
    }

    #[test]
    fn lock_checked_and_labelled_statements_are_walked_through() {
        let g = graph(
            &method(
                "        lock (Gate()) {\n            Guarded();\n        }\n        checked {\n            Counted();\n        }\n        top: Tagged();",
            ),
            "Run",
        );
        assert_eq!(
            kinds(&g, "call"),
            vec!["Gate(…)", "Guarded(…)", "Counted(…)", "Tagged(…)"],
            "the lock subject runs before its body: {:?}",
            g.nodes
        );
    }

    #[test]
    fn a_constructor_is_a_valid_target() {
        let g = graph(
            "class Person {\n    public Person(string name) {\n        Validate(name);\n    }\n}\n",
            "Person",
        );
        assert_eq!(g.nodes[0].label, "Person(string name)");
        assert_eq!(kinds(&g, "call"), vec!["Validate(…)"]);
    }

    #[test]
    fn a_catch_filter_is_in_the_label_and_its_calls_run() {
        let g = graph(
            &method(
                "        try {\n            Commit();\n        } catch (Exception e) when (Bad(e)) {\n            Rollback();\n        }\n        After();",
            ),
            "Run",
        );
        let t = g.nodes.iter().find(|n| n.kind == "branch").unwrap();
        let bad = node(&g, "Bad(…)");
        assert_eq!(
            edge(&g, t.id, "catch (Exception e) when (Bad(e))"),
            Some(bad),
            "the filter belongs in the label and runs first: {:?}",
            g.edges
        );
        assert!(has_edge(&g, bad, node(&g, "Rollback(…)")));
    }

    #[test]
    fn a_property_with_accessors_is_rejected() {
        let source = "class C {\n    int Rate { get { return Lookup(); } }\n}\n";
        let err = build(source, "Test.cs", "Rate", Some("C"), 2).unwrap_err();
        assert!(
            err.to_string()
                .contains("C.Rate is a property with accessors"),
            "unexpected error: {err}"
        );
    }
}
