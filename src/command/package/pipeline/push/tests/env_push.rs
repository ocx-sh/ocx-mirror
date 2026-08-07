// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use super::support::*;
use tempfile::tempdir;

// ── Env (pylock/pypi) push dispatch ────────────────────────────────────

#[test]
fn execute_pylock_push_reads_env_manifest_and_writes_summary() {
    let _env_lock = job_url_env_lock();
    let bundles_dir = tempdir().unwrap();
    let junit_dir = tempdir().unwrap();
    let summary_path = tempdir().unwrap().path().join("run-summary.json");

    let version = "1.0.0";
    write_env_manifest(
        bundles_dir.path(),
        version,
        &[("linux_amd64", "linux/amd64"), ("linux_arm64", "linux/arm64")],
        &["pycowsay", "six"],
    );

    // mirror-pylock.yml declares no containers/tests, so each platform
    // evaluates in native mode against a single `_native_` JUNIT. A
    // passing suite for both platforms reaches the Green branch, so the
    // loop attempts the env push (which fails — no `ocx` on PATH —
    // recorded as `push_error`), exercising the multi-layer argv path.
    for (platform, slug) in [("linux/amd64", "linux_amd64"), ("linux/arm64", "linux_arm64")] {
        write_junit(
            junit_dir.path(),
            version,
            slug,
            "_native_",
            &passing_junit(version, platform, "_native_"),
        );
    }

    let spec_path =
        std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mirror-pylock.yml")).to_path_buf();

    let result = run_push_cmd(
        spec_path,
        junit_dir.path().to_path_buf(),
        bundles_dir.path().to_path_buf(),
        summary_path.clone(),
    );

    // The push subprocess fails in the test environment (no `ocx` on
    // PATH) → push_error → any_red → ExecutionFailed, same exit
    // contract as the archive path. The summary must still be written.
    assert!(
        matches!(result, Err(MirrorError::ExecutionFailed(_))),
        "expected ExecutionFailed from push_error, got {result:?}",
    );
    assert!(summary_path.exists(), "run-summary.json must be written");

    let summary: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&summary_path).unwrap()).unwrap();
    assert_eq!(summary["schema_version"], serde_json::json!(1));

    let versions = summary["versions"].as_array().unwrap();
    assert_eq!(versions.len(), 1, "one version from the env manifest");
    assert_eq!(versions[0]["version"], serde_json::json!(version));

    let failures = versions[0]["platforms_failed"].as_array().unwrap();
    assert_eq!(failures.len(), 2, "both platforms fail via push_error: {failures:?}");
    for f in failures {
        assert_eq!(f["reason"], serde_json::json!("push_error"));
    }
}

/// Regression: a `source.type: pypi` spec must dispatch to the env-push
/// path exactly like `pylock`. A dispatch matching only `Source::Pylock`
/// let pypi fall through to the archive loop, find no `bundle-*.tar.xz`
/// (prepare writes env-manifest.json for env sources), and silently
/// succeed with an empty summary.
#[test]
fn execute_routes_pypi_source_through_env_push() {
    let _env_lock = job_url_env_lock();
    let bundles_dir = tempdir().unwrap();
    let junit_dir = tempdir().unwrap();
    let summary_path = tempdir().unwrap().path().join("run-summary.json");

    let version = "1.0.0";
    write_env_manifest(
        bundles_dir.path(),
        version,
        &[("linux_amd64", "linux/amd64")],
        &["pycowsay"],
    );

    write_junit(
        junit_dir.path(),
        version,
        "linux_amd64",
        "_native_",
        &passing_junit(version, "linux/amd64", "_native_"),
    );

    let spec_path =
        std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mirror-pypi.yml")).to_path_buf();

    let result = run_push_cmd(
        spec_path,
        junit_dir.path().to_path_buf(),
        bundles_dir.path().to_path_buf(),
        summary_path.clone(),
    );

    // Env path taken → green JUNIT reaches the env push, which fails in
    // the test environment (no `ocx` on PATH) → push_error → any_red →
    // ExecutionFailed. The archive-path bug instead returned Ok(()) with
    // an empty versions array (no bundle files to enumerate).
    assert!(
        matches!(result, Err(MirrorError::ExecutionFailed(_))),
        "pypi source must take the env-push path (push_error expected), got {result:?}",
    );

    let summary: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&summary_path).unwrap()).unwrap();
    let versions = summary["versions"].as_array().unwrap();
    assert_eq!(versions.len(), 1, "env manifest version must be processed");
    assert_eq!(
        versions[0]["platforms_failed"][0]["reason"],
        serde_json::json!("push_error")
    );
}
