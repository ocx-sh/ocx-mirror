// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::*;

fn composed(base: &str, relative: &str) -> String {
    mirrored_url(&url::Url::parse(base).expect("test base URL must parse"), relative)
        .expect("composition must succeed")
        .to_string()
}

#[test]
fn a_mirrored_url_keeps_the_path_the_base_carries() {
    assert_eq!(
        composed("https://art.test/artifactory/ocx-dist", "v0.5.8/ocx.tar.gz"),
        "https://art.test/artifactory/ocx-dist/v0.5.8/ocx.tar.gz"
    );
}

#[test]
fn a_trailing_slash_on_the_base_does_not_double_the_separator() {
    assert_eq!(
        composed("https://art.test/ocx-dist/", "v0.5.8/ocx.tar.gz"),
        "https://art.test/ocx-dist/v0.5.8/ocx.tar.gz"
    );
}

/// The GitLab generic package shape, which is the reason `url` is rewritten at
/// mirror time rather than composed by each consumer.
#[test]
fn a_gitlab_generic_package_base_composes_into_a_reachable_url() {
    assert_eq!(
        composed(
            "https://gitlab.test/api/v4/projects/42/packages/generic/ocx",
            "0.5.8/ocx-x86_64-unknown-linux-gnu.tar.gz"
        ),
        "https://gitlab.test/api/v4/projects/42/packages/generic/ocx/0.5.8/ocx-x86_64-unknown-linux-gnu.tar.gz"
    );
}

#[test]
fn the_manifest_file_names_are_the_ones_consumers_configure() {
    // These are fixed rather than templated: `OCX_INSTALL_DIST_URL` is set
    // once per consumer and must not move when `publish.layout` changes.
    assert_eq!(MANIFEST_NAME, "dist.json");
    assert_eq!(MANIFEST_SIDECAR, "dist.json.sha256");
    assert_eq!(SNAPSHOT_DIR, "dist");
}
