// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use ocx_lib::oci::{Algorithm, Descriptor, Digest, Platform};

// ── bin_scan: the drift comparison against a claim it cannot recompute ──
//
// `plan` and `patch` are download-free by design, so neither can run the
// scan that produces `binaries`. Every assertion below defends the same
// property from a different side: the expectation must adopt the published
// claim, or a scanned mirror drifts on every run and each patch republishes
// the claim away.

/// The metadata a mirror's spec file declares — no `binaries`, because a
/// scanned one is never written into the spec.
fn spec_expectation() -> ExpectedMetadata {
    let authoring = serde_json::from_slice(br#"{"type":"bundle","version":1,"strip_components":1,"env":[]}"#)
        .expect("metadata fixture parses");
    ExpectedMetadata::render(authoring, &scan_platform()).expect("the fixture renders both projections")
}

fn scan_platform() -> Platform {
    "linux/amd64".parse().expect("valid platform")
}

/// What the registry records for a tile a `bin_scan` mirror published: the
/// spec's metadata plus the scanned claim.
fn published_with_binaries() -> Metadata {
    serde_json::from_slice(
        br#"{"type":"bundle","version":1,"strip_components":1,"env":[],"binaries":["cmake","ctest"]}"#,
    )
    .expect("published fixture parses")
}

fn image_recording(config_digest: &str) -> PublishedImage {
    PublishedImage {
        version: Version::parse("3.29.0").expect("valid version"),
        platform: scan_platform(),
        manifest_digest: Digest::Sha256("b".repeat(64)),
        config: Descriptor {
            media_type: "application/vnd.ocx.package.metadata.v1+json".to_string(),
            digest: config_digest.to_string(),
            size: 42,
            urls: None,
            annotations: None,
            artifact_type: None,
        },
        layers: vec![],
    }
}

/// A correctly published `bin_scan` tile must never be settled from its
/// config digest.
///
/// The expectation resolved from the spec structurally cannot carry
/// `binaries`, so its digest can only match a tile that has none. Trusting
/// the digest therefore reports drift on every correctly published tile,
/// forever — and `patch` acting on that republishes the claim away.
#[test]
fn only_an_incomplete_bin_scan_tile_skips_its_config_digest() {
    let expected = spec_expectation();
    let bytes = serde_json::to_vec(&expected.published).expect("serializes");
    // The strongest possible case for a skip: the digests are identical.
    let image = image_recording(&Algorithm::Sha256.hash(&bytes).to_string());

    assert!(
        settled_by_digest(&image, &expected.published, BinScanMode::Off).expect("digest compares"),
        "control: without a bin_scan an identical config digest must settle the tile",
    );
    for mode in [BinScanMode::Auto, BinScanMode::Verify] {
        assert!(
            !settled_by_digest(&image, &expected.published, mode).expect("digest compares"),
            "{mode:?} on a spec declaring no binaries must fall through to the blob read that \
             adopts the published claim, even on an identical config digest",
        );
    }

    // A spec that hand-declares `binaries` computes the whole document
    // download-free, scanning or not — mirror-kitware is exactly this. Making
    // it skip the digest anyway costs a config-blob GET per tile per cron run
    // and turns one unparseable blob into an aborted discover job.
    let declared: ocx_lib::package::metadata::authoring::AuthoringMetadata = serde_json::from_slice(
        br#"{"type":"bundle","version":1,"strip_components":1,"env":[],"binaries":["cmake","ctest"]}"#,
    )
    .expect("declared fixture parses");
    let complete = ExpectedMetadata::render(declared, &scan_platform()).expect("renders");
    let bytes = serde_json::to_vec(&complete.published).expect("serializes");
    let image = image_recording(&Algorithm::Sha256.hash(&bytes).to_string());

    for mode in [BinScanMode::Off, BinScanMode::Auto, BinScanMode::Verify] {
        assert!(
            settled_by_digest(&image, &complete.published, mode).expect("digest compares"),
            "{mode:?} with a declared claim is fully computable and must settle on the digest",
        );
    }
}

/// The published claim must reach both projections of the expectation.
///
/// `published` decides drift; `sidecar_json` is what `patch` pushes. A fix
/// that adopted only the first would still delete the claim the moment any
/// unrelated field drifted.
#[test]
fn adopting_carries_the_published_binaries_into_both_projections() {
    let expected = spec_expectation();
    assert!(
        expected.published.binaries().is_none(),
        "the spec-resolved expectation must start without binaries, or this test proves nothing",
    );

    let adopted = expected
        .adopting_binaries_from(&published_with_binaries(), &scan_platform())
        .expect("adoption renders");

    assert_eq!(
        adopted.published.binaries().map(|binaries| binaries.len()),
        Some(2),
        "the drift comparison must see the published claim",
    );
    assert!(
        adopted.sidecar_json.contains("cmake") && adopted.sidecar_json.contains("ctest"),
        "the sidecar patch republishes with must carry the claim too, or the push deletes it: {}",
        adopted.sidecar_json,
    );
}

/// The whole point, stated as the comparison itself: a tile differing from
/// the spec *only* by its scanned `binaries` is current, not drifted.
#[test]
fn a_tile_differing_only_by_its_scanned_binaries_is_current() {
    let published = published_with_binaries();
    let expected = spec_expectation();

    assert!(
        metadata_drifted(&published, &expected.published).expect("compares"),
        "control: unadopted, the published claim reads as drift — this is the bug being prevented",
    );
    assert!(
        !metadata_drifted(
            &published,
            &expected
                .adopting_binaries_from(&published, &scan_platform())
                .expect("adoption renders")
                .published,
        )
        .expect("compares"),
        "with the claim adopted, the tile must read as current",
    );
}

/// A `binaries` list the *spec* declares must never be adopted away.
///
/// Adoption exists for the claim only the artifact knows — a scanned one.
/// Applying it to a hand-written list rewrites the expectation to whatever
/// the registry already holds, so that field can never drift: a maintainer
/// correcting a wrong declared list gets silence, `plan` reports nothing,
/// and the fix never reaches the published versions. Includes `verify`,
/// which checks the declaration against the tree rather than replacing it.
#[test]
fn a_spec_declared_binaries_claim_is_never_adopted_away() {
    let declared: ocx_lib::package::metadata::authoring::AuthoringMetadata = serde_json::from_slice(
        br#"{"type":"bundle","version":1,"strip_components":1,"env":[],"binaries":["cmake","ctest"]}"#,
    )
    .expect("declared fixture parses");
    let expected = ExpectedMetadata::render(declared, &scan_platform()).expect("renders");

    // What the registry still records: the shorter, wrong list the spec was
    // just corrected away from.
    let published: Metadata =
        serde_json::from_slice(br#"{"type":"bundle","version":1,"strip_components":1,"env":[],"binaries":["cmake"]}"#)
            .expect("published fixture parses");

    let adopted = expected
        .adopting_binaries_from(&published, &scan_platform())
        .expect("adoption renders");

    assert_eq!(
        adopted.published.binaries().map(|binaries| binaries.len()),
        Some(2),
        "the spec's declared list must survive adoption untouched",
    );
    assert!(
        metadata_drifted(&published, &adopted.published).expect("compares"),
        "a corrected hand-written list must report drift so the fix can land",
    );
}

/// Adoption must not invent a claim. A mirror that turns `bin_scan` on
/// publishes `binaries` from the next push onward; until then the published
/// tiles have none, and reporting them as current would hide real drift on
/// every other field.
#[test]
fn adopting_from_a_tile_without_binaries_changes_nothing() {
    let expected = spec_expectation();
    let published: Metadata =
        serde_json::from_slice(br#"{"type":"bundle","version":1,"env":[]}"#).expect("published fixture parses");

    let adopted = expected
        .adopting_binaries_from(&published, &scan_platform())
        .expect("adoption renders");

    assert!(
        adopted.published.binaries().is_none(),
        "nothing to adopt must leave the expectation untouched",
    );
    assert!(
        metadata_drifted(&published, &adopted.published).expect("compares"),
        "a tile that really has drifted (strip_components) must still report drift",
    );
}
