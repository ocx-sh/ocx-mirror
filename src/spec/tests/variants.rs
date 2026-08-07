// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;

// -- variant tests --

#[test]
fn parse_spec_with_variants() {
    let yaml = r#"
name: python
target:
  registry: ocx.sh
  repository: python
source:
  type: github_release
  owner: astral-sh
  repo: python-build-standalone
  tag_pattern: "^(?P<version>\\d+\\.\\d+\\.\\d+)\\+\\d+$"
variants:
  - name: pgo.lto
    default: true
    assets:
      linux/amd64:
        - "cpython-.*-x86_64-.*-pgo\\+lto-.*\\.tar\\.zst"
      darwin/arm64:
        - "cpython-.*-aarch64-apple-darwin-pgo\\+lto-.*\\.tar\\.zst"
  - name: debug
    assets:
      linux/amd64:
        - "cpython-.*-x86_64-.*-debug-.*\\.tar\\.zst"
metadata:
  default: metadata/python.json
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(spec.name, "python");
    assert!(spec.assets.is_none(), "top-level assets should be None");
    let variants = spec.variants.as_ref().unwrap();
    assert_eq!(variants.len(), 2);
    assert_eq!(variants[0].name.as_deref(), Some("pgo.lto"));
    assert!(variants[0].default);
    assert_eq!(variants[1].name.as_deref(), Some("debug"));
    assert!(!variants[1].default);
}

#[test]
fn parse_spec_without_variants_backward_compat() {
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
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(spec.assets.is_some());
    assert!(spec.variants.is_none());
    let errors = spec.validate(Path::new("test.yaml"));
    assert!(errors.is_empty(), "backward-compat spec should validate: {errors:?}");
}

#[test]
fn validate_reject_both_assets_and_variants() {
    let yaml = r#"
name: test
target:
  registry: ocx.sh
  repository: test
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
assets:
  linux/amd64:
    - "test\\.tar\\.gz"
variants:
  - name: debug
    default: true
    assets:
      linux/amd64:
        - "test-debug\\.tar\\.gz"
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    let errors = spec.validate(Path::new("test.yaml"));
    assert!(
        errors.iter().any(|e| e.contains("cannot specify both")),
        "Expected mutual exclusivity error, got: {errors:?}"
    );
}

#[test]
fn validate_reject_neither_assets_nor_variants() {
    let yaml = r#"
name: test
target:
  registry: ocx.sh
  repository: test
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    let errors = spec.validate(Path::new("test.yaml"));
    assert!(
        errors.iter().any(|e| e.contains("must specify either")),
        "Expected missing assets/variants error, got: {errors:?}"
    );
}

#[test]
fn validate_variant_exactly_one_default() {
    let yaml = r#"
name: test
target:
  registry: ocx.sh
  repository: test
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
variants:
  - name: debug
    assets:
      linux/amd64:
        - "test-debug\\.tar\\.gz"
  - name: release
    assets:
      linux/amd64:
        - "test-release\\.tar\\.gz"
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    let errors = spec.validate(Path::new("test.yaml"));
    assert!(
        errors.iter().any(|e| e.contains("exactly one variant must be default")),
        "Expected default count error, got: {errors:?}"
    );
}

#[test]
fn validate_variant_two_defaults() {
    let yaml = r#"
name: test
target:
  registry: ocx.sh
  repository: test
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
variants:
  - name: debug
    default: true
    assets:
      linux/amd64:
        - "test-debug\\.tar\\.gz"
  - name: release
    default: true
    assets:
      linux/amd64:
        - "test-release\\.tar\\.gz"
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    let errors = spec.validate(Path::new("test.yaml"));
    assert!(
        errors
            .iter()
            .any(|e| e.contains("exactly one variant must be default, found 2")),
        "Expected two-default error, got: {errors:?}"
    );
}

#[test]
fn validate_variant_invalid_name() {
    let yaml = r#"
name: test
target:
  registry: ocx.sh
  repository: test
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
variants:
  - name: Debug-Build
    default: true
    assets:
      linux/amd64:
        - "test\\.tar\\.gz"
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    let errors = spec.validate(Path::new("test.yaml"));
    assert!(
        errors.iter().any(|e| e.contains("invalid name")),
        "Expected invalid name error, got: {errors:?}"
    );
}

