// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::*;

fn request(bin_scan: BinScanMode, libc_lint: bool) -> Vec<String> {
    let platform: Platform = "linux/amd64".parse().expect("valid platform");
    CreateRequest {
        content_dir: Path::new("/work/content"),
        platform: &platform,
        metadata: Path::new("/work/authoring-metadata.json"),
        output: Path::new("/work/bundle.tar.xz"),
        compression_threads: 4,
        bin_scan,
        libc_lint,
        remote: false,
    }
    .args()
}

/// The fixed part of the argv: the tree, the platform the pins resolve
/// against, the authoring input, the bundle, and the thread count the spec's
/// concurrency asked for.
#[test]
fn create_argv_names_the_tree_platform_metadata_output_and_threads() {
    assert_eq!(
        request(BinScanMode::Auto, true),
        [
            "package",
            "create",
            "/work/content",
            "--platform",
            "linux/amd64",
            "--metadata",
            "/work/authoring-metadata.json",
            "--output",
            "/work/bundle.tar.xz",
            "--force",
            "--threads",
            "4",
        ],
    );
}

/// `bin_scan` maps one-for-one onto create's modes. `off` must say so
/// explicitly: create's default is `auto`, so omitting the flag would scan and
/// fill a claim the spec asked never to be invented.
#[test]
fn create_argv_maps_every_bin_scan_mode() {
    let tail = |mode| request(mode, true)[12..].to_vec();
    assert_eq!(tail(BinScanMode::Off), ["--no-bin-scan"]);
    assert!(tail(BinScanMode::Auto).is_empty(), "auto is create's default");
    assert_eq!(tail(BinScanMode::Verify), ["--bin-scan"]);
}

/// `libc_lint: false` is create's `--no-libc-lint`, and nothing is passed for
/// the default — the check runs.
#[test]
fn create_argv_carries_the_libc_opt_out_only_when_set() {
    assert!(!request(BinScanMode::Off, true).contains(&"--no-libc-lint".to_string()));
    assert_eq!(
        request(BinScanMode::Off, false).last().map(String::as_str),
        Some("--no-libc-lint")
    );
}

/// Each build pins a tag-only dependency to what the tag names now, so the
/// argv asks for registry resolution — as a root option, ahead of the
/// subcommand, where `ocx` accepts it — and carries nothing when told not to.
#[test]
fn create_argv_leads_with_remote_only_when_asked() {
    let platform: Platform = "linux/amd64".parse().expect("valid platform");
    let args = |remote| {
        CreateRequest {
            content_dir: Path::new("/work/content"),
            platform: &platform,
            metadata: Path::new("/work/authoring-metadata.json"),
            output: Path::new("/work/bundle.tar.xz"),
            compression_threads: 4,
            bin_scan: BinScanMode::Auto,
            libc_lint: true,
            remote,
        }
        .args()
    };
    assert_eq!(args(true)[..3], ["--remote", "package", "create"]);
    assert_eq!(args(true)[1..], args(false)[..]);
    assert!(!args(false).contains(&"--remote".to_string()));
}

/// Remote resolution is the default, and yields to an operator who asked for
/// local resolution: `OCX_OFFLINE` has no network to resolve against, and
/// `OCX_FROZEN` makes `ocx` refuse `--remote` outright.
#[tokio::test]
async fn tags_resolve_remotely_unless_the_operator_pins_resolution_locally() {
    use ocx_config::env::keys::{OCX_FROZEN, OCX_OFFLINE};

    let _lock = ocx_mirror_test_support::OCX_ENV_LOCK.lock().await;
    for (offline, frozen, remote) in [
        (None, None, true),
        (Some("0"), Some("false"), true),
        (Some("1"), None, false),
        (None, Some("true"), false),
    ] {
        let _env = ocx_mirror_test_support::EnvRestore::set(&[(OCX_OFFLINE, offline), (OCX_FROZEN, frozen)]);
        assert_eq!(
            resolves_tags_remotely(),
            remote,
            "OCX_OFFLINE={offline:?} OCX_FROZEN={frozen:?}",
        );
    }
}

/// ocx derives the sidecar from the bundle name; the mirror has to read it back
/// from exactly there.
#[test]
fn the_compiled_sidecar_sits_beside_the_bundle_under_ocx_naming() {
    assert_eq!(
        compiled_sidecar_path(Path::new("/work/bundle.tar.xz")),
        Path::new("/work/bundle-metadata.json")
    );
}

/// A refusal's reason lives on create's stderr; the error must carry it.
#[cfg(unix)]
#[tokio::test]
async fn a_failed_create_carries_its_stderr() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("tempdir");
    let script = dir.path().join("fake-ocx");
    std::fs::write(
        &script,
        "#!/bin/sh\necho 'dependency tag jre-25 not found' >&2\nexit 79\n",
    )
    .expect("write");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let error = create(&script, &[], CREATE_TIMEOUT)
        .await
        .expect_err("a non-zero create must fail");
    let rendered = format!("{error:#}");
    assert!(rendered.contains("jre-25 not found"), "got: {rendered}");
    assert!(rendered.contains("79"), "and the exit status: {rendered}");
}
