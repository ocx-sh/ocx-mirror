// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;

#[test]
fn reject_missing_name() {
    let yaml = r#"
target:
  registry: ocx.sh
  repository: cmake
source:
  type: github_release
  owner: Kitware
  repo: CMake
assets:
  linux/amd64:
    - "cmake-.*\\.tar\\.gz"
"#;

    let result: Result<MirrorSpec, _> = serde_yaml_ng::from_str(yaml);
    assert!(result.is_err());
}

#[test]
fn reject_missing_target() {
    let yaml = r#"
name: cmake
source:
  type: github_release
  owner: Kitware
  repo: CMake
assets:
  linux/amd64:
    - "cmake-.*\\.tar\\.gz"
"#;

    let result: Result<MirrorSpec, _> = serde_yaml_ng::from_str(yaml);
    assert!(result.is_err());
}

#[test]
fn validate_tag_pattern_without_version_group() {
    let yaml = r#"
name: cmake
target:
  registry: ocx.sh
  repository: cmake
source:
  type: github_release
  owner: Kitware
  repo: CMake
  tag_pattern: "^v(\\d+\\.\\d+\\.\\d+)$"
assets:
  linux/amd64:
    - "cmake-.*\\.tar\\.gz"
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    let errors = spec.validate(Path::new("test.yaml"));
    assert!(
        errors.iter().any(|e| e.contains("version")),
        "Expected version group error, got: {errors:?}"
    );
}

#[test]
fn validate_invalid_regex_in_assets() {
    let yaml = r#"
name: cmake
target:
  registry: ocx.sh
  repository: cmake
source:
  type: github_release
  owner: Kitware
  repo: CMake
assets:
  linux/amd64:
    - "[invalid"
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    let errors = spec.validate(Path::new("test.yaml"));
    assert!(
        errors.iter().any(|e| e.contains("regex")),
        "Expected regex error, got: {errors:?}"
    );
}

#[test]
fn reject_url_index_with_neither_url_nor_versions_nor_generator() {
    let yaml = r#"
name: test
target:
  registry: localhost:5000
  repository: test
source:
  type: url_index
assets:
  linux/amd64:
    - "test\\.tar\\.gz"
"#;

    let result: Result<MirrorSpec, _> = serde_yaml_ng::from_str(yaml);
    assert!(result.is_err(), "Expected parse error for empty url_index");
}

#[test]
fn parse_url_index_generator_spec() {
    let yaml = r#"
name: nodejs
target:
  registry: ocx.sh
  repository: nodejs
source:
  type: url_index
  generator:
    command: ["uv", "run", "generate.py"]
    working_directory: scripts
assets:
  linux/amd64:
    - "node-.*-linux-x64\\.tar\\.xz"
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    if let Source::UrlIndex(UrlIndexSource::Generator { generator }) = &spec.source {
        assert_eq!(generator.command, vec!["uv", "run", "generate.py"]);
        assert_eq!(generator.working_directory.as_deref(), Some("scripts"));
    } else {
        panic!("Expected UrlIndex Generator source, got: {:?}", spec.source);
    }
}

#[test]
fn parse_url_index_generator_default_working_directory() {
    let yaml = r#"
name: nodejs
target:
  registry: ocx.sh
  repository: nodejs
source:
  type: url_index
  generator:
    command: ["uv", "run", "generate.py"]
assets:
  linux/amd64:
    - "node-.*-linux-x64\\.tar\\.xz"
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    if let Source::UrlIndex(UrlIndexSource::Generator { generator }) = &spec.source {
        assert!(generator.working_directory.is_none());
        let resolved = generator.resolve_working_directory(Path::new("/mirrors/nodejs"));
        assert_eq!(resolved, Path::new("/mirrors/nodejs"));
    } else {
        panic!("Expected UrlIndex Generator source, got: {:?}", spec.source);
    }
}

#[test]
fn validate_generator_empty_command() {
    let yaml = r#"
name: test
target:
  registry: localhost:5000
  repository: test
source:
  type: url_index
  generator:
    command: []
assets:
  linux/amd64:
    - "test\\.tar\\.gz"
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    let errors = spec.validate(Path::new("test.yaml"));
    assert!(
        errors.iter().any(|e| e.contains("non-empty")),
        "Expected empty command error, got: {errors:?}"
    );
}

#[test]
fn default_values() {
    let yaml = r#"
name: minimal
target:
  registry: ocx.sh
  repository: minimal
source:
  type: github_release
  owner: test
  repo: test
assets:
  linux/amd64:
    - "test\\.tar\\.gz"
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(spec.build_timestamp, BuildTimestampFormat::Datetime);
    assert!(spec.cascade.enabled);
    assert!(!spec.skip_prereleases);
    assert!(spec.asset_type.is_none(), "asset_type should default to None");
    assert_eq!(spec.concurrency.max_downloads, 8);
    assert_eq!(spec.concurrency.rate_limit_ms, 0);
    assert_eq!(spec.concurrency.max_retries, 3);
    assert!(!spec.allow_manual_edits, "allow_manual_edits should default to false");
}