#[test]
fn validate_variant_latest_reserved() {
    let yaml = r#"
name: test
target:
  registry: ocx.sh
  repository: test
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
variants:
  - name: latest
    default: true
    assets:
      linux/amd64:
        - "test\\.tar\\.gz"
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    let errors = spec.validate(Path::new("test.yaml"));
    assert!(
        errors.iter().any(|e| e.contains("reserved")),
        "Expected reserved name error, got: {errors:?}"
    );
}

#[test]
fn validate_variant_duplicate_names() {
    let yaml = r#"
name: test
target:
  registry: ocx.sh
  repository: test
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
variants:
  - name: debug
    default: true
    assets:
      linux/amd64:
        - "test\\.tar\\.gz"
  - name: debug
    assets:
      linux/amd64:
        - "test2\\.tar\\.gz"
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    let errors = spec.validate(Path::new("test.yaml"));
    assert!(
        errors.iter().any(|e| e.contains("duplicate")),
        "Expected duplicate name error, got: {errors:?}"
    );
}

#[test]
fn effective_variants_without_variants_key() {
    let yaml = r#"
name: cmake
target:
  registry: ocx.sh
  repository: cmake
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
assets:
  linux/amd64:
    - "cmake-.*\\.tar\\.gz"
metadata:
  default: metadata/cmake.json
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    let variants = spec.effective_variants();
    assert_eq!(variants.len(), 1);
    assert!(variants[0].name.is_none());
    assert!(variants[0].is_default);
    assert!(variants[0].metadata.is_some());
}

#[test]
fn effective_variants_unnamed_default_with_named_variant() {
    let yaml = r#"
name: cpython
target:
  registry: ocx.sh
  repository: cpython
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
variants:
  - default: true
    assets:
      linux/amd64:
        - "install_only\\.tar\\.gz"
  - name: slim
    assets:
      linux/amd64:
        - "install_only_stripped\\.tar\\.gz"
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    let errors = spec.validate(Path::new("test.yaml"));
    assert!(errors.is_empty(), "Expected no errors, got: {errors:?}");

    let variants = spec.effective_variants();
    assert_eq!(variants.len(), 2);

    assert!(variants[0].name.is_none());
    assert!(variants[0].is_default);

    assert_eq!(variants[1].name.as_deref(), Some("slim"));
    assert!(!variants[1].is_default);
}

#[test]
fn validate_variant_unnamed_non_default_rejected() {
    let yaml = r#"
name: test
target:
  registry: ocx.sh
  repository: test
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
variants:
  - name: release
    default: true
    assets:
      linux/amd64:
        - "release\\.tar\\.gz"
  - assets:
      linux/amd64:
        - "other\\.tar\\.gz"
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    let errors = spec.validate(Path::new("test.yaml"));
    assert!(
        errors.iter().any(|e| e.contains("unnamed variant must be the default")),
        "Expected unnamed-must-be-default error, got: {errors:?}"
    );
}

#[test]
fn effective_variants_with_variants_key() {
    let yaml = r#"
name: python
target:
  registry: ocx.sh
  repository: python
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
variants:
  - name: pgo.lto
    default: true
    assets:
      linux/amd64:
        - "pgo-lto-.*\\.tar\\.gz"
  - name: debug
    assets:
      linux/amd64:
        - "debug-.*\\.tar\\.gz"
metadata:
  default: metadata/python.json
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    let variants = spec.effective_variants();
    assert_eq!(variants.len(), 2);

    assert_eq!(variants[0].name.as_deref(), Some("pgo.lto"));
    assert!(variants[0].is_default);
    // Inherits top-level metadata
    assert!(variants[0].metadata.is_some());

    assert_eq!(variants[1].name.as_deref(), Some("debug"));
    assert!(!variants[1].is_default);
    // Also inherits top-level metadata
    assert!(variants[1].metadata.is_some());
}

#[test]
fn effective_variants_variant_overrides_metadata() {
    let yaml = r#"
name: python
target:
  registry: ocx.sh
  repository: python
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
variants:
  - name: pgo.lto
    default: true
    assets:
      linux/amd64:
        - "pgo-lto-.*\\.tar\\.gz"
    metadata:
      default: metadata/python-pgo.json
  - name: debug
    assets:
      linux/amd64:
        - "debug-.*\\.tar\\.gz"
metadata:
  default: metadata/python.json
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    let variants = spec.effective_variants();

    // pgo.lto overrides metadata
    let pgo = &variants[0];
    assert!(pgo.metadata.is_some());

    // debug inherits top-level metadata
    let debug = &variants[1];
    assert!(debug.metadata.is_some());
}

