// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! C-043 — the `--dry-run` byte estimate.

use super::super::*;
use super::support::*;

#[test]
fn it_sums_every_missing_descriptor() {
    let missing = [(digest("a"), 100), (digest("b"), 250)];

    assert_eq!(dry_run_byte_estimate(&missing), 350);
}

#[test]
fn each_distinct_digest_is_counted_once() {
    // A blob shared by six platform manifests is one transfer, so counting it
    // six times would report an estimate the run can never match — and the
    // figure is sold as exact, not a heuristic.
    let shared = digest("shared");
    let missing = [
        (shared.clone(), 1_000),
        (digest("other"), 7),
        (shared.clone(), 1_000),
        (shared, 1_000),
    ];

    assert_eq!(dry_run_byte_estimate(&missing), 1_007);
}

#[test]
fn a_fully_warm_destination_estimates_zero() {
    assert_eq!(dry_run_byte_estimate(&[]), 0);
}

#[test]
fn absurd_descriptor_sizes_saturate_instead_of_overflowing() {
    // `size` is foreign data off an upstream manifest, so nothing upstream of
    // this function bounds it. Summed unchecked, two near-maximum descriptors
    // panic a debug build outright and wrap a release one to an under-count.
    let missing = [(digest("a"), u64::MAX - 1), (digest("b"), u64::MAX - 1)];

    assert_eq!(dry_run_byte_estimate(&missing), u64::MAX);
}
