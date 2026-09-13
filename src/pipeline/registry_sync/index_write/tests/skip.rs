// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! C-032 — the skip predicate. All four conditions, the key→digest **pair**
//! comparison that is the guard on the headline requirement, and the
//! package-level field comparison that lets a description-only change through.

use super::super::*;
use super::support::*;

/// A source root as the predicate takes it: the bytes the index served and
/// the typed shape parsed from them.
fn source(raw: Vec<u8>) -> (Vec<u8>, IndexRoot) {
    let root = parse_root(&raw);
    (raw, root)
}

/// A catalog holding the one correct entry for `local`: exactly
/// `sha256(local root bytes)`, which is what `write_root` derives.
fn consistent_catalog(local: &[u8]) -> CatalogIndex {
    let mut catalog = CatalogIndex::new();
    catalog.insert(PACKAGE.to_string(), IndexStore::root_catalog_entry(local));
    catalog
}

#[test]
fn a_fully_consistent_package_is_skipped() {
    let content = digest("a");
    let local = root_document(POINTER, &[("3.28.1", &content), ("latest", &content)]);
    let source = source(root_document(
        "oci://ghcr.io/ocx-sh/cmake",
        &[("3.28.1", &content), ("latest", &content)],
    ));

    let skipped = should_skip(
        PACKAGE,
        &source.0,
        &source.1,
        Some(&read_result(&local)),
        &consistent_catalog(&local),
    );

    assert!(skipped, "all three conditions hold");
}

#[test]
fn a_repointed_tag_is_not_skipped() {
    // The headline guard. `latest` moved to a new digest without adding a key,
    // so the two KEY SETS are identical — a key-set comparison would skip this
    // package forever and the mirror would serve a stale digest silently. The
    // tag union makes that permanent, since after any union `local ⊇ source`
    // holds unconditionally.
    let old = digest("old");
    let new = digest("new");
    let local = root_document(POINTER, &[("3.28.1", &old), ("latest", &old)]);
    let catalog = consistent_catalog(&local);

    // GREEN — same keys, same digests: skipped.
    let unchanged = source(root_document(
        "oci://ghcr.io/ocx-sh/cmake",
        &[("3.28.1", &old), ("latest", &old)],
    ));
    assert!(should_skip(
        PACKAGE,
        &unchanged.0,
        &unchanged.1,
        Some(&read_result(&local)),
        &catalog
    ));

    // RED — the SAME key set, one digest moved: must not be skipped.
    let repointed = source(root_document(
        "oci://ghcr.io/ocx-sh/cmake",
        &[("3.28.1", &old), ("latest", &new)],
    ));
    let source_keys: Vec<_> = repointed.1.tags.keys().cloned().collect();
    let local_keys: Vec<_> = parse_root(&local).tags.keys().cloned().collect();
    assert_eq!(source_keys, local_keys, "the fixture must keep the key sets identical");
    assert!(
        !should_skip(
            PACKAGE,
            &repointed.0,
            &repointed.1,
            Some(&read_result(&local)),
            &catalog
        ),
        "a re-pointed tag was skipped — the predicate is comparing key sets, not pairs"
    );
}

#[test]
fn a_package_with_no_root_on_disk_is_not_skipped() {
    let source = source(root_document("oci://ghcr.io/ocx-sh/cmake", &[("3.28.1", &digest("a"))]));

    assert!(!should_skip(PACKAGE, &source.0, &source.1, None, &CatalogIndex::new()));
}

#[test]
fn a_tag_added_upstream_is_not_skipped() {
    let content = digest("a");
    let local = root_document(POINTER, &[("3.28.1", &content)]);
    let source = source(root_document(
        "oci://ghcr.io/ocx-sh/cmake",
        &[("3.28.1", &content), ("3.29.0", &digest("b"))],
    ));

    assert!(!should_skip(
        PACKAGE,
        &source.0,
        &source.1,
        Some(&read_result(&local)),
        &consistent_catalog(&local)
    ));
}

