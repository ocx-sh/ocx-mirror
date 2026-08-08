// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use std::time::Duration;

use super::super::*;
use super::support::*;
use crate::pipeline::ocx_cli::announce::build_announce_args;
use crate::run_summary::VersionStatus;
use tempfile::tempdir;

// ── Index announce (E-P4) ─────────────────────────────────────────────
//
// One announce per run, carrying the union of every cascade tag the run
// wrote. `--tags-from-file` (additive) never `--tags` (replacing), because a
// mirror announcing a replacing tag set would delete every previously
// announced version the moment one run published a new one.

fn announce_config() -> AnnounceConfig {
    serde_yaml_ng::from_str("package: bazelbuild/bazelisk\nfork: ocx-contrib/index\nindex_repo: ocx-sh/index\n")
        .unwrap()
}

/// A stand-in `ocx` that appends its argv (one invocation per line) to
/// `log`. Lets the announce subprocess boundary be exercised without
/// mutating process environment.
#[cfg(unix)]
/// An `ocx` that reports a real announce: tags curated, pull request opened.
fn fake_ocx(dir: &Path, log: &Path, exit_code: u8) -> PathBuf {
    fake_ocx_reporting(
        dir,
        log,
        exit_code,
        r#"{"status":"updated","pull_request_url":"https://github.com/ocx-sh/index/pull/81"}"#,
    )
}

