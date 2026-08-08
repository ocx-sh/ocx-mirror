// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! The go/no-go decision for one `(version, platform)` pair.
//!
//! AND across containers: every declared test in every gating container leg
//! must be green, and a missing JUnit file counts as red. A platform that
//! cannot be judged is never published.

use std::path::Path;

use super::VpDecision;
use super::bundles::platform_to_slug;
use crate::junit::{self, JunitTestcase};
use crate::run_summary::{PlatformFailure, TestFailure, VersionStatus};

/// Evaluate the JUNIT files for a `(version, platform)` pair across all
/// declared container IDs, returning a go/no-go decision.
///
/// Takes the platform in slash form and slugs it here: reporting a failure
/// needs the platform, finding the file needs the slug, and the slug does not
/// reverse for a platform carrying `os.features`.
///
/// AND-logic: all containers must be green for all declared tests.
pub async fn evaluate_junit(
    junit_dir: &Path,
    version: &str,
    platform: &str,
    container_ids: &[String],
    declared_test_names: &[String],
) -> VpDecision {
    let platform_slug = platform_to_slug(platform);
    let mut platform_test_failures: Vec<TestFailure> = Vec::new();
    let mut missing_reasons: Vec<String> = Vec::new();
    // Capture the first `ci.job.url` we encounter across all containers in this
    // leg. Every container in the matrix leg shares the same matrix-leg job
    // URL, so first-non-empty wins.
    let mut job_url: Option<String> = None;

    for container_id in container_ids {
        let junit_path = junit_dir.join(format!("junit-{version}-{platform_slug}-{container_id}.xml"));

        if !junit_path.exists() {
            missing_reasons.push(format!("missing junit for container {container_id}"));
            continue;
        }

        // Parse the JUNIT file asynchronously.
        let suite = match junit::parse_async(&junit_path).await {
            Ok(s) => s,
            Err(e) => {
                missing_reasons.push(format!("parse error for {container_id}: {e}"));
                continue;
            }
        };

        if job_url.is_none() {
            job_url = suite
                .properties
                .get("ci.job.url")
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty());
        }

        // Check suite-level failure/error counts first for efficiency.
        let suite_has_failures = suite.failures > 0 || suite.errors > 0;

        // Find all failing testcases.
        let failures_in_suite: Vec<&JunitTestcase> = suite
            .testcases
            .iter()
            .filter(|tc| tc.failure_message.is_some())
            .collect();

        for failing_tc in &failures_in_suite {
            platform_test_failures.push(TestFailure {
                version: version.to_string(),
                platform: platform.to_string(),
                container: container_id.clone(),
                test: failing_tc.name.clone(),
                message: failing_tc.failure_message.clone().unwrap_or_default(),
            });
        }

        // If suite counts indicate failures but no explicit testcase had a
        // failure_message, still treat it as failed.
        if suite_has_failures && failures_in_suite.is_empty() {
            platform_test_failures.push(TestFailure {
                version: version.to_string(),
                platform: platform.to_string(),
                container: container_id.clone(),
                test: "<suite>".to_string(),
                message: format!(
                    "testsuite reports {} failure(s) and {} error(s)",
                    suite.failures, suite.errors
                ),
            });
        }

        // Check that every declared test name is present in the JUNIT.
        if !declared_test_names.is_empty() {
            let found_names: std::collections::HashSet<&str> =
                suite.testcases.iter().map(|tc| tc.name.as_str()).collect();
            for expected_name in declared_test_names {
                if !found_names.contains(expected_name.as_str()) {
                    platform_test_failures.push(TestFailure {
                        version: version.to_string(),
                        platform: platform.to_string(),
                        container: container_id.clone(),
                        test: expected_name.clone(),
                        message: format!("test '{expected_name}' not found in JUNIT"),
                    });
                }
            }
        }
    }

    // Missing JUNIT files count as failures.
    if !missing_reasons.is_empty() {
        let reason = missing_reasons.join("; ");
        let failure = PlatformFailure {
            platform: platform.to_string(),
            reason: "missing_junit".to_string(),
            failed_tests: vec![],
            job_url: job_url.clone(),
        };
        return VpDecision::Red {
            platform_failure: failure,
            test_failures: vec![TestFailure {
                version: version.to_string(),
                platform: platform.to_string(),
                container: "_missing_".to_string(),
                test: "<junit>".to_string(),
                message: reason,
            }],
        };
    }

    if platform_test_failures.is_empty() {
        VpDecision::Green
    } else {
        let failure = PlatformFailure {
            platform: platform.to_string(),
            reason: "test_failed".to_string(),
            failed_tests: platform_test_failures.clone(),
            job_url,
        };
        VpDecision::Red {
            platform_failure: failure,
            test_failures: platform_test_failures,
        }
    }
}

/// Determine the `VersionStatus` for a version based on push outcomes.
///
/// A verdict, not a tag rewriter. `cascade_tags_written` records what the
/// registry actually received; editing it here would make `run-summary.json`
/// and the Discord report describe a registry state that does not exist. A
/// `Partial` version carries only its exact `X.Y.Z` because the push loop
/// never gave it `--cascade`, not because anything trimmed the list.
///
/// The `is_newest` flag is informational — the `ocx package push --cascade`
/// subprocess handles `latest` tag writes internally based on cascade version
/// ordering.
pub fn determine_status(
    platforms_pushed: &[String],
    platforms_failed: &[PlatformFailure],
    all_skipped_existing: bool,
    _is_newest: bool,
) -> VersionStatus {
    if all_skipped_existing && platforms_pushed.is_empty() && platforms_failed.is_empty() {
        return VersionStatus::SkippedExisting;
    }

    if platforms_pushed.is_empty() && !platforms_failed.is_empty() {
        // All platforms failed.
        return VersionStatus::Failed;
    }

    if !platforms_pushed.is_empty() && platforms_failed.is_empty() {
        // All platforms pushed successfully.
        // The cascade tags are whatever the push subprocess returned. If `latest`
        // was not returned by the subprocess but should be written, the subprocess
        // handles that internally (ocx package push --cascade logic).
        // We don't inject `latest` ourselves — trust the subprocess output.
        return VersionStatus::Published;
    }

    // Mixed: some pushed, some failed.
    VersionStatus::Partial
}
