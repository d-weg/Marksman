//! Structure extraction — the live tree-sitter walk that turns a parsed Rust file into the
//! `Node` contract (`structure()`): one `Node` per named declaration, struct fields / enum
//! variants as top-level dotted symbols (`Type.member`), and the `:doc`/`:params`/`:param.N`/
//! `:return`/`:body` sub-node anchors that make declarations surgically addressable.
use ci_core::{Node, NodeKind, Range, SymbolKind};
use ci_treesitter::{syntax_node, ts_range};
use tree_sitter::Node as TsNode;

/// Walk an item list, emitting a `Node` per named declaration. `fn_kind` is the kind for
/// `function_item`s found here (Function at top level, Method inside an `impl`). `prefix` is
/// the id stem (`"file.rs#"`, or `"file.rs#Type."` inside an impl).
pub(crate) fn collect_items(node: TsNode, bytes: &[u8], prefix: &str, fn_kind: SymbolKind, out: &mut Vec<Node>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "function_item" => {
                if let Some(mut n) = named_node(&child, bytes, prefix, fn_kind) {
                    add_fn_subnodes(&mut n, &child, bytes);
                    out.push(n);
                }
            }
            // Structs and enums expose their members the way lang-ts exposes class/interface
            // fields: the container gets a `:body` sub-node (the member-list insertion anchor)
            // and each field/variant is a TOP-LEVEL dotted symbol (`Type.field`) — that is
            // what feeds field-level addressing (replace_text name=Type.field), the index
            // (fields are searchable symbols), and retrieval's inline one-line pointers.
            // Without them a Rust struct was an opaque block the agent had to read whole.
            "struct_item" | "union_item" => {
                if let Some(n) = named_node(&child, bytes, prefix, SymbolKind::Struct) {
                    let type_prefix = format!("{}.", n.id);
                    out.push(n);
                    let parent_idx = out.len() - 1;
                    if let Some(body) = child.child_by_field_name("body") {
                        if body.kind() == "field_declaration_list" {
                            let body_id = format!("{}:body", out[parent_idx].id);
                            out[parent_idx].children.push(syntax_node(&body_id, None, "body", &body));
                            let mut c2 = body.walk();
                            for f in body.named_children(&mut c2) {
                                if f.kind() == "field_declaration" {
                                    push_member(&f, bytes, &type_prefix, out);
                                }
                            }
                        }
                    }
                }
            }
            "enum_item" => {
                if let Some(n) = named_node(&child, bytes, prefix, SymbolKind::Enum) {
                    let type_prefix = format!("{}.", n.id);
                    out.push(n);
                    let parent_idx = out.len() - 1;
                    if let Some(body) = child.child_by_field_name("body") {
                        let body_id = format!("{}:body", out[parent_idx].id);
                        out[parent_idx].children.push(syntax_node(&body_id, None, "body", &body));
                        let mut c2 = body.walk();
                        for v in body.named_children(&mut c2) {
                            if v.kind() == "enum_variant" {
                                push_member(&v, bytes, &type_prefix, out);
                            }
                        }
                    }
                }
            }
            "trait_item" => push(&child, bytes, prefix, SymbolKind::Interface, out),
            "type_item" => push(&child, bytes, prefix, SymbolKind::TypeAlias, out),
            "const_item" | "static_item" => push(&child, bytes, prefix, SymbolKind::Variable, out),
            "macro_definition" => push(&child, bytes, prefix, SymbolKind::Function, out),
            "impl_item" => {
                let ty = child
                    .child_by_field_name("type")
                    .and_then(|t| type_text(&t, bytes))
                    .unwrap_or_else(|| "impl".to_string());
                if let Some(body) = child.child_by_field_name("body") {
                    let inner = format!("{prefix}{ty}.");
                    collect_items(body, bytes, &inner, SymbolKind::Method, out);
                }
            }
            "mod_item" => {
                if let Some(body) = child.child_by_field_name("body") {
                    collect_items(body, bytes, prefix, SymbolKind::Function, out);
                }
            }
            _ => {}
        }
    }
}