/// An `ocx` that exits `exit_code` after printing `report` on stdout.
///
/// The report is the whole point: `ocx package announce` exits 0 whether it
/// curated tags or changed nothing, so a stub that only sets an exit code
/// cannot express the difference the caller has to detect.
fn fake_ocx_reporting(dir: &Path, log: &Path, exit_code: u8, report: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = dir.join("fake-ocx");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncat <<'EOF'\n{report}\nEOF\nexit {exit_code}\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

#[test]
fn announce_tag_union_dedups_across_versions_and_platforms() {
    // Each platform's push report re-lists the same cascade hierarchy, and
    // consecutive versions share the rolling `1` / `latest` tags. The
    // union must carry each tag exactly once, in run order.
    let versions = vec![
        version_summary(
            "1.20.0",
            VersionStatus::Published,
            &["linux/amd64", "darwin/arm64"],
            &["1.20.0", "1.20", "1", "latest"],
        ),
        version_summary(
            "1.21.0",
            VersionStatus::Published,
            &["linux/amd64"],
            &["1.21.0", "1.21", "1", "latest"],
        ),
    ];

    assert_eq!(
        announce_tag_union(&versions),
        vec!["1.20.0", "1.20", "1", "latest", "1.21.0", "1.21"],
    );
}

#[test]
fn announce_tag_union_covers_partial_versions_but_not_unpublished_ones() {
    // Partial with at least one platform pushed still wrote its exact
    // version tag → include that. Failed / skipped_existing wrote nothing
    // new → exclude, so a run that published nothing announces nothing.
    let versions = vec![
        version_summary("1.0.0", VersionStatus::SkippedExisting, &[], &["1.0.0"]),
        version_summary("2.0.0", VersionStatus::Failed, &[], &[]),
        version_summary("3.0.0", VersionStatus::Partial, &["linux/amd64"], &["3.0.0"]),
    ];

    assert_eq!(announce_tag_union(&versions), vec!["3.0.0"]);

    let nothing_published = vec![
        version_summary("1.0.0", VersionStatus::SkippedExisting, &[], &["1.0.0"]),
        version_summary("2.0.0", VersionStatus::Failed, &[], &[]),
    ];
    assert!(announce_tag_union(&nothing_published).is_empty());
}

#[test]
fn a_run_with_a_partial_version_still_announces_the_whole_one_in_full() {
    // bazelisk 1.21.0 on linux + darwin, darwin red, alongside a fully
    // published 1.20.0. `latest` and `1` are announced — in the registry
    // they still point at 1.20.0's complete index, because the push loop
    // never gave 1.21.0 `--cascade`. Filtering them here (the shape this
    // replaces) suppressed a truthful alias while doing nothing about the
    // untruthful one, which `announce`'s re-observation of the already
    // curated set would have re-committed regardless.
    let versions = vec![
        version_summary(
            "1.20.0",
            VersionStatus::Published,
            &["linux/amd64", "darwin/arm64"],
            &["1.20.0", "1.20", "1", "latest"],
        ),
        version_summary("1.21.0", VersionStatus::Partial, &["linux/amd64"], &["1.21.0"]),
    ];

    assert_eq!(
        announce_tag_union(&versions),
        vec!["1.20.0", "1.20", "1", "latest", "1.21.0"],
    );
}

#[test]
fn build_announce_args_uses_additive_tags_file_never_replacing_tags() {
    let tags = ["1.20.0".to_string()];
    let source = TagSource::File {
        path: Path::new("/tmp/tags.txt"),
        tags: &tags,
    };
    let args = build_announce_args(&announce_config(), &source, None).unwrap();

    assert_eq!(
        args,
        vec![
            "--format",
            "json",
            "package",
            "announce",
            "--package",
            "bazelbuild/bazelisk",
            "--tags-from-file",
            "/tmp/tags.txt",
            "--fork",
            "ocx-contrib/index",
            "--index-repo",
            "ocx-sh/index",
        ],
    );
    assert!(
        !args.iter().any(|a| a == "--tags"),
        "--tags REPLACES the curated set — a mirror must never use it",
    );
}

#[test]
fn build_announce_args_from_registry_is_additive_and_never_replacing_tags() {
    let args = build_announce_args(&announce_config(), &TagSource::FromRegistry, None).unwrap();

    assert_eq!(
        args,
        vec![
            "--format",
            "json",
            "package",
            "announce",
            "--package",
            "bazelbuild/bazelisk",
            "--tags-from-registry",
            "--fork",
            "ocx-contrib/index",
            "--index-repo",
            "ocx-sh/index",
        ],
    );
    // Same invariant as the file mode: catching a mirror up must never be
    // able to drop a tag the index already commits.
    assert!(
        !args.iter().any(|a| a == "--tags"),
        "--tags REPLACES the curated set — a mirror must never use it",
    );
}

/// `--dry-run` must not be able to open a pull request.
///
/// `--out` and `--fork` are mutually exclusive on the `ocx` side, so
/// emitting both would fail the call outright; emitting `--fork` alone
/// would make a "dry" run write to the index for real.
#[test]
fn build_announce_args_dry_run_writes_out_instead_of_opening_a_pull_request() {
    let out = Path::new("/tmp/announce-out");
    let args = build_announce_args(&announce_config(), &TagSource::FromRegistry, Some(out)).unwrap();

    assert_eq!(
        args,
        vec![
            "--format",
            "json",
            "package",
            "announce",
            "--package",
            "bazelbuild/bazelisk",
            "--tags-from-registry",
            "--out",
            "/tmp/announce-out",
            "--index-repo",
            "ocx-sh/index",
        ],
    );
    assert!(
        !args.iter().any(|a| a == "--fork"),
        "a dry run must not carry --fork; got: {args:?}",
    );
}

#[cfg(unix)]
#[test]
fn announce_runs_exactly_once_per_run_with_the_union_of_tags() {
    let dir = tempdir().unwrap();
    let log = dir.path().join("invocations.log");
    let ocx = fake_ocx(dir.path(), &log, 0);
    let tags_file = dir.path().join("run-summary.announce-tags");
    let config = announce_config();

    let versions = vec![
        version_summary(
            "1.20.0",
            VersionStatus::Published,
            &["linux/amd64"],
            &["1.20.0", "1.20"],
        ),
        version_summary(
            "1.21.0",
            VersionStatus::Published,
            &["linux/amd64"],
            &["1.21.0", "1.21"],
        ),
    ];

    let rt = tokio::runtime::Runtime::new().unwrap();
    let outcome = rt.block_on(run_announce(
        Some(&config),
        &versions,
        &tags_file,
        Some("gh-token"),
        &ocx,
    ));

    assert_eq!(
        outcome,
        Some(AnnounceOutcome::Announced {
            package: "bazelbuild/bazelisk".to_string(),
            tags: vec![
                "1.20.0".to_string(),
                "1.20".to_string(),
                "1.21.0".to_string(),
                "1.21".to_string()
            ],
            pull_request_url: Some("https://github.com/ocx-sh/index/pull/81".to_string()),
        }),
    );

    // Exactly one subprocess, not one per version and not one per platform.
    let invocations = std::fs::read_to_string(&log).unwrap();
    assert_eq!(
        invocations.lines().count(),
        1,
        "announce must run once per pipeline run, got: {invocations}",
    );
    assert!(invocations.contains("--tags-from-file"), "got: {invocations}");

    // The tag set travels in the file, so the whole union lands even when
    // it outgrows anything comfortable on a command line.
    let written = std::fs::read_to_string(&tags_file).unwrap();
    assert_eq!(written, "1.20.0\n1.20\n1.21.0\n1.21");
}

/// A no-op announce must not be recorded as `announced`.
///
/// `ocx package announce` exits 0 whether it curated tags or found the
/// index already current, so this outcome used to be read off the exit
/// status and every no-op was filed as a success. Run `30241738383`
/// reported `announced` having done nothing; the index's tags had come
/// from a hand-made PR twenty minutes earlier.
#[cfg(unix)]
#[test]
fn an_announce_that_changed_nothing_is_not_recorded_as_announced() {
    let dir = tempdir().unwrap();
    let log = dir.path().join("invocations.log");
    let ocx = fake_ocx_reporting(
        dir.path(),
        &log,
        0,
        r#"{"status":"unchanged","pull_request_url":null,"written_paths":[]}"#,
    );
    let versions = vec![version_summary(
        "1.20.0",
        VersionStatus::Published,
        &["linux/amd64"],
        &["1.20.0"],
    )];

    let rt = tokio::runtime::Runtime::new().unwrap();
    let outcome = rt.block_on(run_announce(
        Some(&announce_config()),
        &versions,
        &dir.path().join("tags"),
        Some("gh-token"),
        &ocx,
    ));

    assert_eq!(
        outcome,
        Some(AnnounceOutcome::AlreadyCurrent {
            package: "bazelbuild/bazelisk".to_string(),
        }),
        "an unchanged announce with no pull request changed nothing",
    );

    // The call was made — this is not `nothing_to_announce`, and the
    // summary must not let the two collapse into one another.
    assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 1);
    let status = serde_json::to_value(outcome.unwrap()).unwrap()["status"].clone();
    assert_eq!(status, "already_current");
}

