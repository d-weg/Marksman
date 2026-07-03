/// Lowercase a raw token and strip surrounding punctuation. The index and the
/// query side must agree on this, or nothing matches.
pub fn normalize(token: &str) -> String {
    token.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase()
}

/// Split source text into normalized tokens (empty tokens dropped).
pub fn tokenize(text: &str) -> Vec<String> {
    text.split_whitespace().map(normalize).filter(|t| !t.is_empty()).collect()
}
