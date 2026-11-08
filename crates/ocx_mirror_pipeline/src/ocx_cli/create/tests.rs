// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use ocx_config::exec_state::{ExecState, Narrowing};

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
    let (frozen_key, offline_key) = (ocx_env::OCX_FROZEN.name, ocx_env::OCX_OFFLINE.name);
    let state_key = ocx_env::__OCX_ENV_STATE.name;

    let _lock = ocx_mirror_test_support::OCX_ENV_LOCK.lock().await;
    for (offline, frozen, remote) in [
        (None, None, true),
        (Some("0"), Some("false"), true),
        (Some("1"), None, false),
        (None, Some("true"), false),
    ] {
        let _env = ocx_mirror_test_support::EnvRestore::set(&[
            (offline_key, offline),
            (frozen_key, frozen),
            (state_key, None),
        ]);
        assert_eq!(
            resolves_tags_remotely(),
            remote,
            "OCX_OFFLINE={offline:?} OCX_FROZEN={frozen:?}",
        );
    }
}

fn encoded_state(narrowing: Narrowing) -> String {
    let state = ExecState {
        narrowing,
        ..ExecState::default()
    };
    ocx_config::exec_state::encode(&state)
        .value
        .expect("a header-only state always encodes")
}

/// A parent `ocx --offline` or `--frozen` reaches the mirror only as the
/// narrowing in `__OCX_ENV_STATE`, never as `OCX_OFFLINE`/`OCX_FROZEN`; the
/// credential-helper marker implies offline. A narrowing that restricts
/// nothing else leaves resolution remote.
#[tokio::test]
async fn an_inherited_narrowing_resolves_tags_locally() {
    let (frozen_key, offline_key) = (ocx_env::OCX_FROZEN.name, ocx_env::OCX_OFFLINE.name);
    let state_key = ocx_env::__OCX_ENV_STATE.name;

    let _lock = ocx_mirror_test_support::OCX_ENV_LOCK.lock().await;
    for (label, narrowing, remote) in [
        ("none", Narrowing::default(), true),
        (
            "offline",
            Narrowing {
                offline: true,
                ..Narrowing::default()
            },
            false,
        ),
        (
            "frozen",
            Narrowing {
                frozen: true,
                ..Narrowing::default()
            },
            false,
        ),
        (
            "credential_helper",
            Narrowing {
                credential_helper: true,
                ..Narrowing::default()
            },
            false,
        ),
        (
            "no_consent",
            Narrowing {
                no_consent: true,
                ..Narrowing::default()
            },
            true,
        ),
    ] {
        let value = encoded_state(narrowing);
        let _env = ocx_mirror_test_support::EnvRestore::set(&[
            (offline_key, None),
            (frozen_key, None),
            (state_key, Some(&value)),
        ]);
        assert_eq!(resolves_tags_remotely(), remote, "narrowing: {label}");
    }
}

/// A value the decoder cannot read carries no narrowing and resolves remotely,
/// unless its plaintext header still names one: the header is read first so a
/// bad payload cannot hide it.
#[tokio::test]
async fn an_unreadable_state_resolves_remotely_unless_its_header_narrows() {
    let (frozen_key, offline_key) = (ocx_env::OCX_FROZEN.name, ocx_env::OCX_OFFLINE.name);
    let state_key = ocx_env::__OCX_ENV_STATE.name;

    let offline = encoded_state(Narrowing {
        offline: true,
        ..Narrowing::default()
    });
    let header_end = offline.find('.').expect("header and payload are dot-separated");
    let broken_payload = format!("{}.!!!not-a-payload", &offline[..header_end]);

    let _lock = ocx_mirror_test_support::OCX_ENV_LOCK.lock().await;
    for (label, value, remote) in [
        ("garbage", "not-an-ocx-state".to_string(), true),
        ("offline header, broken payload", broken_payload, false),
    ] {
        let _env = ocx_mirror_test_support::EnvRestore::set(&[
            (offline_key, None),
            (frozen_key, None),
            (state_key, Some(&value)),
        ]);
        assert_eq!(resolves_tags_remotely(), remote, "{label}");
    }
}

/// The mirror's own `OCX_OFFLINE` still wins when the inherited state narrows nothing.
#[tokio::test]
async fn the_mirrors_own_offline_wins_over_an_unnarrowed_state() {
    let (frozen_key, offline_key) = (ocx_env::OCX_FROZEN.name, ocx_env::OCX_OFFLINE.name);
    let state_key = ocx_env::__OCX_ENV_STATE.name;
    let value = encoded_state(Narrowing::default());

    let _lock = ocx_mirror_test_support::OCX_ENV_LOCK.lock().await;
    let _env = ocx_mirror_test_support::EnvRestore::set(&[
        (offline_key, Some("1")),
        (frozen_key, None),
        (state_key, Some(&value)),
    ]);
    assert!(!resolves_tags_remotely());
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
