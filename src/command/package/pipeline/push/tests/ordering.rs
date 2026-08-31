// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;

// ── Version ordering: total order, newest last ─────────────────────────
#[test]
fn registry_tag_newer_than_ignores_rolling_and_canonical_tags() {
    // Real reserved shapes, not the old `sha256.abc123` placeholder — that hex
    // is 6 chars, so it is not a keep tag at all and pinned nothing. ocx 0.6.0
    // writes `__ocx.keep.<alg>-<hex>`; the frozen legacy `<alg>.<hex>` form
    // still exists in already-published repositories. Both must be skipped, and
    // so must the OCI referrers / cosign sidecars the copy path now carries.
    let hex = "a".repeat(64);
    let tags: Vec<String> = [
        "latest".to_string(),
        format!("__ocx.keep.sha256-{hex}"),
        format!("sha256.{hex}"),
        format!("sha256-{hex}"),
        format!("sha256-{hex}.sig"),
        "__ocx.desc".to_string(),
        "0.0.0.1".to_string(),
        "0.0.0.2".to_string(),
    ]
    .into_iter()
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
/// Pins the cross-binary fact the mirror's keep-tag spelling decision rests on.
///
/// `registry_copy::push_canonical_tag` deliberately keeps writing the frozen
/// legacy `<alg>.<hex>` form rather than following ocx 0.6.0's rename to
/// `__ocx.keep.<alg>-<hex>`. That is only safe while `ocx_lib` still classifies
/// the legacy spelling as reserved — otherwise the mirror's own deletion
/// safety-net tags would start reading back as versions at every consumer.
///
/// Cannot fail against ocx 0.6.0; it is here to fail loudly on a future
/// submodule bump that drops the `Tag::LegacyKeep` read arm, which would
/// otherwise be a silent divergence between two binaries at one registry.
#[test]
fn ocx_still_classifies_the_legacy_keep_tag_spelling_as_reserved() {
    use ocx_lib::package::tag::Tag;
    let hex = "a".repeat(64);

    // The spelling `push_canonical_tag` writes.
    assert!(Tag::is_reserved_str(&format!("sha256.{hex}")));
    // The spelling `ocx package push` writes as of 0.6.0.
    assert!(Tag::is_reserved_str(&format!("__ocx.keep.sha256-{hex}")));
    // A real version must stay unreserved, or the filter would eat the tags
    // the alias decision exists to compare.
    assert!(!Tag::is_reserved_str("1.2.3"));
}
