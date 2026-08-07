// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;

// ── env sources: pylock / pypi ───────────────────────────────────────────

#[test]
fn parse_and_validate_pylock_spec_with_wheels() {
    let yaml = r#"
name: acme-app
target:
  registry: ocx.sh
  repository: acme-app
source:
  type: pylock
  path: pylock.toml
python:
  version: "3.13.1"
  abi: cp313
  interpreter_package: "ocx.sh/python/cpython:3.13.1"
wheel_scope: acme-wheels
wheels:
  "linux/amd64+libc.glibc": ~
  "linux/amd64+libc.musl": [musllinux, any]
  darwin/arm64: ~
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    assert!(matches!(spec.source, Source::Pylock { .. }));
    assert_eq!(spec.wheel_scope, "acme-wheels");
    assert!(spec.python.is_some());
    assert_eq!(spec.wheels.as_ref().unwrap().filters.len(), 3);

    let errors = spec.validate(Path::new("test.yaml"));
    assert!(errors.is_empty(), "valid pylock spec should validate: {errors:?}");
}

#[test]
fn pylock_spec_defaults_wheel_scope() {
    let yaml = r#"
name: acme-app
target:
  registry: ocx.sh
  repository: acme-app
source:
  type: pylock
  path: pylock.toml
python:
  version: "3.13.1"
  abi: cp313
  interpreter_package: "ocx.sh/python/cpython:3.13.1"
wheels:
  linux/amd64: ~
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    assert_eq!(spec.wheel_scope, "pip-packages");
    assert!(spec.validate(Path::new("test.yaml")).is_empty());
}

#[test]
fn validate_reject_env_spec_without_wheels() {
    let yaml = r#"
name: acme-app
target:
  registry: ocx.sh
  repository: acme-app
source:
  type: pylock
  path: pylock.toml
python:
  version: "3.13.1"
  abi: cp313
  interpreter_package: "ocx.sh/python/cpython:3.13.1"
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    let errors = spec.validate(Path::new("test.yaml"));
    assert!(
        errors
            .iter()
            .any(|e| e.contains("wheels: required for source.type 'pylock'")),
        "Expected wheels-required error, got: {errors:?}"
    );
}

#[test]
fn validate_reject_env_spec_with_variants() {
    // Breaking (intended): env packages model libc via `+libc.*` wheels
    // keys (os.features platform axis), never via `variants:`.
    let yaml = r#"
name: acme-app
target:
  registry: ocx.sh
  repository: acme-app
source:
  type: pylock
  path: pylock.toml
python:
  version: "3.13.1"
  abi: cp313
  interpreter_package: "ocx.sh/python/cpython:3.13.1"
wheels:
  linux/amd64: ~
variants:
  - name: musl
    default: true
    assets:
      linux/amd64:
        - "acme-.*-musl\\.tar\\.gz"
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    let errors = spec.validate(Path::new("test.yaml"));
    assert!(
        errors
            .iter()
            .any(|e| e.contains("variants: not supported for source.type 'pylock'")),
        "Expected variants-on-env error, got: {errors:?}"
    );
}

#[test]
fn validate_reject_wheels_on_archive_source() {
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
wheels:
  linux/amd64: ~
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    let errors = spec.validate(Path::new("test.yaml"));
    assert!(
        errors
            .iter()
            .any(|e| e.contains("wheels: only supported for source.type 'pylock'/'pypi'")),
        "Expected wheels-on-archive error, got: {errors:?}"
    );
}

#[test]
fn validate_wheels_platforms_cross_coverage() {
    // A wheels key whose base os/arch is not a declared platform leg, and a
    // declared platform leg no wheels key covers — both rejected.
    let yaml = r#"
name: acme-app
target:
  registry: ocx.sh
  repository: acme-app
source:
  type: pylock
  path: pylock.toml
python:
  version: "3.13.1"
  abi: cp313
  interpreter_package: "ocx.sh/python/cpython:3.13.1"
wheels:
  "linux/arm64+libc.glibc": ~
platforms:
  linux/amd64:
    runner: ubuntu-latest
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    let errors = spec.validate(Path::new("test.yaml"));
    assert!(
        errors
            .iter()
            .any(|e| e.contains("base platform 'linux/arm64' is not declared under 'platforms'")),
        "Expected uncovered-wheels-key error, got: {errors:?}"
    );
    assert!(
        errors
            .iter()
            .any(|e| e.contains("platforms.linux/amd64: no wheels key covers this platform")),
        "Expected uncovered-platform-leg error, got: {errors:?}"
    );
}

