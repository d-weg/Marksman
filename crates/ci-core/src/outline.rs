//! Skeletal-context helper: replace byte ranges in source with a `{ /* … */ }` placeholder.
//! Language-agnostic — each provider finds the function/method body ranges with its own
//! tree-sitter grammar and calls this to fold them.

/// Replace the given byte `ranges` in `content` with a placeholder, keeping only the
/// OUTERMOST of any nested/overlapping ranges (so an outer body subsumes inner closures).
/// Ranges that aren't on char boundaries or overlap an already-emitted one are skipped.
pub fn elide_bodies(content: &str, mut ranges: Vec<(usize, usize)>) -> String {
    ranges.sort_by_key(|&(s, e)| (s, std::cmp::Reverse(e)));
    let mut pos = 0usize;
    let mut out = String::with_capacity(content.len());
    for (s, e) in ranges {
        if s < pos || e > content.len() || s >= e {
            continue; // contained in a prior range, or invalid
        }
        if !content.is_char_boundary(s) || !content.is_char_boundary(e) {
            continue;
        }
        out.push_str(&content[pos..s]);
        out.push_str("{ /* … */ }");
        pos = e;
    }
    out.push_str(&content[pos..]);
    out
}

#[cfg(test)]
mod tests {
    use super::elide_bodies;

    #[test]
    fn elides_outermost_only() {
        let s = "fn a() { let x = || { 1 }; x() } fn b() { 2 }";
        // elide both top-level bodies + the inner closure; only outermost kept.
        let got = elide_bodies(s, vec![(7, 32), (20, 25), (40, 45)]);
        assert_eq!(got, "fn a() { /* … */ } fn b() { /* … */ }");
    }
}
