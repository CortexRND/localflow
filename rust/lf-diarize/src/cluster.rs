//! Deterministic agglomerative clustering (average linkage, cosine distance);
//! port of `cluster.py`, in f64 throughout.

use anyhow::{bail, Result};
use std::collections::BTreeSet;

pub const DIST_SLACK: f64 = 1e-12;

/// Clusters `vectors` and returns labels `0..k` numbered by first appearance.
///
/// Merges the closest pair (ties: lowest row-major index pair) while its distance
/// is `<= threshold` (plus [`DIST_SLACK`]), then keeps merging until at most
/// `max_speakers` clusters remain. Distances are clamped at 0; zero vectors have
/// distance 1 to everything else. Non-finite input, mixed dimensions and
/// `max_speakers == Some(0)` are errors.
pub fn cluster<V: AsRef<[f64]>>(
    vectors: &[V],
    threshold: f64,
    max_speakers: Option<usize>,
) -> Result<Vec<usize>> {
    if max_speakers == Some(0) {
        bail!("max_speakers must be >= 1");
    }
    if vectors.is_empty() {
        return Ok(Vec::new());
    }
    let dim = vectors[0].as_ref().len();
    let mut x: Vec<Vec<f64>> = Vec::with_capacity(vectors.len());
    for v in vectors {
        let v = v.as_ref();
        if v.len() != dim {
            bail!("vectors must share one dimension");
        }
        if !v.iter().all(|c| c.is_finite()) {
            bail!("vectors must be finite (no NaN or inf)");
        }
        let norm = v.iter().map(|c| c * c).sum::<f64>().sqrt();
        x.push(if norm > 0.0 {
            v.iter().map(|c| c / norm).collect()
        } else {
            vec![0.0; dim]
        });
    }
    let n = x.len();
    // The Python matrix's upper triangle (i < j). Dead clusters' entries are
    // never read again, so they are left as they are rather than set to +inf.
    let mut dist = vec![0.0f64; n * n];
    // Every live pair keyed by (distance, row, column): the first key is
    // np.argmin's first row-major minimum. Keys are removed exactly when their
    // pair changes, so the index holds at most n(n-1)/2 entries, never stale ones.
    let mut pairs = BTreeSet::new();
    for i in 0..n {
        for j in i + 1..n {
            let dot: f64 = x[i].iter().zip(&x[j]).map(|(a, b)| a * b).sum();
            let d = (1.0 - dot).max(0.0);
            dist[i * n + j] = d;
            pairs.insert(Pair::new(d, i, j));
        }
    }
    let mut sizes = vec![1.0f64; n];
    let mut alive_mask = vec![true; n];
    let mut parent: Vec<usize> = (0..n).collect();
    let mut alive = n;
    let limit = max_speakers.unwrap_or(n);
    while alive > 1 {
        let Some(&Pair(d, i, j)) = pairs.first() else {
            break;
        };
        if d > threshold + DIST_SLACK && alive <= limit {
            break;
        }
        let at = |dist: &[f64], a: usize, b: usize| dist[a.min(b) * n + a.max(b)];
        alive_mask[j] = false;
        for k in (0..n).filter(|&k| alive_mask[k]) {
            if k == i {
                continue;
            }
            let (d_ki, d_kj) = (at(&dist, k, i), at(&dist, k, j));
            pairs.remove(&Pair::new(d_ki, k.min(i), k.max(i)));
            pairs.remove(&Pair::new(d_kj, k.min(j), k.max(j)));
            // Same arithmetic as the Python update, so ties break identically.
            let merged = (sizes[i] * d_ki + sizes[j] * d_kj) / (sizes[i] + sizes[j]);
            dist[k.min(i) * n + k.max(i)] = merged;
            pairs.insert(Pair::new(merged, k.min(i), k.max(i)));
        }
        pairs.remove(&Pair::new(at(&dist, i, j), i, j));
        sizes[i] += sizes[j];
        for p in parent.iter_mut() {
            if *p == j {
                *p = i;
            }
        }
        alive -= 1;
    }
    let mut labels = vec![usize::MAX; n];
    let mut next = 0;
    Ok(parent
        .iter()
        .map(|&p| {
            if labels[p] == usize::MAX {
                labels[p] = next;
                next += 1;
            }
            labels[p]
        })
        .collect())
}

/// `(distance, row, column)`, ordered by distance, then row, then column.
#[derive(Clone, Copy, Debug)]
struct Pair(f64, usize, usize);
impl Pair {
    /// `-0.0` becomes `0.0`, so the order matches `<` on the Python matrix.
    fn new(d: f64, i: usize, j: usize) -> Self {
        Self(if d == 0.0 { 0.0 } else { d }, i, j)
    }
}
impl PartialEq for Pair {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}
impl Eq for Pair {}
impl PartialOrd for Pair {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Pair {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0
            .total_cmp(&other.0)
            .then(self.1.cmp(&other.1))
            .then(self.2.cmp(&other.2))
    }
}
