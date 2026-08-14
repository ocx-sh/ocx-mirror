// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! C-039 — the short-circuit's two conditions, and the per-root fallback that
//! runs when it does not fire.

use ocx_lib::file_structure::{CatalogEntryStatus, IndexStore, RootReadResult};
use ocx_lib::oci::index::{IndexRoot, serialize_root};
use serde_json::{Value, json};

use super::super::*;
use super::support::*;

const DIGEST: &str = "sha256:0f1e2d3c";

fn filtered(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_string()).collect()
}

fn local(names: &[&str]) -> CatalogIndex {
    names
        .iter()
        .map(|name| ((*name).to_string(), digest(name).to_string()))
        .collect()
}

/// What a fully successful run over `source_digest` + `local` would have
/// recorded — spelled through the production builder, so a test cannot pass by
/// agreeing with a record shape the code no longer writes.
fn recorded(source_digest: &str, local: &CatalogIndex) -> String {
    super::super::cache_record(source_digest, local)
}

#[test]
fn it_fires_only_when_all_three_conditions_hold() {
    let selected = filtered(&["kitware/cmake"]);
    let mirrored = local(&["kitware/cmake"]);

    assert!(short_circuit(
        DIGEST,
        Some(&recorded(DIGEST, &mirrored)),
        &selected,
        &mirrored
    ));

    // A changed source catalog: something upstream moved.
    let stale_source = recorded("sha256:deadbeef", &mirrored);
    assert!(!short_circuit(DIGEST, Some(&stale_source), &selected, &mirrored));

    // No previous fully-successful run recorded anything.
    assert!(!short_circuit(DIGEST, None, &selected, &mirrored));

    // The catalog is unchanged, but a filtered package was never mirrored —
    // the widened-`include:` blind spot a digest-only comparison misses.
    let elsewhere = local(&["other/pkg"]);
    assert!(!short_circuit(
        DIGEST,
        Some(&recorded(DIGEST, &elsewhere)),
        &selected,
        &elsewhere
    ));
}

#[test]
fn a_catalog_entry_corrupted_in_place_defeats_the_short_circuit() {
    // The defect the acceptance suite caught, at unit scale. The KEY survives
    // the corruption, so the subset test passes; the source catalog never
    // moved, so the digest test passes. Without the fingerprint the whole
    // source is skipped in one request, C-032's third condition never runs,
    // and the drifted entry stays drifted forever — silently, and reported as
    // "unchanged since the last run".
    let selected = filtered(&["kitware/cmake"]);
    let mirrored = local(&["kitware/cmake"]);
    let record = recorded(DIGEST, &mirrored);

    // GREEN — intact catalog, nothing upstream moved.
    assert!(short_circuit(DIGEST, Some(&record), &selected, &mirrored));

    // RED — same keys, one value rewritten to a digest that matches no root.
    let mut corrupted = mirrored.clone();
    corrupted.insert("kitware/cmake".to_string(), format!("sha256:{}", "0".repeat(64)));
    assert_eq!(
        corrupted.keys().collect::<Vec<_>>(),
        mirrored.keys().collect::<Vec<_>>(),
        "the fixture must keep the key set identical, or it proves nothing"
    );
    assert!(
        !short_circuit(DIGEST, Some(&record), &selected, &corrupted),
        "a corrupted catalog entry was skipped past — the short-circuit is comparing keys, not contents"
    );
}

#[test]
fn a_deleted_catalog_entry_defeats_the_short_circuit_too() {
    // The `[missing]` half. It also breaks the subset test, which is why it
    // passed even before the fingerprint existed — and why its passing gave
    // false confidence about the corrupted case.
    let selected = filtered(&["kitware/cmake"]);
    let mirrored = local(&["kitware/cmake"]);
    let record = recorded(DIGEST, &mirrored);

    assert!(!short_circuit(DIGEST, Some(&record), &selected, &CatalogIndex::new()));
}

#[test]
fn the_fingerprint_separates_keys_from_values() {
    // Without the `\0` separators `{"a": "bc"}` and `{"ab": "c"}` hash the
    // same, and a rename-plus-edit would slip through as unchanged.
    let one: CatalogIndex = [("a".to_string(), "bc".to_string())].into_iter().collect();
    let other: CatalogIndex = [("ab".to_string(), "c".to_string())].into_iter().collect();

    assert_ne!(
        super::super::catalog_fingerprint(&one),
        super::super::catalog_fingerprint(&other)
    );
}

