//! Flat dense vector store: a row-major `[n * dim]` f32 matrix of normalized
//! embeddings, ranked by dot product (== cosine, since rows are normalized).
//! Port of rankMatrix/cosineNormalized in src/embeddings.ts. Dot accumulates in
//! f64 to track the JS (f64) arithmetic for parity.

pub fn cosine_normalized(query: &[f32], matrix: &[f32], offset: usize) -> f64 {
    let mut dot = 0f64;
    for i in 0..query.len() {
        dot += query[i] as f64 * matrix[offset + i] as f64;
    }
    dot
}

/// Rank rows of `matrix` (length n*dim) against `query`; descending (row, score).
pub fn rank_matrix(matrix: &[f32], dim: usize, query: &[f32], top_k: usize) -> Vec<(usize, f64)> {
    let n = if dim > 0 { matrix.len() / dim } else { 0 };
    let mut scored: Vec<(usize, f64)> =
        (0..n).map(|r| (r, cosine_normalized(query, matrix, r * dim))).collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(top_k);
    scored
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranks_nearest_row_first() {
        // 3 rows, dim 2. Query == row 1.
        let matrix = vec![1.0, 0.0, 0.0, 1.0, 0.7, 0.7];
        let q = vec![0.0, 1.0];
        let r = rank_matrix(&matrix, 2, &q, 3);
        assert_eq!(r[0].0, 1);
        assert!(r[0].1 > r[1].1);
    }
}
