// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::*;

fn archive(name: &str, outcome: ArchiveOutcome, detail: Option<&str>) -> ArchiveReport {
    ArchiveReport {
        name: name.to_string(),
        outcome,
        detail: detail.map(str::to_string),
    }
}

#[test]
fn a_clean_run_reports_no_failures() {
    let report = DistSyncReport {
        counters: RunCounters {
            total: 2,
            copied: 2,
            ..RunCounters::default()
        },
        ..DistSyncReport::default()
    };

    assert!(!report.has_failures());
    assert!(
        report.failures().is_none(),
        "a clean run must not produce an error carrying nothing at exit 1"
    );
}

#[test]
fn a_failed_archive_is_named_in_the_error() {
    let report = DistSyncReport {
        archives: vec![
            archive("0.5.8/x86_64-unknown-linux-gnu", ArchiveOutcome::Copied, None),
            archive(
                "0.5.8/aarch64-apple-darwin",
                ArchiveOutcome::Failed,
                Some("digest mismatch"),
            ),
        ],
        counters: RunCounters {
            total: 2,
            copied: 1,
            failed: 1,
            ..RunCounters::default()
        },
        ..DistSyncReport::default()
    };

    let errors = report.failures().expect("a failed run must produce errors");

    assert_eq!(errors, vec!["0.5.8/aarch64-apple-darwin: digest mismatch".to_string()]);
}

/// Counters are derived from the rows rather than incremented beside them, so
/// the summary line cannot disagree with the table printed above it.
#[test]
fn the_counters_are_derived_from_the_rows() {
    let archives = vec![
        archive("0.5.8/linux", ArchiveOutcome::Copied, None),
        archive("0.5.8/darwin", ArchiveOutcome::Skipped, None),
        archive("0.4.0/linux", ArchiveOutcome::Failed, Some("digest mismatch")),
        archive("0.4.0/darwin", ArchiveOutcome::Planned, None),
    ];

    let counters = RunCounters::from_archives(&archives, 7, 2);

    assert_eq!(
        counters,
        RunCounters {
            total: 4,
            copied: 1,
            skipped: 1,
            failed: 1,
            uploaded: 7,
            already_present: 2,
        },
        "a `planned` row counts toward the total and nothing else"
    );
}

/// The pair the derivation exists to keep honest: whatever `failures()` finds
/// is exactly what `counters.failed` reports.
#[test]
fn the_failure_count_always_matches_the_failure_messages() {
    let archives = vec![
        archive("0.5.8/linux", ArchiveOutcome::Copied, None),
        archive("0.4.0/linux", ArchiveOutcome::Failed, Some("digest mismatch")),
        archive("0.4.0/darwin", ArchiveOutcome::Failed, Some("connection reset")),
    ];
    let report = DistSyncReport {
        counters: RunCounters::from_archives(&archives, 0, 0),
        archives,
        ..DistSyncReport::default()
    };

    let errors = report.failures().expect("two failed rows must produce errors");

    assert_eq!(errors.len(), report.counters.failed);
    assert!(report.has_failures());
}

/// Silence is indistinguishable from "did not run" in a CI log.
#[test]
fn a_no_op_run_still_prints_a_summary_line() {
    let summary = DistSyncReport::default().summary_line();

    assert_eq!(
        summary,
        "0 total, 0 copied, 0 skipped, 0 failed; 0 uploaded, 0 already present"
    );
}

#[test]
fn the_json_outcome_labels_match_the_plain_ones() {
    for outcome in [
        ArchiveOutcome::Copied,
        ArchiveOutcome::Skipped,
        ArchiveOutcome::Planned,
        ArchiveOutcome::Failed,
    ] {
        let json = serde_json::to_string(&outcome).expect("an outcome must serialize");
        assert_eq!(
            json.trim_matches('"'),
            outcome.label(),
            "the two renderings must agree on the spelling"
        );
    }
}

#[test]
fn the_manifest_digest_is_omitted_from_json_when_no_manifest_was_written() {
    let json = serde_json::to_string(&DistSyncReport::default()).expect("the report must serialize");

    assert!(
        !json.contains("manifest_sha256"),
        "a dry run or a failed run publishes no manifest, and must not claim a digest: {json}"
    );
}
