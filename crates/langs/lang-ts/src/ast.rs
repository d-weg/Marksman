//! SCIP + tree-sitter merge. tree-sitter (in-process, no external tool) subdivides
//! each SCIP-anchored symbol into local sub-nodes (parameters / return type / body).
//! tree-sitter is used ONLY for local syntactic structure inside a range the SCIP
//! already pinned semantically — never for cross-file/semantic work — so its
//! precision limits don't apply. This turns `structure()` from symbol-level into
//! AST-level (`Granularity::Ast`), unlocking precise sub-symbol edits.
use ci_core::{Node, NodeKind, Range, SymbolKind};
use ci_treesitter::{syntax_node, ts_range};
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
            } else if matches!(n.kind, NodeKind::Symbol(SymbolKind::Variable)) {
                // SCIP gives a field/variable a NAME-ONLY range (`k1`), so `replace_text` /
                // `replace_node` scoped to it can't see its initializer (`k1 = 1.5;`). Widen the
                // range to the enclosing declaration so editing the field by name works.
                if let Some(r) = field_decl_range(&root, content, &n) {
                    n.range = r;
                }
            }
            n
        })
        .collect()
}

/// Climb from a field/variable's name to its full declaration node and return that range, so the
/// node spans `k1 = 1.5;` not just `k1`. Bounded climb; bails if it would leave the member.
fn field_decl_range(root: &TsNode, content: &str, sym: &Node) -> Option<Range> {
    let a = sym.name_range.as_ref().unwrap_or(&sym.range);
    let s = point_byte(content, a.start_line, a.start_char)?;
    let e = point_byte(content, a.end_line, a.end_char)?.max(s + 1);
    let mut n = root.descendant_for_byte_range(s, e)?;
    for _ in 0..6 {
        match n.kind() {
            "public_field_definition" | "field_definition" | "property_declaration"
            | "property_signature" | "variable_declarator" | "lexical_declaration"
            | "enum_assignment" => return Some(ts_range(&n)),
            // don't climb out of the member into the class/program (a function/method shouldn't
            // reach here — it gets sub-nodes instead).
            "class_body" | "statement_block" | "program" => return None,
            _ => {}
        }
        n = n.parent()?;
    }
    None
}

fn subnodes(root: &TsNode, content: &str, bytes: &[u8], sym: &Node) -> Option<Vec<Node>> {
    // Anchor on the symbol's name position (most reliable), else its full range.
    let anchor = sym.name_range.as_ref().unwrap_or(&sym.range);
    let s = point_byte(content, anchor.start_line, anchor.start_char)?;
    let e = point_byte(content, anchor.end_line, anchor.end_char)?.max(s + 1);
    let decl = decl_with_fields(root.descendant_for_byte_range(s, e)?);

    // Guard against climbing PAST the symbol into an enclosing declaration. A class field has no
    // params/body of its own, so `decl_with_fields` would climb to the enclosing CLASS and hand
    // back the class body as the field's `:body` (e.g. `BM25.k1:body` = the whole class). A real
    // function/method's decl starts at/after its own symbol; an ancestor we climbed into starts
    // BEFORE it. Reject the latter so we never emit a sub-node range that isn't the symbol's own.
    let sym_start = point_byte(content, sym.range.start_line, sym.range.start_char)?;
    if decl.start_byte() < sym_start {
        return None;
    }

    let mut children = Vec::new();
    // Leading comment / JSDoc — the `:doc` anchor, editable like any other sub-node.
    if let Some(r) = doc_comment_range(decl) {
        children.push(Node {
            id: format!("{}:doc", sym.id),
            name: None,
            kind: NodeKind::Syntax("doc".into()),
            range: r,
            name_range: None,
            children: vec![],
        });
    }
    if let Some(params) = decl.child_by_field_name("parameters") {
        let mut cursor = params.walk();
        for (i, p) in params.named_children(&mut cursor).enumerate() {
            let name = p.utf8_text(bytes).ok().map(str::to_string);
            children.push(syntax_node(&format!("{}:param.{i}", sym.id), name, "parameter", &p));
        }
    }
    if let Some(rt) = decl.child_by_field_name("return_type") {
        children.push(syntax_node(&format!("{}:return", sym.id), None, "returnType", &rt));
    }
    if let Some(body) = decl.child_by_field_name("body") {
        children.push(syntax_node(&format!("{}:body", sym.id), None, "body", &body));
    }
    if children.is_empty() {
        None
    } else {
        Some(children)
    }
}

