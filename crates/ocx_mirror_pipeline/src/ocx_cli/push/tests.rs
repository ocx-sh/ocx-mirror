// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Unit tests for the `ocx package push` subprocess and its retry ladder.

use tempfile::tempdir;

use super::*;
use ocx_mirror_test_support::ocx_env_lock;
#[cfg(unix)]
use ocx_mirror_test_support::{fake_ocx_flaky_push, push_attempts};

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
        .block_on(push_once(&script, &[], Duration::from_millis(200), None))
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
    // C-057: 83 joined the set with `--sign`. A push that landed and then
    // could not reach Rekor is the same "try again" class as a registry
    // timeout — and without this the whole version is thrown away over a
    // transparency log that was briefly down.
    assert!(
        push_exit_is_transient(Some(ExitCode::TransparencyLogUnavailable as i32)),
        "83 is a Rekor outage, which a retry can clear",
    );
    // The two neighbours are deliberately NOT in the set: 84 is a registry
    // with no Referrers API and 85 a key backend this binary was not built
    // with. Both are permanent facts, and retrying either only slows the
    // failure down.
    for permanent in [ExitCode::ReferrersUnsupported, ExitCode::UnsupportedKeyBackend] {
        assert!(
            !push_exit_is_transient(Some(permanent as i32)),
            "{permanent:?} is a permanent fact about the destination or the binary",
        );
    }

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

// ── Push retry (issue #50) ────────────────────────────────────────────
//
// The fast ladder checks live here, driving `push_with_retry` directly:
// `push_retry_delay`'s `cfg(test)` scaling only applies in this crate's own
// test build (plan D-P6). They prove the ladder honours the budget it is
// *handed* — read off the fixture spec by the test itself. The root runs the
// ladder end to end separately: `a_retried_push_lands_as_published` and
// `max_retries_one_is_exactly_two_attempts` go through `Push::execute` with
// `mirror-push-retry-one.yml` (about one real second each), and
// `max_retries_zero_is_a_single_attempt` pins the floor — all in the root's
// `push/tests/retry.rs`.

/// The single version `mirror-push-retry.yml` publishes in these tests.
#[cfg(unix)]
const PUSH_RETRY_VERSION: &str = "3.7.0";

/// The retry fixture's `concurrency.max_retries`, read from the spec file the
/// root's push tests publish with.
#[cfg(unix)]
fn retry_fixture_max_retries() -> u32 {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/mirror-push-retry.yml"
    );
    let spec: ocx_mirror_spec::MirrorSpec = serde_yaml_ng::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    spec.concurrency.max_retries
}

/// Drive one push of the retry fixture's version through the ladder.
#[cfg(unix)]
fn push_retry_fixture(script: &Path, budget: u32) -> Result<PushReport, String> {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(push_with_retry(
        script,
        &[],
        budget,
        "shfmt",
        &format!("ocx.sh/shfmt:{PUSH_RETRY_VERSION}"),
        "linux/amd64",
        None,
    ))
}

#[cfg(unix)]
#[test]
fn a_transient_push_failure_is_retried_and_the_tile_still_lands() {
    // A registry 503 on the first attempt. The bundle is built, the tests
    // are green, and the only thing between the run and a published image
    // is one more request — reporting `push_error` here throws the whole
    // leg away over a blip that costs a second to ride out.
    let _env_lock = ocx_env_lock();
    let dir = tempdir().unwrap();
    let script = fake_ocx_flaky_push(dir.path(), 1, ExitCode::TempFail as u8, PUSH_RETRY_VERSION);

    let report = push_retry_fixture(&script, retry_fixture_max_retries())
        .expect("a transient push failure must not fail the push");

    assert_eq!(push_attempts(dir.path()), 2, "the failed attempt must be retried once");
    assert_eq!(report.cascade_tags_written, vec![PUSH_RETRY_VERSION.to_string()]);
    assert_eq!(report.status.as_deref(), Some("pushed"));
}

#[cfg(unix)]
#[test]
fn push_retries_stop_at_the_budget_handed_in() {
    // The ladder is bounded (a registry that is down stays down — the run
    // must not sit there forever), and its length is the budget passed in.
    // The fixture's `max_retries: 2` is a value no plausible hardcoding
    // inside the ladder produces: the default 3 would spend four attempts,
    // an off-by-one two. Whether the root passes the spec's value at all is
    // pinned root-side (see the section comment above).
    let _env_lock = ocx_env_lock();
    let dir = tempdir().unwrap();
    let script = fake_ocx_flaky_push(dir.path(), 99, ExitCode::TempFail as u8, PUSH_RETRY_VERSION);

    let budget = retry_fixture_max_retries();
    assert_eq!(
        budget, 2,
        "the attempt count below assumes the fixture's `max_retries: 2`"
    );
    let error = push_retry_fixture(&script, budget).expect_err("an exhausted retry ladder must still fail the push");

    assert_eq!(
        push_attempts(dir.path()),
        3,
        "one attempt plus the two retries the spec grants",
    );
    assert!(error.contains("gave up after 3 attempt(s)"), "got: {error}");
}
