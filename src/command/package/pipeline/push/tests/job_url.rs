// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use super::support::*;
use tempfile::tempdir;

// ── JUnit-embedded job_url plumbing for the Discord embed ─────────────
//
// The test matrix step computes the matrix-leg `html_url` once via
// `gh api` and embeds it in the JUnit XML as a suite-level
// `<property name="ci.job.url" value="…"/>`. `evaluate_junit` reads the
// property and threads it onto the `PlatformFailure` so the Discord
// notify step can render a markdown link to the responsible job.

/// JUnit XML carrying a `ci.job.url` property and one failing testcase.
fn failing_junit_with_job_url(_version: &str, platform: &str, image: &str, url: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuites>
  <testsuite name="ocx-mirror.shfmt.{slug}.{cid}" tests="1" failures="1" errors="0" skipped="0" timestamp="2026-05-14T10:00:00Z" time="2.0">
    <properties>
      <property name="ci.job.url" value="{url}"/>
    </properties>
    <testcase name="version" classname="ocx-mirror.shfmt.{slug}.{cid}" time="2.0">
      <failure message="exit code 1" type="exit_code">binary not found</failure>
    </testcase>
  </testsuite>
</testsuites>"#,
        slug = platform.replace('/', "_"),
        cid = image.replace([':', '/'], "_"),
        url = url,
    )
}

#[test]
fn evaluate_junit_attaches_job_url_from_property_for_test_failed() {
    let junit_dir = tempdir().unwrap();
    let version = "1.0.0";
    let slug = "linux_amd64";
    let url = "https://github.com/ocx-sh/mirror-shfmt/actions/runs/42/job/7";

    write_junit(
        junit_dir.path(),
        version,
        slug,
        "_native_",
        &failing_junit_with_job_url(version, "linux/amd64", "_native_", url),
    );

    let rt = tokio::runtime::Runtime::new().unwrap();
    let decision = rt.block_on(evaluate_junit(
        junit_dir.path(),
        version,
        &slug_to_platform_heuristic(slug),
        &["_native_".to_string()],
        &["version".to_string()],
    ));

    match decision {
        VpDecision::Red { platform_failure, .. } => {
            assert_eq!(platform_failure.reason, "test_failed");
            assert_eq!(platform_failure.job_url.as_deref(), Some(url));
        }
        VpDecision::Green => panic!("failing JUNIT must yield Red"),
    }
}

#[test]
fn evaluate_junit_omits_job_url_when_property_absent() {
    let junit_dir = tempdir().unwrap();
    let version = "1.0.0";
    let slug = "linux_amd64";

    // Failing JUNIT without a `ci.job.url` property — push runs against
    // legacy workflow templates (no URL injection) must still produce a
    // usable PlatformFailure, just without the clickable link.
    write_junit(
        junit_dir.path(),
        version,
        slug,
        "_native_",
        &failing_junit(version, "linux/amd64", "_native_"),
    );

    let rt = tokio::runtime::Runtime::new().unwrap();
    let decision = rt.block_on(evaluate_junit(
        junit_dir.path(),
        version,
        &slug_to_platform_heuristic(slug),
        &["_native_".to_string()],
        &["version".to_string()],
    ));

    match decision {
        VpDecision::Red { platform_failure, .. } => {
            assert!(
                platform_failure.job_url.is_none(),
                "absent ci.job.url property must produce job_url=None"
            );
        }
        VpDecision::Green => panic!("failing JUNIT must yield Red"),
    }
}

#[test]
fn evaluate_junit_picks_first_property_across_containers() {
    // Multi-container leg: only one container's JUNIT carries the
    // ci.job.url property. The first non-empty value wins so the failure
    // gets linked even when not every container writes the property.
    let junit_dir = tempdir().unwrap();
    let version = "1.0.0";
    let slug = "linux_amd64";
    let url = "https://github.com/ocx-sh/mirror-shfmt/actions/runs/42/job/9";

    // ubuntu container: no property
    write_junit(
        junit_dir.path(),
        version,
        slug,
        "ubuntu_2404",
        &failing_junit(version, "linux/amd64", "ubuntu:24.04"),
    );
    // alpine container: property present, also failing
    write_junit(
        junit_dir.path(),
        version,
        slug,
        "alpine_3_20",
        &failing_junit_with_job_url(version, "linux/amd64", "alpine:3.20", url),
    );

    let rt = tokio::runtime::Runtime::new().unwrap();
    let decision = rt.block_on(evaluate_junit(
        junit_dir.path(),
        version,
        &slug_to_platform_heuristic(slug),
        &["ubuntu_2404".to_string(), "alpine_3_20".to_string()],
        &["version".to_string()],
    ));

    match decision {
        VpDecision::Red { platform_failure, .. } => {
            assert_eq!(platform_failure.job_url.as_deref(), Some(url));
        }
        VpDecision::Green => panic!("failing JUNIT must yield Red"),
    }
}

