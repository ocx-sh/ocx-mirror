// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `ocx-mirror package pipeline cascade` — re-point the target repository's
//! broken rolling aliases at content the registry already serves.
//!
//! A cascade breaks when a push lands out of order or dies half-way: `3.29.0`
//! is published but `3.29`, `3` and `latest` still name the version before it,
//! so everyone installing the unpinned name gets the older package while the
//! registry looks complete. Nothing about that is recomputable from the spec —
//! it is decided by what the registry currently holds — so the audit and the
//! repair both live in `ocx package cascade`, and this command is the wrapper
//! that drives it against the spec's target and closes the index behind it.
//!
//! One verb, not two: `repair --dry-run` prints the plan it would apply and
//! exits 65 when that plan is non-empty, which is exactly what `check` reports.
//! It maps 1:1 onto the generated workflow's `dry_run` input, whose default is
//! true — so a dispatch that names nothing audits, and repairing is the
//! deliberate second dispatch.
//!
//! A repaired alias points at a digest the index does not know, so a run that
//! moved anything ends by announcing — including a run that exits 65, because
//! the aliases it *did* re-point are live either way and leaving them
//! unannounced is the stale-index state this chain exists to prevent. An absent
//! `OCX_ANNOUNCE_TOKEN` is a valid configuration and degrades to a notice
//! exactly as `pipeline push` and `pipeline patch` do.
//!
//! Needs **ocx 0.5.4 or newer** on PATH: `package cascade` does not exist
//! before it, and an older binary rejects the verb as an unknown argument.
//!
//! # Errors
//!
//! - [`MirrorError::SpecNotFound`] / [`MirrorError::SpecInvalid`] from
//!   `load_spec`.
//! - [`MirrorError::CascadeUnrepaired`] (exit 65) when the repair ran and
//!   findings remain — the audit outcome, not a failure of the tool.
//! - [`MirrorError::ExecutionFailed`] when `ocx package cascade repair` cannot
//!   be run at all, exits some other non-zero code, or the closing announce
//!   fails.

use std::path::{Path, PathBuf};

use ocx_lib::cli::DataInterface;
use ocx_lib::log;

use crate::command::package::pipeline::announce;
use crate::command::package::pipeline::push::{
    ANNOUNCE_TIMEOUT, ENV_ANNOUNCE_TOKEN, TagSource, announce_token, forward_ocx_env, invoke_announce,
    resolve_ocx_binary,
};
use crate::error::MirrorError;
use crate::spec;

/// `ocx-mirror package pipeline cascade` subcommand.
#[derive(clap::Parser)]
pub struct Cascade {
    /// Path to the mirror spec file.
    #[arg(long, default_value = "./mirror.yml")]
    pub spec: PathBuf,

    /// Report the repair plan without writing a tag or touching the index.
    ///
    /// Exits 65 when the plan is non-empty, so a scheduled audit can red on a
    /// broken cascade without changing anything.
    #[arg(long)]
    pub dry_run: bool,
}

/// `ocx package cascade repair`'s exit code for "the repair ran, and findings
/// remain" — an audit result, deliberately distinct from the tool failing.
const OCX_FINDINGS_REMAIN: i32 = 65;

/// Where the repair records the tags it moved, one per line.
///
/// Under the pipeline's own work dir rather than the shared system temp dir,
/// for the reason `pipeline announce` gives: a predictable `/tmp/<fixed-name>`
/// is a path any other local user can pre-create as a symlink.
const ANNOUNCE_TAGS_FILE: &str = ".ocx-mirror/cascade.announce-tags";

