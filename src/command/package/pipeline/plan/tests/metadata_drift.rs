// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use super::support::*;
use ocx_lib::oci::Algorithm;

// ── metadata drift (ocx-mirror#9) ─────────────────────────────────────
//
// The comparison decides whether an already-published version gets
// re-pushed. A false positive republishes the fleet and re-points every
// version tag; a false negative leaves the stale metadata that made
// `pipeline patch` necessary in the first place. So each case below is
// asserted in both directions.

/// A published `metadata.json` from before `binaries` existed, and the same
/// document as the spec writes it today. This is the drift the pilots have.
const WITHOUT_BINARIES: &str = r#"{"type":"bundle","version":1,"strip_components":1}"#;
const WITH_BINARIES: &str = r#"{"type":"bundle","version":1,"strip_components":1,"binaries":["cmake"]}"#;

#[test]
fn identical_metadata_is_not_drift() {
    // Green half of the pair: the steady state of every healthy mirror. If
    // this ever reds, the next cron run republishes every published version
    // of every mirror.
    let published = metadata(WITH_BINARIES);
    let expected = metadata(WITH_BINARIES);
    assert!(
        !metadata_drifted(&published, &expected).expect("comparison succeeds"),
        "identical metadata must not report drift"
    );
}

#[test]
fn a_changed_field_is_drift() {
    // Red half: the spec gained `binaries` after the version was published.
    let published = metadata(WITHOUT_BINARIES);
    let expected = metadata(WITH_BINARIES);
    assert!(
        metadata_drifted(&published, &expected).expect("comparison succeeds"),
        "a metadata field added by the spec must report drift"
    );
}

#[test]
fn key_order_does_not_fabricate_drift() {
    // The reason the comparison is by value and not by bytes. Both
    // documents describe the same package; only the key order differs, as
    // it does across the ocx versions that wrote the fleet's config blobs.
    const REORDERED: &str = r#"{"binaries":["cmake"],"strip_components":1,"version":1,"type":"bundle"}"#;

    // Guard: if these ever became byte-equal the test would pass without
    // exercising anything — a byte comparison must be able to red here.
    assert_ne!(
        WITH_BINARIES.as_bytes(),
        REORDERED.as_bytes(),
        "the fixtures must differ as bytes, or this test proves nothing"
    );

    assert!(
        !metadata_drifted(&metadata(REORDERED), &metadata(WITH_BINARIES)).expect("comparison succeeds"),
        "key order alone must not report drift — it would republish the whole fleet"
    );
}

/// A published image whose config descriptor records `blob` verbatim —
/// exactly what the registry returns for bytes some earlier `ocx` wrote.
fn published_image(blob: &str) -> PublishedImage {
    PublishedImage {
        version: Version::parse("3.29.0").expect("valid version"),
        platform: "linux/amd64".parse().expect("valid platform"),
        manifest_digest: Algorithm::Sha256.hash(b"manifest"),
        config: ocx_lib::oci::Descriptor {
            media_type: "application/vnd.sh.ocx.package.v1+json".to_string(),
            digest: Algorithm::Sha256.hash(blob.as_bytes()).to_string(),
            size: blob.len() as i64,
            urls: None,
            artifact_type: None,
            annotations: None,
        },
        layers: vec![],
    }
}

#[test]
fn matching_config_digest_settles_it_without_a_blob_fetch() {
    // The steady state: the recorded blob is the byte-for-byte output of
    // serializing the expected metadata, so the fast path answers "no
    // drift" and `plan` never fetches the blob.
    let expected = metadata(WITH_BINARIES);
    let recorded = String::from_utf8(serde_json::to_vec(&expected).expect("serializes")).expect("utf-8");
    let image = published_image(&recorded);

    assert!(
        config_bytes_match(&image, &expected).expect("digest compares"),
        "identical config bytes must short-circuit the comparison"
    );
}

#[test]
fn reordered_config_bytes_do_not_short_circuit_as_drift() {
    // The asymmetry the fast path rests on. These bytes describe the same
    // package but were written with a different key order, so their digest
    // differs — `config_bytes_match` must answer "cannot tell" (false) and
    // hand the pair to the value comparison, which finds no drift. A fast
    // path that reported drift on a digest mismatch would republish every
    // version the fleet holds.
    const REORDERED: &str = r#"{"binaries":["cmake"],"strip_components":1,"version":1,"type":"bundle"}"#;
    let expected = metadata(WITH_BINARIES);
    let image = published_image(REORDERED);

    assert!(
        !config_bytes_match(&image, &expected).expect("digest compares"),
        "a different serialization must not match by digest, or this test proves nothing"
    );
    assert!(
        !metadata_drifted(&metadata(REORDERED), &expected).expect("comparison succeeds"),
        "the fallback comparison must clear it — same value, different bytes"
    );
}

