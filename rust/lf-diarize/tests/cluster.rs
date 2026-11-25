use lf_diarize::cluster;

/// The previous implementation, verbatim: a full row-major rescan per merge,
/// mirroring `cluster.py`. The ordered-index version must match it exactly.
fn oracle(vectors: &[Vec<f64>], threshold: f64, max_speakers: Option<usize>) -> Vec<usize> {
    let n = vectors.len();
    if n == 0 {
        return Vec::new();
    }
    let x: Vec<Vec<f64>> = vectors
        .iter()
        .map(|v| {
            let norm = v.iter().map(|c| c * c).sum::<f64>().sqrt();
            if norm > 0.0 {
                v.iter().map(|c| c / norm).collect()
            } else {
                vec![0.0; v.len()]
            }
        })
        .collect();
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
        let mut flat = 0;
        for k in 1..n * n {
            if dist[k] < dist[flat] {
                flat = k;
            }
        }
        let (i, j) = (flat / n, flat % n);
        if dist[flat] > threshold + 1e-12 && alive <= limit {
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
    parent
        .iter()
        .map(|p| match labels.iter().position(|l| l == p) {
            Some(label) => label,
            None => {
                labels.push(*p);
                labels.len() - 1
            }
        })
        .collect()
}

/// Deterministic xorshift, so the cases never change between runs.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
    }
}

/// Mixed workloads: continuous noise, a few tight speakers, and coarse
/// {-1, 0, 1} vectors with duplicates and zero vectors that force exact ties.
fn case(rng: &mut Rng, kind: u64, n: usize, dim: usize) -> Vec<Vec<f64>> {
    let centers: Vec<Vec<f64>> = (0..4)
        .map(|_| (0..dim).map(|_| rng.unit()).collect())
        .collect();
    (0..n)
        .map(|_| match kind {
            0 => (0..dim).map(|_| rng.unit()).collect(),
            1 => {
                let c = &centers[(rng.next() % 4) as usize];
                c.iter().map(|v| v + 0.15 * rng.unit()).collect()
            }
            _ => (0..dim).map(|_| (rng.next() % 3) as f64 - 1.0).collect(),
        })
        .collect()
}

#[test]
fn ordered_index_matches_the_rescan_oracle_on_diverse_and_tied_cases() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut checked = 0;
    for round in 0..90u64 {
        let kind = round % 3;
        let n = 1 + (rng.next() % 40) as usize;
        let dim = 1 + (rng.next() % 5) as usize;
        let vectors = case(&mut rng, kind, n, dim);
        for threshold in [-1.0, 0.0, 0.2, 0.5, 1.0, 1.0 - 1e-13, 2.5] {
            for max_speakers in [None, Some(1), Some(2), Some(3), Some(10)] {
                assert_eq!(
                    cluster(&vectors, threshold, max_speakers).unwrap(),
                    oracle(&vectors, threshold, max_speakers),
                    "round {round} kind {kind} n {n} threshold {threshold} max {max_speakers:?}"
                );
                checked += 1;
            }
        }
    }
    assert_eq!(checked, 90 * 35);
    // Forced merges among all-equal distances, at larger scale.
    let eye: Vec<Vec<f64>> = (0..25)
        .map(|i| (0..25).map(|j| f64::from(u8::from(i == j))).collect())
        .collect();
    for max_speakers in [Some(1), Some(4), Some(7), Some(24)] {
        assert_eq!(
            cluster(&eye, -1.0, max_speakers).unwrap(),
            oracle(&eye, -1.0, max_speakers)
        );
    }
}

#[test]
fn meeting_scale_workload_matches_the_oracle_and_runs_at_full_size() {
    let mut rng = Rng(42);
    // Mid-size: still cheap for the cubic oracle in debug builds.
    let vectors = case(&mut rng, 1, 160, 32);
    assert_eq!(
        cluster(&vectors, 0.5, Some(10)).unwrap(),
        oracle(&vectors, 0.5, Some(10))
    );
    // About an hour of 30 s chunks with several turns each, CAM++-sized vectors.
    // No timing assertion: this only has to finish and stay well-formed.
    let vectors = case(&mut rng, 1, 1500, 192);
    let labels = cluster(&vectors, 0.5, Some(10)).unwrap();
    assert_eq!(labels.len(), 1500);
    let k = labels.iter().max().unwrap() + 1;
    assert!((1..=10).contains(&k), "{k}");
    // Labels are numbered by first appearance.
    let mut seen = 0;
    for &label in &labels {
        assert!(label <= seen);
        seen = seen.max(label + 1);
    }
}