impl Cascade {
    pub async fn execute(&self, _printer: &DataInterface) -> Result<(), MirrorError> {
        let spec = spec::load_spec(&self.spec).await?;
        // The physical repository, never `announce.package`: a repair writes
        // tags to the registry, and the logical index name resolves elsewhere.
        let identifier = format!("{}/{}", spec.target.registry, spec.target.repository);
        let ocx_binary = resolve_ocx_binary().map_err(|e| MirrorError::ExecutionFailed(vec![e]))?;

        // Only a mirror with an index entry has anything to announce, so only
        // that one asks the repair to record what it moved.
        let tags_file = spec.announce.as_ref().map(|_| PathBuf::from(ANNOUNCE_TAGS_FILE));
        if let Some(path) = tags_file.as_deref() {
            prepare_tags_file(path)
                .await
                .map_err(|e| MirrorError::ExecutionFailed(vec![e]))?;
        }

        let status = invoke_cascade_repair(&ocx_binary, &identifier, self.dry_run, tags_file.as_deref()).await?;
        let unrepaired = status.code() == Some(OCX_FINDINGS_REMAIN);
        if !status.success() && !unrepaired {
            // Exit 64 is what an `ocx` too old to know the verb answers with
            // (clap's unrecognized-subcommand exit) — everything else is a real
            // failure the version hint would only misdirect from.
            let hint = if status.code() == Some(64) {
                " (`package cascade` needs ocx 0.5.4 or newer)"
            } else {
                ""
            };
            return Err(MirrorError::ExecutionFailed(vec![format!(
                "ocx package cascade repair exited {status} for {identifier}{hint}"
            )]));
        }

        let mut failures: Vec<String> = Vec::new();
        let tags = match tags_file.as_deref() {
            Some(path) => match recorded_tags(path).await {
                Ok(tags) => tags,
                Err(error) => {
                    failures.push(error);
                    Vec::new()
                }
            },
            None => Vec::new(),
        };

        // Runs even when the repair exited 65: the aliases it did move are live
        // under digests the index does not know, and an index left pointing at
        // what they replaced is worse than the unrepaired cascade was.
        //
        // No `OCX_ANNOUNCE_TOKEN` is a valid configuration — forks and test
        // repositories — and degrades exactly as the push job's announce does:
        // recorded, not fatal. Failing here would red a run whose tags already
        // landed, over an announce that was never attempted.
        if !tags.is_empty() {
            if self.dry_run {
                log::info!(
                    "[cascade:dry-run] {identifier} — {} tag(s) would be re-pointed and announced; \
                     re-dispatch with the `dry_run` input unticked (it defaults to true), or without \
                     `--dry-run`, to repair for real",
                    tags.len(),
                );
            } else if let Some(config) = spec.announce.as_ref() {
                if announce_token().is_none() {
                    println!(
                        "::notice title=Index announce skipped::No {ENV_ANNOUNCE_TOKEN} secret — \
                         {} re-pointed {} tag(s) but the index was not updated.",
                        config.package,
                        tags.len(),
                    );
                } else {
                    let source = TagSource::File {
                        path: Path::new(ANNOUNCE_TAGS_FILE),
                        tags: &tags,
                    };
                    match invoke_announce(config, &source, None, &ocx_binary, ANNOUNCE_TIMEOUT).await {
                        Ok(report) => announce::log_report(false, &report, config, &identifier),
                        Err(error) => failures.push(format!(
                            "index announce for {} failed: {error} — {} re-pointed tag(s) are live and the \
                             index still points at the digests they replaced",
                            config.package,
                            tags.len(),
                        )),
                    }
                }
            }
        }

        // An announce that failed outranks the findings: the exit-65 contract
        // says "the repair ran, act on what it reported", and a run whose index
        // hop broke needs a rerun, not a reading.
        if !failures.is_empty() {
            return Err(MirrorError::ExecutionFailed(failures));
        }
        if unrepaired {
            return Err(MirrorError::CascadeUnrepaired(identifier));
        }
        Ok(())
    }
}

/// The `ocx package cascade repair` argv. Pure and unit-testable — locks the
/// flag set without spawning a subprocess.
///
/// The identifier is positional and goes last, after every flag.
fn cascade_repair_args(identifier: &str, dry_run: bool, announce_tags: Option<&Path>) -> Vec<String> {
    let mut args: Vec<String> = ["package", "cascade", "repair"]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    if dry_run {
        args.push("--dry-run".to_string());
    }
    if let Some(path) = announce_tags {
        args.push("--announce-tags".to_string());
        args.push(path.display().to_string());
    }
    args.push(identifier.to_string());
    args
}

