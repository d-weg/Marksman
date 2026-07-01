//! Shared tree-sitter → `ci-core` glue.
//!
//! Every `LanguageProvider` backed by tree-sitter repeats the same handful of mechanical
//! conversions: a node's span → a [`ci_core::Range`], a sub-symbol `Syntax` leaf, the span of
//! the leading doc-comment block, and the function-body byte ranges to fold for an outline.
//! Those are grammar-*independent* — they only touch positions, `body` fields, and
//! caller-supplied node-kind names — so they live here once. Each language crate keeps only
//! *its grammar's node-kind strings* (`"function_item"`, `"statement_block"`, `"comment"`, …).
use ci_core::{Node, NodeKind, Range};
use tree_sitter::Node as TsNode;

/// The `ci-core` [`Range`] for a single tree-sitter node. tree-sitter positions are 0-based
/// (row, column); the manifest contract is 1-based lines with 0-based chars.
pub fn ts_range(n: &TsNode) -> Range {
    range_between(n, n)
}

/// [`Range`] from the start of `first` to the end of `last` (pass the same node twice for one
/// node's own span). Both bounds converted to the 1-based-line / 0-based-char contract.
fn range_between(first: &TsNode, last: &TsNode) -> Range {
    let s = first.start_position();
    let e = last.end_position();
    Range {
        start_line: s.row as u32 + 1,
        start_char: s.column as u32,
        end_line: e.row as u32 + 1,
        end_char: e.column as u32,
    }
}

/// A `Syntax`-kind leaf [`Node`] — the sub-symbol anchors (`parameter`, `returnType`, `body`,
/// `doc`, …) providers hang under a named declaration. `kind` is the free-form syntax tag.
pub fn syntax_node(id: &str, name: Option<String>, kind: &str, n: &TsNode) -> Node {
    Node {
        id: id.to_string(),
        name,
        kind: NodeKind::Syntax(kind.to_string()),
        range: ts_range(n),
        name_range: None,
        children: vec![],
    }
}

/// [`Range`] over the contiguous run of leading sibling nodes directly above `node` whose kind
/// satisfies `is_comment` — the doc-comment block that anchors a `:doc` sub-node. `None` when the
/// immediately-preceding sibling isn't a comment. The caller climbs to whatever node level its
/// grammar attaches comments as siblings of, and decides which kinds count as comments.
pub fn leading_comment_range(node: &TsNode, is_comment: impl Fn(&TsNode) -> bool) -> Option<Range> {
    let last = node.prev_sibling().filter(&is_comment)?;
    let mut first = last;
    while let Some(prev) = first.prev_sibling() {
        if is_comment(&prev) {
            first = prev;
        } else {
            break;
        }
    }
    Some(range_between(&first, &last))
}

/// Byte ranges of the `body` fields to fold for a skeletal outline (feed to
/// [`ci_core::elide_bodies`]). A body is collected when the enclosing node's kind is in
/// `def_kinds` (empty = accept any enclosing node) AND the body node's own kind is in
/// `body_kinds` (empty = accept any body). Recurses the whole named-node tree; the elide step
/// keeps only the outermost of overlapping ranges, so nested closures are subsumed.
pub fn body_ranges(root: TsNode, def_kinds: &[&str], body_kinds: &[&str]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    collect_bodies(root, def_kinds, body_kinds, &mut out);
    out
}

fn collect_bodies(
    node: TsNode,
    def_kinds: &[&str],
    body_kinds: &[&str],
    out: &mut Vec<(usize, usize)>,
) {
    if def_kinds.is_empty() || def_kinds.contains(&node.kind()) {
        if let Some(body) = node.child_by_field_name("body") {
            if body_kinds.is_empty() || body_kinds.contains(&body.kind()) {
                out.push((body.start_byte(), body.end_byte()));
            }
        }
    }
    let mut c = node.walk();
    for ch in node.named_children(&mut c) {
        collect_bodies(ch, def_kinds, body_kinds, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn parse(src: &str) -> tree_sitter::Tree {
        let mut p = Parser::new();
        p.set_language(&tree_sitter_rust::LANGUAGE.into()).unwrap();
        p.parse(src, None).unwrap()
    }

    #[test]
    fn ts_range_is_one_based_line_zero_based_char() {
        let src = "fn a() {}\n";
        let tree = parse(src);
        let func = tree.root_node().named_child(0).unwrap();
        let r = ts_range(&func);
        assert_eq!((r.start_line, r.start_char), (1, 0));
        assert_eq!(r.end_line, 1);
    }

    #[test]
    fn body_ranges_filters_by_def_and_body_kind() {
        let src = "fn a() { let x = 1; }\nstruct S { x: i32 }\n";
        let tree = parse(src);
        // Only `function_item` bodies of kind `block` — the struct's field block is excluded.
        let bodies = body_ranges(tree.root_node(), &["function_item"], &["block"]);
        assert_eq!(bodies.len(), 1, "one fn body, not the struct: {bodies:?}");
        let (s, e) = bodies[0];
        assert_eq!(&src[s..e], "{ let x = 1; }");
    }

    fn find_kind<'a>(root: TsNode<'a>, kind: &str) -> TsNode<'a> {
        let mut c = root.walk();
        let kids: Vec<TsNode<'a>> = root.named_children(&mut c).collect();
        kids.into_iter().find(|n| n.kind() == kind).expect("node kind present")
    }

    #[test]
    fn leading_comment_range_spans_contiguous_comments() {
        // Root children are [line_comment, line_comment, function_item]; anchor on the fn.
        let src = "/// one\n/// two\nfn a() {}\n";
        let tree = parse(src);
        let func = find_kind(tree.root_node(), "function_item");
        let is_comment = |n: &TsNode| matches!(n.kind(), "line_comment" | "block_comment");
        let r = leading_comment_range(&func, is_comment).expect("has doc comments");
        // Starts at the first `///`; ends at/after the second (line_comment nodes include the
        // trailing newline, so the end lands on the following line — the fn's line).
        assert_eq!(r.start_line, 1, "anchored at the first doc line");
        assert!(r.end_line >= 2, "spans past the first /// line: {r:?}");
    }

    #[test]
    fn leading_comment_range_none_without_comment() {
        let src = "fn a() {}\n\nstruct S;\n";
        let tree = parse(src);
        // `struct S;`'s previous sibling is a function, not a comment.
        let s = find_kind(tree.root_node(), "struct_item");
        let is_comment = |n: &TsNode| matches!(n.kind(), "line_comment" | "block_comment");
        assert!(leading_comment_range(&s, is_comment).is_none());
    }
}