#[test]
fn drift_entry_names_the_version_and_platforms() {
    let entry = drift_entry(
        "slim-3.13.9_20260610",
        Some("slim"),
        vec!["darwin/arm64".to_string(), "linux/amd64".to_string()],
    );

    let value: serde_json::Value = serde_json::to_value(&entry).expect("entry serializes");
    assert_eq!(value["kind"].as_str().unwrap(), "metadata-drift");
    assert_eq!(value["version"].as_str().unwrap(), "slim-3.13.9_20260610");
    assert_eq!(
        value["platforms"].as_array().unwrap(),
        &vec![
            serde_json::Value::from("darwin/arm64"),
            serde_json::Value::from("linux/amd64")
        ]
    );
    assert_eq!(value["variant"].as_str().unwrap(), "slim");
    // A patch downloads nothing: no source version, no resolved assets.
    assert_eq!(value["source_version"].as_str().unwrap(), "");
    assert!(value["assets"].as_array().unwrap().is_empty());
}

#[test]
fn drift_entry_survives_the_discover_projection() {
    // The generated workflow projects `[.versions[] | {version, platforms,
    // kind}]` into the job matrix (generate/templates/workflow.yml). An
    // added kind must keep every projected field present and non-null.
    let report = PlanReport {
        schema_version: 3,
        has_new: false,
        has_drift: true,
        versions: vec![drift_entry("3.29.0", None, vec!["linux/amd64".to_string()])],
        target: "ocx.sh/cmake".to_string(),
        ocx_mirror_rev: None,
    };

    let value: serde_json::Value = serde_json::to_value(&report).expect("report serializes");
    assert!(value["has_drift"].as_bool().unwrap());
    assert!(
        !value["has_new"].as_bool().unwrap(),
        "drift alone must not set has_new — the build jobs gate on it and a patch has nothing to build"
    );
    let projected = &value["versions"][0];
    assert!(projected["version"].is_string());
    assert!(projected["platforms"].is_array());
    assert_eq!(projected["kind"].as_str().unwrap(), "metadata-drift");
}

#[test]
fn plan_version_kind_str_matches_serde() {
    // `as_str` renders the plain table; serde renders plan.json. Two
    // spellings of one wire name — this is the lock that keeps them equal.
    for kind in [
        PlanVersionKind::New,
        PlanVersionKind::BackfillPartial,
        PlanVersionKind::MetadataDrift,
    ] {
        let serialized = serde_json::to_value(&kind).expect("kind serializes");
        assert_eq!(
            serialized.as_str().unwrap(),
            kind.as_str(),
            "as_str must match the serde spelling for {kind:?}"
        );
    }
}

#[test]
fn leaf_versions_drops_cascade_aliases() {
    // `3.29.0_20260610` cascades to `3.29.0`, `3.29`, `3` over the same
    // child manifests. Scanning the aliases would schedule the same patch
    // four times.
    let tags: Vec<String> = ["3.29.0_20260610", "3.29.0", "3.29", "3", "latest"]
        .iter()
        .map(|t| t.to_string())
        .collect();

    let leaves = leaf_versions(&tags);
    let leaves: Vec<&str> = leaves.values().map(String::as_str).collect();
    assert_eq!(leaves, vec!["3.29.0_20260610"]);
}

#[test]
fn leaf_versions_keeps_a_mirror_without_build_timestamps() {
    // Not every mirror stamps a build timestamp; there the patch version
    // itself is the leaf, and dropping it would disable drift detection
    // entirely for that mirror.
    let tags: Vec<String> = ["3.29.0", "3.28.5", "3.29", "3.28", "3", "latest"]
        .iter()
        .map(|t| t.to_string())
        .collect();

    let leaves = leaf_versions(&tags);
    let leaves: Vec<&str> = leaves.values().map(String::as_str).collect();
    // BTreeMap keys are Version-ordered, so the output is oldest first.
    assert_eq!(leaves, vec!["3.28.5", "3.29.0"]);
}

#[test]
fn leaf_versions_keeps_variants_apart() {
    // `slim-3.13.9` is not an alias of `3.13.9` — different variant, its own
    // metadata block, its own patch.
    let tags: Vec<String> = ["3.13.9", "slim-3.13.9", "3.13", "slim-3.13"]
        .iter()
        .map(|t| t.to_string())
        .collect();

    let leaves = leaf_versions(&tags);
    let leaves: Vec<&str> = leaves.values().map(String::as_str).collect();
    assert_eq!(leaves.len(), 2, "got {leaves:?}");
    assert!(leaves.contains(&"3.13.9"));
    assert!(leaves.contains(&"slim-3.13.9"));
}

#[test]
fn plan_version_entry_omits_pylock_key_when_not_pypi_derived() {
    // `pylock` is set only for `source.type: pypi` entries (the derived
    // lock a version was resolved from); every other source type must
    // leave it absent from the JSON entirely, not `null`.
    let value = serde_json::to_value(entry("3.29.0", &["linux/amd64"], PlanVersionKind::New)).unwrap();
    assert!(
        value.as_object().unwrap().get("pylock").is_none(),
        "expected no 'pylock' key, got: {value}"
    );
}
