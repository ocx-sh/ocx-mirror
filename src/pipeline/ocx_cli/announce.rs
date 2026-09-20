// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! The `ocx package announce` subprocess: the token, the tag source, argv
//! assembly, and one bounded invocation.
//!
//! Four commands announce — `pipeline push`, `pipeline patch`,
//! `pipeline cascade` and `pipeline announce` — so the subprocess boundary
//! belongs beside the other `ocx` plumbing rather than inside whichever of
//! them happened to grow it first.

use std::path::Path;
use std::time::Duration;

use super::forward_ocx_env;
use crate::spec::{AnnounceConfig, WriteTransport};

/// The operator's own announce credential — rung 1 of `ocx`'s forge
/// credential ladder, and the name the skip notice tells them to set.
pub(crate) const ENV_ANNOUNCE_TOKEN: &str = ocx_config::env::keys::OCX_ANNOUNCE_TOKEN;

/// Whether `ocx package announce` would find a credential for `config`.
///
/// Both rungs of `ocx`'s API credential ladder, not just the first name:
/// under `transport: git` inside a GitLab job the ladder falls through from an
/// unset `OCX_ANNOUNCE_TOKEN` to the job's own `CI_JOB_TOKEN`, and a gate that
/// only knew `OCX_ANNOUNCE_TOKEN` would skip the announce a job token can
/// make. Empty counts as unset on every name — CI runners routinely export a
/// variable with no value.
///
/// A repository with no credential is a valid configuration — forks and test
/// repos — so every caller degrades on `false` rather than failing: the
/// packages are in the registry either way, and an announce that was never
/// attempted must not red a run that published exactly what it was asked to.
pub(crate) fn announce_credential_present(config: &AnnounceConfig) -> bool {
    let job_token = config.transport() == WriteTransport::Git
        && non_empty("GITLAB_CI").is_some()
        && non_empty("CI_JOB_TOKEN").is_some();
    non_empty(ENV_ANNOUNCE_TOKEN).is_some() || job_token
}

/// An environment variable's value, or `None` when it is unset **or** empty.
fn non_empty(key: &str) -> Option<String> {
    ocx_util::env::var(key).filter(|value| !value.is_empty())
}

/// What the skip notice tells the operator to do about a missing credential.
///
/// Per transport and per forge, because the fix is: a GitLab job under
/// `transport: api` already carries a `CI_JOB_TOKEN` the ladder refuses
/// there, and "set `OCX_ANNOUNCE_TOKEN`" alone would hide the one-line spec
/// change that lets its job token through. On GitHub — the default index —
/// that spec change is one `validate_announce_config` refuses (`transport:
/// git` is GitLab-only), so the clause is offered only when the index
/// resolves to GitLab, the way `validate_announce_config` resolves it. An
/// index that does not resolve (`plan` already refused it) gets the plain
/// hint rather than a clause for a forge nobody can name.
pub(crate) fn missing_credential_hint(config: &AnnounceConfig) -> String {
    match config.transport() {
        WriteTransport::Api if index_is_gitlab(config) => format!(
            "set {ENV_ANNOUNCE_TOKEN} — a GitLab CI_JOB_TOKEN cannot open a merge request over the api transport; \
             use `transport: git` to announce with it"
        ),
        WriteTransport::Api => format!("set {ENV_ANNOUNCE_TOKEN}"),
        WriteTransport::Git => {
            format!("set {ENV_ANNOUNCE_TOKEN}, or run inside a GitLab job (GITLAB_CI + CI_JOB_TOKEN)")
        }
    }
}

/// Whether `index_repo` resolves to GitLab — declared `forge` first, then
/// the index host, exactly as `ForgeKind::resolve` is applied in
/// `validate_announce_config`. Unparsable or unresolvable is `false`.
fn index_is_gitlab(config: &AnnounceConfig) -> bool {
    crate::spec::forge_is_gitlab(&config.index_repo, config.forge_kind())
}

/// Where `ocx package announce` takes its tag set from.
///
/// Both variants are **additive** — neither can remove a tag the index already
/// commits, and yank markers survive. The third mode `ocx package announce`
/// offers, `--tags`, *replaces* the curated set; a mirror must never use it,
/// because one run publishing one new version would delete every previously
/// announced version from the index entry.
pub(crate) enum TagSource<'a> {
    /// This run's own tags, handed over in a file. The pipeline's normal mode:
    /// it announces exactly what the run published and nothing else.
    File { path: &'a Path, tags: &'a [String] },
    /// Every tag the physical repository currently holds, listed by `ocx`
    /// itself. Used to catch up a mirror that published before it had an
    /// `announce:` block, where no single run's tag set can ever cover the
    /// backlog.
    FromRegistry,
}

