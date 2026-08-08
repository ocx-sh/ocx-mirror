// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Unit tests for the `ocx package push` subprocess and its retry ladder.

use tempfile::tempdir;

use super::*;
use crate::test_support::ocx_env_lock;

#[cfg(unix)]
#[test]
fn a_hung_push_is_killed_by_the_push_timeout() {
    // Two claims, and the second is the one with teeth: the wait is
    // bounded, and the child is dead when it returns. Tokio leaves a
    // timed-out child running, so without `kill_on_drop` an orphaned push
    // keeps streaming its bundle at the registry while the retry sends the
    // same one — two writers, one tag. Observed as a marker file only a
    // survivor lives long enough to write.
    use std::os::unix::fs::PermissionsExt;

    let _env_lock = ocx_env_lock();
    let dir = tempdir().unwrap();
    let marker = dir.path().join("survived-the-timeout");
    let script = dir.path().join("hanging-ocx");
    std::fs::write(&script, format!("#!/bin/sh\nsleep 1\ntouch '{}'\n", marker.display())).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    let started = std::time::Instant::now();
    let failure = rt
        .block_on(push_once(&script, &[], Duration::from_millis(200)))
        .expect_err("a hung push must not hang the run");

    assert!(failure.message.contains("timed out"), "got: {}", failure.message);
    assert!(failure.transient, "a stall is the retryable case, not a verdict");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the timeout must bound the wait, took {:?}",
        started.elapsed(),
    );

    // Past the point a surviving child would have written the marker.
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        !marker.exists(),
        "a timed-out push must be killed, not orphaned to race its own retry",
    );
}

#[test]
fn the_retry_ladder_doubles_until_the_cap_and_then_stops() {
    // The cap is the only thing standing between a generous
    // `concurrency.max_retries` and a job parked on backoff alone, and no
    // pipeline test reaches it: the retry fixture grants two retries, so
    // nothing above attempt 2 is ever asked for and deleting `.min(...)`
    // leaves the whole suite green.
    //
    // `u32::MAX` is not a plausible spec value — it pins that the doubling
    // saturates instead of panicking, which is what makes the cap safe to
    // reach from any input at all.
    for (attempt, seconds) in [(1, 1), (2, 2), (3, 4), (6, 30), (u32::MAX, 30)] {
        assert_eq!(
            push_retry_backoff(attempt),
            Duration::from_secs(seconds),
            "attempt {attempt}",
        );
    }
}

/// The spread is a tenth and stays one: it rides on top of the cap rather
/// than replacing it, so the capped delay lands in 27–33s and a bug that
/// made the entropy the delay would put the ladder anywhere.
#[test]
fn jitter_spreads_the_delay_by_a_tenth_and_no_further() {
    // Sampled rather than asserted once: the entropy is the wall clock, so
    // a single call proves nothing about the range it can produce.
    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..200 {
        let spread = jitter(Duration::from_secs(30));
        assert!(
            (Duration::from_secs(27)..=Duration::from_secs(33)).contains(&spread),
            "got: {spread:?}",
        );
        seen.insert(spread);
    }
    // The range alone passes for a `jitter` that returns its argument, which
    // is the one mutation this test exists to catch. Distinctness is the
    // assertion that does not: two samples differ only if the spread is
    // actually applied.
    //
    // Not flaky. `subsec_nanos()` comes from `clock_gettime`, which has
    // nanosecond resolution, and each iteration allocates into a `BTreeSet`
    // — the loop period is neither zero nor a stable multiple of the 21-value
    // modulus, so 200 samples cannot collapse onto one bucket.
    assert!(
        seen.len() > 1,
        "one value across 200 samples — the delay is not being spread: {seen:?}",
    );
}

#[test]
fn only_a_temporary_fault_is_worth_retrying() {
    // The retry predicate decides whether a failed push costs one second or
    // is thrown away. 75 is the one code `ocx` promises a rerun may answer
    // differently; everything else — including 69, which it uses precisely
    // for "rerunning will not change the outcome" — answers identically on
    // the second ask, and a signal-killed child (`None`) means something
    // outside the run wants it to stop.
    assert!(
        push_exit_is_transient(Some(ExitCode::TempFail as i32)),
        "75 is the temporary fault that may clear",
    );

    for code in [
        ExitCode::Failure,
        ExitCode::UsageError,
        ExitCode::DataError,
        ExitCode::Unavailable,
        ExitCode::IoError,
        ExitCode::PermissionDenied,
        ExitCode::ConfigError,
        ExitCode::AuthError,
    ] {
        assert!(
            !push_exit_is_transient(Some(code as i32)),
            "{code:?} reproduces exactly on a retry",
        );
    }
    assert!(!push_exit_is_transient(Some(70)), "an ocx crash is not a blip");
    assert!(!push_exit_is_transient(None), "a signal-killed push is not retried");
}
