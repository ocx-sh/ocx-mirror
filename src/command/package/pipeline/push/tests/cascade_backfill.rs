// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use std::collections::BTreeMap;

use ocx_lib::ci::CiFlavor;

use super::super::*;
use super::support::*;
use crate::annotations::build_annotations_for;
use crate::command::package::pipeline::patch::patch_push_args;
use crate::pipeline::ocx_cli::push::build_push_args;
use crate::pipeline::target_registry;
use crate::test_support::EnvRestore;

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

    let args = patch_push_args(
        "ghcr.io/ocx-sh/black:26.5.1",
        &image,
        &sidecar,
        &BTreeMap::new(),
        true,
        None,
    )
    .expect("the published layer media type has an archive extension");

    assert_eq!(
        args,
        vec![
            "--format",
            "json",
            "package",
            "push",
            "--cascade",
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
        None,
    )
    .expect("utf-8 bundle path");

    assert_eq!(args.len(), 10);
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
        None,
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
            "-p",
            "linux/amd64",
            "-i",
            "ghcr.io/ocx-sh/shfmt:3.8.0",
            "/bundles/shfmt.tar.xz",
        ],
    );
}

/// The argv boundary: the `ocx` child inherits the runner's whole environment,
/// so the only way a token reaches a published index is through an
/// `--annotation` the mirror assembled. Every credential name the runner
/// could carry answers with a canary here, and none of it may surface —
/// while the allowlisted GitHub names still do, so the guard cannot pass on
/// an empty argv.
#[test]
fn build_push_args_never_carries_a_non_allowlisted_env_value() {
    const TOKEN: &str = "ghs_liveTokenFromTheRunnerEnvironment";
    let _guard = job_url_env_lock();
    let _restore = EnvRestore::set(&[
        ("GH_TOKEN", Some(TOKEN)),
        ("GITHUB_TOKEN", Some(TOKEN)),
        ("OCX_ANNOUNCE_TOKEN", Some(TOKEN)),
        ("CI_JOB_TOKEN", Some(TOKEN)),
        ("GITHUB_SERVER_URL", Some("https://github.com")),
        ("GITHUB_REPOSITORY", Some("ocx-sh/mirror-shfmt")),
        ("GITHUB_SHA", Some("a1b2c3d4")),
    ]);

    let annotations = build_annotations_for(Some(CiFlavor::GitHubActions), &BTreeMap::new());
    let args = build_push_args(
        "linux/amd64",
        "ghcr.io/ocx-sh/shfmt:3.8.0",
        &["/bundles/shfmt.tar.xz"],
        None,
        &annotations,
        true,
        None,
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

/// `image.created` is the wall clock when nothing pins it, so a map built per
/// leg dates one version's platforms differently. Structural, as
/// `pipeline::push::tests` pins its signing shape: the property is that the
/// archive loop hands `invoke_push` the run's map instead of letting it build
/// its own — once for the archive run, once for the env run, and no third.
#[test]
fn the_annotation_map_is_built_once_per_run_and_handed_to_every_leg() {
    let source: String = include_str!("../../push.rs")
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .flat_map(|line| line.chars().filter(|c| !c.is_whitespace()))
        .collect();

    assert_eq!(
        source.matches("build_annotations(").count(),
        2,
        "one map per run path; a per-leg build re-stamps `image.created`"
    );
    assert!(
        source.contains("invoke_push(&spec,platform_str,&target_ref,bundle_path,&annotations,cascade,sign.as_ref()"),
        "the archive loop passes the run's map into every leg"
    );
}