fn eye(n: usize) -> Vec<Vec<f64>> {
    (0..n)
        .map(|i| (0..n).map(|j| f64::from(u8::from(i == j))).collect())
        .collect()
}

#[test]
fn three_tight_groups_and_first_appearance_numbering() {
    let vectors = vec![
        vec![0.0, 1.0, 0.0],
        vec![1.0, 0.01, 0.0],
        vec![0.01, 1.0, 0.0],
        vec![0.0, 0.0, 1.0],
        vec![0.99, 0.0, 0.02],
        vec![0.0, 0.02, 0.98],
    ];
    assert_eq!(cluster(&vectors, 0.3, None).unwrap(), [0, 1, 0, 2, 1, 2]);
    // Deterministic across calls.
    assert_eq!(
        cluster(&vectors, 0.3, None).unwrap(),
        cluster(&vectors, 0.3, None).unwrap()
    );
}

#[test]
fn threshold_extremes_and_slack() {
    assert_eq!(cluster(&eye(3), 0.5, None).unwrap(), [0, 1, 2]);
    assert_eq!(cluster(&eye(3), 1.0, None).unwrap(), [0, 0, 0]);
    // Distance exactly 1 merges at threshold 1 - 1e-13 thanks to DIST_SLACK.
    assert_eq!(cluster(&eye(2), 1.0 - 1e-13, None).unwrap(), [0, 0]);
    assert_eq!(cluster(&eye(2), 1.0 - 1e-11, None).unwrap(), [0, 1]);
    // Opposite vectors (distance 2) only merge with a large threshold.
    let opposite = vec![vec![1.0, 0.0], vec![-1.0, 0.0]];
    assert_eq!(cluster(&opposite, 1.5, None).unwrap(), [0, 1]);
    assert_eq!(cluster(&opposite, 2.0, None).unwrap(), [0, 0]);
}

#[test]
fn max_speakers_cap_forces_merges_with_row_major_ties() {
    assert_eq!(cluster(&eye(4), -1.0, Some(2)).unwrap(), [0, 0, 0, 1]);
    assert_eq!(cluster(&eye(4), -1.0, Some(1)).unwrap(), [0, 0, 0, 0]);
    assert_eq!(cluster(&eye(4), -1.0, Some(4)).unwrap(), [0, 1, 2, 3]);
    assert_eq!(cluster(&eye(3), 0.5, Some(10)).unwrap(), [0, 1, 2]);
}

#[test]
fn average_linkage_uses_cluster_sizes() {
    // a, b identical; c at distance 0.4 from both; d far. Average linkage merges
    // {a,b} first, then c (0.4), and leaves d.
    let c = [0.6f64, 0.8];
    let vectors = vec![vec![1.0, 0.0], vec![1.0, 0.0], c.to_vec(), vec![-1.0, 0.0]];
    assert_eq!(cluster(&vectors, 0.45, None).unwrap(), [0, 0, 0, 1]);
    assert_eq!(cluster(&vectors, 0.35, None).unwrap(), [0, 0, 1, 2]);
}

#[test]
fn zero_vectors_are_distance_one_and_errors_are_reported() {
    let vectors = vec![vec![0.0, 0.0], vec![1.0, 0.0], vec![0.0, 0.0]];
    assert_eq!(cluster(&vectors, 0.9, None).unwrap(), [0, 1, 2]);
    assert_eq!(cluster(&vectors, 1.0, None).unwrap(), [0, 0, 0]);
    let empty: Vec<Vec<f64>> = Vec::new();
    assert!(cluster(&empty, 0.5, None).unwrap().is_empty());
    assert_eq!(cluster(&[vec![1.0]], 0.5, None).unwrap(), [0]);
    assert!(cluster(&[vec![f64::NAN, 0.0]], 0.5, None).is_err());
    assert!(cluster(&[vec![f64::INFINITY]], 0.5, None).is_err());
    assert!(cluster(&[vec![1.0, 0.0], vec![1.0]], 0.5, None).is_err());
    assert!(cluster(&eye(2), 0.5, Some(0)).is_err());
}