/// `bin_scan` follows the same override-with-fallback rule as `metadata`
/// and `asset_type`: a slim variant ships a different binary set than the
/// full one, so it may need a different mode than the spec's — and a
/// variant that says nothing must inherit rather than silently reset to
/// `off`.
#[test]
fn effective_variants_bin_scan_overrides_per_variant_and_falls_back() {
    let yaml = r#"
name: python
target:
  registry: ocx.sh
  repository: python
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
bin_scan: verify
variants:
  - default: true
    assets:
      linux/amd64:
        - "full-.*\\.tar\\.gz"
  - name: slim
    bin_scan: auto
    assets:
      linux/amd64:
        - "slim-.*\\.tar\\.gz"
  - name: legacy
    bin_scan: off
    assets:
      linux/amd64:
        - "legacy-.*\\.tar\\.gz"
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).expect("spec parses");
    let variants = spec.effective_variants();

    assert_eq!(
        variants[0].bin_scan,
        BinScanMode::Verify,
        "a variant that states no mode inherits the spec's",
    );
    assert_eq!(variants[1].bin_scan, BinScanMode::Auto, "a variant may override it");
    assert_eq!(
        variants[2].bin_scan,
        BinScanMode::Off,
        "including overriding it back off — `off` must not read as 'unset'",
    );
}

/// A `bin_scan` on a *variant* must be gated even when the spec-level mode
/// is `off` — checking only `self.bin_scan` lets exactly the interesting
/// case through, and the slim variant then publishes `binaries: []`.
///
/// The default variant in the same spec keeps a bare `${installPath}` and
/// must stay silent, or the gate is just rejecting the file globally.
#[test]
fn a_scanning_variant_is_gated_even_when_the_spec_level_mode_is_off() {
    let dir = tempfile::TempDir::new().unwrap();
    for (file, value) in [("metadata.json", "${installPath}"), ("slim.json", "${installPath}")] {
        std::fs::write(
            dir.path().join(file),
            format!(
                r#"{{"type":"bundle","version":1,"env":[
                       {{"key":"PATH","type":"path","required":true,"value":"{value}","visibility":"public"}}]}}"#
            ),
        )
        .unwrap();
    }

    let yaml = r#"
name: tool
target:
  registry: ocx.sh
  repository: tool
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
metadata:
  default: metadata.json
variants:
  - default: true
    assets:
      linux/amd64: ["full-.*\\.tar\\.gz"]
  - name: slim
    bin_scan: auto
    metadata:
      default: slim.json
    assets:
      linux/amd64: ["slim-.*\\.tar\\.gz"]
"#;
    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).expect("spec parses");
    assert_eq!(spec.bin_scan, BinScanMode::Off, "the spec level must stay off");

    let errors = spec.validate(&dir.path().join("mirror.yml"));
    assert_eq!(
        errors.len(),
        1,
        "exactly the scanning variant must be reported: {errors:?}"
    );
    assert!(
        errors[0].starts_with("variants.slim.bin_scan:") && errors[0].contains("slim.json"),
        "the error must name the variant and its file: {}",
        errors[0],
    );
}

/// The control: the same unscannable metadata with the scan off is a
/// perfectly good spec and must keep loading. Without this the gate could
/// be rejecting on the metadata shape alone and nobody would notice.
#[test]
fn a_bare_install_path_var_loads_fine_when_the_scan_is_off() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("metadata.json"),
        r#"{"type":"bundle","version":1,"env":[
               {"key":"PATH","type":"path","required":true,"value":"${installPath}","visibility":"public"}]}"#,
    )
    .unwrap();

    let yaml = r#"
name: shfmt
target:
  registry: ocx.sh
  repository: shfmt
source:
  type: github_release
  owner: mvdan
  repo: sh
  tag_pattern: "^v(?P<version>\\d+)$"
metadata:
  default: metadata.json
asset_type:
  type: binary
  name: shfmt
assets:
  linux/amd64:
    - "shfmt_.*_linux_amd64$"
"#;
    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).expect("spec parses");
    let errors = spec.validate(&dir.path().join("mirror.yml"));
    assert!(errors.is_empty(), "bin_scan: off must not gate anything: {errors:?}");
}