/// The other direction: a real announce still records as `announced`, and
/// carries the pull request that proves it.
#[cfg(unix)]
#[test]
fn an_announce_that_opened_a_pull_request_records_it() {
    let dir = tempdir().unwrap();
    let log = dir.path().join("invocations.log");
    let ocx = fake_ocx_reporting(
        dir.path(),
        &log,
        0,
        r#"{"status":"updated","pull_request_url":"https://github.com/ocx-sh/index/pull/81"}"#,
    );
    let versions = vec![version_summary(
        "1.20.0",
        VersionStatus::Published,
        &["linux/amd64"],
        &["1.20.0"],
    )];

    let rt = tokio::runtime::Runtime::new().unwrap();
    let outcome = rt.block_on(run_announce(
        Some(&announce_config()),
        &versions,
        &dir.path().join("tags"),
        Some("gh-token"),
        &ocx,
    ));

    assert_eq!(
        outcome,
        Some(AnnounceOutcome::Announced {
            package: "bazelbuild/bazelisk".to_string(),
            tags: vec!["1.20.0".to_string()],
            pull_request_url: Some("https://github.com/ocx-sh/index/pull/81".to_string()),
        }),
    );
}

/// An `unchanged` run that still ensured a pull request *did* announce:
/// its branch is ahead of the index base, so the tags are pending review
/// exactly as a fresh run's would be.
#[cfg(unix)]
#[test]
fn an_unchanged_announce_with_an_ensured_pull_request_still_counts() {
    let dir = tempdir().unwrap();
    let log = dir.path().join("invocations.log");
    let ocx = fake_ocx_reporting(
        dir.path(),
        &log,
        0,
        r#"{"status":"unchanged","pull_request_url":"https://github.com/ocx-sh/index/pull/81"}"#,
    );
    let versions = vec![version_summary(
        "1.20.0",
        VersionStatus::Published,
        &["linux/amd64"],
        &["1.20.0"],
    )];

    let rt = tokio::runtime::Runtime::new().unwrap();
    let outcome = rt.block_on(run_announce(
        Some(&announce_config()),
        &versions,
        &dir.path().join("tags"),
        Some("gh-token"),
        &ocx,
    ));

    assert!(
        matches!(outcome, Some(AnnounceOutcome::Announced { .. })),
        "got: {outcome:?}",
    );
}