#[test]
fn validate_wheels_dual_libc_keys_cover_one_platform_leg() {
    // The dual-libc shape: two `+libc.*` keys sharing one base cover the
    // same CI matrix leg — one package, one tag, two index entries.
    let yaml = r#"
name: acme-app
target:
  registry: ocx.sh
  repository: acme-app
source:
  type: pylock
  path: pylock.toml
python:
  version: "3.13.1"
  abi: cp313
  interpreter_package: "ocx.sh/python/cpython:3.13.1"
wheels:
  "linux/amd64+libc.glibc": ~
  "linux/amd64+libc.musl": ~
platforms:
  linux/amd64:
    runner: ubuntu-latest
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    let errors = spec.validate(Path::new("test.yaml"));
    assert!(errors.is_empty(), "dual-libc keys must validate: {errors:?}");
}

#[test]
fn validate_reject_pylock_missing_python_block() {
    let yaml = r#"
name: acme-app
target:
  registry: ocx.sh
  repository: acme-app
source:
  type: pylock
  path: pylock.toml
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    let errors = spec.validate(Path::new("test.yaml"));
    assert!(
        errors.iter().any(|e| e.contains("python: required")),
        "Expected missing python block error, got: {errors:?}"
    );
}

#[test]
fn validate_reject_pylock_with_top_level_assets() {
    let yaml = r#"
name: acme-app
target:
  registry: ocx.sh
  repository: acme-app
source:
  type: pylock
  path: pylock.toml
python:
  version: "3.13.1"
  abi: cp313
  interpreter_package: "ocx.sh/python/cpython:3.13.1"
assets:
  linux/amd64:
    - "should-not-be-here\\.whl"
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    let errors = spec.validate(Path::new("test.yaml"));
    assert!(
        errors
            .iter()
            .any(|e| e.contains("assets") && e.contains("not supported for source.type 'pylock'")),
        "Expected asset-patterns-on-pylock error, got: {errors:?}"
    );
}

#[test]
fn validate_reject_pylock_with_top_level_metadata() {
    let yaml = r#"
name: acme-app
target:
  registry: ocx.sh
  repository: acme-app
source:
  type: pylock
  path: pylock.toml
python:
  version: "3.13.1"
  abi: cp313
  interpreter_package: "ocx.sh/python/cpython:3.13.1"
metadata:
  default: metadata.json
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    let errors = spec.validate(Path::new("test.yaml"));
    assert!(
        errors.iter().any(|e| *e == metadata_not_supported_error("pylock")),
        "Expected exact metadata-not-supported-for-pylock error, got: {errors:?}"
    );
}

#[test]
fn validate_reject_pypi_with_top_level_metadata() {
    let yaml = r#"
name: acme-app
target:
  registry: ocx.sh
  repository: acme-app
source:
  type: pypi
  package: acme-app
python:
  version: "3.13.1"
  abi: cp313
  interpreter_package: "ocx.sh/python/cpython:3.13.1"
metadata:
  default: metadata.json
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    let errors = spec.validate(Path::new("test.yaml"));
    assert!(
        errors.iter().any(|e| *e == metadata_not_supported_error("pypi")),
        "Expected exact metadata-not-supported-for-pypi error, got: {errors:?}"
    );
}