/// Omitting the key everywhere must leave every variant unscanned: turning
/// a scan on by default would start publishing a `binaries` claim no
/// publisher made, across the whole fleet, on the next cron run.
#[test]
fn bin_scan_defaults_to_off_for_a_spec_that_never_mentions_it() {
    let yaml = r#"
name: shfmt
target:
  registry: ocx.sh
  repository: shfmt
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
assets:
  linux/amd64:
    - "shfmt_.*_linux_amd64$"
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).expect("spec parses");
    assert_eq!(spec.bin_scan, BinScanMode::Off);
    assert_eq!(spec.effective_variants()[0].bin_scan, BinScanMode::Off);
}

/// The opposite default to `bin_scan`'s, and the assertion that has to
/// break if anyone flips it: a spec that never mentions `libc_lint` is
/// checked. Every ported spec's declared `os.features` already match their
/// binaries, so on-by-default reds nothing that ships — and a check the
/// whole fleet leaves off is not a check.
#[test]
fn libc_lint_defaults_to_on_for_a_spec_that_never_mentions_it() {
    let yaml = r#"
name: shfmt
target:
  registry: ocx.sh
  repository: shfmt
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
assets:
  linux/amd64:
    - "shfmt_.*_linux_amd64$"
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).expect("spec parses");
    assert!(spec.libc_lint, "an unmentioned libc_lint must be on");
    assert!(spec.effective_variants()[0].libc_lint, "and must reach the variant");
}

/// `libc_lint` follows the same override-with-fallback rule as `bin_scan`:
/// one variant's upstream build can be the only one the check misreads, and
/// bypassing the whole spec to get that variant through would silently stop
/// checking the others. A variant that says nothing inherits rather than
/// resetting to the type default.
#[test]
fn effective_variants_libc_lint_overrides_per_variant_and_falls_back() {
    let yaml = r#"
name: python
target:
  registry: ocx.sh
  repository: python
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
libc_lint: false
variants:
  - default: true
    assets:
      linux/amd64:
        - "full-.*\\.tar\\.gz"
  - name: slim
    libc_lint: true
    assets:
      linux/amd64:
        - "slim-.*\\.tar\\.gz"
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).expect("spec parses");
    let variants = spec.effective_variants();

    assert!(
        !variants[0].libc_lint,
        "a variant saying nothing inherits the spec-level value"
    );
    assert!(variants[1].libc_lint, "a variant may override it");
    assert!(
        !spec.libc_lint,
        "the spec level must stay off while one variant turns it on"
    );

    // Both fallback directions, because `true` is also `bool`'s type
    // default: the spec above kills a hardcoded `unwrap_or(true)`, and only
    // a spec that omits the key kills `unwrap_or_default()`. Either
    // mutation survives the other case.
    let inheriting_on = yaml
        .replace("libc_lint: false\n", "")
        .replace("    libc_lint: true\n", "");
    let spec: MirrorSpec = serde_yaml_ng::from_str(&inheriting_on).expect("spec parses");
    assert!(
        spec.effective_variants().iter().all(|v| v.libc_lint),
        "with the key omitted every variant inherits the on default"
    );
}

/// A misspelled variant key must be rejected, naming the key. The escape
/// hatch is the reason this matters: `libc-lint: false` is the spelling the
/// docs put in front of operators (`ocx package create --no-libc-lint`), and
/// silently dropping it leaves the check on, the build still refusing, and
/// no way to tell that the bypass never applied. The same misspelling at the
/// top level has always been a hard error.
#[test]
fn unknown_variant_key_is_rejected_and_named() {
    let yaml = r#"
name: python
target:
  registry: ocx.sh
  repository: python
source:
  type: github_release
  owner: test
  repo: test
  tag_pattern: "^v(?P<version>\\d+)$"
variants:
  - default: true
    libc-lint: false
    assets:
      linux/amd64:
        - "full-.*\\.tar\\.gz"
"#;

    let err = serde_yaml_ng::from_str::<MirrorSpec>(yaml).expect_err("a misspelled variant key must red");
    let msg = err.to_string();
    assert!(
        msg.contains("libc-lint"),
        "the error must name the offending key: {msg}"
    );

    // The correct spelling still parses — otherwise this test would pass
    // just as well against a parser that rejects every variant.
    let spec: MirrorSpec =
        serde_yaml_ng::from_str(&yaml.replace("libc-lint:", "libc_lint:")).expect("the declared key parses");
    assert!(!spec.effective_variants()[0].libc_lint);
}
