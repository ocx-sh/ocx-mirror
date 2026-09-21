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
    if let Source::UrlIndex(UrlIndexSource {
        mode: UrlIndexMode::Generator { generator },
        ..
    }) = &spec.source
    {
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
    if let Source::UrlIndex(UrlIndexSource {
        mode: UrlIndexMode::Generator { generator },
        ..
    }) = &spec.source
    {
        assert!(generator.working_directory.is_none());
        let resolved = generator.resolve_working_directory(Path::new("/mirrors/nodejs"));
        assert_eq!(resolved, Path::new("/mirrors/nodejs"));
    } else {
        panic!("Expected UrlIndex Generator source, got: {:?}", spec.source);
    }
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

/// A `github_release` spec whose only varying lines are the `verify:` block.
fn spec_with_verify(verify: &str) -> Result<MirrorSpec, serde_yaml_ng::Error> {
    serde_yaml_ng::from_str(&format!(
        r#"
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
{verify}
"#
    ))
}

fn verify_of(verify: &str) -> VerifyConfig {
    spec_with_verify(verify)
        .expect("spec must parse")
        .verify
        .expect("verify:")
}

#[test]
fn default_verify_values() {
    let verify = verify_of("verify:\n  github_asset_digest: false");
    assert_eq!(verify.github_asset_digest, DigestPolicy::Off);
    assert!(verify.checksums_file.is_none());
}

#[test]
fn verify_digest_policy_defaults_to_if_present() {
    // No `verify:` at all, and a `verify:` that names only the other axis:
    // both resolve to the same default, which is what `digest_policy` leans on.
    let spec = spec_with_verify("").expect("spec must parse");
    assert!(spec.verify.is_none());
    assert_eq!(spec.digest_policy(), DigestPolicy::IfPresent);

    let verify = verify_of("verify:\n  checksums_file: https://example.com/SHA256SUMS");
    assert_eq!(verify.github_asset_digest, DigestPolicy::IfPresent);
    assert_eq!(verify.url_index_digest, DigestPolicy::IfPresent);
}

#[test]
fn verify_github_asset_digest_bool_still_parses() {
    // 98 contrib specs are on the bool spelling; `true`/`false` must keep
    // their exact pre-#76 meaning or the fleet changes behaviour on upgrade.
    assert_eq!(
        verify_of("verify:\n  github_asset_digest: true").github_asset_digest,
        DigestPolicy::IfPresent
    );
    assert_eq!(
        verify_of("verify:\n  github_asset_digest: false").github_asset_digest,
        DigestPolicy::Off
    );
}

#[test]
fn verify_url_index_digest_require_parses() {
    let verify = verify_of("verify:\n  url_index_digest: require");
    assert_eq!(verify.url_index_digest, DigestPolicy::Require);
    assert_eq!(
        verify.github_asset_digest,
        DigestPolicy::IfPresent,
        "axes are independent"
    );
}

#[test]
fn verify_rejects_unknown_key() {
    // A `sha265_file:` in a committed spec would otherwise verify nothing,
    // silently, which is the failure this block exists to prevent.
    let err = spec_with_verify("verify:\n  sha265_file: x")
        .expect_err("an unknown verify key must be refused")
        .to_string();
    assert!(err.contains("unknown field `sha265_file`"), "{err}");
    assert!(err.contains("checksums_file"), "{err}");
}

#[test]
fn verify_rejects_unknown_policy_name() {
    let err = spec_with_verify("verify:\n  url_index_digest: required")
        .expect_err("a misspelled policy must be refused")
        .to_string();
    assert!(err.contains("unknown variant `required`"), "{err}");
    assert!(err.contains("require"), "{err}");
}

#[test]
fn digest_policy_is_off_for_env_sources() {
    // Wheels verify against the lock's own hashes in `python_prepare`; a
    // second digest path there would be a lie, `VersionInfo.assets` being
    // empty by construction.
    let spec: MirrorSpec = serde_yaml_ng::from_str(
        r#"
name: acme-app
target:
  registry: ocx.sh
  repository: acme-app
source:
  type: pylock
  path: pylock.toml
python:
  version: "3.12.8"
  abi: cp312
  interpreter_package: ocx.sh/python
wheels:
  linux/amd64: ~
verify:
  github_asset_digest: require
  url_index_digest: require
"#,
    )
    .expect("spec must parse");
    assert_eq!(spec.digest_policy(), DigestPolicy::Off);
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

// ── C-050: the `sign:` block reaches `MirrorSpec.sign` ───────────────────

/// The block is optional and reaches the spec in both tag forms. The shape
/// corpus lives beside `sign_config.rs`; what this asserts is only the field
/// wiring — that a `sign:` in a real document is not silently dropped.
#[test]
fn parse_sign_block_in_both_tag_forms() {
    let base = r#"
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
"#;

    let keyless: MirrorSpec = serde_yaml_ng::from_str(&format!("{base}sign:\n  keyless: {{}}\n"))
        .expect("a keyless `sign:` block must parse");
    assert!(matches!(
        keyless.sign,
        Some(SignConfig {
            keyless: Some(_),
            key: None
        })
    ));

    let key: MirrorSpec = serde_yaml_ng::from_str(&format!("{base}sign:\n  key: env://MIRROR_SIGNING_KEY\n"))
        .expect("a key-mode `sign:` block must parse");
    assert!(matches!(
        key.sign,
        Some(SignConfig {
            keyless: None,
            key: Some(KeyConfig::Reference(Ref::Env(_)))
        })
    ));

    let unsigned: MirrorSpec = serde_yaml_ng::from_str(base).expect("a spec without `sign:` must parse");
    assert!(unsigned.sign.is_none(), "an absent `sign:` must stay absent");
}

// ── issue #86: per-platform override keys are platform keys ─────────────

/// The base spec with an `asset_type` per-platform map whose override key is
/// `key`, so one test body covers a good key and a nonsense one.
fn spec_with_asset_type_override(key: &str) -> MirrorSpec {
    let yaml = format!(
        r#"
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
  default:
    type: binary
    name: shfmt
  platforms:
    "{key}":
      type: binary
      name: shfmt.exe
"#
    );
    serde_yaml_ng::from_str(&yaml).expect("the nested form parses; only the key is under test")
}

#[test]
fn a_non_platform_key_in_the_asset_type_map_is_an_error() {
    // The map is looked up by exact string equality, so a key that is not a
    // platform matches nothing and falls through to `default`. The spec says
    // one thing and the bundle contains another, silently.
    let errors = spec_with_asset_type_override("totally/bogus").validate(std::path::Path::new("mirror.yml"));
    assert!(
        errors
            .iter()
            .any(|error| error.contains("asset_type.platforms") && error.contains("totally/bogus")),
        "expected an asset_type.platforms key error, got: {errors:?}"
    );
}

#[test]
fn a_real_platform_key_in_the_asset_type_map_is_accepted() {
    let errors = spec_with_asset_type_override("windows/amd64").validate(std::path::Path::new("mirror.yml"));
    assert!(
        !errors.iter().any(|error| error.contains("asset_type.platforms")),
        "a platform key must pass: {errors:?}"
    );
    // The libc-qualified grammar is the same one `assets:` uses.
    let errors = spec_with_asset_type_override("linux/amd64+libc.musl").validate(std::path::Path::new("mirror.yml"));
    assert!(
        !errors.iter().any(|error| error.contains("asset_type.platforms")),
        "a libc-qualified key must pass: {errors:?}"
    );
}

#[test]
fn a_non_platform_key_in_the_metadata_map_is_an_error() {
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
metadata:
  default: metadata.json
  platforms:
    win/amd64: metadata-windows.json
"#;
    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    let errors = spec.validate(std::path::Path::new("mirror.yml"));
    assert!(
        errors
            .iter()
            .any(|error| error.contains("metadata.platforms") && error.contains("win/amd64")),
        "expected a metadata.platforms key error, got: {errors:?}"
    );
}

#[test]
fn a_flat_metadata_map_is_a_spec_load_failure() {
    // The same silent drop `asset_type` had: without `platforms:`, the
    // override landed in no declared field and every platform published the
    // default file.
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
metadata:
  default: metadata.json
  windows/amd64: metadata-windows.json
"#;
    let error = serde_yaml_ng::from_str::<MirrorSpec>(yaml).expect_err("the flat metadata form must be refused");
    assert!(
        error.to_string().contains("windows/amd64"),
        "the message must name the stray key: {error}"
    );
}

/// The second per-platform map, one level further down inside an `archive`
/// entry's `strip_components:`.
fn spec_with_strip_components_override(key: &str) -> MirrorSpec {
    let yaml = format!(
        r#"
name: tool
target:
  registry: ocx.sh
  repository: tool
source:
  type: url_index
  url: "http://127.0.0.1:1/index.json"
assets:
  linux/amd64:
    - "tool-linux-amd64$"
asset_type:
  default:
    type: archive
    strip_components:
      default: 1
      platforms:
        {key}: 0
"#
    );
    serde_yaml_ng::from_str(&yaml).expect("the spec parses; only the key is under test")
}

#[test]
fn a_non_platform_key_in_the_nested_strip_components_map_is_an_error() {
    // Issue #86's other half, in the map it was first skipped for: the keys
    // *inside* `platforms:` are a `HashMap<String, u8>` that takes anything,
    // which `deny_unknown_fields` on the block around it never touched. A
    // typo'd `windows/amd65` strips the default instead of 0, in silence.
    let errors = spec_with_strip_components_override("windows/amd65").validate(std::path::Path::new("mirror.yml"));
    assert!(
        errors
            .iter()
            .any(|error| error.contains("asset_type.strip_components.platforms") && error.contains("windows/amd65")),
        "expected a nested strip_components key error, got: {errors:?}"
    );
}

#[test]
fn a_real_platform_key_in_the_nested_strip_components_map_is_accepted() {
    let errors = spec_with_strip_components_override("windows/amd64").validate(std::path::Path::new("mirror.yml"));
    assert!(
        !errors.iter().any(|error| error.contains("strip_components")),
        "a platform key must pass: {errors:?}"
    );
}
