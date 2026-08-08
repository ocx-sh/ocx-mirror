// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use std::collections::BTreeMap;

use super::super::*;
use crate::command::package::pipeline::patch::patch_push_args;
use crate::pipeline::ocx_cli::push::build_push_args;
use crate::pipeline::target_registry;

// ── Backfill cascade repair (BUG3) ────────────────────────────────────

/// A published `(version, platform)` tile, as `fetch_published_images`
/// returns it. Only `platform` and the layer/config descriptors matter
/// here — the re-push re-references both by digest.
fn published_tile(platform: &str, layer_media_type: &str) -> target_registry::PublishedImage {
    target_registry::PublishedImage {
        version: Version::parse("26.5.1").expect("valid version"),
        platform: platform.parse().expect("valid platform"),
        manifest_digest: ocx_lib::oci::Digest::Sha256("b".repeat(64)),
        config: ocx_lib::oci::Descriptor {
            media_type: "application/vnd.sh.ocx.package.v1+json".to_string(),
            digest: format!("sha256:{}", "c".repeat(64)),
            size: 42,
            urls: None,
            artifact_type: None,
            annotations: None,
        },
        layers: vec![ocx_lib::oci::Descriptor {
            media_type: layer_media_type.to_string(),
            digest: format!("sha256:{}", "a".repeat(64)),
            size: 1024,
            urls: None,
            artifact_type: None,
            annotations: None,
        }],
    }
}

#[test]
fn a_version_completed_across_two_runs_re_cascades_the_earlier_runs_entries() {
    // Live-pilot BUG3 (mirror-pypi wave 1+2). `ocx/black:26.5.1` completed
    // across two runs: run 1 published glibc + musl + darwin with the
    // windows leg red, so phase 2 withheld `--cascade` from all three; run 2
    // backfilled windows alone — `pipeline plan` had trimmed the three
    // already-published tiles — and cascaded only that one. `:26.5.1` merged
    // to all four entries, but `26.5`, `26` and `latest` carried
    // windows/amd64 alone, so a bare `ocx/black` reference failed to resolve
    // on every non-windows host.
    let published = [
        published_tile("linux/amd64+libc.glibc", "application/vnd.oci.image.layer.v1.tar+zstd"),
        published_tile("linux/amd64+libc.musl", "application/vnd.oci.image.layer.v1.tar+zstd"),
        published_tile("darwin/arm64", "application/vnd.oci.image.layer.v1.tar+zstd"),
        published_tile("windows/amd64", "application/vnd.oci.image.layer.v1.tar+zstd"),
    ];

    let awaiting: Vec<String> = entries_awaiting_cascade(&published, &["windows/amd64".to_string()])
        .iter()
        .map(|image| image.platform.to_string())
        .collect();

    assert_eq!(
        awaiting,
        vec!["linux/amd64+libc.glibc", "linux/amd64+libc.musl", "darwin/arm64"],
        "the rolling tags must carry the whole merged version index, not this run's legs",
    );
}

#[test]
fn a_version_pushed_whole_in_one_run_has_nothing_left_to_re_cascade() {
    // The single-run case (pycowsay, yt-dlp in the pilot) is already correct:
    // every leg carried `--cascade`. The repair must not re-push tiles this
    // run just published — that would spend a config-blob upload per tile
    // per version on every green run.
    let published = [
        published_tile("linux/amd64", "application/vnd.oci.image.layer.v1.tar+xz"),
        published_tile("darwin/arm64", "application/vnd.oci.image.layer.v1.tar+xz"),
    ];
    let pushed = vec!["linux/amd64".to_string(), "darwin/arm64".to_string()];

    assert!(
        entries_awaiting_cascade(&published, &pushed).is_empty(),
        "a version pushed whole in one run needs no repair",
    );
}

