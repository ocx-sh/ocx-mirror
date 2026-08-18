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
  indexes:
    - url: "ftp://pypi.example.com"
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
            .any(|e| e.contains("source.indexes[0].url") && e.contains("http(s)")),
        "Expected bad index URL error, got: {errors:?}"
    );
}

/// A credential in a committed spec is refused, not stripped: `mirror.yml` is
/// contributed, and the URL would also reach the `uv` subprocess argv, where
/// `/proc/<pid>/cmdline` is world-readable.
#[test]
fn validate_reject_pypi_index_url_with_userinfo() {
    let yaml = r#"
name: acme-app
target:
  registry: ocx.sh
  repository: acme-app
source:
  type: pypi
  package: acme-app
  indexes:
    - url: "https://ci:hunter2@nexus.corp.example/repository/pypi/simple"
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
            .any(|e| e.contains("source.indexes[0].url") && e.contains("must not embed credentials")),
        "Expected a credential-in-URL rejection, got: {errors:?}"
    );
    assert!(
        !errors.iter().any(|e| e.contains("hunter2")),
        "the rejection must not echo the secret: {errors:?}"
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
