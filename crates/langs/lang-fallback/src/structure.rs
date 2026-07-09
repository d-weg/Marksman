//! Structure extraction — the tree-sitter walks that turn a parsed file into the `Node`
//! contract (`structure()`). Two collectors: Python keeps its specialized walk (docstrings,
//! decorated definitions), every other language shares ONE generic collector driven by the
//! tree-sitter field convention (`name` / `parameters` / `body`) plus [`classify`]'s
//! per-language kind table — adding a language is table rows, no new walker.
use ci_core::{Node, NodeKind, Range, SymbolKind};
use ci_treesitter::{syntax_node, ts_range};
use tree_sitter::Node as TsNode;

use crate::FbLang;

// ── the generic collector (every fallback language except Python) ───────────

/// What a matched node kind IS, per language: a function/method (emits with sub-nodes, no
/// recursion into its body), a named type (leaf — struct/enum/type alias), or a container
/// (class/module/namespace — emits, then recurses into its body with a qualified prefix).
enum Shape {
    Fn,
    Type,
    Container,
    /// A named value: class field, top-level const, interface property, enum member. Emitted
    /// as `SymbolKind::Variable`; the range widens to the enclosing STATEMENT so editing the
    /// field by name can see its initializer (`k1 = 1.5;`, not just `k1`).
    Field,
}

/// The per-language kind table — the ONLY language-specific part of the generic collector.
fn classify(lang: FbLang, kind: &str) -> Option<Shape> {
    use Shape::*;
    Some(match (lang, kind) {
        // JS (grammar includes JSX): classes qualify their methods; arrow-function consts are
        // variable declarators (no name field on the function) and stay out — same tradeoff as
        // scip's Term handling, revisit if it bites.
        (FbLang::Js | FbLang::Ts, "function_declaration" | "generator_function_declaration" | "method_definition") => Fn,
        (FbLang::Js | FbLang::Ts, "class_declaration") => Container,
        (FbLang::Ts, "interface_declaration" | "enum_declaration" | "abstract_class_declaration") => Container,
        (FbLang::Ts, "type_alias_declaration") => Type,
        // Named values: SCIP collects these as Term symbols; edit targets in every language.
        (FbLang::Ts, "public_field_definition" | "property_signature" | "enum_assignment") => Field,
        (FbLang::Js, "field_definition") => Field,
        (FbLang::Js | FbLang::Ts, "variable_declarator") => Field,
        (FbLang::Go, "const_spec" | "var_spec") => Field,
        (FbLang::Java, "field_declaration") => Field,
        (FbLang::Go, "function_declaration" | "method_declaration") => Fn,
        (FbLang::Go, "type_spec") => Type,
        (FbLang::Java, "method_declaration" | "constructor_declaration") => Fn,
        (FbLang::Java, "class_declaration" | "interface_declaration" | "enum_declaration" | "record_declaration") => Container,
        // PHP: `function` at any scope is `function_definition`; a member is `method_declaration`
        // (the `Container.` prefix promotes it to Method). Properties/consts nest a
        // `property_element`/`const_element` inside their declaration statement — classify the
        // ELEMENT (the declaration is a transparent wrapper the collector recurses through), and
        // widen its span to the enclosing statement so the initializer edits as a unit.
        (FbLang::Php, "method_declaration" | "function_definition") => Fn,
        (FbLang::Php, "class_declaration" | "interface_declaration" | "trait_declaration" | "enum_declaration") => Container,
        (FbLang::Php, "property_element" | "const_element") => Field,
        (FbLang::Ruby, "method" | "singleton_method") => Fn,
        (FbLang::Ruby, "class" | "module") => Container,
        (FbLang::C | FbLang::Cpp, "function_definition") => Fn,
        (FbLang::C | FbLang::Cpp, "struct_specifier" | "enum_specifier" | "union_specifier") => Type,
        (FbLang::Cpp, "class_specifier") => Container,
        (FbLang::Cpp, "namespace_definition") => Container,
        // Swift: the grammar collapses struct/class/enum/extension all onto `class_declaration`
        // (protocol has its own node); each has a `body` and qualifies its members, so all are
        // Containers. `init_declaration`/`protocol_function_declaration` join the free/member
        // `function_declaration` as Fn. Named values: `property_declaration` (var/let) and enum
        // `enum_entry` (which may list several cases — the collector emits the first name).
        (FbLang::Swift, "function_declaration" | "init_declaration" | "protocol_function_declaration") => Fn,
        (FbLang::Swift, "class_declaration" | "protocol_declaration") => Container,
        (FbLang::Swift, "property_declaration" | "enum_entry") => Field,
        _ => return None,
    })
}