#[test]
fn the_re_cascade_argv_carries_cascade_and_the_published_layer_digests() {
    // The repair re-emits the tile from the registry's OWN descriptors: the
    // published layers by digest (never re-uploaded, never re-downloaded) and
    // `--cascade`, which is the entire point of the re-push.
    let image = published_tile("linux/amd64+libc.glibc", "application/vnd.oci.image.layer.v1.tar+zstd");
    let sidecar = PathBuf::from("/work/26.5.1-linux_amd64_libc.glibc-metadata.json");

    let args = patch_push_args("ghcr.io/ocx-sh/black:26.5.1", &image, &sidecar, &BTreeMap::new(), true)
        .expect("the published layer media type has an archive extension");

    assert_eq!(
        args,
        vec![
            "--format",
            "json",
            "package",
            "push",
            "--cascade",
            "--new",
            "-p",
            "linux/amd64+libc.glibc",
            "-i",
            "ghcr.io/ocx-sh/black:26.5.1",
            "--metadata",
            "/work/26.5.1-linux_amd64_libc.glibc-metadata.json",
            &format!("sha256:{}.tar.zst", "a".repeat(64)),
        ],
    );
}

#[test]
fn build_push_args_without_annotations_matches_the_bare_invocation() {
    let args = build_push_args(
        "linux/amd64",
        "ghcr.io/ocx-sh/shfmt:3.8.0",
        &["/bundles/shfmt.tar.xz"],
        None,
        &BTreeMap::new(),
        true,
    )
    .expect("utf-8 bundle path");

    assert_eq!(args.len(), 11);
    assert!(!args.iter().any(|arg| arg == "--annotation"));
}

#[test]
fn build_push_args_omits_cascade_so_a_platform_can_land_without_moving_an_alias() {
    // The non-cascade shape still names the exact version tag, and the
    // registry merges the platform into that tag's image index — a version
    // can therefore be assembled platform by platform and only advertised
    // through `latest` / `X` / `X.Y` once it is whole.
    let args = build_push_args(
        "linux/amd64",
        "ghcr.io/ocx-sh/shfmt:3.8.0",
        &["/bundles/shfmt.tar.xz"],
        None,
        &BTreeMap::new(),
        false,
    )
    .expect("utf-8 bundle path");

    assert!(!args.iter().any(|arg| arg == "--cascade"), "got: {args:?}");
    assert_eq!(
        args,
        vec![
            "--format",
            "json",
            "package",
            "push",
            "--new",
            "-p",
            "linux/amd64",
            "-i",
            "ghcr.io/ocx-sh/shfmt:3.8.0",
            "/bundles/shfmt.tar.xz",
        ],
    );
}

/// The `ocx` subprocess inherits the runner environment — the generated
/// workflow's push step carries `GH_TOKEN` — so the assembled argv must
/// never carry a value sourced from outside the three-name allowlist.
///
/// Same guarantee as `annotations::tests::secret_shaped_env_never_reaches_an_annotation`,
/// one boundary further out: that one stops at the map, this one at the
/// argv the subprocess actually receives. Driven through the injected
/// lookup rather than the real environment — reading `std::env` would make
/// the assertion depend on where it runs, and CI legitimately carries the
/// allowlisted values under other names too (`GITHUB_WORKFLOW_SHA` holds
/// the same SHA as `GITHUB_SHA`), so a real-env read collides on *value*
/// and no name skip or length threshold can repair it.
#[test]
fn build_push_args_never_carries_a_non_allowlisted_env_value() {
    const TOKEN: &str = "ghs_liveTokenFromTheRunnerEnvironment";

    let annotations = crate::annotations::assemble(&BTreeMap::new(), |name| match name {
        "GITHUB_SERVER_URL" => Some("https://github.com".to_string()),
        "GITHUB_REPOSITORY" => Some("ocx-sh/mirror-shfmt".to_string()),
        "GITHUB_SHA" => Some("a1b2c3d4".to_string()),
        // Every other name the function might reach for answers with a token.
        _ => Some(TOKEN.to_string()),
    });

    let args = build_push_args(
        "linux/amd64",
        "ghcr.io/ocx-sh/shfmt:3.8.0",
        &["/bundles/shfmt.tar.xz"],
        None,
        &annotations,
        true,
    )
    .expect("utf-8 bundle path");

    assert!(
        !args.iter().any(|arg| arg.contains(TOKEN)),
        "argv carries a value from outside the allowlist: {args:?}"
    );
    // Positive half, so the assertion above cannot pass on an empty argv.
    assert!(
        args.contains(&"org.opencontainers.image.source=https://github.com/ocx-sh/mirror-shfmt".to_string())
            && args.contains(&"org.opencontainers.image.revision=a1b2c3d4".to_string()),
        "allowlisted values must still reach the argv: {args:?}"
    );
}