#[test]
fn an_uncatalogued_root_is_not_skipped() {
    // The interrupted-run damage state: a completed root in `p/` that `commit`
    // never published. Without condition 3 the next run skips it and publishes
    // a catalog permanently missing the package.
    let content = digest("a");
    let local = root_document(POINTER, &[("3.28.1", &content)]);
    let source = source(root_document("oci://ghcr.io/ocx-sh/cmake", &[("3.28.1", &content)]));

    assert!(!should_skip(
        PACKAGE,
        &source.0,
        &source.1,
        Some(&read_result(&local)),
        &CatalogIndex::new()
    ));
}

#[test]
fn a_drifted_catalog_entry_is_not_skipped() {
    let content = digest("a");
    let local = root_document(POINTER, &[("3.28.1", &content)]);
    let source = source(root_document("oci://ghcr.io/ocx-sh/cmake", &[("3.28.1", &content)]));
    let mut catalog = CatalogIndex::new();
    catalog.insert(PACKAGE.to_string(), digest("something else").to_string());

    assert!(!should_skip(
        PACKAGE,
        &source.0,
        &source.1,
        Some(&read_result(&local)),
        &catalog
    ));
}

#[test]
fn a_local_root_holding_extra_tags_is_still_skipped() {
    // The union means `local ⊇ source` is the steady state, so a tag the
    // source has since retired must not force a re-copy every run — and the
    // rewritten `repository` is the other steady-state difference. Those two
    // keys are exactly what condition 4 leaves out of its comparison.
    let content = digest("a");
    let local = root_document(POINTER, &[("3.27.0", &digest("retired")), ("3.28.1", &content)]);
    let source = source(root_document("oci://ghcr.io/ocx-sh/cmake", &[("3.28.1", &content)]));

    assert!(should_skip(
        PACKAGE,
        &source.0,
        &source.1,
        Some(&read_result(&local)),
        &consistent_catalog(&local)
    ));
}

/// A description-only change upstream — `desc` moved, not one tag added or
/// re-pointed — must not be skipped (ocx-mirror#70).
///
/// Falsified by dropping condition 4: conditions 1–3 all hold here, and the
/// mirrored root then keeps the old README and logo digests on every run.
#[test]
fn a_description_only_change_is_not_skipped() {
    let content = digest("a");
    let local = described_root(POINTER, &content, "sha256:aaaa");
    let catalog = consistent_catalog(&local);

    // GREEN — identical `desc`: skipped.
    let unchanged = source(described_root("oci://ghcr.io/ocx-sh/cmake", &content, "sha256:aaaa"));
    assert!(should_skip(
        PACKAGE,
        &unchanged.0,
        &unchanged.1,
        Some(&read_result(&local)),
        &catalog
    ));

    // RED — same tags, `desc.readme` moved: must not be skipped.
    let updated = source(described_root("oci://ghcr.io/ocx-sh/cmake", &content, "sha256:bbbb"));
    assert_eq!(
        parse_root(&local).tags.keys().collect::<Vec<_>>(),
        updated.1.tags.keys().collect::<Vec<_>>(),
        "the fixture must keep the tag pairs identical"
    );
    assert!(
        !should_skip(PACKAGE, &updated.0, &updated.1, Some(&read_result(&local)), &catalog),
        "a description-only change was skipped — the predicate is comparing tags alone"
    );
}

/// Condition 4 reads bytes, and bytes that are not a JSON object are "not
/// skipped" — the copy path then reports them — never "nothing to compare".
#[test]
fn unparseable_source_bytes_are_not_skipped() {
    let content = digest("a");
    let local = root_document(POINTER, &[("3.28.1", &content)]);
    let source = parse_root(&root_document("oci://ghcr.io/ocx-sh/cmake", &[("3.28.1", &content)]));

    assert!(
        !should_skip(
            PACKAGE,
            b"[]",
            &source,
            Some(&read_result(&local)),
            &consistent_catalog(&local)
        ),
        "a non-object source document must fall through to the copy path"
    );
}

/// [`root_document`] with a `desc` object whose `readme` pointer is `readme`.
fn described_root(repository: &str, content: &Digest, readme: &str) -> Vec<u8> {
    let mut document: serde_json::Value =
        serde_json::from_slice(&root_document(repository, &[("3.28.1", content)])).expect("parse root document");
    document["desc"] = serde_json::json!({
        "digest": "sha256:0000",
        "title": "cmake",
        "readme": readme,
        "logo": null,
    });
    serialize_root(&document)
}
