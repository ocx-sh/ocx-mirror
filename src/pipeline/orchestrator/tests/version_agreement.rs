// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! [`prepare_version`] names the whole run after one tag, so every task it is
//! handed has to agree on that tag.
//!
//! Issue #35 was two names for one thing. Deriving the name from
//! `tasks.first()` fixed it for a single-variant spec and left it open for a
//! `variants:` one: `build_tasks_for_version` matches a bare `--version`
//! against *every* effective variant, so `3.7.0` produces tasks carrying both
//! `3.7.0_<stamp>` and `slim-3.7.0_<stamp>`. The manifest went under whichever
//! variant happened to be first, listed every variant's bundles, and collided
//! two `linux_amd64` rows in one `bundles` array — while the other variant's
//! directory got no manifest at all. Reordering `variants:` moved the file.

use super::super::*;
use super::support::*;

/// The pipeline's concurrency knobs; the guard under test runs before any of
/// them matter, so one slot each is enough.
fn concurrency() -> ConcurrencyParams {
    ConcurrencyParams {
        max_downloads: 1,
        max_bundles: 1,
        compression_threads: 1,
    }
}

/// Two tasks, two tags: refused before a byte is downloaded.
///
/// The `--plan` path already refuses this (`version '{v}' names N plan
/// entries`); this is the same refusal for the path that has no plan.
#[cfg(unix)]
#[tokio::test]
async fn tasks_disagreeing_on_the_normalized_version_are_refused() {
    let spec_dir = tempfile::tempdir().expect("temp spec dir");
    let work_dir = tempfile::tempdir().expect("temp work dir");

    let mut default_variant = offline_task(spec_dir.path(), BinScanMode::Off, false);
    default_variant.normalized_version = "3.7.0_20260921120000".into();
    let mut slim_variant = offline_task(spec_dir.path(), BinScanMode::Off, false);
    slim_variant.normalized_version = "slim-3.7.0_20260921120000".into();

    let error = prepare_version(
        &[default_variant, slim_variant],
        work_dir.path(),
        &reqwest::Client::new(),
        &concurrency(),
    )
    .await
    .expect_err("two tags in one run must be refused");

    let message = error.to_string();
    for expected in [
        "3.7.0_20260921120000",
        "slim-3.7.0_20260921120000",
        "one version per run",
    ] {
        assert!(message.contains(expected), "{expected:?} missing from: {message}");
    }
    assert_eq!(
        error.kind_exit_code(),
        ocx_exit::ExitCode::UsageError,
        "an ambiguous --version is a usage error, not a failed run"
    );
}

/// The agreeing case still runs — the guard must not refuse the normal shape.
///
/// Asserts only that the refusal did *not* fire; the run itself then fails on
/// the unreachable download host, which is the point where this test stops.
#[cfg(unix)]
#[tokio::test]
async fn tasks_agreeing_on_the_normalized_version_pass_the_guard() {
    let spec_dir = tempfile::tempdir().expect("temp spec dir");
    let work_dir = tempfile::tempdir().expect("temp work dir");

    let first = offline_task(spec_dir.path(), BinScanMode::Off, false);
    let mut second = offline_task(spec_dir.path(), BinScanMode::Off, false);
    second.platform = platform("linux/arm64");

    let error = prepare_version(
        &[first, second],
        work_dir.path(),
        &reqwest::Client::new(),
        &concurrency(),
    )
    .await
    .expect_err("example.invalid does not resolve");

    assert!(
        !error.to_string().contains("one version per run"),
        "the guard fired on tasks that agree: {error}"
    );
}
