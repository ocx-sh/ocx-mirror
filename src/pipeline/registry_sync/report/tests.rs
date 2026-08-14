// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Report tests (C-042, and the output half of C-043).
//!
//! `summary_line` / `has_failures` are pure and asserted directly. The two
//! renderings are asserted differently: JSON is an external contract that
//! something in CI parses, so those tests round-trip the actual
//! `serde_json::to_string_pretty` call `report_registry_sync`'s `Json` arm
//! makes and assert on the parsed structure — never on a hardcoded string,
//! which would pass for the wrong reasons the moment a field is added. Plain
//! rendering writes straight to the real stdout (`Printer` has no capture
//! seam and this crate has no stdout-capture dependency to add one), so it is
//! exercised for its one externally-observable failure mode: it must not
//! panic — an empty table, a zero-length source, or an unset `detail` must
//! all render, not index-panic or divide by zero.

use super::*;

// ── Fixtures ─────────────────────────────────────────────────────────────

fn package(name: &str, outcome: PackageOutcome, detail: Option<&str>) -> PackageReport {
    PackageReport {
        name: name.to_string(),
        outcome,
        detail: detail.map(str::to_string),
    }
}

fn source(as_name: &str, short_circuited: bool, packages: Vec<PackageReport>) -> SourceReport {
    SourceReport {
        as_name: as_name.to_string(),
        short_circuited,
        packages,
    }
}

/// Every package copied, across two sources.
fn all_success_report() -> RegistrySyncReport {
    RegistrySyncReport {
        sources: vec![
            source(
                "ocx.sh",
                false,
                vec![package("kitware/cmake", PackageOutcome::Copied, None)],
            ),
            source(
                "ghcr.io",
                false,
                vec![package("bazelbuild/buildifier", PackageOutcome::Copied, None)],
            ),
        ],
        counters: RunCounters {
            total: 2,
            copied: 2,
            skipped: 0,
            failed: 0,
        },
        estimated_bytes: None,
    }
}

/// Every package fails, each carrying a detail message (S-009/S-011-shaped).
fn all_failure_report() -> RegistrySyncReport {
    RegistrySyncReport {
        sources: vec![source(
            "ocx.sh",
            false,
            vec![
                package("kitware/cmake", PackageOutcome::Failed, Some("digest mismatch")),
                package(
                    "bazelbuild/buildifier",
                    PackageOutcome::Failed,
                    Some("referrers detected"),
                ),
            ],
        )],
        counters: RunCounters {
            total: 2,
            copied: 0,
            skipped: 0,
            failed: 2,
        },
        estimated_bytes: None,
    }
}

/// One of each outcome, split across sources — the shape a real multi-source
/// `continue` run produces.
fn mixed_report() -> RegistrySyncReport {
    RegistrySyncReport {
        sources: vec![
            source(
                "ocx.sh",
                false,
                vec![
                    package("kitware/cmake", PackageOutcome::Copied, None),
                    package("bazelbuild/buildifier", PackageOutcome::Skipped, None),
                ],
            ),
            source(
                "ghcr.io",
                false,
                vec![package(
                    "acme/widget",
                    PackageOutcome::Failed,
                    Some("source unreachable"),
                )],
            ),
        ],
        counters: RunCounters {
            total: 3,
            copied: 1,
            skipped: 1,
            failed: 1,
        },
        estimated_bytes: None,
    }
}

/// A run that matched no packages at all — no sources, zero counters.
fn empty_report() -> RegistrySyncReport {
    RegistrySyncReport::default()
}

/// S-002's shape: sources present, every one short-circuited, so every
/// package list is empty even though the run touched real sources.
fn fully_short_circuited_report() -> RegistrySyncReport {
    RegistrySyncReport {
        sources: vec![source("ocx.sh", true, Vec::new()), source("ghcr.io", true, Vec::new())],
        counters: RunCounters::default(),
        estimated_bytes: None,
    }
}

/// Packages present but every one already up to date — `has_failures` must
/// not confuse "skipped" with "failed".
fn skipped_only_report() -> RegistrySyncReport {
    RegistrySyncReport {
        sources: vec![source(
            "ocx.sh",
            false,
            vec![
                package("kitware/cmake", PackageOutcome::Skipped, None),
                package("bazelbuild/buildifier", PackageOutcome::Skipped, None),
            ],
        )],
        counters: RunCounters {
            total: 2,
            copied: 0,
            skipped: 2,
            failed: 0,
        },
        estimated_bytes: None,
    }
}

