//! Skeletal context: in-process tree-sitter folding that keeps every signature/type but
//! replaces function & method BODIES with `{ /* … */ }`. A 200-line file collapses to ~15
//! lines of pure signal — the agent gets exact arguments/return types without the bodies.
use tree_sitter::{Node as TsNode, Parser};

/// Return `content` with function/method bodies elided. On any parse failure, returns the
/// original text unchanged (best-effort).
pub fn outline(content: &str) -> String {
    let mut parser = Parser::new();
    let lang: tree_sitter::Language = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
    if parser.set_language(&lang).is_err() {
        return content.to_string();
    }
    let Some(tree) = parser.parse(content, None) else { return content.to_string() };
    let mut bodies = Vec::new();
    collect_bodies(tree.root_node(), &mut bodies);
    ci_core::elide_bodies(content, bodies)
}

/// Every `statement_block` that is a function/method `body` (recurses through the tree; the
/// elide step keeps only the outermost, so nested closures are subsumed).
fn collect_bodies(node: TsNode, out: &mut Vec<(usize, usize)>) {
    if let Some(body) = node.child_by_field_name("body") {
        if body.kind() == "statement_block" {
            out.push((body.start_byte(), body.end_byte()));
        }
    }
    let mut c = node.walk();
    for ch in node.named_children(&mut c) {
        collect_bodies(ch, out);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn elides_bodies_keeps_signatures() {
        let src = "export function add(a: number, b: number): number {\n  const x = a + b;\n  return x;\n}\nexport const C = 1;\n";
        let o = super::outline(src);
        assert!(o.contains("export function add(a: number, b: number): number"), "signature kept: {o}");
        assert!(!o.contains("const x = a + b"), "body elided: {o}");
        assert!(o.contains("export const C = 1"), "non-fn kept: {o}");
        assert!(o.contains("/*"), "placeholder present: {o}");
    }
}
