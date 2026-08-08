// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;

// ── Version ordering: total order, newest last ─────────────────────────
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
