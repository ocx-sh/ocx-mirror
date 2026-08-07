// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use super::support::*;
use tempfile::tempdir;

// ── Version verdict closes before any rolling alias moves ──────────────

/// A dual-libc spec: one glibc container leg, one musl container leg, so
/// each `wheels:` key is gated by exactly one of them.
fn dual_libc_spec_yaml() -> &'static str {
    r#"
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
"#
}

/// One green suite carrying the spec's declared `version` test.
fn green_junit_for(version: &str, container_id: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuites>
  <testsuite name="{version}.linux_amd64.{container_id}" tests="1" failures="0" errors="0">
    <testcase name="version" classname="{version}.linux_amd64.{container_id}" time="1.0"/>
  </testsuite>
</testsuites>"#
    )
}

/// Drive `pipeline push` over a dual-libc manifest for `version` where only
/// the debian (glibc) leg reported a JUnit, returning the fake `ocx`'s
/// recorded argv lines and the parsed run summary.
#[cfg(unix)]
fn push_dual_libc_with_one_red_leg(version: &str) -> (String, serde_json::Value) {
    let dir = tempdir().unwrap();
    let bundles_dir = tempdir().unwrap();
    let junit_dir = tempdir().unwrap();
    let summary_path = dir.path().join("run-summary.json");
    let log = dir.path().join("invocations.log");
    let script = fake_ocx_logging_push(dir.path(), &log);

    let spec_path = dir.path().join("mirror.yml");
    std::fs::write(&spec_path, dual_libc_spec_yaml()).unwrap();

    write_env_manifest(
        bundles_dir.path(),
        version,
        &[
            ("linux_amd64", "linux/amd64+libc.glibc"),
            ("linux_amd64", "linux/amd64+libc.musl"),
        ],
        &["pycowsay"],
    );
    // Only debian (gnu) reported — the musl entry's only gate (alpine) is
    // missing, so it reds in phase 1.
    write_junit(
        junit_dir.path(),
        version,
        "linux_amd64",
        "debian_12",
        &green_junit_for(version, "debian_12"),
    );

    // SAFETY: test-only process env, serialised by the lock the caller holds.
    unsafe { std::env::set_var("OCX_BINARY_PIN", &script) };
    let result = run_push_cmd(
        spec_path,
        junit_dir.path().to_path_buf(),
        bundles_dir.path().to_path_buf(),
        summary_path.clone(),
    );
    // SAFETY: cleanup so neighbouring tests don't inherit the pin.
    unsafe { std::env::remove_var("OCX_BINARY_PIN") };

    assert!(
        matches!(result, Err(MirrorError::ExecutionFailed(_))),
        "a red entry must red the run, got {result:?}",
    );

    let invocations = std::fs::read_to_string(&log).unwrap_or_default();
    let summary = serde_json::from_str(&std::fs::read_to_string(&summary_path).unwrap()).unwrap();
    (invocations, summary)
}

#[cfg(unix)]
#[test]
fn a_red_entry_stops_its_version_from_cascading_even_though_a_sibling_landed() {
    // The env path used to decide and push entry by entry: the glibc entry
    // was pushed with `--cascade` (a whole-version claim) before the musl
    // entry had been looked at, moving `latest`/`X`/`X.Y` onto a version
    // that then came out Partial. `announce_tag_union` relies on that never
    // happening.
    let _env_lock = job_url_env_lock();
    let (invocations, summary) = push_dual_libc_with_one_red_leg("1.0.0");

    // The green entry still publishes — nothing about phase 2 withholds it.
    assert!(
        invocations.contains("ocx.sh/pycowsay:1.0.0"),
        "the green glibc entry must still be pushed, got: {invocations}",
    );
    assert!(
        !invocations.contains("--cascade"),
        "no push of a version with a red entry may cascade, got: {invocations}",
    );

    let version_summary = &summary["versions"][0];
    assert_eq!(version_summary["status"], "partial", "got: {version_summary}");
    let tags = version_summary["cascade_tags_written"].as_array().unwrap();
    assert_eq!(
        tags,
        &vec![serde_json::json!("1.0.0")],
        "a partial version carries only its exact tag",
    );
}

#[cfg(unix)]
#[test]
fn a_red_entry_stops_the_latest_alias_for_a_version_ocx_cannot_cascade() {
    // Same hazard on the explicit-alias half: `--cascade` is unavailable for
    // a PEP 440 version ocx cannot parse, so `:latest` is written by an
    // extra push — which must be gated on the same closed verdict.
    let _env_lock = job_url_env_lock();
    let (invocations, summary) = push_dual_libc_with_one_red_leg("0.0.0.2");

    assert!(
        invocations.contains("ocx.sh/pycowsay:0.0.0.2"),
        "the green glibc entry must still be pushed, got: {invocations}",
    );
    assert!(
        !invocations.contains(":latest"),
        "a version with a red entry must never be aliased under :latest, got: {invocations}",
    );

    let tags = summary["versions"][0]["cascade_tags_written"].as_array().unwrap();
    assert!(
        !tags.iter().any(|t| t == "latest"),
        "no alias may be reported for a partial version, got: {tags:?}",
    );
}
