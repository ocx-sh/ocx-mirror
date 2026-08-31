// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use super::support::*;

// ── argv construction ─────────────────────────────────────────────────

#[test]
fn a_published_layer_becomes_a_digest_reference_with_its_media_type_extension() {
    let args = patch_push_args(
        "ghcr.io/ocx-sh/cmake:3.29.0_20260610",
        &image(vec![descriptor(tar_xz())]),
        Path::new("/work/3.29.0-linux_amd64-metadata.json"),
        &BTreeMap::new(),
        true,
    )
    .expect("argv assembles");

    assert_eq!(
        args,
        vec![
            "--format",
            "json",
            "package",
            "push",
            "--cascade",
            "-p",
            "linux/amd64",
            "-i",
            "ghcr.io/ocx-sh/cmake:3.29.0_20260610",
            "--metadata",
            "/work/3.29.0-linux_amd64-metadata.json",
            &format!("sha256:{}.tar.xz", "a".repeat(64)),
        ],
    );
}

/// Order is the layer stack. A patch that reorders or drops one republishes
/// a different package under the same tag.
#[test]
fn every_layer_is_re_referenced_in_manifest_order() {
    let mut base = descriptor(tar_gz());
    base.digest = format!("sha256:{}", "1".repeat(64));
    let mut top = descriptor(tar_xz());
    top.digest = format!("sha256:{}", "2".repeat(64));

    let args = patch_push_args(
        "ghcr.io/ocx-sh/cmake:3.29.0",
        &image(vec![base, top]),
        Path::new("/work/metadata.json"),
        &BTreeMap::new(),
        true,
    )
    .expect("argv assembles");

    let layers: Vec<&String> = args.iter().filter(|arg| arg.starts_with("sha256:")).collect();
    assert_eq!(
        layers,
        vec![
            &format!("sha256:{}.tar.gz", "1".repeat(64)),
            &format!("sha256:{}.tar.xz", "2".repeat(64)),
        ],
    );
}

/// `--cascade` is the spec's call, and it is what re-points `3.29.0`,
/// `3.29`, `3` and `latest` onto the patched manifest.
#[test]
fn cascade_follows_the_spec_flag() {
    let cascading = patch_push_args(
        "ghcr.io/ocx-sh/cmake:3.29.0",
        &image(vec![descriptor(tar_xz())]),
        Path::new("/work/metadata.json"),
        &BTreeMap::new(),
        true,
    )
    .expect("argv assembles");
    let plain = patch_push_args(
        "ghcr.io/ocx-sh/cmake:3.29.0",
        &image(vec![descriptor(tar_xz())]),
        Path::new("/work/metadata.json"),
        &BTreeMap::new(),
        false,
    )
    .expect("argv assembles");

    assert!(cascading.iter().any(|arg| arg == "--cascade"), "got: {cascading:?}");
    assert!(!plain.iter().any(|arg| arg == "--cascade"), "got: {plain:?}");
}

/// A sidecar without the stamped `platform` field exits 65 on the `ocx`
/// side, so the flag carrying it is load-bearing, not cosmetic.
#[test]
fn the_sidecar_is_always_passed_explicitly() {
    let args = patch_push_args(
        "ghcr.io/ocx-sh/cmake:3.29.0",
        &image(vec![descriptor(tar_xz())]),
        Path::new("/work/metadata.json"),
        &BTreeMap::new(),
        true,
    )
    .expect("argv assembles");

    let flag = args
        .iter()
        .position(|arg| arg == "--metadata")
        .expect("--metadata present");
    assert_eq!(args.get(flag + 1).map(String::as_str), Some("/work/metadata.json"));
}

#[test]
fn spec_annotations_survive_onto_the_patched_index() {
    let annotations = BTreeMap::from([(
        "org.opencontainers.image.source".to_string(),
        "https://github.com/ocx-sh/mirror-cmake".to_string(),
    )]);
    let args = patch_push_args(
        "ghcr.io/ocx-sh/cmake:3.29.0",
        &image(vec![descriptor(tar_xz())]),
        Path::new("/work/metadata.json"),
        &annotations,
        true,
    )
    .expect("argv assembles");

    assert!(
        args.contains(&"org.opencontainers.image.source=https://github.com/ocx-sh/mirror-cmake".to_string()),
        "got: {args:?}",
    );
}

/// Defaulting the extension would re-emit the manifest declaring a format
/// the layer's bytes do not have, and every consumer would fail to unpack
/// it — silently, since the digest still verifies.
#[test]
fn an_unmappable_layer_media_type_errors_instead_of_guessing() {
    let error = patch_push_args(
        "ghcr.io/ocx-sh/cmake:3.29.0",
        &image(vec![descriptor("application/vnd.oci.image.layer.v1.tar")]),
        Path::new("/work/metadata.json"),
        &BTreeMap::new(),
        true,
    )
    .expect_err("must reject");

    assert!(error.contains("application/vnd.oci.image.layer.v1.tar"), "got: {error}");
}

/// The spec `republish` reads: the target reference it pushes to, and
/// whether that push cascades.
fn minimal_spec() -> MirrorSpec {
    serde_yaml_ng::from_str(
        r#"
name: cmake
target:
  registry: ghcr.io
  repository: ocx-sh/cmake
source:
  type: github_release
  owner: kitware
  repo: cmake
assets:
  linux/amd64:
    - "cmake\\.tar\\.xz"
"#,
    )
    .expect("the fixture parses")
}

/// The message a failed patch reports to its caller, verbatim.
///
/// `republish` runs through `pipeline push`'s bounded `push_once` so a hung
/// registry cannot park the patch job until GitHub's cap. That refactor must
/// not have cost what the registry actually said — a patch run reds on this
/// string and nothing else carries the reason.
#[cfg(unix)]
#[tokio::test]
async fn a_rejected_republish_reports_the_exit_code_and_the_stderr() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("tempdir");
    let script = dir.path().join("fake-ocx");
    std::fs::write(&script, "#!/bin/sh\necho 'manifest rejected' >&2\nexit 65\n").expect("script writes");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let error = republish(
        &minimal_spec(),
        "3.29.0",
        &image(vec![descriptor(tar_xz())]),
        r#"{"type":"bundle","version":1}"#,
        &dir.path().join("3.29.0-linux_amd64-metadata.json"),
        &BTreeMap::new(),
        &script,
    )
    .await
    .expect_err("a rejected push must not read as a republished manifest");

    assert!(error.contains("ocx package push exited"), "got: {error}");
    assert!(error.contains("65"), "got: {error}");
    assert!(error.contains("manifest rejected"), "got: {error}");
}

/// The success half of the same contract, which routing through `push_once`
/// changed and nothing covered: exit 0 used to be unconditional success,
/// and is now success only when stdout parses as a `PushReport`. `{}` is the
/// minimum that does — every field defaults — so this pins that a report
/// carrying no fields is still a republished manifest, and that the
/// stricter contract did not turn an ordinary patch run red.
#[cfg(unix)]
#[tokio::test]
async fn a_republish_that_exits_zero_with_a_parseable_report_succeeds() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("tempdir");
    let script = dir.path().join("fake-ocx");
    std::fs::write(&script, "#!/bin/sh\necho '{}'\n").expect("script writes");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    republish(
        &minimal_spec(),
        "3.29.0",
        &image(vec![descriptor(tar_xz())]),
        r#"{"type":"bundle","version":1}"#,
        &dir.path().join("3.29.0-linux_amd64-metadata.json"),
        &BTreeMap::new(),
        &script,
    )
    .await
    .expect("exit 0 with a parseable report is a republished manifest");
}