fn printer() -> DataInterface {
    DataInterface::new(ocx_lib::cli::Printer::new(false, false))
}

// ── C-042 — summary_line ────────────────────────────────────────────────

#[test]
fn summary_line_reports_all_success() {
    assert_eq!(
        all_success_report().summary_line(),
        "2 total, 2 copied, 0 skipped, 0 failed"
    );
}

#[test]
fn summary_line_reports_all_failure() {
    assert_eq!(
        all_failure_report().summary_line(),
        "2 total, 0 copied, 0 skipped, 2 failed"
    );
}

#[test]
fn summary_line_reports_mixed_outcomes() {
    assert_eq!(mixed_report().summary_line(), "3 total, 1 copied, 1 skipped, 1 failed");
}

/// The C-042 non-silence guarantee: a no-op run still prints the line, with
/// `0 copied` rather than nothing.
#[test]
fn summary_line_is_not_silent_for_an_empty_run() {
    assert_eq!(empty_report().summary_line(), "0 total, 0 copied, 0 skipped, 0 failed");
}

#[test]
fn summary_line_is_not_silent_when_every_source_short_circuits() {
    assert_eq!(
        fully_short_circuited_report().summary_line(),
        "0 total, 0 copied, 0 skipped, 0 failed"
    );
}

// ── has_failures ─────────────────────────────────────────────────────────

#[test]
fn has_failures_is_false_for_all_success() {
    assert!(!all_success_report().has_failures());
}

#[test]
fn has_failures_is_true_for_all_failure() {
    assert!(all_failure_report().has_failures());
}

#[test]
fn has_failures_is_true_for_mixed_outcomes() {
    assert!(mixed_report().has_failures());
}

#[test]
fn has_failures_is_false_for_an_empty_run() {
    assert!(!empty_report().has_failures());
}

/// `Skipped` is a success shape (already present upstream and downstream),
/// never a failure — this pins the boundary explicitly rather than trusting
/// the mixed-report test to exercise it.
#[test]
fn has_failures_is_false_when_every_package_was_only_skipped() {
    assert!(!skipped_only_report().has_failures());
}

// ── JSON rendering — external contract, round-tripped rather than
//    string-matched ────────────────────────────────────────────────────────

/// The literal call `report_registry_sync`'s `Json` arm makes. Testing it
/// directly (rather than capturing the function's `println!`, which this
/// crate has no seam for — `Printer` writes straight to real stdout) still
/// exercises the exact rendering: `report_registry_sync`'s JSON branch is
/// nothing but this call plus a `println!`.
fn rendered_json(report: &RegistrySyncReport) -> serde_json::Value {
    let text = serde_json::to_string_pretty(report).expect("RegistrySyncReport always serializes");
    serde_json::from_str(&text).expect("rendered JSON must parse")
}

#[test]
fn json_rendering_parses_and_carries_the_counters_for_a_mixed_report() {
    let value = rendered_json(&mixed_report());

    assert_eq!(value["counters"]["total"], 3);
    assert_eq!(value["counters"]["copied"], 1);
    assert_eq!(value["counters"]["skipped"], 1);
    assert_eq!(value["counters"]["failed"], 1);

    let sources = value["sources"].as_array().expect("sources is an array");
    assert_eq!(sources.len(), 2);
    assert_eq!(sources[0]["as_name"], "ocx.sh");
    assert_eq!(sources[0]["packages"][0]["name"], "kitware/cmake");
    assert_eq!(sources[0]["packages"][0]["outcome"], "copied");
    assert_eq!(sources[0]["packages"][1]["outcome"], "skipped");
    assert_eq!(sources[1]["packages"][0]["outcome"], "failed");
    assert_eq!(sources[1]["packages"][0]["detail"], "source unreachable");
}

#[test]
fn json_rendering_parses_for_an_empty_report_with_zero_counters() {
    let value = rendered_json(&empty_report());

    assert_eq!(value["counters"]["total"], 0);
    assert_eq!(value["counters"]["copied"], 0);
    assert_eq!(value["counters"]["skipped"], 0);
    assert_eq!(value["counters"]["failed"], 0);
    assert_eq!(value["sources"].as_array().expect("sources is an array").len(), 0);
}