#[test]
fn a_narrowed_include_keeps_short_circuiting_forever() {
    // `⊆`, not `==`. The mirror is append-only, so the local catalog only ever
    // grows: the first time an operator narrows `include:` or widens
    // `exclude:`, an equality test stops holding for the life of the tree and
    // the headline no-op behaviour silently degrades from one request to one
    // per package, every run, with no diagnostic anywhere.
    let mirrored = local(&["kitware/cmake", "ninja-build/ninja", "retired/tool"]);

    // RED under `==`: strictly fewer filtered names than local keys.
    let narrowed = filtered(&["kitware/cmake"]);
    assert!(narrowed.len() < mirrored.len(), "the fixture must not be an equal set");
    let record = recorded(DIGEST, &mirrored);
    assert!(
        short_circuit(DIGEST, Some(&record), &narrowed, &mirrored),
        "a narrowed include: must still short-circuit — this is `⊆`, not `==`"
    );

    // GREEN either way: the sets are equal.
    let unchanged = filtered(&["kitware/cmake", "ninja-build/ninja", "retired/tool"]);
    assert!(short_circuit(DIGEST, Some(&record), &unchanged, &mirrored));

    // Still refused when the filter WIDENS onto something never mirrored,
    // which is what `⊆` keeps closed.
    let widened = filtered(&["kitware/cmake", "brand/new"]);
    assert!(!short_circuit(DIGEST, Some(&record), &widened, &mirrored));
}

#[test]
fn an_empty_filter_set_short_circuits_against_any_local_catalog() {
    // Vacuously `⊆`. A source whose globs select nothing has no work, and
    // making it re-walk 121 roots every run would be the same NFR regression
    // in a different disguise.
    let empty = local(&[]);
    assert!(short_circuit(
        DIGEST,
        Some(&recorded(DIGEST, &empty)),
        &filtered(&[]),
        &empty
    ));
}

// ── The per-root fallback ────────────────────────────────────────────────────

/// A root document carrying `tag → content` pointers.
fn root_document(repository: &str, tags: &[(&str, &ocx_lib::oci::Digest)]) -> Vec<u8> {
    let mut document = serde_json::Map::new();
    document.insert("repository".to_string(), json!(repository));
    let mut tag_map = serde_json::Map::new();
    for (tag, content) in tags {
        tag_map.insert((*tag).to_string(), json!({ "content": content.to_string() }));
    }
    document.insert("tags".to_string(), Value::Object(tag_map));
    serialize_root(&Value::Object(document))
}

fn parse_root(raw: &[u8]) -> IndexRoot {
    serde_json::from_slice(raw).expect("parse root document")
}

fn read_result(raw: &[u8]) -> RootReadResult {
    RootReadResult {
        bytes: raw.to_vec(),
        root: parse_root(raw),
        catalog_status: CatalogEntryStatus::NoCatalog,
    }
}

/// The fallback, spelled the way the orchestrator spells it: the work list
/// trimmed by `index_write::should_skip`. There is deliberately no second
/// predicate here — C-039 and C-032 are one comparison in the design.
fn packages_needing_copy(work: &[PackageWork], source: &IndexRoot, local: &RootReadResult) -> Vec<String> {
    let mut catalog = CatalogIndex::new();
    for package in work {
        catalog.insert(package.name.clone(), IndexStore::root_catalog_entry(&local.bytes));
    }
    work.iter()
        .filter(|package| !super::super::super::index_write::should_skip(&package.name, source, Some(local), &catalog))
        .map(|package| package.name.clone())
        .collect()
}

#[test]
fn the_fallback_re_copies_a_tag_re_pointed_without_a_new_key() {
    // The pairs-not-key-sets rule, observed through the plan path rather than
    // through `index_write` directly: a cascade repair or a yank-driven
    // `latest` re-point leaves the KEY SETS identical, and C-047's union makes
    // `local ⊇ source` unconditionally true after any run — so a key-set
    // comparison would skip this package forever and the mirror would serve a
    // stale digest silently.
    let spec = spec("{registry}/{namespace}/{package}");
    let work = expand_source(&spec, &source(&[], &[]), "ocx.sh", &catalog(&["kitware/cmake"])).expect("expand");
    let old = digest("old");
    let new = digest("new");
    let mirrored = root_document("oci://registry.test/mirror/ocx.sh/kitware/cmake", &[("latest", &old)]);

    // GREEN — same key, same digest: nothing to do.
    let unchanged = parse_root(&root_document("oci://ghcr.io/ocx-sh/cmake", &[("latest", &old)]));
    assert!(packages_needing_copy(&work, &unchanged, &read_result(&mirrored)).is_empty());

    // RED — the same key set, one digest moved.
    let repointed = parse_root(&root_document("oci://ghcr.io/ocx-sh/cmake", &[("latest", &new)]));
    assert_eq!(
        packages_needing_copy(&work, &repointed, &read_result(&mirrored)),
        vec!["kitware/cmake"],
        "a re-pointed tag was skipped — the fallback is comparing key sets, not pairs"
    );
}

#[test]
fn the_fallback_re_copies_a_tag_added_upstream() {
    let spec = spec("{registry}/{namespace}/{package}");
    let work = expand_source(&spec, &source(&[], &[]), "ocx.sh", &catalog(&["kitware/cmake"])).expect("expand");
    let content = digest("a");
    let mirrored = root_document(
        "oci://registry.test/mirror/ocx.sh/kitware/cmake",
        &[("3.28.1", &content)],
    );
    let grown = parse_root(&root_document(
        "oci://ghcr.io/ocx-sh/cmake",
        &[("3.28.1", &content), ("3.29.0", &digest("b"))],
    ));

    assert_eq!(
        packages_needing_copy(&work, &grown, &read_result(&mirrored)),
        vec!["kitware/cmake"]
    );
}