fn push(item: &TsNode, bytes: &[u8], prefix: &str, kind: SymbolKind, out: &mut Vec<Node>) {
    if let Some(n) = named_node(item, bytes, prefix, kind) {
        out.push(n);
    }
}

/// One struct field / enum variant as a top-level dotted symbol (`Type.member`), mirroring
/// lang-ts's class-field shape (kind `var`, single line — which is what lets retrieval inline
/// its source). Fields with doc comments get a `:doc` sub-node like any other declaration.
fn push_member(item: &TsNode, bytes: &[u8], type_prefix: &str, out: &mut Vec<Node>) {
    // named_node's doc_range skips attributes, so #[serde(...)]-decorated fields keep docs.
    if let Some(n) = named_node(item, bytes, type_prefix, SymbolKind::Variable) {
        out.push(n);
    }
}

/// Build a declaration `Node` from an item with a `name` field. Attaches a `:doc` sub-node for
/// the item's leading doc comments (`///` / `//!` / `/** */`) so they're editable like any anchor.
fn named_node(item: &TsNode, bytes: &[u8], prefix: &str, kind: SymbolKind) -> Option<Node> {
    let name_node = item.child_by_field_name("name")?;
    let name = name_node.utf8_text(bytes).ok()?.to_string();
    let id = format!("{prefix}{name}");
    let mut children = Vec::new();
    if let Some(r) = doc_range(item) {
        children.push(Node {
            id: format!("{id}:doc"),
            name: None,
            kind: NodeKind::Syntax("doc".to_string()),
            range: r,
            name_range: None,
            children: vec![],
        });
    }
    Some(Node {
        id,
        name: Some(name),
        kind: NodeKind::Symbol(kind),
        range: ts_range(item),
        name_range: Some(ts_range(&name_node)),
        children,
    })
}

/// Range spanning the contiguous leading comment lines above `item` (Rust doc comments
/// `///` / `//!` are `line_comment`s; `/** */` is a `block_comment`). Attributes between the
/// docs and the item are SKIPPED, not counted: `/// docs` + `#[derive(Clone)]` + `struct X`
/// is the dominant real-world shape, and treating the attribute as a doc-breaker silently
/// dropped the `:doc` anchor from nearly every derive-decorated type.
fn doc_range(item: &TsNode) -> Option<Range> {
    let mut anchor = *item;
    while let Some(prev) = anchor.prev_sibling() {
        if prev.kind() == "attribute_item" {
            anchor = prev;
        } else {
            break;
        }
    }
    ci_treesitter::leading_comment_range(&anchor, |n| {
        matches!(n.kind(), "line_comment" | "block_comment")
    })
}

/// Attach params / return type / body as `Syntax` sub-nodes of a function/method.
fn add_fn_subnodes(n: &mut Node, item: &TsNode, bytes: &[u8]) {
    if let Some(params) = item.child_by_field_name("parameters") {
        // The whole `(...)` list — the insertion anchor for `add_parameter` / a missing return type.
        n.children.push(syntax_node(&format!("{}:params", n.id), None, "params", &params));
        let mut cursor = params.walk();
        for (i, p) in params.named_children(&mut cursor).enumerate() {
            let name = p.utf8_text(bytes).ok().map(str::to_string);
            n.children.push(syntax_node(&format!("{}:param.{i}", n.id), name, "parameter", &p));
        }
    }
    if let Some(rt) = item.child_by_field_name("return_type") {
        n.children.push(syntax_node(&format!("{}:return", n.id), None, "returnType", &rt));
    }
    if let Some(body) = item.child_by_field_name("body") {
        n.children.push(syntax_node(&format!("{}:body", n.id), None, "body", &body));
    }
}

/// First `type_identifier` inside an impl's `type` node (the base type being implemented).
fn type_text(t: &TsNode, bytes: &[u8]) -> Option<String> {
    if t.kind() == "type_identifier" {
        return t.utf8_text(bytes).ok().map(str::to_string);
    }
    let mut cursor = t.walk();
    for c in t.named_children(&mut cursor) {
        if c.kind() == "type_identifier" {
            return c.utf8_text(bytes).ok().map(str::to_string);
        }
    }
    t.utf8_text(bytes).ok().map(str::to_string)
}
