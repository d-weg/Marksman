//! Reciprocal Rank Fusion — port of src/rrf.ts. score = Σ 1/(k + rank), rank ≥ 1.
//! sorted_by_score breaks ties by id (ascending) for deterministic output.
use std::collections::HashMap;

pub fn reciprocal_rank_fusion(lists: &[Vec<String>], k: f64) -> HashMap<String, f64> {
    let mut fused: HashMap<String, f64> = HashMap::new();
    for list in lists {
        for (i, id) in list.iter().enumerate() {
            let contribution = 1.0 / (k + (i as f64 + 1.0));
            *fused.entry(id.clone()).or_insert(0.0) += contribution;
        }
    }
    fused
}

pub fn sorted_by_score(fused: &HashMap<String, f64>) -> Vec<(String, f64)> {
    let mut v: Vec<(String, f64)> = fused.iter().map(|(k, s)| (k.clone(), *s)).collect();
    v.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuses_and_ranks() {
        let a = vec!["x".to_string(), "y".to_string()];
        let b = vec!["y".to_string(), "z".to_string()];
        let fused = reciprocal_rank_fusion(&[a, b], 60.0);
        let sorted = sorted_by_score(&fused);
        // y appears in both lists -> highest fused score
        assert_eq!(sorted[0].0, "y");
    }
}