/// The definition's name node: the `name` field, else (C/C++) the first identifier-ish node
/// down the `declarator` chain (`function_definition → function_declarator → identifier`).
fn def_name<'a>(node: &TsNode<'a>) -> Option<TsNode<'a>> {
    // Swift `enum_entry` lists its cases as bare `simple_identifier` children (no `name` field);
    // name it by the first case. Every other Swift decl DOES carry a `name` field (`pattern` for
    // a property, `type_identifier`/`user_type` for a type, `simple_identifier` for a function),
    // so the generic `name`-field path below handles them.
    if node.kind() == "enum_entry" {
        let mut c = node.walk();
        return node.named_children(&mut c).find(|ch| ch.kind() == "simple_identifier");
    }
    if let Some(n) = node.child_by_field_name("name") {
        return Some(n);
    }
    // PHP's `const_element` names its constant with a bare `name` child (no `name` field and no
    // declarator chain) — take the first `name`/`variable_name` child before descending.
    let mut c = node.walk();
    if let Some(n) = node.named_children(&mut c).find(|ch| matches!(ch.kind(), "name" | "variable_name")) {
        return Some(n);
    }
    let mut d = node.child_by_field_name("declarator")?;
    for _ in 0..6 {
        if d.kind().ends_with("identifier") {
            return Some(d);
        }
        if let Some(n) = d.child_by_field_name("name") {
            return Some(n); // java: field_declaration -> variable_declarator(name)
        }
        d = d.child_by_field_name("declarator")?;
    }
    None
}

/// Walk the tree emitting definitions per [`classify`]. Function bodies are NOT descended into
/// (locals are not symbols); container bodies are, with a `Container.` qualified prefix.
/// Unmatched nodes are transparent wrappers (declaration lists, export statements, preproc…).
pub(crate) fn collect_generic(lang: FbLang, node: TsNode, bytes: &[u8], prefix: &str, out: &mut Vec<Node>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let Some(shape) = classify(lang, child.kind()) else {
            collect_generic(lang, child, bytes, prefix, out);
            continue;
        };
        let Some(name_node) = def_name(&child) else { continue };
        let Ok(name) = name_node.utf8_text(bytes) else { continue };
        // A C `struct Foo x;` mentions the kind without a body — only DEFINITIONS count. Two
        // bodyless exceptions ARE definitions: go's `type_spec` (payload in its `type` field,
        // only appears inside `type_declaration`) and TS's `type_alias_declaration` (payload
        // in its `value` field).
        let is_definition = child.child_by_field_name("body").is_some()
            || matches!(shape, Shape::Fn | Shape::Field)
            || (lang == FbLang::Go && child.kind() == "type_spec")
            || (lang == FbLang::Ts && child.kind() == "type_alias_declaration");
        if !is_definition {
            continue;
        }
        // Inside a container the prefix is `file#Scope.` — a trailing `.` marks a member.
        let kind = match shape {
            Shape::Fn if prefix.ends_with('.') => SymbolKind::Method,
            Shape::Fn => SymbolKind::Function,
            Shape::Type | Shape::Container => SymbolKind::Class,
            Shape::Field => SymbolKind::Variable,
        };
        // A declarator's own range stops at the name — climb to the declaration statement so
        // `replace_text`/`replace_node` on the field can see `const k1 = 1.5;` whole.
        let span_node = if matches!(shape, Shape::Field)
            && matches!(child.kind(), "variable_declarator" | "property_element" | "const_element")
        {
            child.parent().unwrap_or(child)
        } else {
            child
        };
        let mut n = Node {
            id: format!("{prefix}{name}"),
            name: Some(name.to_string()),
            kind: NodeKind::Symbol(kind),
            range: ts_range(&span_node),
            name_range: Some(ts_range(&name_node)),
            children: vec![],
        };
        // Leading comment → the `:doc` anchor (parity with the gated providers). The comment
        // may sit above a single-child WRAPPER instead (`// doc` above go's `type Bucket …`
        // annotates the `type_declaration`, we emit its inner `type_spec`) — climb one level
        // when the parent wraps exactly this definition.
        let is_comment = |c: &TsNode| matches!(c.kind(), "comment" | "line_comment" | "block_comment");
        let doc = ci_treesitter::leading_comment_range(&child, is_comment).or_else(|| {
            child
                .parent()
                .filter(|p| p.named_child_count() == 1)
                .and_then(|p| ci_treesitter::leading_comment_range(&p, is_comment))
        });
        if let Some(r) = doc {
            n.children.push(Node {
                id: format!("{}:doc", n.id),
                name: None,
                kind: NodeKind::Syntax("doc".into()),
                range: r,
                name_range: None,
                children: vec![],
            });
        }
        match shape {
            Shape::Fn => {
                add_fn_subnodes(&mut n, &child, bytes, lang == FbLang::Swift);
                out.push(n);
            }
            Shape::Type | Shape::Field => out.push(n),
            Shape::Container => {
                let inner = format!("{prefix}{name}.");
                out.push(n);
                if let Some(body) = child.child_by_field_name("body") {
                    collect_generic(lang, body, bytes, &inner, out);
                }
            }
        }
    }
}

