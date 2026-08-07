// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;

/// Parse a spec whose only varying lines are `cascade` / `build_timestamp`,
/// for exercising [`MirrorSpec::cascade_without_build_stamp`].
fn spec_with(cascade: &str, build_timestamp: &str) -> MirrorSpec {
    let yaml = format!(
        r#"
name: gctest
target:
  registry: ocx.sh
  repository: gctest
source:
  type: github_release
  owner: o
  repo: r
  tag_pattern: "^v(?P<version>\\d+\\.\\d+\\.\\d+)$"
assets:
  linux/amd64:
    - "x\\.tar\\.gz"
cascade: {cascade}
build_timestamp: {build_timestamp}
"#
    );
    serde_yaml_ng::from_str(&yaml).unwrap()
}

#[test]
fn cascade_without_build_stamp_flags_only_none_plus_cascade() {
    // The GC-unsafe combination: re-pointable cascade tags, no unique stamp.
    assert!(spec_with("true", "none").cascade_without_build_stamp());

    // A retained per-build tag keeps every digest reachable — safe.
    assert!(!spec_with("true", "date").cascade_without_build_stamp());
    assert!(!spec_with("true", "datetime").cascade_without_build_stamp());

    // No cascade means no rolling tag to re-point — safe even with `none`.
    assert!(!spec_with("false", "none").cascade_without_build_stamp());
}

#[test]
fn parse_github_release_spec() {
    let yaml = r#"
name: cmake
target:
  registry: ocx.sh
  repository: cmake
source:
  type: github_release
  owner: Kitware
  repo: CMake
  tag_pattern: "^v(?P<version>\\d+\\.\\d+\\.\\d+)$"
assets:
  linux/amd64:
    - "cmake-.*-linux-x86_64\\.tar\\.gz"
  linux/arm64:
    - "cmake-.*-linux-aarch64\\.tar\\.gz"
  darwin/amd64:
    - "cmake-.*-macos-universal\\.tar\\.gz"
  darwin/arm64:
    - "cmake-.*-macos-universal\\.tar\\.gz"
metadata:
  default: metadata/cmake.json
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(spec.name, "cmake");
    assert_eq!(spec.target.registry, "ocx.sh");
    assert_eq!(spec.target.repository, "cmake");
    assert!(matches!(spec.source, Source::GithubRelease { .. }));
    assert_eq!(spec.build_timestamp, BuildTimestampFormat::Datetime);
    assert!(spec.cascade.enabled);
    assert!(!spec.skip_prereleases);
}

#[test]
fn parse_url_index_inline_spec() {
    let yaml = r#"
name: test-tool
target:
  registry: localhost:5000
  repository: test-tool
source:
  type: url_index
  versions:
    "1.0.0":
      assets:
        test-tool-1.0.0-linux-amd64.tar.gz: "https://example.com/test-tool-1.0.0-linux-amd64.tar.gz"
    "1.1.0":
      prerelease: true
      assets:
        test-tool-1.1.0-linux-amd64.tar.gz: "https://example.com/test-tool-1.1.0-linux-amd64.tar.gz"
assets:
  linux/amd64:
    - "test-tool-.*-linux-amd64\\.tar\\.gz"
build_timestamp: date
cascade: false
skip_prereleases: true
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(spec.name, "test-tool");
    assert_eq!(spec.build_timestamp, BuildTimestampFormat::Date);
    assert!(!spec.cascade.enabled);
    assert!(spec.skip_prereleases);

    if let Source::UrlIndex(UrlIndexSource::Inline { versions }) = &spec.source {
        assert_eq!(versions.len(), 2);
        assert!(versions["1.1.0"].prerelease);
    } else {
        panic!("Expected UrlIndex Inline source, got: {:?}", spec.source);
    }
}

#[test]
fn parse_url_index_remote_spec() {
    let yaml = r#"
name: test-tool
target:
  registry: localhost:5000
  repository: test-tool
source:
  type: url_index
  url: "https://example.com/versions.json"
assets:
  linux/amd64:
    - "test-tool-.*-linux-amd64\\.tar\\.gz"
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    if let Source::UrlIndex(UrlIndexSource::Remote { url }) = &spec.source {
        assert_eq!(url, "https://example.com/versions.json");
    } else {
        panic!("Expected UrlIndex Remote source, got: {:?}", spec.source);
    }
}
