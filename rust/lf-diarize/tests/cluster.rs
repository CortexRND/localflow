use lf_diarize::cluster;

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