// ── the Python collector ─────────────────────────────────────────────────────

/// Walk a statement list, emitting a `Node` per function / class. `fn_kind` is the kind for
/// functions found here (Function at module level, Method inside a class body). `prefix` is the
/// id stem (`"file.py#"`, or `"file.py#Class."` inside a class).
pub(crate) fn collect_items(node: TsNode, bytes: &[u8], prefix: &str, fn_kind: SymbolKind, out: &mut Vec<Node>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "function_definition" => emit_fn(&child, bytes, prefix, fn_kind, out),
            "class_definition" => emit_class(&child, bytes, prefix, out),
            "decorated_definition" => {
                if let Some(def) = child.child_by_field_name("definition") {
                    match def.kind() {
                        "function_definition" => emit_fn(&def, bytes, prefix, fn_kind, out),
                        "class_definition" => emit_class(&def, bytes, prefix, out),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
}

fn emit_fn(def: &TsNode, bytes: &[u8], prefix: &str, fn_kind: SymbolKind, out: &mut Vec<Node>) {
    if let Some(mut n) = named_node(def, bytes, prefix, fn_kind) {
        add_fn_subnodes(&mut n, def, bytes, false); // Python collector — never Swift
        out.push(n);
    }
}

fn emit_class(def: &TsNode, bytes: &[u8], prefix: &str, out: &mut Vec<Node>) {
    if let Some(mut n) = named_node(def, bytes, prefix, SymbolKind::Class) {
        let inner = format!("{prefix}{}.", n.name.as_deref().unwrap_or_default());
        if let Some(body) = def.child_by_field_name("body") {
            // class docstring → `:doc` anchor (parity with functions/methods).
            if let Some(ds) = python_docstring(&body) {
                n.children.push(syntax_node(&format!("{}:doc", n.id), None, "doc", &ds));
            }
        }
        out.push(n);
        if let Some(body) = def.child_by_field_name("body") {
            collect_items(body, bytes, &inner, SymbolKind::Method, out);
        }
    }
}

fn named_node(item: &TsNode, bytes: &[u8], prefix: &str, kind: SymbolKind) -> Option<Node> {
    let name_node = item.child_by_field_name("name")?;
    let name = name_node.utf8_text(bytes).ok()?.to_string();
    Some(Node {
        id: format!("{prefix}{name}"),
        name: Some(name),
        kind: NodeKind::Symbol(kind),
        range: ts_range(item),
        name_range: Some(ts_range(&name_node)),
        children: vec![],
    })
}

fn add_fn_subnodes(n: &mut Node, item: &TsNode, bytes: &[u8], swift: bool) {
    // Swift's `function_declaration`/`init_declaration` break the field convention: the
    // parameters are unfielded `parameter` children between anonymous `(`/`)` tokens, and the
    // return type sits under the SAME `name` field as the function's own name (the second such
    // child, after the `->`). Handle it explicitly rather than bend the generic path. Gated on
    // the Swift LANGUAGE, not the node kind — `function_declaration` is shared with Go/JS/TS.
    if swift {
        swift_fn_subnodes(n, item, bytes);
        return;
    }
    // The parameter list: a `parameters` field, else (C/C++) the one inside the declarator
    // chain (`function_definition → function_declarator(parameters: …)`).
    let params_node = item.child_by_field_name("parameters").or_else(|| {
        let mut d = item.child_by_field_name("declarator")?;
        for _ in 0..6 {
            if let Some(p) = d.child_by_field_name("parameters") {
                return Some(p);
            }
            d = d.child_by_field_name("declarator")?;
        }
        None
    });
    if let Some(params) = params_node {
        // The whole `(...)` list — the insertion anchor for `add_parameter` / a missing return type.
        n.children.push(syntax_node(&format!("{}:params", n.id), None, "params", &params));
        let mut cursor = params.walk();
        for (i, p) in params.named_children(&mut cursor).enumerate() {
            // skip `self`/`cls` — they aren't meaningful edit targets
            let name = p.utf8_text(bytes).ok().map(str::to_string);
            if matches!(name.as_deref(), Some("self") | Some("cls")) {
                continue;
            }
            n.children.push(syntax_node(&format!("{}:param.{i}", n.id), name, "parameter", &p));
        }
    }
    // Return type field name varies by grammar: `return_type` (python/ruby), `result` (go),
    // `type` (java's method_declaration return).
    if let Some(rt) = item
        .child_by_field_name("return_type")
        .or_else(|| item.child_by_field_name("result"))
        .or_else(|| item.child_by_field_name("type"))
    {
        n.children.push(syntax_node(&format!("{}:return", n.id), None, "returnType", &rt));
    }
    if let Some(body) = item.child_by_field_name("body") {
        // Docstring = the first statement when it's a bare string literal — the `:doc` anchor.
        if let Some(ds) = python_docstring(&body) {
            n.children.push(syntax_node(&format!("{}:doc", n.id), None, "doc", &ds));
        }
        n.children.push(syntax_node(&format!("{}:body", n.id), None, "body", &body));
    }
}

/// Swift function/init sub-nodes. The `:params` anchor spans the `(...)` list (from the `(`
/// token to the `)` token, so `set_return_type` inserts `-> T` immediately after `)` — Swift is
/// SUFFIX-typed); each `parameter` child is a `:param.N`. The return type is the `name`-field
/// child following the `->` token — the SAME field the function's own name uses, so it is
/// identified positionally (after `->`) not by field, and surfaces as `:return`.
fn swift_fn_subnodes(n: &mut Node, item: &TsNode, bytes: &[u8]) {
    // The parenthesized parameter list: bracket its span with the `(`/`)` tokens.
    let mut open: Option<TsNode> = None;
    let mut close: Option<TsNode> = None;
    let mut arrow_seen = false;
    let mut return_ty: Option<TsNode> = None;
    let mut params: Vec<TsNode> = Vec::new();
    let mut c = item.walk();
    for ch in item.children(&mut c) {
        match ch.kind() {
            "(" => open = Some(ch),
            ")" => close = Some(ch),
            "->" => arrow_seen = true,
            "parameter" => params.push(ch),
            // After the `->`, the first `user_type`/type node is the return type. Before it, a
            // type node is the function's own name (`func f`) or lives inside a parameter.
            _ if arrow_seen && return_ty.is_none() && ch.is_named() && ch.kind() != "function_body" => {
                return_ty = Some(ch);
            }
            _ => {}
        }
    }
    if let (Some(o), Some(cl)) = (open, close) {
        let range = Range {
            start_line: o.start_position().row as u32 + 1,
            start_char: o.start_position().column as u32,
            end_line: cl.end_position().row as u32 + 1,
            end_char: cl.end_position().column as u32,
        };
        n.children.push(Node {
            id: format!("{}:params", n.id),
            name: None,
            kind: NodeKind::Syntax("params".into()),
            range,
            name_range: None,
            children: vec![],
        });
        for (i, p) in params.iter().enumerate() {
            let name = p.utf8_text(bytes).ok().map(str::to_string);
            n.children.push(syntax_node(&format!("{}:param.{i}", n.id), name, "parameter", p));
        }
    }
    if let Some(rt) = return_ty {
        n.children.push(syntax_node(&format!("{}:return", n.id), None, "returnType", &rt));
    }
    // `init_declaration` has no `name` field but does carry a `body`; the free/member
    // `function_declaration` bodies too. A protocol requirement is bodyless — no `:body`.
    if let Some(body) = item.child_by_field_name("body") {
        n.children.push(syntax_node(&format!("{}:body", n.id), None, "body", &body));
    }
}

/// The docstring node of a function/class body: its first statement, if that statement is a bare
/// string expression (`"""…"""` / `'…'`).
fn python_docstring<'a>(body: &TsNode<'a>) -> Option<TsNode<'a>> {
    let first = body.named_child(0)?;
    if first.kind() == "expression_statement" {
        let s = first.named_child(0)?;
        if s.kind() == "string" {
            return Some(s);
        }
    }
    None
}