/// `bin_scan` has nowhere to look on an env spec — its content tree is
/// composed from wheels, never extracted from an archive — so a declared
/// scan mode is rejected like `metadata:`. `libc_lint` is the deliberate
/// counter-case: the env prepare pipeline runs it over the composed tree,
/// so the same spec keeps the check on and must validate clean.
#[test]
fn validate_rejects_bin_scan_on_env_spec_but_accepts_inert_libc_lint() {
    let base = r#"
name: acme-app
target:
  registry: ocx.sh
  repository: acme-app
source:
  type: pypi
  package: acme-app
python:
  version: "3.13.1"
  abi: cp313
  interpreter_package: "ocx.sh/python/cpython:3.13.1"
wheels:
  linux/amd64: ~
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(&format!("{base}bin_scan: verify\n")).unwrap();
    let errors = spec.validate(Path::new("test.yaml"));
    assert!(
        errors.iter().any(|e| *e == bin_scan_not_supported_error("pypi")),
        "Expected exact bin_scan-not-supported-for-pypi error, got: {errors:?}"
    );

    // `off` is the default every env spec carries without saying so — it
    // must not red, or no env spec would ever validate.
    let spec: MirrorSpec = serde_yaml_ng::from_str(&format!("{base}bin_scan: off\n")).unwrap();
    assert!(spec.validate(Path::new("test.yaml")).is_empty());

    // The libc check stays declarable, in both directions, and on by
    // default — the env leg is where a `+libc.*` key can be contradicted.
    let spec: MirrorSpec = serde_yaml_ng::from_str(base).unwrap();
    assert!(spec.libc_lint, "an unmentioned libc_lint must be on for env specs too");
    for value in ["true", "false"] {
        let spec: MirrorSpec = serde_yaml_ng::from_str(&format!("{base}libc_lint: {value}\n")).unwrap();
        let errors = spec.validate(Path::new("test.yaml"));
        assert!(errors.is_empty(), "libc_lint: {value} must validate on env: {errors:?}");
    }
}

#[tokio::test]
async fn pypi_fixture_spec_loads_and_validates() {
    let spec_path = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mirror-pypi.yml"));
    let spec = load_spec(&spec_path)
        .await
        .expect("pypi fixture spec must load and validate");
    assert!(matches!(spec.source, Source::Pypi { .. }));
}

#[test]
fn validate_reject_pypi_missing_python_block() {
    let yaml = r#"
name: acme-app
target:
  registry: ocx.sh
  repository: acme-app
source:
  type: pypi
  package: acme-app
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    let errors = spec.validate(Path::new("test.yaml"));
    assert!(
        errors.iter().any(|e| e.contains("python: required")),
        "Expected missing python block error, got: {errors:?}"
    );
}

#[test]
fn validate_reject_pypi_with_top_level_assets() {
    let yaml = r#"
name: acme-app
target:
  registry: ocx.sh
  repository: acme-app
source:
  type: pypi
  package: acme-app
python:
  version: "3.13.1"
  abi: cp313
  interpreter_package: "ocx.sh/python/cpython:3.13.1"
assets:
  linux/amd64:
    - "should-not-be-here\\.whl"
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    let errors = spec.validate(Path::new("test.yaml"));
    assert!(
        errors
            .iter()
            .any(|e| e.contains("assets") && e.contains("not supported for source.type 'pypi'")),
        "Expected asset-patterns-on-pypi error, got: {errors:?}"
    );
}

#[test]
fn validate_reject_pypi_bad_index_url() {
    let yaml = r#"
name: acme-app
target:
  registry: ocx.sh
  repository: acme-app
source:
  type: pypi
  package: acme-app
  index: "ftp://pypi.example.com"
python:
  version: "3.13.1"
  abi: cp313
  interpreter_package: "ocx.sh/python/cpython:3.13.1"
wheels:
  linux/amd64: ~
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    let errors = spec.validate(Path::new("test.yaml"));
    assert!(
        errors
            .iter()
            .any(|e| e.contains("source.index") && e.contains("http(s)")),
        "Expected bad index URL error, got: {errors:?}"
    );
}

#[test]
fn validate_reject_pylock_with_python_lock_field() {
    // `python.lock` configures lock *derivation*, which only makes sense
    // for `source.type: pypi` — a `pylock` source already resolves its
    // own committed lock.
    let yaml = r#"
name: acme-app
target:
  registry: ocx.sh
  repository: acme-app
source:
  type: pylock
  path: pylock.toml
python:
  version: "3.13.1"
  abi: cp313
  interpreter_package: "ocx.sh/python/cpython:3.13.1"
  lock: {}
"#;

    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
    let errors = spec.validate(Path::new("test.yaml"));
    assert!(
        errors
            .iter()
            .any(|e| e.contains("python.lock") && e.contains("only supported for source.type 'pypi'")),
        "Expected python.lock-on-pylock error, got: {errors:?}"
    );
}
