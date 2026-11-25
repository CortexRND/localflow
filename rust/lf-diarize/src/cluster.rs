//! Deterministic agglomerative clustering (average linkage, cosine distance);
//! port of `cluster.py`, in f64 throughout.

use anyhow::{bail, Result};

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
    // Upper triangle only (i < j); everything else is +inf, as in the Python matrix.
    let mut dist = vec![f64::INFINITY; n * n];
    for i in 0..n {
        for j in i + 1..n {
            let dot: f64 = x[i].iter().zip(&x[j]).map(|(a, b)| a * b).sum();
            dist[i * n + j] = (1.0 - dot).max(0.0);
        }
    }
    let mut sizes = vec![1.0f64; n];
    let mut parent: Vec<usize> = (0..n).collect();
    let mut alive = n;
    let limit = max_speakers.unwrap_or(n);
    while alive > 1 {
        // np.argmin: the first minimum in row-major order.
        let mut flat = 0;
        for k in 1..n * n {
            if dist[k] < dist[flat] {
                flat = k;
            }
        }
        let (i, j) = (flat / n, flat % n);
        let d = dist[flat];
        if d > threshold + DIST_SLACK && alive <= limit {
            break;
        }
        let at = |a: usize, b: usize| dist[a * n + b];
        let merged: Vec<f64> = (0..n)
            .map(|k| {
                let col_i = at(k, i).min(at(i, k));
                let col_j = at(k, j).min(at(j, k));
                (sizes[i] * col_i + sizes[j] * col_j) / (sizes[i] + sizes[j])
            })
            .collect();
        for (k, value) in merged.iter().enumerate() {
            if k < i {
                dist[k * n + i] = *value;
            } else if k > i {
                dist[i * n + k] = *value;
            }
        }
        for k in 0..n {
            dist[j * n + k] = f64::INFINITY;
            dist[k * n + j] = f64::INFINITY;
        }
        sizes[i] += sizes[j];
        for p in parent.iter_mut() {
            if *p == j {
                *p = i;
            }
        }
        alive -= 1;
    }
    let mut labels: Vec<usize> = Vec::new();
    Ok(parent
        .iter()
        .map(|p| match labels.iter().position(|l| l == p) {
            Some(label) => label,
            None => {
                labels.push(*p);
                labels.len() - 1
            }
        })
        .collect())
}
