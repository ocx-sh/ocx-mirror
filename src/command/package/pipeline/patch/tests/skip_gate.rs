// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use super::support::*;
use crate::pipeline::orchestrator::ExpectedMetadata;
use crate::spec::BinScanMode;
use ocx_lib::oci::Algorithm;

// ── skip gate ─────────────────────────────────────────────────────────

/// The expectation a fresh push of the fixture would record.
fn expected_metadata() -> ExpectedMetadata {
    let authoring = serde_json::from_slice(br#"{"type":"bundle","version":1,"strip_components":1,"env":[]}"#)
        .expect("metadata fixture parses");
    ExpectedMetadata::render(authoring, &"linux/amd64".parse().expect("valid platform"))
        .expect("the fixture renders both projections")
}

fn image_recording(config_digest: &str) -> PublishedImage {
    let mut image = image(vec![descriptor(tar_xz())]);
    image.config.digest = config_digest.to_string();
    image
}

fn offline_publisher() -> Publisher {
    Publisher::new(ClientBuilder::from_env().expect("client builds"))
}

/// Patch must refuse when the layer layout changed under it.
///
/// `strip_components` is the whole mapping from a layer's archive paths onto
/// `${installPath}`, fixed when the layer is built. Patch re-references the
/// published layers by digest, so republishing metadata that strips
/// differently describes a tree they do not contain — every
/// `${installPath}`-rooted PATH entry names a directory absent from the
/// bundle, and the package installs broken while its digest still verifies.
///
/// Live on `mirror-astral-sh`: windows/amd64 published as
/// `strip_components: 1` with `PATH=${installPath}`, then had to move to
/// `strip_components: 0` with `PATH=${installPath}/python` because the
/// `bin_scan` load gate rejects the bare form. All six leaf tags read as
/// drifted, and patching them would have shipped exactly that.
#[test]
fn a_strip_components_change_is_a_layout_change_not_a_metadata_fix() {
    let strip = |n: Option<u8>| -> ocx_lib::package::metadata::Metadata {
        let field = n.map(|n| format!(r#","strip_components":{n}"#)).unwrap_or_default();
        serde_json::from_str(&format!(r#"{{"type":"bundle","version":1{field},"env":[]}}"#))
            .expect("metadata fixture parses")
    };

    // Every published-vs-expected pair a patch run can see. Only an equal
    // pair may be republished; the rest describe a different tree.
    for (was, now, patchable) in [
        (Some(1), Some(1), true),
        (None, None, true),
        (Some(1), Some(0), false),
        (Some(0), Some(1), false),
        // `None` and `Some(0)` mean the same layout but are different
        // published bytes. Refusing costs one message a human resolves;
        // the opposite ships a broken package. Pinned so a future
        // "normalize None to 0" is a decision, not a silent drift.
        (None, Some(0), false),
    ] {
        let refusal = layout_refusal(&strip(was), &strip(now));
        assert_eq!(
            refusal.is_none(),
            patchable,
            "published strip {was:?} -> spec strip {now:?}, got: {refusal:?}",
        );
        if let Some(message) = refusal {
            assert!(
                message.contains("Re-mirror"),
                "a refusal must name the remedy, since the patch workflow is the signposted one: {message}",
            );
        }
    }
}

/// The idempotency mechanism, and the whole reason there is no patch
/// ledger: a `(version, platform)` whose config blob already carries what
/// the spec would publish is settled from the descriptor's digest alone —
/// no blob fetch, no push.
#[tokio::test]
async fn matching_metadata_skips_without_a_registry_call() {
    let expected = expected_metadata();
    let bytes = serde_json::to_vec(&expected.published).expect("serializes");
    let image = image_recording(&Algorithm::Sha256.hash(&bytes).to_string());

    let drift = image_drift(
        &offline_publisher(),
        &Identifier::new_registry("mirror/cmake", "registry.test"),
        &image,
        &expected,
        BinScanMode::Off,
    )
    .await
    .expect("the config digest settles it without a fetch");

    assert!(drift.is_none(), "a matching config digest must not schedule a patch");
}

/// The red half of the test above: with a config digest that does NOT
/// match, `Ok(None)` — the value that produces a skip — must be
/// unreachable, whatever the registry then answers.
#[tokio::test]
async fn a_differing_config_digest_never_settles_as_a_skip() {
    let image = image_recording(&format!("sha256:{}", "f".repeat(64)));

    let result = image_drift(
        &offline_publisher(),
        &Identifier::new_registry("mirror/cmake", "registry.invalid"),
        &image,
        &expected_metadata(),
        BinScanMode::Off,
    )
    .await;

    assert!(
        !matches!(result, Ok(None)),
        "a config digest that differs must never settle as 'already current': {}",
        result.is_ok(),
    );
}
