// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;

// ── Version ordering: total order, newest last ─────────────────────────

#[test]
fn pep440_sort_key_is_a_total_order_over_versions_the_ocx_parser_rejects() {
    // The replaced comparator was `(Some, Some) => semver, _ => text`, which
    // on this exact triple cycles: `ocx_lib::Version` rejects `2.0rc1`, so
    // 10.0.0 > 3.0.0 (semver), 3.0.0 > 2.0rc1 (text) and 2.0rc1 > 10.0.0
    // (text). `sort_by` leaves the result unspecified for such a predicate.
    let mut versions = vec![
        "10.0.0".to_string(),
        "3.0.0".to_string(),
        "2.0rc1".to_string(),
        "2.0".to_string(),
        "0.0.0.2".to_string(),
    ];
    versions.sort_by_key(|v| pep440_sort_key(v));
    assert_eq!(versions, ["0.0.0.2", "2.0rc1", "2.0", "3.0.0", "10.0.0"]);

    // Newest LAST is the contract every `.last()` reader here depends on.
    assert_eq!(versions.last().map(String::as_str), Some("10.0.0"));

    // A tag no PEP 440 parser accepts sorts first, so it can never be
    // mistaken for the newest.
    let mut mixed = vec!["nightly".to_string(), "1.0.0".to_string()];
    mixed.sort_by_key(|v| pep440_sort_key(v));
    assert_eq!(mixed, ["nightly", "1.0.0"]);
}

#[test]
fn registry_tag_newer_than_ignores_rolling_and_canonical_tags() {
    let tags: Vec<String> = ["latest", "sha256.abc123", "0.0.0.1", "0.0.0.2"]
        .iter()
        .map(|t| (*t).to_string())
        .collect();

    // Nothing published is newer than the run's newest → the alias may move.
    assert_eq!(registry_tag_newer_than(&tags, "0.0.0.2"), None);

    // A backfill run whose newest version is older than a published one
    // must not re-point `:latest` at it.
    assert_eq!(registry_tag_newer_than(&tags, "0.0.0.1"), Some("0.0.0.2"));

    // An unorderable pair (the run's version does not parse as PEP 440)
    // counts as newer, so the caller declines — the safe direction.
    assert_eq!(registry_tag_newer_than(&tags, "nightly"), Some("0.0.0.1"));
}
