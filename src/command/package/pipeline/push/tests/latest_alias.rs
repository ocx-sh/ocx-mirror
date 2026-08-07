// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use super::support::*;
use tempfile::tempdir;

// ── `:latest` alias for non-semver env versions ────────────────────────

#[cfg(unix)]
#[test]
fn a_non_semver_env_version_is_aliased_under_latest() {
    // `ocx package push --cascade` derives rolling tags from an X.Y.Z
    // parse, so a PEP 440 version like `0.0.0.2` never reaches `latest`
    // and a bare `repo` reference stays unresolvable. The newest such
    // version's green entries get an explicit `:latest` push.
    let _env_lock = job_url_env_lock();
    // Hermetic registry-newest gate: an empty published-tag set means the
    // run's newest is trivially the registry's newest.
    *LATEST_TAGS_OVERRIDE.lock().unwrap() = Some(Vec::new());
    let dir = tempdir().unwrap();
    let bundles_dir = tempdir().unwrap();
    let junit_dir = tempdir().unwrap();
    let summary_path = dir.path().join("run-summary.json");
    let log = dir.path().join("invocations.log");
    let script = fake_ocx_logging_push(dir.path(), &log);

    let version = "0.0.0.2";
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

    // SAFETY: test-only process env, serialised by the lock above.
    unsafe { std::env::set_var("OCX_BINARY_PIN", &script) };
    let result = run_push_cmd(
        spec_path,
        junit_dir.path().to_path_buf(),
        bundles_dir.path().to_path_buf(),
        summary_path.clone(),
    );
    // SAFETY: cleanup so neighbouring tests don't inherit the pin.
    unsafe { std::env::remove_var("OCX_BINARY_PIN") };
    result.expect("a green non-semver env push must exit 0");

    let invocations = std::fs::read_to_string(&log).unwrap();
    assert!(
        invocations.contains(":latest"),
        "the newest non-semver version must be aliased under :latest, got: {invocations}",
    );
    assert!(
        !invocations.contains("--cascade"),
        "a version ocx cannot parse must never ask for cascade, got: {invocations}",
    );

    let summary: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&summary_path).unwrap()).unwrap();
    let tags = summary["versions"][0]["cascade_tags_written"].as_array().unwrap();
    assert!(
        tags.iter().any(|t| t == "latest"),
        "a landed alias must be reported as a written tag, got: {tags:?}",
    );
}

#[cfg(unix)]
#[test]
fn a_failed_latest_alias_does_not_red_the_version() {
    // The primary publish already succeeded — the images ARE in the
    // registry. A `latest` alias that could not be written is corrected by
    // the next run, so it warns instead of turning a published version red.
    use std::os::unix::fs::PermissionsExt;

    let _env_lock = job_url_env_lock();
    // Hermetic registry-newest gate (see `fetch_published_tags`): an empty
    // published-tag set lets the alias leg run without live registry state.
    *LATEST_TAGS_OVERRIDE.lock().unwrap() = Some(Vec::new());
    let dir = tempdir().unwrap();
    let bundles_dir = tempdir().unwrap();
    let junit_dir = tempdir().unwrap();
    let summary_path = dir.path().join("run-summary.json");

    // Succeeds for the version tag, fails for `:latest`; logs every argv
    // so the alias attempt itself is provable below.
    let log = dir.path().join("invocations.log");
    let script = dir.path().join("fake-ocx-latest-fails");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\ncase \"$*\" in\n  *:latest*) echo 'registry rejected the tag' >&2; exit 69 ;;\n  \
             *) echo '{{\"cascade_tags_written\":[],\"status\":\"pushed\"}}' ;;\nesac\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let version = "0.0.0.2";
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

    // SAFETY: test-only process env, serialised by the lock above.
    unsafe { std::env::set_var("OCX_BINARY_PIN", &script) };
    let result = run_push_cmd(
        spec_path,
        junit_dir.path().to_path_buf(),
        bundles_dir.path().to_path_buf(),
        summary_path.clone(),
    );
    // SAFETY: cleanup so neighbouring tests don't inherit the pin.
    unsafe { std::env::remove_var("OCX_BINARY_PIN") };
    result.expect("a failed alias is best-effort and must not fail the run");

    let summary: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&summary_path).unwrap()).unwrap();
    let version_summary = &summary["versions"][0];
    assert_eq!(version_summary["status"], "published", "got: {version_summary}");
    let tags = version_summary["cascade_tags_written"].as_array().unwrap();
    assert!(
        !tags.iter().any(|t| t == "latest"),
        "a failed alias must not be reported as written, got: {tags:?}",
    );
    // The failure branch must actually have been exercised: the alias
    // push reached the fake ocx (otherwise this test is unchecked-green).
    let invocations = std::fs::read_to_string(&log).unwrap();
    assert!(
        invocations.contains(":latest"),
        "the :latest alias push was never attempted; invocations:\n{invocations}"
    );
}
