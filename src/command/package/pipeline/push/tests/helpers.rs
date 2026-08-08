// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use super::support::*;
use crate::run_summary::LayerReuse;
use crate::run_summary::VersionStatus;
use tempfile::tempdir;

// ── Additional unit tests for helpers ─────────────────────────────────

const EXCLUDE_SPEC: &str = r#"
name: testtool
target:
  registry: ocx.sh
  repository: testtool
source:
  type: github_release
  owner: owner
  repo: repo
  tag_pattern: "^v(?P<version>\\d+\\.\\d+\\.\\d+)$"
assets:
  linux/amd64:
    - "tool-linux-amd64$"
asset_type:
  type: binary
  name: tool
platforms:
  linux/amd64:
    runner: ubuntu-latest
  windows/arm64:
    runner: windows-11-arm
    exclude:
      - version: "0.16.0"
        reason: "aarch64-windows build-exe segfault"
        severity: broken
  darwin/amd64:
    runner: macos-14
    exclude:
      - version: "0.16.0"
        severity: skip
"#;

#[test]
fn collect_excluded_platforms_records_broken_only() {
    let spec: MirrorSpec = serde_yaml_ng::from_str(EXCLUDE_SPEC).unwrap();

    // windows/arm64 = broken (recorded); darwin/amd64 = skip (silent).
    let excluded = collect_excluded_platforms(&spec, "0.16.0");
    assert_eq!(
        excluded.len(),
        1,
        "only broken-severity excludes recorded: {excluded:?}"
    );
    assert_eq!(excluded[0].platform, "windows/arm64");
    assert_eq!(
        excluded[0].reason.as_deref(),
        Some("aarch64-windows build-exe segfault")
    );
}

#[test]
fn collect_excluded_platforms_strips_build_metadata() {
    let spec: MirrorSpec = serde_yaml_ng::from_str(EXCLUDE_SPEC).unwrap();
    // The bundle version carries a build stamp; the exclude is declared bare.
    let excluded = collect_excluded_platforms(&spec, "0.16.0_20260604120000");
    assert_eq!(excluded.len(), 1);
    assert_eq!(excluded[0].platform, "windows/arm64");
}

#[test]
fn collect_excluded_platforms_strips_variant_prefix() {
    let spec: MirrorSpec = serde_yaml_ng::from_str(EXCLUDE_SPEC).unwrap();
    // Variant mirrors key off variant-prefixed versions (e.g. `debug-0.16.0`);
    // the exclude is declared bare. The 🔒 row must still be recorded.
    let excluded = collect_excluded_platforms(&spec, "debug-0.16.0");
    assert_eq!(
        excluded.len(),
        1,
        "variant-prefixed version still records broken exclude: {excluded:?}"
    );
    assert_eq!(excluded[0].platform, "windows/arm64");
    // Variant + build stamp together.
    let stamped = collect_excluded_platforms(&spec, "debug-0.16.0_20260604120000");
    assert_eq!(stamped.len(), 1);
    assert_eq!(stamped[0].platform, "windows/arm64");
}

#[test]
fn collect_excluded_platforms_empty_for_unaffected_version() {
    let spec: MirrorSpec = serde_yaml_ng::from_str(EXCLUDE_SPEC).unwrap();
    assert!(collect_excluded_platforms(&spec, "0.17.0").is_empty());
}

#[test]
fn parse_bundle_filename_roundtrips() {
    // Verify parse_bundle_filename handles standard version + platform slugs.
    let cases = [
        ("bundle-3.7.0-linux_amd64.tar.xz", Some(("3.7.0", "linux_amd64"))),
        ("bundle-3.29.0-darwin_arm64.tar.xz", Some(("3.29.0", "darwin_arm64"))),
        ("bundle-1.2.3-windows_amd64.tar.xz", Some(("1.2.3", "windows_amd64"))),
        ("not-a-bundle.tar.xz", None),
        ("bundle-invalid.tar.xz", None),
    ];

    for (input, expected) in &cases {
        assert_eq!(parse_bundle_filename(input), *expected, "input: {input}");
    }
}

#[test]
fn slug_to_platform_roundtrips() {
    assert_eq!(slug_to_platform_heuristic("linux_amd64"), "linux/amd64");
    assert_eq!(slug_to_platform_heuristic("darwin_arm64"), "darwin/arm64");
    assert_eq!(slug_to_platform_heuristic("windows_amd64"), "windows/amd64");
}