/// Range of the leading comment immediately above a declaration (JSDoc `/** */` or `//`), if any.
/// The comment is a sibling of the STATEMENT-level node (the decl, or its `export` wrapper), so
/// climb to that node first, then take the previous sibling. Spans a contiguous run of comment
/// lines so a multi-line `//` block is captured whole (a JSDoc `/** */` is a single comment node).
fn doc_comment_range(decl: TsNode) -> Option<Range> {
    let mut stmt = decl;
    while let Some(p) = stmt.parent() {
        if matches!(p.kind(), "program" | "class_body" | "statement_block" | "module" | "interface_body") {
            break;
        }
        stmt = p;
    }
    ci_treesitter::leading_comment_range(&stmt, |n| n.kind() == "comment")
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

/// Byte offset of (1-based line, 0-based char). ASCII-accurate (code is ~ASCII).
fn point_byte(content: &str, line_1: u32, char_0: u32) -> Option<usize> {
    if line_1 == 0 {
        return None;
    }
    // `line_1` is 1-based, so start the counter at 1; `off` tracks the byte start of each line.
    let mut off = 0;
    for (ln, l) in (1u32..).zip(content.split_inclusive('\n')) {
        if ln == line_1 {
            let add: usize = l.chars().take(char_0 as usize).map(char::len_utf8).sum();
            return Some(off + add);
        }
        off += l.len();
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

    // Regression: a class FIELD has no body of its own, so it must NOT borrow the enclosing
    // class body as its `:body` anchor (which would let `set_body` on a field overwrite the whole
    // class). A real method on the same class still gets its own sub-nodes.
    #[test]
    fn field_does_not_borrow_class_body() {
        let content = "class C {\n  k1 = 1.5;\n  run(): number { return this.k1; }\n}\n";
        let field = Node {
            id: "f.ts#C.k1".into(),
            name: Some("k1".into()),
            kind: NodeKind::Symbol(SymbolKind::Variable),
            range: Range { start_line: 2, start_char: 2, end_line: 2, end_char: 11 },
            name_range: Some(Range { start_line: 2, start_char: 2, end_line: 2, end_char: 4 }),
            children: vec![],
        };
        let method = Node {
            id: "f.ts#C.run".into(),
            name: Some("run".into()),
            kind: NodeKind::Symbol(SymbolKind::Method),
            range: Range { start_line: 3, start_char: 2, end_line: 3, end_char: 35 },
            name_range: Some(Range { start_line: 3, start_char: 2, end_line: 3, end_char: 5 }),
            children: vec![],
        };
        let deep = deepen(content, vec![field, method]);
        let f = deep.iter().find(|n| n.id == "f.ts#C.k1").unwrap();
        assert!(f.children.is_empty(), "field must not borrow the class body: {:?}", f.children);
        let m = deep.iter().find(|n| n.id == "f.ts#C.run").unwrap();
        assert!(m.children.iter().any(|c| c.id == "f.ts#C.run:body"), "method keeps its own :body");
    }

    // Regression: SCIP hands a field a NAME-ONLY range; deepen must widen it to the full
    // declaration so `replace_text`/`replace_node` can see the initializer (`k1 = 1.5;`).
    #[test]
    fn field_range_widens_to_declaration() {
        let content = "class C {\n  k1 = 1.5;\n}\n";
        let field = Node {
            id: "f.ts#C.k1".into(),
            name: Some("k1".into()),
            kind: NodeKind::Symbol(SymbolKind::Variable),
            range: Range { start_line: 2, start_char: 2, end_line: 2, end_char: 4 }, // just "k1"
            name_range: Some(Range { start_line: 2, start_char: 2, end_line: 2, end_char: 4 }),
            children: vec![],
        };
        let k1 = deepen(content, vec![field]).pop().unwrap();
        assert!(k1.range.end_char > 4, "field range widened past the name `k1`: {:?}", k1.range);
    }

    #[test]
    fn leading_comment_becomes_doc_anchor() {
        let content = "/** Adds two numbers. */\nexport function add(a: number): number {\n  return a;\n}\n";
        let scip = vec![Node {
            id: "m.ts#add".into(),
            name: Some("add".into()),
            kind: NodeKind::Symbol(SymbolKind::Function),
            range: Range { start_line: 2, start_char: 0, end_line: 4, end_char: 1 },
            name_range: Some(Range { start_line: 2, start_char: 16, end_line: 2, end_char: 19 }),
            children: vec![],
        }];
        let add = deepen(content, scip).pop().unwrap();
        let doc = add.children.iter().find(|c| c.id == "m.ts#add:doc").expect("doc anchor");
        assert_eq!(doc.range.start_line, 1, "JSDoc on line 1: {:?}", doc.range);
        assert_eq!(doc.range.end_line, 1);
    }
}
