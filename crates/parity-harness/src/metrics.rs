//! Parity metrics for fixture comparisons (design D6).
//!
//! [`recall_at_k`] scores a system's top-k results against *provided* ground
//! truth (exact top-k chunk-id lists, e.g. exported from the Go oracle
//! alongside the SYNX fixture). The harness deliberately does not recompute
//! brute-force ground truth: at N = 1M the exact reference is the exporter's
//! job, and the harness only needs the comparison arithmetic.

/// Mean recall@k of `candidates` against `ground_truth`.
///
/// One entry per query: `candidates[i]` is the chunk-id list the system
/// returned for query `i`, `ground_truth[i]` the exact top-k list for the same
/// query. The per-query score is `|intersection| / ground_truth[i].len()`, and
/// the result is the mean over all queries. Lists are expected to hold unique
/// ids (as an ANN engine does).
///
/// Returns `None` for a caller bug (different query counts or no queries at
/// all) instead of panicking; a candidate list shorter than the ground truth
/// (e.g. an empty index) simply scores 0.0 for its query.
pub fn recall_at_k(candidates: &[Vec<u32>], ground_truth: &[Vec<u32>]) -> Option<f64> {
    if candidates.is_empty() || candidates.len() != ground_truth.len() {
        return None;
    }
    let sum: f64 = candidates
        .iter()
        .zip(ground_truth)
        .map(|(cand, gt)| {
            if gt.is_empty() {
                0.0
            } else {
                cand.iter().filter(|id| gt.contains(id)).count() as f64 / gt.len() as f64
            }
        })
        .sum();
    Some(sum / candidates.len() as f64)
}

#[cfg(test)]
mod tests {
    // Test code: unwrap is intentional (the fixtures under test are ours).
    #![allow(clippy::unwrap_used)]

    use super::recall_at_k;

    #[test]
    fn perfect_candidates_score_one() {
        let gt = vec![vec![1, 2, 3], vec![4, 5, 6]];
        assert!((recall_at_k(&gt, &gt).unwrap() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn disjoint_candidates_score_zero() {
        let candidates = vec![vec![7, 8, 9], vec![10, 11, 12]];
        let gt = vec![vec![1, 2, 3], vec![4, 5, 6]];
        assert_eq!(recall_at_k(&candidates, &gt).unwrap(), 0.0);
    }

    #[test]
    fn partial_overlap_scores_intersection_over_k_per_query() {
        // Query 0: 2 of 4 hit -> 0.5. Query 1: 1 of 4 hit (shorter candidate
        // list) -> 0.25. Mean = 0.375.
        let candidates = vec![vec![1, 2, 9, 10], vec![4, 8]];
        let gt = vec![vec![1, 2, 3, 4], vec![4, 5, 6, 7]];
        assert!((recall_at_k(&candidates, &gt).unwrap() - 0.375).abs() < 1e-12);
    }

    #[test]
    fn empty_candidate_list_scores_zero_for_its_query() {
        // Query 0: empty result (e.g. empty index) -> 0.0. Query 1: 1 of 2 hit
        // -> 0.5. Mean = 0.25.
        let candidates = vec![Vec::<u32>::new(), vec![1]];
        let gt = vec![vec![1, 2], vec![1, 2]];
        assert!((recall_at_k(&candidates, &gt).unwrap() - 0.25).abs() < 1e-12);
    }

    #[test]
    fn mismatched_query_counts_are_none() {
        assert_eq!(recall_at_k(&[vec![1]], &[vec![1], vec![2]]), None);
        assert_eq!(recall_at_k(&[vec![1], vec![2]], &[vec![1]]), None);
        assert_eq!(recall_at_k(&[], &[]), None);
    }
}
