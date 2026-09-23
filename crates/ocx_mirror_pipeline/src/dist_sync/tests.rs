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

/// The default `publish.dist` reproduces the fixed tree earlier releases
/// wrote: `OCX_INSTALL_DIST_URL` is set once per consumer and must not move
/// when the mirror is upgraded.
#[test]
fn the_default_manifest_paths_are_the_ones_consumers_already_configure() {
    let spec: DistSpec = serde_yaml_ng::from_str(
        r"
output: ./public
publish:
  base_url: https://art.test/ocx-dist
",
    )
    .expect("the fixture must deserialize");

    let docs = spec.publish.dist_docs();
    assert_eq!(docs.path, "dist.json");
    assert_eq!(
        SnapshotTemplate::parse(&docs.snapshots)
            .expect("the default must parse")
            .expand("abc"),
        "dist/abc.json"
    );
    assert!(docs.upload_path && docs.upload_snapshots);
}