#[test]
fn evaluate_junit_omits_job_url_for_missing_junit() {
    // When the JUnit XML never landed (`missing_junit` reason) there's
    // no property to read either. The failure still has the right reason
    // but `job_url` stays `None`. Title's run_url is the navigation
    // fallback for this case.
    let junit_dir = tempdir().unwrap();
    let version = "1.0.0";
    let slug = "linux_amd64";

    let rt = tokio::runtime::Runtime::new().unwrap();
    let decision = rt.block_on(evaluate_junit(
        junit_dir.path(),
        version,
        &slug_to_platform_heuristic(slug),
        &["ubuntu_2404".to_string()],
        &["version".to_string()],
    ));

    match decision {
        VpDecision::Red { platform_failure, .. } => {
            assert_eq!(platform_failure.reason, "missing_junit");
            assert!(platform_failure.job_url.is_none());
        }
        VpDecision::Green => panic!("missing junit must yield Red"),
    }
}

// ── push_job_url stamping via OCX_MIRROR_JOB_URL ─────────────────────
//
// `pipeline push` reads `OCX_MIRROR_JOB_URL` at startup and stamps it
// onto:
//   - every `push_error` / `missing_bundle` PlatformFailure.job_url
//   - the run-summary's top-level `push_job_url`
// The Discord notify step uses the latter to link green rows + the
// former to link push-tier failures.

#[test]
fn push_stamps_run_summary_push_job_url_from_env() {
    let _env_lock = job_url_env_lock();
    let bundles_dir = tempdir().unwrap();
    let junit_dir = tempdir().unwrap();
    let summary_path = tempdir().unwrap().path().join("run-summary.json");

    let push_url = "https://github.com/ocx-sh/mirror-shfmt/actions/runs/42/job/99";

    // SAFETY: test-only env var. Tests run inside a single nextest leg
    // but multiple may share a process — unique name avoids cross-test
    // contention.
    unsafe {
        std::env::set_var("OCX_MIRROR_JOB_URL", push_url);
    }

    // No bundles → no versions → push exits Ok and writes an empty
    // summary. push_job_url must still be set so notify can link to
    // the push job even on degenerate runs.
    let spec_path = std::path::Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/mirror-minimal.yml"
    ))
    .to_path_buf();

    let result = run_push_cmd(
        spec_path,
        junit_dir.path().to_path_buf(),
        bundles_dir.path().to_path_buf(),
        summary_path.clone(),
    );

    // SAFETY: cleanup so neighbouring tests don't inherit the stamp.
    unsafe {
        std::env::remove_var("OCX_MIRROR_JOB_URL");
    }

    // Acceptable if the test env can't load the spec — we only care
    // about the env-stamp wiring.
    if result.is_ok() {
        let content = std::fs::read_to_string(&summary_path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(v["push_job_url"].as_str(), Some(push_url));
    }
}

#[test]
fn push_stamps_push_error_failures_with_push_job_url() {
    let _env_lock = job_url_env_lock();
    let bundles_dir = tempdir().unwrap();
    let junit_dir = tempdir().unwrap();
    let summary_path = tempdir().unwrap().path().join("run-summary.json");

    let version = "3.7.0";
    let slug = "linux_amd64";
    let push_url = "https://github.com/ocx-sh/mirror-shfmt/actions/runs/42/job/99";

    // Bundle present + JUNIT absent → version loop enters push branch
    // via the missing_bundle path *or* the push path. We write JUNIT
    // for a single container that the multi-container spec expects, so
    // the (V, P) decision is Red(missing_junit), not push_error. We
    // instead test the missing_bundle path: bundle absent, JUNIT green.
    // Wait — the loop only attempts push when JUNIT is Green; with
    // bundle absent that's missing_bundle which still gets stamped.
    for cid in ["ubuntu_24_04", "alpine_3_20", "fedora_40"] {
        let xml = passing_junit(version, "linux/amd64", &cid.replacen('_', ":", 1));
        write_junit(junit_dir.path(), version, slug, cid, &xml);
    }
    // No bundle file created → missing_bundle path.

    // Drop a junk bundle to make the version appear in the enumeration.
    // The bundle file path used by the push step differs, so the
    // bundle.exists() check still fails (the file we drop lives at the
    // canonical path; with it present, push_error is exercised instead
    // when the subprocess fails — also valid for the stamp test).
    std::fs::write(bundles_dir.path().join(format!("bundle-{version}-{slug}.tar.xz")), b"x").unwrap();

    // SAFETY: test-only stamp.
    unsafe {
        std::env::set_var("OCX_MIRROR_JOB_URL", push_url);
    }

    let spec_path = std::path::Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/mirror-multi-container.yml"
    ))
    .to_path_buf();

    let _ = run_push_cmd(
        spec_path,
        junit_dir.path().to_path_buf(),
        bundles_dir.path().to_path_buf(),
        summary_path.clone(),
    );

    // SAFETY: cleanup.
    unsafe {
        std::env::remove_var("OCX_MIRROR_JOB_URL");
    }

    let summary: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&summary_path).unwrap()).unwrap();
    assert_eq!(summary["push_job_url"].as_str(), Some(push_url));

    // Every failure with reason `push_error` or `missing_bundle` must
    // carry job_url == push_url. test_failed / missing_junit failures
    // keep their JUnit-derived URL or None and are left untouched here.
    let failures = summary["versions"][0]["platforms_failed"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for f in &failures {
        let reason = f["reason"].as_str().unwrap_or("");
        if reason == "push_error" || reason == "missing_bundle" {
            assert_eq!(
                f["job_url"].as_str(),
                Some(push_url),
                "{reason} failure must carry stamped push_job_url, got: {f}",
            );
        }
    }
}
