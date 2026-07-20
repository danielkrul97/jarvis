//! Vector math for hybrid retrieval (phase 3): cosine similarity, brute-force
//! KNN, and Reciprocal Rank Fusion. The corpus is small (hundreds-to-thousands
//! of short texts), so brute-force is plenty and no ANN index is needed.

use std::collections::HashMap;
use std::hash::Hash;

/// Cosine similarity. e5 embeddings are L2-normalized (a dot product would
/// suffice), but we compute the full version for robustness against non-normalized ones.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0f32, 0f32, 0f32);
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    let denom = (na.sqrt() * nb.sqrt()).max(1e-12);
    dot / denom
}

/// Brute-force KNN: `ref_id` sorted by cosine similarity (desc), top-k.
pub fn knn(query: &[f32], items: &[(i64, Vec<f32>)], k: usize) -> Vec<i64> {
    let mut scored: Vec<(i64, f32)> =
        items.iter().map(|(id, v)| (*id, cosine(query, v))).collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.into_iter().take(k).map(|(id, _)| id).collect()
}

/// Reciprocal Rank Fusion: merges multiple key rankings into one order. A
/// key's score = Σ 1/(k + rank) over the rankings it appears in (rank
/// 0-based). Higher = better. `k` dampens the effect of absolute position
/// (typically 60). A robust way to combine lexical (BM25) and vector ranking
/// without score scaling. Generic over the key, so namespaced sources
/// (conversations vs. utterances) can be fused too.
pub fn rrf<T: Eq + Hash + Ord + Clone>(rankings: &[Vec<T>], k: f64) -> Vec<T> {
    let mut score: HashMap<T, f64> = HashMap::new();
    for list in rankings {
        for (rank, id) in list.iter().enumerate() {
            *score.entry(id.clone()).or_insert(0.0) += 1.0 / (k + rank as f64 + 1.0);
        }
    }
    let mut ids: Vec<(T, f64)> = score.into_iter().collect();
    // stable tie-break by key, so ordering is deterministic
    ids.sort_by(|a, b| {
        b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0))
    });
    ids.into_iter().map(|(id, _)| id).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_orders_by_similarity() {
        let q = [1.0, 0.0, 0.0];
        let near = [0.9, 0.1, 0.0];
        let far = [0.0, 1.0, 0.0];
        assert!(cosine(&q, &near) > cosine(&q, &far));
        assert!((cosine(&q, &q) - 1.0).abs() < 1e-6);
        // different length / empty = 0 (safe)
        assert_eq!(cosine(&q, &[1.0, 0.0]), 0.0);
        assert_eq!(cosine(&[], &[]), 0.0);
    }

    #[test]
    fn knn_ranks_and_caps() {
        let q = vec![1.0, 0.0];
        let items = vec![
            (10, vec![0.0, 1.0]),  // orthogonal (worst)
            (20, vec![1.0, 0.0]),  // identical (best)
            (30, vec![0.8, 0.2]),  // close
        ];
        assert_eq!(knn(&q, &items, 2), vec![20, 30]);
        assert_eq!(knn(&q, &items, 10).len(), 3);
        assert!(knn(&q, &[], 3).is_empty());
    }

    #[test]
    fn rrf_fuses_rankings() {
        // id 2 ranks high in both rankings → wins the fusion
        let fts = vec![1, 2, 3];
        let vec_ = vec![2, 4, 1];
        let fused = rrf(&[fts, vec_], 60.0);
        assert_eq!(fused[0], 2, "shodně vysoké v obou → první");
        // every id shows up, no duplicates
        let mut all = fused.clone();
        all.sort();
        assert_eq!(all, vec![1, 2, 3, 4]);
    }

    #[test]
    fn rrf_single_ranking_preserves_order() {
        assert_eq!(rrf(&[vec![5i64, 6, 7]], 60.0), vec![5, 6, 7]);
        assert!(rrf::<i64>(&[], 60.0).is_empty());
        // also works with a namespaced key (source, id)
        let fused = rrf(&[vec![(0u8, 1i64), (0, 2)], vec![(1u8, 9i64)]], 60.0);
        assert_eq!(fused.len(), 3);
    }
}
