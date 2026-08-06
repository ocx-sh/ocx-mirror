// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `ocx-mirror package pipeline announce` — announce every tag the registry
//! currently holds into the index.
//!
//! The push job announces only what *its own run* published, which is the right
//! scope for a mirror that had `announce:` from the start. It cannot catch up a
//! mirror that published first and opted in later: no future run's tag set ever
//! covers the backlog, so every run reports `nothing_to_announce` indefinitely.
//!
//! This command closes that gap by listing the physical repository's tags and
//! unioning them onto the committed index root. It is additive — like
//! `--tags-from-file` it can never drop a committed tag, and yank markers
//! survive — so it is safe to dispatch against a mirror that is already
//! current.
//!
//! # Errors
//!
//! - [`MirrorError::SpecNotFound`] / [`MirrorError::SpecInvalid`] from
//!   `load_spec`.
//! - [`MirrorError::SpecUsageError`] (exit 64) when the spec has no `announce:`
//!   block — there is no index package to announce into.
//! - [`MirrorError::ExecutionFailed`] when the `ocx package announce`
//!   subprocess fails or reports no readable JSON.

use std::path::PathBuf;

use ocx_lib::cli::DataInterface;
use ocx_lib::log;

use crate::command::package::pipeline::push::{
    ANNOUNCE_TIMEOUT, AnnounceReport, TagSource, invoke_announce, resolve_ocx_binary,
};
use crate::error::MirrorError;
use crate::spec;

/// `ocx-mirror package pipeline announce` subcommand.
#[derive(clap::Parser)]
pub struct Announce {
    /// Path to the mirror spec file.
    #[arg(long, default_value = "./mirror.yml")]
    pub spec: PathBuf,

    /// Report what the announce would change without opening an index pull
    /// request: the rebuilt entry is written to a temporary directory and
    /// discarded.
    #[arg(long)]
    pub dry_run: bool,
}

impl Announce {
    pub async fn execute(&self, _printer: &DataInterface) -> Result<(), MirrorError> {
        let spec = spec::load_spec(&self.spec).await?;

        let Some(config) = spec.announce.as_ref() else {
            return Err(MirrorError::SpecUsageError(format!(
                "{} has no `announce:` block — add one naming the logical index package \
                 before announcing from the registry",
                self.spec.display()
            )));
        };

        let ocx_binary = resolve_ocx_binary().map_err(|e| MirrorError::ExecutionFailed(vec![e]))?;

        // The dry run needs somewhere to throw the rebuilt entry away. It goes
        // under the pipeline's own `.ocx-mirror` work dir rather than the shared
        // system temp dir: a predictable `/tmp/<fixed-name>` is a path any other
        // local user can pre-create as a symlink, and `--out` writes through it.
        // `tempfile` would solve that too but is a dev-dependency here, and this
        // needs no randomness once the directory is not shared. `ocx package
        // announce --out` creates it.
        let out = self
            .dry_run
            .then(|| std::path::PathBuf::from(".ocx-mirror").join(format!("announce-dry-run-{}", std::process::id())));

        let result = invoke_announce(
            config,
            &TagSource::FromRegistry,
            out.as_deref(),
            &ocx_binary,
            ANNOUNCE_TIMEOUT,
        )
        .await;

        if let Some(directory) = &out {
            let _ = tokio::fs::remove_dir_all(directory).await;
        }

        let report = result.map_err(|e| MirrorError::ExecutionFailed(vec![e]))?;

        let target = format!("{}/{}", spec.target.registry, spec.target.repository);
        log_report(self.dry_run, &report, config, &target);

        Ok(())
    }
}

/// Log `report` at the level its outcome deserves.
///
/// Every caller of `invoke_announce` goes through here. Formatting and level
/// selection are one call precisely because they drifted apart once: `pipeline
/// patch` grew its own `({status}, {pr})` line and kept printing it through two
/// commits that fixed the same defect next door.
pub(crate) fn log_report(dry_run: bool, report: &AnnounceReport, config: &spec::AnnounceConfig, target: &str) {
    let stranded = is_stranded(dry_run, report);
    for line in report_lines(dry_run, report, config, target) {
        if stranded {
            log::warn!("{line}");
        } else {
            log::info!("{line}");
        }
    }
}