/// An unreadable report is an unknown, not a success.
#[cfg(unix)]
#[test]
fn an_announce_reporting_nothing_readable_is_recorded_as_failed() {
    let dir = tempdir().unwrap();
    let log = dir.path().join("invocations.log");
    let ocx = fake_ocx_reporting(dir.path(), &log, 0, "announced 3 tags");
    let versions = vec![version_summary(
        "1.20.0",
        VersionStatus::Published,
        &["linux/amd64"],
        &["1.20.0"],
    )];

    let rt = tokio::runtime::Runtime::new().unwrap();
    let outcome = rt.block_on(run_announce(
        Some(&announce_config()),
        &versions,
        &dir.path().join("tags"),
        Some("gh-token"),
        &ocx,
    ));

    assert!(
        matches!(outcome, Some(AnnounceOutcome::Failed { .. })),
        "got: {outcome:?}",
    );
}

#[cfg(unix)]
#[test]
fn announce_skipped_without_token_and_stays_distinguishable_in_the_summary() {
    let dir = tempdir().unwrap();
    let log = dir.path().join("invocations.log");
    let ocx = fake_ocx(dir.path(), &log, 0);
    let config = announce_config();
    let versions = vec![version_summary(
        "1.20.0",
        VersionStatus::Published,
        &["linux/amd64"],
        &["1.20.0"],
    )];

    let rt = tokio::runtime::Runtime::new().unwrap();
    let outcome = rt.block_on(run_announce(
        Some(&config),
        &versions,
        &dir.path().join("tags"),
        None,
        &ocx,
    ));

    assert_eq!(
        outcome,
        Some(AnnounceOutcome::SkippedNoCredential {
            package: "bazelbuild/bazelisk".to_string(),
        }),
    );
    assert!(!log.exists(), "no token must mean no announce subprocess");

    // A run that pushed and skipped announcing must not read like one that
    // announced, nor like one that tried and failed.
    let rendered = |o: &AnnounceOutcome| serde_json::to_value(o).unwrap()["status"].clone();
    assert_eq!(rendered(&outcome.unwrap()), "skipped_no_credential");
    assert_eq!(
        rendered(&AnnounceOutcome::Announced {
            package: "p/q".into(),
            tags: vec![],
            pull_request_url: None,
        }),
        "announced",
    );
    assert_eq!(
        rendered(&AnnounceOutcome::Failed {
            package: "p/q".into(),
            error: "boom".into()
        }),
        "failed",
    );
}