/// Run `ocx package cascade repair` with inherited stdio.
///
/// Inherited rather than captured because the repair's report — which alias
/// moved, from which digest, to which — *is* the output of a dry run, and a
/// wrapper that swallowed it would leave the maintainer nothing to decide on.
async fn invoke_cascade_repair(
    ocx_binary: &Path,
    identifier: &str,
    dry_run: bool,
    announce_tags: Option<&Path>,
) -> Result<std::process::ExitStatus, MirrorError> {
    let mut cmd = tokio::process::Command::new(ocx_binary);
    cmd.args(cascade_repair_args(identifier, dry_run, announce_tags));
    forward_ocx_env(&mut cmd);

    cmd.status()
        .await
        .map_err(|e| MirrorError::ExecutionFailed(vec![format!("failed to spawn ocx: {e}")]))
}

/// Clear any previous run's record, and create the directory `ocx` writes into.
///
/// `ocx` writes the file with a bare `tokio::fs::write`, which does not create
/// its parent. The removal guards the narrower case that the write itself does
/// not cover: a repair that dies *before* writing would otherwise leave an
/// earlier run's — or the preceding dry run's — tags to be announced as if this
/// run had moved them.
async fn prepare_tags_file(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("failed to remove {}: {error}", path.display())),
    }
}

/// The tags the repair recorded.
///
/// `ocx` writes the file on every run, empty when nothing moved, so an absent
/// file means the repair never reached its write. Reading that as an empty set
/// matches what the empty file would have said. Any other read error is
/// reported instead: silently reading it as empty would skip the announce a
/// repair that *did* move tags requires, and leave the index stale while the
/// run reads green.
async fn recorded_tags(path: &Path) -> Result<Vec<String>, String> {
    match tokio::fs::read_to_string(path).await {
        Ok(body) => Ok(parse_announce_tags(&body)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(format!("failed to read {}: {error}", path.display())),
    }
}

/// One tag per line, blanks dropped.
///
/// `ocx package announce --tags-from-file` splits on newlines and would take a
/// trailing blank line as a tag of its own.
fn parse_announce_tags(body: &str) -> Vec<String> {
    body.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The plain repair: no flags, identifier positional.
    #[test]
    fn a_repair_names_the_physical_repository_last() {
        assert_eq!(
            cascade_repair_args("ghcr.io/ocx-sh/cmake", false, None),
            vec!["package", "cascade", "repair", "ghcr.io/ocx-sh/cmake"],
        );
    }

    /// `--dry-run` is the whole `check` verb: it must reach `ocx`, or the
    /// workflow's default dispatch would repair a cascade nobody approved.
    #[test]
    fn the_dry_run_flag_precedes_the_identifier() {
        assert_eq!(
            cascade_repair_args("ghcr.io/ocx-sh/cmake", true, None),
            vec!["package", "cascade", "repair", "--dry-run", "ghcr.io/ocx-sh/cmake"],
        );
    }

    /// The workflow's default dispatch: `dry_run` ticked, `announce:` block
    /// present — both flags together, identifier still last.
    #[test]
    fn the_default_dispatch_shape_carries_both_flags() {
        assert_eq!(
            cascade_repair_args("ghcr.io/ocx-sh/cmake", true, Some(Path::new(ANNOUNCE_TAGS_FILE))),
            vec![
                "package",
                "cascade",
                "repair",
                "--dry-run",
                "--announce-tags",
                ".ocx-mirror/cascade.announce-tags",
                "ghcr.io/ocx-sh/cmake",
            ],
        );
    }

    /// Without `--announce-tags` the repair records nothing, and a mirror with
    /// an `announce:` block would move tags the index never hears about.
    #[test]
    fn the_announce_tags_file_is_passed_as_a_flag_value() {
        assert_eq!(
            cascade_repair_args("ghcr.io/ocx-sh/cmake", false, Some(Path::new(ANNOUNCE_TAGS_FILE))),
            vec![
                "package",
                "cascade",
                "repair",
                "--announce-tags",
                ".ocx-mirror/cascade.announce-tags",
                "ghcr.io/ocx-sh/cmake",
            ],
        );
    }

    /// A blank line reaching `--tags-from-file` is announced as a tag, so the
    /// trailing newline every line-oriented writer emits has to be dropped here.
    #[test]
    fn blank_lines_never_become_tags() {
        assert!(parse_announce_tags("").is_empty());
        assert!(parse_announce_tags("\n  \n").is_empty());
        assert_eq!(parse_announce_tags("3.29\n3\nlatest\n"), vec!["3.29", "3", "latest"]);
        assert_eq!(parse_announce_tags("3.29\n\n  latest  "), vec!["3.29", "latest"]);
    }
}
