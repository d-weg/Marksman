/// Reciprocal-rank fusion constant: rank dampening for the tail.
pub const RRF_K: f32 = 60.0;

/// Blend two ranked lists (id, score) into one, reciprocal-rank style: each list
/// contributes 1/(RRF_K + rank), summed per id, highest first.
pub fn blend_scores(lexical: &[(String, f32)], semantic: &[(String, f32)]) -> Vec<(String, f32)> {
    let mut fused: std::collections::HashMap<String, f32> = std::collections::HashMap::new();
    for (rank, (id, _)) in lexical.iter().enumerate() {
        *fused.entry(id.clone()).or_insert(0.0) += 1.0 / (RRF_K + rank as f32 + 1.0);
    }
    for (rank, (id, _)) in semantic.iter().enumerate() {
        *fused.entry(id.clone()).or_insert(0.0) += 1.0 / (RRF_K + rank as f32 + 1.0);
    }
    let mut out: Vec<(String, f32)> = fused.into_iter().collect();
    out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    out
}
