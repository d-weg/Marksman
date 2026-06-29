//! SCIP + tree-sitter merge. tree-sitter (in-process, no external tool) subdivides
//! each SCIP-anchored symbol into local sub-nodes (parameters / return type / body).
//! tree-sitter is used ONLY for local syntactic structure inside a range the SCIP
//! already pinned semantically — never for cross-file/semantic work — so its
//! precision limits don't apply. This turns `structure()` from symbol-level into
//! AST-level (`Granularity::Ast`), unlocking precise sub-symbol edits.
use ci_core::{Node, NodeKind, Range};
use tree_sitter::{Node as TsNode, Parser};

/// Attach tree-sitter sub-nodes as children of each SCIP symbol. On any parse
/// failure the symbols are returned unchanged (shallow).
pub fn deepen(content: &str, scip_nodes: Vec<Node>) -> Vec<Node> {
    let mut parser = Parser::new();
    let lang: tree_sitter::Language = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
    if parser.set_language(&lang).is_err() {
        return scip_nodes;
    }
    let Some(tree) = parser.parse(content, None) else { return scip_nodes };
    let root = tree.root_node();
    let bytes = content.as_bytes();

    scip_nodes
        .into_iter()
        .map(|mut n| {
            if let Some(children) = subnodes(&root, content, bytes, &n) {
                n.children = children;
            }
            n
        })
        .collect()
}

fn subnodes(root: &TsNode, content: &str, bytes: &[u8], sym: &Node) -> Option<Vec<Node>> {
    // Anchor on the symbol's name position (most reliable), else its full range.
    let anchor = sym.name_range.as_ref().unwrap_or(&sym.range);
    let s = point_byte(content, anchor.start_line, anchor.start_char)?;
    let e = point_byte(content, anchor.end_line, anchor.end_char)?.max(s + 1);
    let decl = decl_with_fields(root.descendant_for_byte_range(s, e)?);

    let mut children = Vec::new();
    if let Some(params) = decl.child_by_field_name("parameters") {
        let mut cursor = params.walk();
        for (i, p) in params.named_children(&mut cursor).enumerate() {
            let name = p.utf8_text(bytes).ok().map(str::to_string);
            children.push(syntax(&format!("{}:param.{i}", sym.id), name, "parameter", &p));
        }
    }
    if let Some(rt) = decl.child_by_field_name("return_type") {
        children.push(syntax(&format!("{}:return", sym.id), None, "returnType", &rt));
    }
    if let Some(body) = decl.child_by_field_name("body") {
        children.push(syntax(&format!("{}:body", sym.id), None, "body", &body));
    }
    if children.is_empty() {
        None
    } else {
        Some(children)
    }
}

/// Climb to the nearest ancestor (or self) that has parameter/body fields — the
/// enclosing declaration the SCIP name anchor sits inside.
fn decl_with_fields(mut n: TsNode) -> TsNode {
    loop {
        if n.child_by_field_name("body").is_some() || n.child_by_field_name("parameters").is_some() {
            return n;
        }
        match n.parent() {
            Some(p) => n = p,
            None => return n,
        }
    }
}

fn syntax(id: &str, name: Option<String>, kind: &str, n: &TsNode) -> Node {
    Node {
        id: id.to_string(),
        name,
        kind: NodeKind::Syntax(kind.to_string()),
        range: ts_range(n),
        name_range: None,
        children: vec![],
    }
}

fn ts_range(n: &TsNode) -> Range {
    let s = n.start_position();
    let e = n.end_position();
    Range {
        start_line: s.row as u32 + 1,
        start_char: s.column as u32,
        end_line: e.row as u32 + 1,
        end_char: e.column as u32,
    }
}

/// Byte offset of (1-based line, 0-based char). ASCII-accurate (code is ~ASCII).
fn point_byte(content: &str, line_1: u32, char_0: u32) -> Option<usize> {
    if line_1 == 0 {
        return None;
    }
    let mut off = 0;
    let mut ln = 1u32;
    for l in content.split_inclusive('\n') {
        if ln == line_1 {
            let add: usize = l.chars().take(char_0 as usize).map(char::len_utf8).sum();
            return Some(off + add);
        }
        off += l.len();
        ln += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use ci_core::SymbolKind;

    #[test]
    fn deepen_adds_params_return_body() {
        let content = "export function add(a: number, b: number): number {\n  return a + b;\n}\n";
        let scip = vec![Node {
            id: "m.ts#add".into(),
            name: Some("add".into()),
            kind: NodeKind::Symbol(SymbolKind::Function),
            range: Range { start_line: 1, start_char: 0, end_line: 3, end_char: 1 },
            name_range: Some(Range { start_line: 1, start_char: 16, end_line: 1, end_char: 19 }),
            children: vec![],
        }];

        let deep = deepen(content, scip);
        let add = &deep[0];
        let kinds: Vec<&str> = add
            .children
            .iter()
            .filter_map(|c| match &c.kind {
                NodeKind::Syntax(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert!(kinds.contains(&"parameter"), "expected params, got {kinds:?}");
        assert!(kinds.contains(&"returnType"), "expected returnType, got {kinds:?}");
        assert!(kinds.contains(&"body"), "expected body, got {kinds:?}");

        // The body sub-node is addressable and spans the block { ... }.
        let body = add.children.iter().find(|c| c.id == "m.ts#add:body").expect("body node");
        assert_eq!(body.range.start_line, 1);
        assert_eq!(body.range.end_line, 3);
    }
}
