//! Skeletal context: in-process tree-sitter folding that keeps every signature/type but
//! replaces function & method BODIES with `{ /* … */ }`. A 200-line file collapses to ~15
//! lines of pure signal — the agent gets exact arguments/return types without the bodies.
/// Return `content` with function/method bodies elided. On any parse failure, returns the
/// original text unchanged (best-effort).
pub fn outline(content: &str) -> String {
    // Fold every `statement_block` body (any enclosing node — functions, methods, arrows);
    // the elide step keeps only the outermost, so nested closures are subsumed.
    ci_treesitter::outline(
        &tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        content,
        &[],
        &["statement_block"],
    )
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
