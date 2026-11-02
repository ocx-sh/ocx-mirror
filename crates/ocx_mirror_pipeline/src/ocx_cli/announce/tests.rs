// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::AnnounceReport;

/// A report from ocx 0.6.3 carries none of the branch fields: it must still
/// parse, and say nothing noteworthy.
#[test]
fn a_legacy_report_parses_and_has_no_branch_note() {
    let report: AnnounceReport = serde_json::from_str(r#"{"status":"updated","pull_request_url":"u"}"#).unwrap();
    assert_eq!(report.branch_note(), None);
}

/// The newer fields deserialize and the note names the noteworthy ones only.
#[test]
fn a_stale_rebuild_is_named() {
    let report: AnnounceReport = serde_json::from_str(
        r#"{"status":"updated","branch_state":"stale","base_sha":"abc","ref_update":"reset",
            "carried":["1.0","2.0"],"pull_request_number":7,"pull_request_author":"octocat",
            "removed":["0.1"],"durable_missing":["0.2","0.3"]}"#,
    )
    .unwrap();
    let note = report.branch_note().unwrap();
    for needle in [
        "stale announce branch rebuilt",
        "branch ref reset",
        "2 tag(s) carried",
        "opened by octocat",
        "1 gone tag(s) removed",
        "2 durable tag(s) missing",
    ] {
        assert!(note.contains(needle), "{needle} missing from: {note}");
    }
}

/// A live branch fast-forwarded is routine: no note.
#[test]
fn a_live_fast_forward_is_silent() {
    let report: AnnounceReport =
        serde_json::from_str(r#"{"status":"updated","branch_state":"live","ref_update":"fast_forward"}"#).unwrap();
    assert_eq!(report.branch_note(), None);
}
