// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use super::support::*;
use tempfile::tempdir;

// ── wheels-key libc gating (os.features platform entries) ──────────────

fn env_container_spec() -> MirrorSpec {
    let yaml = r#"
name: pycowsay
target:
  registry: ocx.sh
  repository: pycowsay
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
    containers:
      - image: debian:12
      - image: alpine:3.20
"#;
    serde_yaml_ng::from_str(yaml).unwrap()
}

#[test]
fn base_platform_and_libc_feature_parse_full_keys() {
    assert_eq!(base_platform_str("linux/amd64+libc.glibc"), "linux/amd64");
    assert_eq!(base_platform_str("linux/amd64"), "linux/amd64");
    assert_eq!(entry_libc_feature("linux/amd64+libc.glibc"), Some("libc.glibc"));
    assert_eq!(entry_libc_feature("linux/amd64+libc.musl"), Some("libc.musl"));
    assert_eq!(entry_libc_feature("linux/amd64"), None);
}

#[test]
fn gating_featureless_entry_requires_all_containers() {
    // A featureless entry claims to run on ANY libc → every container of
    // its base platform gates it (debian AND alpine).
    let spec = env_container_spec();
    assert_eq!(
        gating_container_ids_for_entry(&spec, "linux/amd64", None),
        vec!["debian_12".to_string(), "alpine_3_20".to_string()]
    );
}

#[test]
fn gating_glibc_entry_ignores_musl_containers() {
    let spec = env_container_spec();
    assert_eq!(
        gating_container_ids_for_entry(&spec, "linux/amd64", Some("libc.glibc")),
        vec!["debian_12".to_string()]
    );
}

#[test]
fn gating_musl_entry_only_gated_by_musl_containers() {
    let spec = env_container_spec();
    assert_eq!(
        gating_container_ids_for_entry(&spec, "linux/amd64", Some("libc.musl")),
        vec!["alpine_3_20".to_string()]
    );
}

#[test]
fn gating_native_leg_counts_as_gnu() {
    // No containers declared → `_native_` (a glibc GHA runner) gates
    // featureless + glibc entries; a musl entry has NO test leg → empty →
    // the caller fails closed.
    let mut spec = env_container_spec();
    if let Some(platforms) = spec.platforms.as_mut()
        && let Some(config) = platforms.get_mut("linux/amd64")
    {
        config.containers = None;
    }
    assert_eq!(
        gating_container_ids_for_entry(&spec, "linux/amd64", None),
        vec!["_native_".to_string()]
    );
    assert_eq!(
        gating_container_ids_for_entry(&spec, "linux/amd64", Some("libc.glibc")),
        vec!["_native_".to_string()]
    );
    assert!(gating_container_ids_for_entry(&spec, "linux/amd64", Some("libc.musl")).is_empty());
}

#[test]
fn execute_pylock_push_gates_libc_entries_per_container() {
    // Dual-libc manifest: the glibc entry needs ONLY the debian junit and
    // the musl entry ONLY the alpine one. With just the debian junit
    // written, the glibc entry passes gating (reaching push → push_error,
    // no `ocx` on PATH) while the musl entry reds as missing_junit for
    // alpine — never the other way around.
    let _env_lock = job_url_env_lock();
    let spec_dir = tempdir().unwrap();
    let spec_yaml = r#"
name: pycowsay
target:
  registry: ocx.sh
  repository: pycowsay
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
tests:
  - name: version
    command: pycowsay --version
platforms:
  linux/amd64:
    runner: ubuntu-latest
    containers:
      - image: debian:12
      - image: alpine:3.20
"#;
    let spec_path = spec_dir.path().join("mirror.yml");
    std::fs::write(&spec_path, spec_yaml).unwrap();

    let bundles_dir = tempdir().unwrap();
    let junit_dir = tempdir().unwrap();
    let summary_path = tempdir().unwrap().path().join("run-summary.json");

    let version = "1.0.0";
    write_env_manifest(
        bundles_dir.path(),
        version,
        &[
            ("linux_amd64", "linux/amd64+libc.glibc"),
            ("linux_amd64", "linux/amd64+libc.musl"),
        ],
        &["pycowsay"],
    );

    // Only the debian (gnu) container leg wrote a junit — with the
    // declared `version` test present so the declared-test check passes.
    let junit = r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuites>
  <testsuite name="1.0.0.linux_amd64.debian_12" tests="1" failures="0" errors="0">
    <testcase name="version" classname="1.0.0.linux_amd64.debian_12" time="1.0"/>
  </testsuite>
</testsuites>"#;
    write_junit(junit_dir.path(), version, "linux_amd64", "debian_12", junit);

    let result = run_push_cmd(
        spec_path,
        junit_dir.path().to_path_buf(),
        bundles_dir.path().to_path_buf(),
        summary_path.clone(),
    );
    assert!(
        matches!(result, Err(MirrorError::ExecutionFailed(_))),
        "musl leg missing junit + glibc push_error → any_red, got {result:?}",
    );

    let summary: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&summary_path).unwrap()).unwrap();
    let failures = summary["versions"][0]["platforms_failed"].as_array().unwrap();
    assert_eq!(failures.len(), 2, "{failures:?}");

    // The glibc entry PASSED junit gating (its only gate is debian) and
    // failed later at push (no `ocx` binary) — proving alpine's missing
    // junit never gated it.
    assert!(
        failures
            .iter()
            .any(|f| f["platform"] == "linux/amd64+libc.glibc" && f["reason"] == "push_error"),
        "{failures:?}"
    );
    // The musl entry is gated ONLY by alpine, whose junit is missing.
    assert!(
        failures
            .iter()
            .any(|f| f["platform"] == "linux/amd64+libc.musl" && f["reason"] == "missing_junit"),
        "{failures:?}"
    );

    // The per-test rows carry the FULL wheels key too. `evaluate_junit`
    // names them by the base platform, which collapses both libc entries
    // onto `linux/amd64` and makes a dual-libc red unattributable in
    // run-summary.json — and therefore in the Discord report built from it.
    let test_failures = summary["versions"][0]["test_failures"].as_array().unwrap();
    assert!(!test_failures.is_empty(), "the missing junit must be recorded");
    assert!(
        test_failures.iter().all(|f| f["platform"] == "linux/amd64+libc.musl"),
        "{test_failures:?}"
    );
}