#[test]
fn platform_to_slug_roundtrips() {
    assert_eq!(platform_to_slug("linux/amd64"), "linux_amd64");
    assert_eq!(platform_to_slug("darwin/arm64"), "darwin_arm64");
    assert_eq!(platform_to_slug("windows/amd64"), "windows_amd64");
}

#[test]
fn determine_status_all_pushed_is_published() {
    // D12: All platforms pushed → Published
    let status = determine_status(&["linux/amd64".to_string()], &[], false, true);
    assert!(matches!(status, VersionStatus::Published));
}

#[test]
fn determine_status_all_failed_is_failed() {
    // D12: All platforms failed → Failed
    let failed = vec![PlatformFailure {
        platform: "linux/amd64".to_string(),
        reason: "test_failed".to_string(),
        failed_tests: vec![],
        job_url: None,
    }];
    let status = determine_status(&[], &failed, false, false);
    assert!(matches!(status, VersionStatus::Failed));
}

#[test]
fn a_partial_version_reports_the_registry_truthfully() {
    // `determine_status` is a verdict, never a tag rewriter: the summary
    // reports what the registry received. A partial version carries its
    // exact version tag alone because the push loop withheld `--cascade`
    // from it, not because anything trimmed the list afterwards — and the
    // announce therefore repeats it verbatim.
    let failed = vec![PlatformFailure {
        platform: "darwin/arm64".to_string(),
        reason: "test_failed".to_string(),
        failed_tests: vec![],
        job_url: None,
    }];
    let status = determine_status(&["linux/amd64".to_string()], &failed, false, true);
    assert!(matches!(status, VersionStatus::Partial));

    let summary = VersionSummary {
        version: "3.7.0".to_string(),
        status,
        platforms_pushed: vec!["linux/amd64".to_string()],
        platforms_failed: failed,
        cascade_tags_written: vec!["3.7.0".into()],
        test_failures: vec![],
        platforms_excluded: vec![],
        layer_reuse: LayerReuse::default(),
    };
    assert_eq!(announce_tag_union(std::slice::from_ref(&summary)), vec!["3.7.0"]);
}

#[test]
fn determine_status_all_skipped_existing() {
    // D12: All skipped → SkippedExisting
    let status = determine_status(&[], &[], true, false);
    assert!(matches!(status, VersionStatus::SkippedExisting));
}

#[test]
fn evaluate_junit_returns_green_when_all_tests_pass() {
    // Unit test for evaluate_junit: all-green JUNIT for native platform.
    let junit_dir = tempdir().unwrap();
    let version = "1.0.0";
    let slug = "linux_amd64";

    write_junit(
        junit_dir.path(),
        version,
        slug,
        "_native_",
        &passing_junit(version, "linux/amd64", "_native_"),
    );

    let rt = tokio::runtime::Runtime::new().unwrap();
    let decision = rt.block_on(evaluate_junit(
        junit_dir.path(),
        version,
        &slug_to_platform_heuristic(slug),
        &["_native_".to_string()],
        &["version".to_string()],
    ));

    assert!(matches!(decision, VpDecision::Green), "All-pass JUNIT must yield Green");
}

#[test]
fn evaluate_junit_returns_red_when_declared_test_missing() {
    // A JUNIT file present but missing a declared test name → Red.
    let junit_dir = tempdir().unwrap();
    let version = "1.0.0";
    let slug = "linux_amd64";

    // Write JUNIT with only "version" test; "smoke" is declared but absent.
    write_junit(
        junit_dir.path(),
        version,
        slug,
        "_native_",
        &passing_junit(version, "linux/amd64", "_native_"),
    );

    let rt = tokio::runtime::Runtime::new().unwrap();
    let decision = rt.block_on(evaluate_junit(
        junit_dir.path(),
        version,
        &slug_to_platform_heuristic(slug),
        &["_native_".to_string()],
        // Both "version" (present) and "smoke" (missing) declared.
        &["version".to_string(), "smoke".to_string()],
    ));

    match decision {
        VpDecision::Red { test_failures, .. } => {
            assert!(
                test_failures.iter().any(|tf| tf.test == "smoke"),
                "Missing 'smoke' test must appear in test_failures"
            );
        }
        VpDecision::Green => panic!("Missing declared test must yield Red decision"),
    }
}