#[cfg(unix)]
#[test]
fn announce_failure_is_recorded_and_does_not_abort_the_run() {
    let dir = tempdir().unwrap();
    let log = dir.path().join("invocations.log");
    let ocx = fake_ocx(dir.path(), &log, 70);
    let versions = vec![version_summary(
        "1.20.0",
        VersionStatus::Published,
        &["linux/amd64"],
        &["1.20.0"],
    )];

    let rt = tokio::runtime::Runtime::new().unwrap();
    let outcome = rt.block_on(run_announce(
        Some(&announce_config()),
        &versions,
        &dir.path().join("tags"),
        Some("gh-token"),
        &ocx,
    ));

    match outcome {
        Some(AnnounceOutcome::Failed { package, error }) => {
            assert_eq!(package, "bazelbuild/bazelisk");
            assert!(error.contains("ocx package announce exited"), "got: {error}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[cfg(unix)]
#[test]
fn nothing_to_announce_stays_distinct_from_never_configured() {
    // Both make no call. They need very different fixes, though: the first
    // is the steady state of an up-to-date mirror *and* the permanent state
    // of one whose `announce:` block was added after everything had already
    // published — where the catch-up is manual. Collapsing both to `None`
    // makes that owner read forever-silence as success.
    let dir = tempdir().unwrap();
    let log = dir.path().join("invocations.log");
    let ocx = fake_ocx(dir.path(), &log, 0);
    let rt = tokio::runtime::Runtime::new().unwrap();

    // Configured, but the run published nothing.
    let barren = vec![version_summary(
        "1.0.0",
        VersionStatus::SkippedExisting,
        &[],
        &["1.0.0"],
    )];
    let outcome = rt.block_on(run_announce(
        Some(&announce_config()),
        &barren,
        &dir.path().join("tags"),
        Some("gh-token"),
        &ocx,
    ));
    assert_eq!(
        outcome,
        Some(AnnounceOutcome::NothingToAnnounce {
            package: "bazelbuild/bazelisk".to_string(),
        }),
    );
    assert_eq!(
        serde_json::to_value(outcome.unwrap()).unwrap()["status"],
        "nothing_to_announce",
    );

    // Published, but no `announce:` block — announce is opt-in.
    let published = vec![version_summary(
        "1.0.0",
        VersionStatus::Published,
        &["linux/amd64"],
        &["1.0.0"],
    )];
    assert_eq!(
        rt.block_on(run_announce(
            None,
            &published,
            &dir.path().join("tags"),
            Some("t"),
            &ocx
        )),
        None,
    );

    assert!(!log.exists(), "neither case may spawn an announce subprocess");
}

#[cfg(unix)]
#[test]
fn announce_writes_its_tags_file_into_a_not_yet_existing_directory() {
    // `--write-summary out/run-summary.json` with `out/` absent: the tags
    // file is a sibling, and the announce runs before the summary write
    // that would have created the directory. Without a create_dir_all here
    // every announce under such a path is a deterministic failure.
    let dir = tempdir().unwrap();
    let log = dir.path().join("invocations.log");
    let ocx = fake_ocx(dir.path(), &log, 0);
    let tags_file = dir.path().join("out").join("run-summary.announce-tags");

    let versions = vec![version_summary(
        "1.0.0",
        VersionStatus::Published,
        &["linux/amd64"],
        &["1.0.0"],
    )];

    let rt = tokio::runtime::Runtime::new().unwrap();
    let outcome = rt.block_on(run_announce(
        Some(&announce_config()),
        &versions,
        &tags_file,
        Some("gh-token"),
        &ocx,
    ));

    assert!(
        matches!(outcome, Some(AnnounceOutcome::Announced { .. })),
        "got: {outcome:?}",
    );
    assert_eq!(std::fs::read_to_string(&tags_file).unwrap(), "1.0.0");
}

#[cfg(unix)]
#[test]
fn a_stalled_announce_is_killed_instead_of_taking_the_job_down_with_it() {
    // The announce pushes a fork branch, calls the PR API and observes the
    // registry. A stall there (a 429 retry loop is enough) used to run
    // until the runner killed the job.
    use std::os::unix::fs::PermissionsExt;

    let dir = tempdir().unwrap();
    let script = dir.path().join("hanging-ocx");
    std::fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    let started = std::time::Instant::now();
    let tags = ["1.0.0".to_string()];
    let tags_file = dir.path().join("tags");
    let result = rt.block_on(invoke_announce(
        &announce_config(),
        &TagSource::File {
            path: &tags_file,
            tags: &tags,
        },
        None,
        &script,
        Duration::from_millis(200),
    ));

    let error = result.expect_err("a hung announce must not hang the run");
    assert!(error.contains("timed out"), "got: {error}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the timeout must bound the wait, took {:?}",
        started.elapsed(),
    );
}

#[cfg(unix)]
#[test]
fn the_run_summary_is_on_disk_before_the_announce_starts() {
    // Twelve images push fine, the announce stalls, the job is killed on
    // the runner timeout. Written after the announce, run-summary.json
    // would never exist: the artifact upload finds nothing, the notify
    // gate reads false, and a dozen live images go unreported.
    use std::os::unix::fs::PermissionsExt;

    let _env_lock = job_url_env_lock();
    let dir = tempdir().unwrap();
    let junit_dir = tempdir().unwrap();
    let bundles_dir = tempdir().unwrap();
    let summary_path = dir.path().join("run-summary.json");
    let log = dir.path().join("announce-observations.log");

    // Stand-in `ocx`: answers a push with a cascade report, and on announce
    // records whether the summary was already on disk when it was called.
    let script = dir.path().join("fake-ocx");
    std::fs::write(
        &script,
        format!(
            r#"#!/bin/sh
case "$*" in
  *"package announce"*)
    if [ -f '{summary}' ]; then echo saw-summary >> '{log}'; else echo saw-nothing >> '{log}'; fi
    echo '{{"status":"updated","pull_request_url":"https://github.com/ocx-sh/index/pull/81"}}'
    ;;
  *)
    echo '{{"cascade_tags_written":["3.7.0"],"status":"pushed"}}'
    ;;
esac
"#,
            summary = summary_path.display(),
            log = log.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let version = "3.7.0";
    write_junit(
        junit_dir.path(),
        version,
        "linux_amd64",
        "_native_",
        &passing_junit(version, "linux/amd64", "_native_"),
    );
    // Contents are irrelevant — the push subprocess is the stand-in above.
    std::fs::write(bundles_dir.path().join("bundle-3.7.0-linux_amd64.tar.xz"), b"x").unwrap();

    // SAFETY: test-only process env, serialised by the lock above.
    unsafe {
        std::env::set_var("OCX_BINARY_PIN", &script);
        std::env::set_var(ENV_ANNOUNCE_TOKEN, "gh-token");
    }

    let result = run_push_cmd(
        std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/mirror-ghcr-announce.yml"
        ))
        .to_path_buf(),
        junit_dir.path().to_path_buf(),
        bundles_dir.path().to_path_buf(),
        summary_path.clone(),
    );

    // SAFETY: cleanup so neighbouring tests don't inherit either var.
    unsafe {
        std::env::remove_var("OCX_BINARY_PIN");
        std::env::remove_var(ENV_ANNOUNCE_TOKEN);
    }
    result.expect("a fully green run must exit 0");

    assert_eq!(
        std::fs::read_to_string(&log).unwrap().trim(),
        "saw-summary",
        "the announce must run against an already-durable run summary",
    );

    // And the announce outcome still lands in the file afterwards.
    let val: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&summary_path).unwrap()).unwrap();
    assert_eq!(val["announce"]["status"], "announced", "got: {val}");
    assert_eq!(val["announce"]["tags"][0], "3.7.0", "got: {val}");
}