#[test]
fn json_rendering_carries_short_circuited_sources_with_empty_package_lists() {
    let value = rendered_json(&fully_short_circuited_report());

    let sources = value["sources"].as_array().expect("sources is an array");
    assert_eq!(sources.len(), 2);
    for entry in sources {
        assert_eq!(entry["short_circuited"], true);
        assert_eq!(entry["packages"].as_array().expect("packages is an array").len(), 0);
    }
}

#[test]
fn estimated_bytes_appears_in_json_only_under_dry_run() {
    let mut report = all_success_report();
    assert!(
        rendered_json(&report).get("estimated_bytes").is_none(),
        "a non-dry-run report must omit the field entirely, not serialize null"
    );

    report.estimated_bytes = Some(123_456);
    assert_eq!(rendered_json(&report)["estimated_bytes"], 123_456);
}

#[test]
fn package_detail_appears_in_json_only_for_failures() {
    let value = rendered_json(&all_success_report());
    assert!(
        value["sources"][0]["packages"][0].get("detail").is_none(),
        "a copied package has no failure detail to report"
    );

    let value = rendered_json(&all_failure_report());
    assert_eq!(value["sources"][0]["packages"][0]["detail"], "digest mismatch");
}

// ── Plain rendering — must not panic on any shape ───────────────────────

/// `report_registry_sync` writes straight to real stdout; there is no
/// capture seam in this crate to assert on the printed bytes. What every one
/// of these fixtures *can* prove is that the function returns rather than
/// panics — which is exactly what a divide-by-zero on an empty counter set,
/// or an out-of-bounds index while zipping the table's columns, would not
/// do. Run both formats so a shape-specific bug in either arm is caught.
fn assert_renders_without_panicking(report: &RegistrySyncReport) {
    let printer = printer();
    report_registry_sync(report, OutputFormat::Plain, &printer);
    report_registry_sync(report, OutputFormat::Json, &printer);
}

#[test]
fn renders_without_panicking_for_all_success() {
    assert_renders_without_panicking(&all_success_report());
}

#[test]
fn renders_without_panicking_for_all_failure() {
    assert_renders_without_panicking(&all_failure_report());
}

#[test]
fn renders_without_panicking_for_mixed_outcomes() {
    assert_renders_without_panicking(&mixed_report());
}

#[test]
fn renders_without_panicking_for_an_empty_run() {
    assert_renders_without_panicking(&empty_report());
}

#[test]
fn renders_without_panicking_when_every_source_short_circuits() {
    assert_renders_without_panicking(&fully_short_circuited_report());
}

#[test]
fn renders_without_panicking_under_dry_run_with_an_estimated_byte_count() {
    let mut report = mixed_report();
    report.estimated_bytes = Some(221_000_000);
    assert_renders_without_panicking(&report);
}

#[test]
fn renders_without_panicking_under_dry_run_with_a_zero_byte_estimate() {
    // Dry run, nothing to transfer — `estimated_bytes` is `Some(0)`, not
    // `None`: the run still executed, it just found nothing missing.
    let mut report = empty_report();
    report.estimated_bytes = Some(0);
    assert_renders_without_panicking(&report);
}

// ── The JSON arm's failure branch ───────────────────────────────────────────

/// A serialization failure must be **reported**, never dropped.
///
/// Structural, and it has to be: every field of [`RegistrySyncReport`]
/// serializes infallibly, so no fixture in this file — or anywhere — can drive
/// `to_string_pretty` into its `Err`. That is precisely why the branch needs a
/// guard rather than a test: nothing else would ever notice it being written
/// back as a swallow, and the swallow's observable behaviour is
/// **no output at all** under `--format json`, which for a run with no failed
/// package also exits 0 — indistinguishable to a parser from a run that
/// produced nothing.
///
/// Comments are stripped first: the branch's own comment explains the shape it
/// replaced, and an unstripped scan would match that explanation instead of the
/// code.
#[test]
fn the_json_arm_reports_a_serialization_failure_instead_of_dropping_it() {
    let source: String = include_str!("../report.rs")
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        source.contains(r#"Err(error) => tracing::error!("cannot render the run report as JSON: {error}")"#),
        "the Err arm must log, and must carry the cause — a bare arm says only that something went wrong"
    );
    assert!(
        !source.contains("if let Ok(json) = serde_json::to_string_pretty"),
        "the `if let Ok` form has no Err arm at all, which is the swallow this guard exists to catch"
    );
}