/// A real run that changed the index but reported no pull request.
///
/// Unreachable from a healthy `ocx package announce`: the `--fork` path only
/// returns `updated` after `open_or_update_pull_request`, so it always carries
/// the pull request it opened. Reaching it means the index moved and nobody
/// was told where to review it — the one outcome of this command that is worse
/// than a failure, because it is silent. Warn rather than info; the exit code
/// stays 0, since the commit did land.
fn is_stranded(dry_run: bool, report: &AnnounceReport) -> bool {
    !dry_run && report.status == "updated" && report.pull_request_url.is_none()
}

/// The line(s) this command logs for `report`.
///
/// Two constraints shape this. A dry run and a real run must never be able to
/// print the same text — `updated` and `no pull request reported` are both
/// legitimate real outcomes, so a dry run reusing that wording leaves a CI log
/// in which nothing happened indistinguishable from one in which the index
/// moved. And the two must not differ in *substance*: the `dry_run` workflow
/// input defaults to true, so the common case is a maintainer who got a dry run
/// without asking for one and needs to know what it found and how to act on it.
///
/// The mode goes in the tag, not a suffix, so it survives a truncated or
/// grepped log, and every arm carries it — `unchanged` included, since a marker
/// present in only some arms makes its own absence meaningless, and "the index
/// already carries every tag" otherwise reads as a settled fact about a
/// reconciliation the dry run never attempted.
///
/// The real arms keep their existing text as a leading prefix; anything reading
/// today's logs still matches. What is appended is what the JSON actually
/// carries — the fork path reports no tag count at all (`written_paths` is
/// empty by construction there), so the real run cannot say how much it
/// curated. That is an upstream gap in `ocx package announce --format json`,
/// not something to approximate here.
pub(crate) fn report_lines(
    dry_run: bool,
    report: &AnnounceReport,
    config: &spec::AnnounceConfig,
    target: &str,
) -> Vec<String> {
    let package = &config.package;
    let index_repo = &config.index_repo;
    let status = report.status.as_str();

    // Reserved drops (`__ocx.*`, canonical `<alg>.<hex>`) are a reported fact of
    // a successful announce and invisible today. Count only — the canonical tags
    // alone run one per published digest, so the list is unbounded.
    let dropped = match report.reserved_tags_dropped.len() {
        0 => String::new(),
        n => format!(" (dropped {n} reserved tag(s))"),
    };

    if dry_run {
        // `--out` writes the whole file set on every run, `unchanged` included,
        // so this count is the size of the curated set — never a delta. It is
        // bound to the `updated` arm, where a real run would in fact commit it.
        // Objects are keyed by manifest digest, so aliased tags (`1`, `1.2.3`,
        // `latest` on one image) share one: this counts files, not tags.
        let would_change = match (status, report.written_paths.len()) {
            ("unchanged", _) => format!("would report unchanged: the index already carries every tag {target} holds"),
            (status, 0) => format!("would report {status}"),
            (status, files) => {
                format!(
                    "would report {status} and commit {files} index file(s) (root + one object per distinct manifest)"
                )
            }
        };
        return vec![
            format!(
                "[announce:dry-run] {package} → {index_repo}: nothing was pushed — no commit, no pull request, no index change. A real run {would_change}.{dropped}"
            ),
            format!(
                "[announce:dry-run] {package} — re-dispatch with the `dry_run` input unticked (it defaults to true), or without `--dry-run`, to announce for real."
            ),
        ];
    }

    // Same reporting shape as the push job's `run_announce`: `unchanged` with
    // no pull request is the no-op, and must not read as a curation that
    // happened.
    let line = match (status, report.pull_request_url.as_deref()) {
        ("unchanged", None) => format!("[announce] {package} — index already carries every tag {target} holds"),
        // `updated` here means the index moved with nothing to review it — see
        // `is_stranded`, which also raises the log level.
        ("updated", None) => format!(
            "[announce] {package} → {index_repo} (updated, no pull request reported) — the index change was \
             committed but nothing was opened to review it"
        ),
        // `unchanged` *with* a pull request: this run curated nothing, and the
        // pull request it names carries commits an earlier run stranded.
        ("unchanged", Some(pull_request_url)) => format!(
            "[announce] {package} → {index_repo} (unchanged, {pull_request_url}) — this run changed nothing; \
             the pull request carries an earlier run's commits"
        ),
        (status, pull_request_url) => {
            let pull_request_url = pull_request_url.unwrap_or("no pull request reported");
            format!("[announce] {package} → {index_repo} ({status}, {pull_request_url})")
        }
    };
    vec![format!("{line}{dropped}")]
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocx_lib::cli::ExitCode;

    /// A mirror without `announce:` has no index package to announce into.
    /// Exit 64 (usage), not 65 — the spec is valid, the command is wrong for it.
    #[tokio::test]
    async fn a_spec_without_an_announce_block_is_a_usage_error() {
        let dir = tempfile::tempdir().unwrap();
        let spec_path = dir.path().join("mirror.yml");
        let fixture = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/mirror-minimal.yml"
        ));
        std::fs::copy(fixture, &spec_path).unwrap();

        let cmd = Announce {
            spec: spec_path,
            dry_run: true,
        };
        let printer = ocx_lib::cli::DataInterface::new(ocx_lib::cli::Printer::new(false, false));
        let error = cmd.execute(&printer).await.expect_err("no announce block must fail");

        assert_eq!(error.kind_exit_code(), ExitCode::UsageError, "got: {error}");
    }

    fn config() -> spec::AnnounceConfig {
        spec::AnnounceConfig {
            package: "bazelbuild/buildifier".to_string(),
            fork: "ocx-contrib/index".to_string(),
            index_repo: "ocx-sh/index".to_string(),
            schedule: None,
        }
    }

    fn report(status: &str, pull_request_url: Option<&str>) -> AnnounceReport {
        AnnounceReport {
            status: status.to_string(),
            pull_request_url: pull_request_url.map(str::to_string),
            written_paths: Vec::new(),
            reserved_tags_dropped: Vec::new(),
        }
    }

    /// A `--out` (dry-run) report: `ocx` writes the root plus one object per
    /// distinct curated tag, and never a pull request URL.
    fn out_report(status: &str, files: usize) -> AnnounceReport {
        AnnounceReport {
            written_paths: (0..files)
                .map(|i| format!("p/bazelbuild/buildifier/o/sha256/{i}.json"))
                .collect(),
            ..report(status, None)
        }
    }

    const TARGET: &str = "ghcr.io/ocx-sh/bazelbuild-buildifier";

    fn line(dry_run: bool, report: &AnnounceReport) -> String {
        report_lines(dry_run, report, &config(), TARGET).join("\n")
    }

    /// The defect: a dry run printed the line a real run prints. Asserted as a
    /// *pair* over every outcome the announce reports — a test that only pinned
    /// the dry-run wording would still pass if the real path drifted onto it.
    #[test]
    fn a_dry_run_and_a_real_run_never_report_the_same_line() {
        for status in ["updated", "unchanged"] {
            for pull_request_url in [None, Some("https://github.com/ocx-sh/index/pull/7")] {
                for files in [0, 16] {
                    let report = AnnounceReport {
                        pull_request_url: pull_request_url.map(str::to_string),
                        ..out_report(status, files)
                    };
                    let dry = line(true, &report);
                    let real = line(false, &report);

                    assert_ne!(dry, real, "status={status}, pr={pull_request_url:?}, files={files}");
                    assert!(
                        dry.lines().all(|l| l.starts_with("[announce:dry-run] ")),
                        "every dry-run line must name its mode up front, got: {dry}"
                    );
                    assert!(
                        !real.contains("dry-run") && !real.contains("dry run"),
                        "the real path must not claim to be a dry run, got: {real}"
                    );
                }
            }
        }
    }

    /// The `dry_run` input defaults to true, so the common case is a maintainer
    /// who did not ask for a dry run. Each line must say nothing was pushed,
    /// what a real run would do, and how to get one.
    #[test]
    fn the_dry_run_says_nothing_was_pushed_what_would_change_and_the_way_out() {
        let updated = report_lines(true, &out_report("updated", 16), &config(), TARGET);
        assert_eq!(
            updated[0],
            "[announce:dry-run] bazelbuild/buildifier → ocx-sh/index: nothing was pushed — no commit, \
             no pull request, no index change. A real run would report updated and commit 16 index \
             file(s) (root + one object per distinct manifest).",
            "got: {updated:?}"
        );
        assert_eq!(
            updated[1],
            "[announce:dry-run] bazelbuild/buildifier — re-dispatch with the `dry_run` input unticked \
             (it defaults to true), or without `--dry-run`, to announce for real.",
            "the way out is the part a merely-distinguishable message misses, got: {updated:?}"
        );

        // `--out` writes every file whatever the status, so the count is the
        // size of the curated set. Claiming a real run would commit it when it
        // would commit nothing is the same false-evidence class as the tag.
        let unchanged = report_lines(true, &out_report("unchanged", 16), &config(), TARGET);
        assert!(
            unchanged[0].ends_with(&format!(
                "A real run would report unchanged: the index already carries every tag {TARGET} holds."
            )),
            "an unchanged dry run must not read as 16 pending files, got: {unchanged:?}"
        );
        assert_eq!(unchanged[1], updated[1], "the way out does not depend on the status");
    }

    /// The real-run text anything reads today stays a leading prefix; substance
    /// is appended, never substituted. Frozen so the dry-run fix cannot be
    /// "kept" by quietly moving the real path onto it instead.
    #[test]
    fn the_real_run_keeps_todays_wording_as_its_prefix() {
        let cases = [
            (
                report("unchanged", None),
                format!("[announce] bazelbuild/buildifier — index already carries every tag {TARGET} holds"),
            ),
            (
                report("unchanged", Some("https://x/1")),
                "[announce] bazelbuild/buildifier → ocx-sh/index (unchanged, https://x/1)".to_string(),
            ),
            (
                report("updated", Some("https://x/1")),
                "[announce] bazelbuild/buildifier → ocx-sh/index (updated, https://x/1)".to_string(),
            ),
            (
                report("updated", None),
                "[announce] bazelbuild/buildifier → ocx-sh/index (updated, no pull request reported)".to_string(),
            ),
        ];
        for (report, prefix) in cases {
            let line = line(false, &report);
            assert!(line.starts_with(&prefix), "expected prefix {prefix:?}, got: {line}");
            assert_eq!(line.lines().count(), 1, "the real path stays one line, got: {line}");
        }
    }

    /// `updated` with no pull request cannot come from a healthy `ocx package
    /// announce` — the fork path returns the pull request it opened. It means a
    /// live index change nobody was told to review, so it is a warn, and only
    /// on the real path: for a dry run it is the expected shape.
    #[test]
    fn an_index_change_with_no_pull_request_is_loud_only_on_the_real_path() {
        assert!(is_stranded(false, &report("updated", None)));
        assert!(
            !is_stranded(true, &out_report("updated", 16)),
            "expected shape for a dry run"
        );
        assert!(!is_stranded(false, &report("updated", Some("https://x/1"))));
        assert!(!is_stranded(false, &report("unchanged", None)));

        assert!(
            line(false, &report("updated", None)).contains("nothing was opened to review it"),
            "the stranded case must say what is wrong, not just omit a URL"
        );
    }

    /// Dropped reserved tags are a reported fact of a successful announce and
    /// invisible in the log today. Count only: canonical `<alg>.<hex>` tags run
    /// one per published digest, so the list has no bound.
    #[test]
    fn dropped_reserved_tags_are_counted_on_both_paths() {
        let dropped = AnnounceReport {
            reserved_tags_dropped: vec!["__ocx.desc".to_string(), format!("sha256.{}", "a".repeat(64))],
            ..out_report("updated", 16)
        };
        for dry_run in [true, false] {
            let line = line(dry_run, &dropped);
            assert!(
                line.contains("(dropped 2 reserved tag(s))"),
                "dry_run={dry_run}, got: {line}"
            );
            assert!(
                !line.contains("a".repeat(64).as_str()),
                "the list is unbounded: count only"
            );
        }
    }
}