/// Build the `ocx package announce` argv. Pure and unit-testable — locks the
/// flag set without spawning a subprocess.
///
/// `out` writes the rebuilt entry to a directory instead of opening a
/// request — `--out` excludes both `--fork` and `--transport` on the `ocx`
/// side (a local write opens no request, so it has no fork to open from and
/// no transport to open it through), so those two are emitted only without
/// `out`, and `--fork` only when the spec names one: no fork means the branch
/// goes to `--index-repo` itself. `--forge` rides along in both modes and,
/// like `--transport`, only when the spec sets it, so an unset key leaves
/// `ocx`'s own default (host inference, `api`) in charge.
pub(crate) fn build_announce_args(
    config: &AnnounceConfig,
    source: &TagSource<'_>,
    out: Option<&Path>,
) -> Result<Vec<String>, String> {
    // Global flags precede the subcommand. JSON because the caller has to
    // read what the announce *did* — its exit code is 0 either way. The
    // package is positional since ocx 0.6.1; `--package` is a hidden alias
    // that warns and is deleted in 0.7.
    let mut args: Vec<String> = ["--format", "json", "package", "announce", &config.package]
        .iter()
        .map(|s| (*s).to_string())
        .collect();

    match source {
        TagSource::File { path, .. } => {
            let file = path
                .to_str()
                .ok_or_else(|| format!("announce tags file path is not valid UTF-8: {}", path.display()))?;
            args.push("--tags-file".to_string());
            args.push(file.to_string());
        }
        TagSource::FromRegistry => args.push("--tags-from-registry".to_string()),
    }

    match out {
        Some(directory) => {
            let dir = directory
                .to_str()
                .ok_or_else(|| format!("announce output directory is not valid UTF-8: {}", directory.display()))?;
            args.push("--out".to_string());
            args.push(dir.to_string());
        }
        None => {
            if let Some(fork) = &config.fork {
                args.push("--fork".to_string());
                args.push(fork.clone());
            }
            if let Some(transport) = &config.transport {
                args.push("--transport".to_string());
                args.push(transport.clone());
            }
        }
    }

    args.push("--index-repo".to_string());
    args.push(config.index_repo.clone());
    if let Some(forge) = &config.forge {
        args.push("--forge".to_string());
        args.push(forge.clone());
    }

    Ok(args)
}

/// How long the announce subprocess may run before it is killed.
///
/// It pushes a branch, calls the request API and observes the registry —
/// network work with no bound of its own. Unbounded, one stalled
/// call (a registry 429 retry loop is enough) takes the whole job down with it
/// on the runner timeout, and everything the run published downstream of the
/// summary write goes unreported.
pub(crate) const ANNOUNCE_TIMEOUT: Duration = Duration::from_secs(600);

/// Run `ocx package announce`, materialising the tags file first when `source`
/// carries one.
///
/// `timeout` is a parameter rather than a constant read so the bound itself can
/// be tested without a ten-minute test.
pub(crate) async fn invoke_announce(
    config: &AnnounceConfig,
    source: &TagSource<'_>,
    out: Option<&Path>,
    ocx_binary: &Path,
    timeout: Duration,
) -> Result<AnnounceReport, String> {
    let args = build_announce_args(config, source, out)?;

    if let TagSource::File { path, tags } = source {
        // The tags file is a sibling of `--write-summary`, and the announce runs
        // before the summary is written — so with `--write-summary out/x.json` and
        // no `out/` yet, nothing has created the directory. Same treatment as
        // `write_run_summary`. An empty parent (a bare relative path) is a no-op.
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("failed to create announce tags directory {}: {e}", parent.display()))?;
        }

        tokio::fs::write(path, tags.join("\n"))
            .await
            .map_err(|e| format!("failed to write announce tags file {}: {e}", path.display()))?;
    }

    let mut cmd = tokio::process::Command::new(ocx_binary);
    cmd.args(&args);
    forward_ocx_env(&mut cmd);
    // Tokio leaves a child running when its future is dropped; on timeout that
    // would orphan an announce still talking to the registry.
    cmd.kill_on_drop(true);

    let output = tokio::time::timeout(timeout, cmd.output())
        .await
        .map_err(|_| format!("ocx package announce timed out after {}s", timeout.as_secs()))?
        .map_err(|e| format!("failed to spawn ocx: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "ocx package announce exited {}: {}",
            output.status,
            stderr.trim()
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(stdout.trim()).map_err(|e| {
        // An unreadable report is a genuine unknown, not a success: the run
        // cannot tell whether the index moved. Recording it as `failed` fails
        // the push job, which is the honest outcome — the images are live and
        // the index state is undetermined.
        format!(
            "ocx package announce reported no readable JSON ({e}): {}",
            stdout.trim()
        )
    })
}

/// The subset of `ocx package announce --format json` this pipeline reads.
///
/// `status` is `"updated"` or `"unchanged"`. `unchanged` does **not** imply no
/// pull request: an announce whose branch is ahead of the index base ensures
/// one without committing anything, and those tags are as pending as a fresh
/// run's. Only `unchanged` *and* no pull request means nothing happened.
///
/// `written_paths` is populated only in `--out` mode — the root plus one object
/// per distinct curated tag, which is the file set a real run would commit. The
/// `--fork` path returns it empty by construction, so it is the *dry run's*
/// only quantitative fact and never the real run's.
#[derive(Debug, serde::Deserialize)]
pub(crate) struct AnnounceReport {
    pub(crate) status: String,
    #[serde(default)]
    pub(crate) pull_request_url: Option<String>,
    #[serde(default)]
    pub(crate) written_paths: Vec<String>,
    #[serde(default)]
    pub(crate) reserved_tags_dropped: Vec<String>,
}
