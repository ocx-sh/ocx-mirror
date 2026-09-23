// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! The declared-digest check on the resume path (#75 / #76).
//!
//! `prepare_task` returns early when `bundle.tar.xz` already exists, before
//! the download-and-verify block. The digest is the one control in that block
//! a resume can still run — it reads the downloaded archive, not the content
//! tree that run discarded — and it is what makes a rewritten download host
//! safe, so it must not be skipped.

use super::super::*;
use super::support::*;

/// The digest of an asset that is *not* the staged one.
const WRONG_DIGEST: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000000";

/// Leaves a `bundle.tar.xz` and its archive in `task_dir`, as an interrupted
/// or partially-pushed earlier run does.
#[cfg(unix)]
async fn first_run(spec_dir: &Path, task_dir: &Path) {
    let task = offline_task(spec_dir, BinScanMode::Off, true);
    run_prepare(&task, task_dir).await.expect("first run succeeds");
    assert!(task_dir.join("bundle.tar.xz").exists(), "first run must leave a bundle");
    assert!(
        task_dir.join("asset.tar.xz").exists(),
        "and the archive the digest is checked against",
    );
}

/// The hole: a work dir carrying a bundle from a run made under a weaker
/// policy was republished without the declared digest ever being compared, so
/// hardening the spec to `require` — or a corrected upstream digest — never
/// reached it.
#[cfg(unix)]
#[tokio::test]
async fn a_resume_re_checks_the_declared_digest() {
    let spec = spec_dir_declaring("");
    let work = tempfile::tempdir().expect("tempdir");
    let task_dir = work.path().join("resume");
    first_run(spec.path(), &task_dir).await;

    let mut task = offline_task(spec.path(), BinScanMode::Off, true);
    task.asset_digest = Some(WRONG_DIGEST.to_string());
    let error = prepare_as_is(&task, &task_dir)
        .await
        .expect_err("a resumed bundle whose archive fails the declared digest must not be adopted");
    let rendered = format!("{error:#}");
    assert!(rendered.contains("digest"), "got: {rendered}");
}

/// A matching digest is the common resume and must stay green — the check
/// costs one hash of a file already on disk.
#[cfg(unix)]
#[tokio::test]
async fn a_resume_with_a_matching_digest_still_succeeds() {
    let spec = spec_dir_declaring("");
    let work = tempfile::tempdir().expect("tempdir");
    let task_dir = work.path().join("resume");
    first_run(spec.path(), &task_dir).await;

    use sha2::Digest as _;
    let staged = tokio::fs::read(task_dir.join("asset.tar.xz")).await.expect("archive");
    let digest = format!("sha256:{}", hex::encode(sha2::Sha256::digest(&staged)));

    let mut task = offline_task(spec.path(), BinScanMode::Off, true);
    task.asset_digest = Some(digest);
    prepare_as_is(&task, &task_dir).await.expect("resume succeeds");
}

/// Fail-closed: with the archive gone there is nothing left to hash, so the
/// bundle is unverifiable. Adopting it would publish bytes no run ever
/// checked against the publisher's declaration.
#[cfg(unix)]
#[tokio::test]
async fn a_resume_whose_archive_is_gone_refuses_the_bundle() {
    let spec = spec_dir_declaring("");
    let work = tempfile::tempdir().expect("tempdir");
    let task_dir = work.path().join("resume");
    first_run(spec.path(), &task_dir).await;
    tokio::fs::remove_file(task_dir.join("asset.tar.xz"))
        .await
        .expect("drop the archive");

    let mut task = offline_task(spec.path(), BinScanMode::Off, true);
    task.asset_digest = Some(WRONG_DIGEST.to_string());
    let error = prepare_as_is(&task, &task_dir)
        .await
        .expect_err("an unverifiable bundle must not be adopted");
    let rendered = format!("{error:#}");
    assert!(rendered.contains("digest"), "got: {rendered}");
    assert!(
        rendered.contains("delete the bundle"),
        "the operator needs the way out: {rendered}"
    );
}

/// `require` with nothing declared fails the fresh path; a bundle lying
/// around must not turn it green.
#[cfg(unix)]
#[tokio::test]
async fn a_resume_under_require_with_no_declared_digest_fails() {
    let spec = spec_dir_declaring("");
    let work = tempfile::tempdir().expect("tempdir");
    let task_dir = work.path().join("resume");
    first_run(spec.path(), &task_dir).await;

    let mut task = offline_task(spec.path(), BinScanMode::Off, true);
    task.require_digest = true;
    let error = prepare_as_is(&task, &task_dir)
        .await
        .expect_err("require with nothing declared must fail on a resume too");
    assert!(format!("{error:#}").contains("'require'"), "got: {error:#}");
}

/// The control: no digest and no `require` is the overwhelmingly common
/// resume, and it must be untouched by the new check.
#[cfg(unix)]
#[tokio::test]
async fn a_resume_without_a_digest_is_unchanged() {
    let spec = spec_dir_declaring("");
    let work = tempfile::tempdir().expect("tempdir");
    let task_dir = work.path().join("resume");
    first_run(spec.path(), &task_dir).await;

    let task = offline_task(spec.path(), BinScanMode::Off, true);
    prepare_as_is(&task, &task_dir).await.expect("resume succeeds");
}
