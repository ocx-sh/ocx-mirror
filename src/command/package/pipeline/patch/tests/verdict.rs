// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use ocx_lib::cli::ExitCode;

// ── closing verdict ───────────────────────────────────────────────────

fn sweep_failure() -> MirrorError {
    // 83 — Rekor unreachable: the retryable one, and the code that must
    // survive the aggregation below it.
    MirrorError::SignFailed {
        target: "ghcr.io/example/mirror".to_string(),
        code: 83,
    }
}

fn announce_failure() -> String {
    "index announce for example/index failed: token expired — 1 republished manifest(s) are live \
     and the index still points at the digests they replaced"
        .to_string()
}

/// A sweep failure keeps its own exit code — flattened into
/// `ExecutionFailed` (1) it would tell the operator to fix the wrong thing.
#[test]
fn a_sweep_failure_wins_the_exit_code_over_the_aggregation() {
    let (_, verdict) = closing_verdict(Err(sweep_failure()), vec![announce_failure()]);

    let error = verdict.expect_err("the sweep failed");
    assert_eq!(error.kind_exit_code(), ExitCode::TransparencyLogUnavailable, "{error}");
}

/// …and the failures it outranks are still reported. `patch` writes no
/// `run-summary.json`, so this vector is the run's only record of a refused
/// layout, a failed republish, or the stale index the announce failure names.
#[test]
fn a_sweep_failure_hands_back_the_failures_it_outranks() {
    let (unreported, _) = closing_verdict(Err(sweep_failure()), vec![announce_failure()]);

    assert_eq!(
        unreported,
        vec![announce_failure()],
        "the only record of the run was dropped"
    );
}

/// With the sweep green the aggregation is the verdict, and reporting is the
/// returned error's job — logging the same lines as well would print one
/// failure twice.
#[test]
fn a_green_sweep_leaves_the_failures_to_the_returned_error() {
    let (unreported, verdict) = closing_verdict(Ok(()), vec![announce_failure()]);

    assert!(unreported.is_empty(), "reported twice: {unreported:?}");
    let error = verdict.expect_err("a failure was recorded");
    assert_eq!(error.kind_exit_code(), ExitCode::Failure, "{error}");
    assert!(error.to_string().contains("token expired"), "{error}");
}

#[test]
fn a_green_sweep_with_nothing_recorded_is_a_clean_run() {
    let (unreported, verdict) = closing_verdict(Ok(()), Vec::new());

    assert!(unreported.is_empty());
    assert!(verdict.is_ok(), "{:?}", verdict.err());
}