#[test]
fn a_spec_that_still_sets_max_pushes_keeps_parsing() {
    // `max_pushes` was removed as a knob nothing read. Every mirror repo in
    // the fleet carries its own `mirror.yml`, so the field outliving the
    // code that named it must stay harmless — which it is only as long as
    // `ConcurrencyConfig` does not deny unknown fields. This pins that.
    let yaml = r#"
name: minimal
target:
  registry: ocx.sh
  repository: minimal
source:
  type: github_release
  owner: test
  repo: test
assets:
  linux/amd64:
    - "test\\.tar\\.gz"
concurrency:
  max_pushes: 4
  max_retries: 5
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).expect("a stale `max_pushes` must not break a mirror");
    assert_eq!(spec.concurrency.max_retries, 5);
}

#[test]
fn parse_allow_manual_edits_true() {
    let yaml = r#"
name: minimal
target:
  registry: ocx.sh
  repository: minimal
source:
  type: github_release
  owner: test
  repo: test
assets:
  linux/amd64:
    - "test\\.tar\\.gz"
allow_manual_edits: true
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(spec.allow_manual_edits, "allow_manual_edits: true must parse");
}

#[test]
fn default_verify_values() {
    let yaml = r#"
name: test
target:
  registry: ocx.sh
  repository: test
source:
  type: github_release
  owner: test
  repo: test
assets:
  linux/amd64:
    - "test\\.tar\\.gz"
verify:
  github_asset_digest: false
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    let verify = spec.verify.unwrap();
    assert!(!verify.github_asset_digest);
    assert!(verify.checksums_file.is_none());
}

#[test]
fn parse_asset_type_archive() {
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
    - "cmake-.*\\.tar\\.gz"
asset_type:
  type: archive
  strip_components: 1
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    match spec.asset_type.as_ref().unwrap().resolve("linux/amd64") {
        asset_type::AssetType::Archive { strip_components } => assert_eq!(strip_components, Some(1)),
        _ => panic!("expected Archive"),
    }
}

#[test]
fn parse_asset_type_archive_per_platform() {
    let yaml = r#"
name: shellcheck
target:
  registry: ocx.sh
  repository: shellcheck
source:
  type: github_release
  owner: koalaman
  repo: shellcheck
  tag_pattern: "^v(?P<version>\\d+\\.\\d+\\.\\d+)$"
assets:
  linux/amd64:
    - "shellcheck-.*\\.tar\\.xz"
asset_type:
  type: archive
  strip_components:
    default: 1
    platforms:
      windows/amd64: 0
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    let at = spec.asset_type.as_ref().unwrap();
    match at.resolve("linux/amd64") {
        asset_type::AssetType::Archive { strip_components } => assert_eq!(strip_components, Some(1)),
        _ => panic!("expected Archive"),
    }
    match at.resolve("windows/amd64") {
        asset_type::AssetType::Archive { strip_components } => assert_eq!(strip_components, Some(0)),
        _ => panic!("expected Archive"),
    }
}

#[test]
fn parse_asset_type_binary() {
    let yaml = r#"
name: shfmt
target:
  registry: ocx.sh
  repository: shfmt
source:
  type: github_release
  owner: mvdan
  repo: sh
  tag_pattern: "^v(?P<version>\\d+\\.\\d+\\.\\d+)$"
assets:
  linux/amd64:
    - "shfmt_v.*_linux_amd64$"
asset_type:
  type: binary
  name: shfmt
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    match spec.asset_type.as_ref().unwrap().resolve("linux/amd64") {
        asset_type::AssetType::Binary { name } => assert_eq!(name, "shfmt"),
        _ => panic!("expected Binary"),
    }
}

#[test]
fn reject_url_index_with_both_url_and_versions() {
    let yaml = r#"
name: test
target:
  registry: localhost:5000
  repository: test
source:
  type: url_index
  url: "https://example.com/versions.json"
  versions:
    "1.0.0":
      assets:
        test.tar.gz: "https://example.com/test.tar.gz"
assets:
  linux/amd64:
    - "test\\.tar\\.gz"
"#;

    let result: Result<MirrorSpec, _> = serde_yaml_ng::from_str(yaml);
    assert!(
        result.is_err(),
        "Expected parse error for url_index with both url and versions"
    );
    let err = result.unwrap_err().to_string();
    assert!(err.contains("exactly one"), "Expected 'exactly one' error, got: {err}");
}

#[test]
fn reject_url_index_with_both_url_and_generator() {
    let yaml = r#"
name: test
target:
  registry: localhost:5000
  repository: test
source:
  type: url_index
  url: "https://example.com/versions.json"
  generator:
    command: ["echo", "{}"]
assets:
  linux/amd64:
    - "test\\.tar\\.gz"
"#;

    let result: Result<MirrorSpec, _> = serde_yaml_ng::from_str(yaml);
    assert!(
        result.is_err(),
        "Expected parse error for url_index with both url and generator"
    );
    let err = result.unwrap_err().to_string();
    assert!(err.contains("exactly one"), "Expected 'exactly one' error, got: {err}");
}

#[test]
fn reject_unknown_source_type() {
    let yaml = r#"
name: test
target:
  registry: ocx.sh
  repository: test
source:
  type: unknown_source
  owner: test
  repo: test
assets:
  linux/amd64:
    - "test\\.tar\\.gz"
"#;

    let result: Result<MirrorSpec, _> = serde_yaml_ng::from_str(yaml);
    assert!(result.is_err());
}
