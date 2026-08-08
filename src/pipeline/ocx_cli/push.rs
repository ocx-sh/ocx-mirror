// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! The `ocx package push` subprocess: argv assembly, one attempt, and the
//! retry ladder around it.
//!
//! Lives at the pipeline layer because both publish legs drive it — the
//! archive leg through `command::package::pipeline::push` and the env leg
//! through `pipeline::python_push`. Owning it in the command module forced
//! the lower layer to reach upward for it.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use ocx_lib::cli::ExitCode;
use ocx_lib::log;

use super::forward_ocx_env;
use crate::run_summary::LayerReuse;

/// Parsed JSON output from `ocx package push --cascade --format json`.
///
/// Fields align with the `PushReport` shape from subsystem-cli.md §2.4.
///
/// Every field defaults, so `{}` satisfies the parse (`patch::republish`
/// relies on that) and an `ocx` predating the layer-mount counters simply
/// reports zeros rather than failing.
#[derive(Debug, serde::Deserialize)]
pub(crate) struct PushReport {
    /// SHA-256 manifest digest of the pushed image. Captured for audit trails
    /// but not surfaced in run-summary.json in this version.
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) manifest_digest: Option<String>,
    #[serde(default)]
    pub(crate) cascade_tags_written: Vec<String>,
    #[serde(default)]
    pub(crate) status: Option<String>,
    /// Layer-push outcome counts (mounted/uploaded/verified) — the shared-wheel
    /// reuse the env path records into `run-summary.json`.
    #[serde(default)]
    pub(crate) layers: LayerReuse,
}

/// Build the `ocx package push` argv. Pure and unit-testable — locks the flag
/// order and the `--annotation KEY=VALUE` tail without spawning a subprocess.
///
/// `--format` is a global ocx flag and must precede the subcommand.
///
/// `layers` are positional layer references in manifest order, each either a
/// path to a built bundle (the push job) or a `sha256:<hex>.<ext>` reference to
/// a layer the registry already holds (`pipeline patch`). `metadata` names the
/// sidecar to publish; `None` lets `ocx` derive it from the first file layer,
/// which is what the push job relies on.
///
/// `cascade` decides whether this push also moves the rolling `latest` / `X` /
/// `X.Y` aliases onto the version's image index. Without it the push writes the
/// exact version tag and nothing else — the platform still merges into that
/// tag's index, so a version can be assembled platform by platform and only
/// advertised once it is whole. Who gets it is decided by the caller, once per
/// version; see the phase-2 loop in [`Push::execute`].
///
/// `--new` makes the FIRST push of a brand-new mirror succeed: a cascade push
/// lists existing tags to compute the rolling tags, but a not-yet-published
/// repository answers `tags/list` with 404 ("repository name not known").
/// `--new` tells `ocx package push` to treat that failure as an empty tag set
/// instead of aborting. It is a no-op once the repository exists (the tag
/// list then succeeds and is used), so the mirror always passes it.
pub(crate) fn build_push_args(
    platform: &str,
    target_ref: &str,
    layers: &[&str],
    metadata: Option<&Path>,
    annotations: &BTreeMap<String, String>,
    cascade: bool,
) -> Result<Vec<String>, String> {
    let mut args: Vec<String> = ["--format", "json", "package", "push"]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    if cascade {
        args.push("--cascade".to_string());
    }
    args.extend(
        ["--new", "-p", platform, "-i", target_ref]
            .iter()
            .map(|s| (*s).to_string()),
    );
    if let Some(path) = metadata {
        let sidecar = path
            .to_str()
            .ok_or_else(|| format!("metadata path is not valid UTF-8: {}", path.display()))?;
        args.push("--metadata".to_string());
        args.push(sidecar.to_string());
    }
    args.extend(layers.iter().map(|layer| (*layer).to_string()));

    args.extend(crate::annotations::push_args(annotations));

    Ok(args)
}

/// How long one push attempt may run before it is killed.
///
/// A backstop against a wedged child, not a throughput expectation. `ocx` bounds
/// every registry request itself — 30s to connect, 120s without a byte read — so
/// an upload that is progressing at all satisfies those, and all this has to
/// catch is a child that hung in some way they did not see.
///
/// Sizing it for throughput instead is what made the previous 900s wrong: a
/// 350 MB tile had to sustain ~390 KiB/s to fit, far above the ~26 KiB/s floor
/// `ocx` itself tolerates on a 3 MiB chunk, so a link healthy by `ocx`'s
/// standard was killed on every attempt and the version never published.
///
/// The worst case is now large enough to matter: one tile exhausting
/// `max_retries: 3` is four attempts, four hours. That fits inside GitHub's
/// default 360-minute job limit, but two tiles doing it do not — the job
/// timeout, not this constant, is the real outer bound on a run, and it is the
/// one that fires first.
pub(crate) const PUSH_TIMEOUT: Duration = Duration::from_secs(3600);

/// First retry delay; each further attempt doubles it.
const PUSH_RETRY_BACKOFF_BASE: Duration = Duration::from_secs(1);

/// Ceiling on the doubling, so a large `max_retries` cannot park the job on
/// backoff alone. The shape of the ladder barely matters either way — a push
/// attempt costs minutes and the delay between them seconds.
const PUSH_RETRY_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// One failed push attempt: what to report, and whether trying again could
/// plausibly change the outcome.
#[derive(Debug)]
pub(crate) struct PushAttemptError {
    pub(crate) message: String,
    transient: bool,
}

/// Whether an `ocx package push` exit code is one this pipeline will try again.
///
/// `ocx` 0.5.3 draws the line for us: 75 means the same command may succeed if
/// it is run again (registry connect failure, timeout, rate limit), and 69
/// means rerunning will not change the outcome. Only 75 is worth an upload.
///
/// A registry denial never reaches either code — 403 is 80 (auth), which is
/// deterministic and not retried. `None` (signal-killed) is not retried either:
/// the signal came from outside, and the runner that sent it is usually about
/// to send another.
///
/// Exit 65 is likewise not retried, and from `ocx` 0.5.5 that is the code a
/// binary *older* than 0.5.5 answers with on every leg: it demands the
/// top-level `platform` key the sidecar no longer carries. Deliberately given
/// no version hint — unlike the exit-64 hint `pipeline cascade` emits for a
/// missing verb, 65 is the ordinary data-error code here and a version guess
/// would misdirect a genuine bad-metadata run. The floor is documented instead.
fn push_exit_is_transient(code: Option<i32>) -> bool {
    matches!(code, Some(code) if code == ExitCode::TempFail as i32)
}

/// Delay before attempt `attempt + 1`, doubling from
/// [`PUSH_RETRY_BACKOFF_BASE`] and capped at [`PUSH_RETRY_BACKOFF_MAX`].
///
/// Kept pure and un-jittered so the ladder is pinned by a table test; the
/// spread is applied by [`push_retry_delay`] at the call site.
fn push_retry_backoff(attempt: u32) -> Duration {
    PUSH_RETRY_BACKOFF_BASE
        .saturating_mul(2u32.saturating_pow(attempt.saturating_sub(1)))
        .min(PUSH_RETRY_BACKOFF_MAX)
}

/// `delay` spread by ±10%.
///
/// The herd this breaks up is not the one inside a run — pushes there are
/// strictly sequential — but the one across repositories: dozens of mirrors run
/// scheduled workflows against the same registry, so a rate limit or an outage
/// starts all of their ladders at the same instant and an undithered ladder
/// keeps them in lockstep for every retry after. Same ±10% default
/// go-containerregistry and oras-go ship, each despite being sequential too.
///
/// The clock's nanoseconds are the entropy. The spread only has to be
/// uncorrelated between processes, which is a far weaker property than
/// randomness, and it costs no dependency.
fn jitter(delay: Duration) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.subsec_nanos());
    delay.saturating_mul(90 + nanos % 21) / 100
}

/// What [`invoke_push`] actually sleeps before attempt `attempt + 1`.
///
/// Scaled down by a thousand under `cfg(test)`: the retry tests drive the ladder
/// through [`Push::execute`], a clap struct with no seam to hand a shorter base
/// in, and four real seconds of sleeping on every `task rust:verify` buys
/// nothing that [`push_retry_backoff`]'s own table test does not already pin.
/// The scaling preserves the ladder's shape; what no test then covers is the
/// production base reaching this call, which is one constant.
fn push_retry_delay(attempt: u32) -> Duration {
    let delay = jitter(push_retry_backoff(attempt));
    #[cfg(test)]
    let delay = delay / 1000;
    delay
}

/// One `ocx package push [--cascade] -p {platform} -i {target_ref} {bundle}
/// --format json` subprocess, bounded by `timeout`.
///
/// `timeout` is a parameter rather than the constant read directly so the bound
/// itself can be tested without an hour-long test.
pub(crate) async fn push_once(
    ocx_binary: &Path,
    args: &[String],
    timeout: Duration,
) -> Result<PushReport, PushAttemptError> {
    let mut cmd = tokio::process::Command::new(ocx_binary);
    cmd.args(args);

    // Forward OCX_* environment variables into the subprocess.
    // This preserves offline mode, remote mode, registry config, etc.
    forward_ocx_env(&mut cmd);

    // Tokio leaves a child running when its future is dropped; on timeout that
    // would orphan a push still streaming a bundle at the registry — and the
    // retry would then race it.
    cmd.kill_on_drop(true);

    let output = match tokio::time::timeout(timeout, cmd.output()).await {
        Err(_) => {
            return Err(PushAttemptError {
                message: format!("ocx package push timed out after {}s", timeout.as_secs()),
                transient: true,
            });
        }
        Ok(Err(e)) => {
            return Err(PushAttemptError {
                message: format!("failed to spawn ocx: {e}"),
                transient: false,
            });
        }
        Ok(Ok(output)) => output,
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(PushAttemptError {
            message: format!("ocx package push exited {}: {}", output.status, stderr.trim()),
            transient: push_exit_is_transient(output.status.code()),
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(stdout.trim()).map_err(|e| PushAttemptError {
        // A push that exited 0 did its work; an unreadable report is this
        // pipeline disagreeing with that `ocx` about a format, which a second
        // run reproduces exactly.
        message: format!("failed to parse push JSON output: {e}\nstdout: {}", stdout.trim()),
        transient: false,
    })
}

/// Run one push argv to a verdict: attempt it, and retry a transient failure
/// (`ocx package push` exit 75 only) up to `budget` further times with
/// [`push_retry_delay`] between attempts.
///
/// Shared by both publish paths — the archive leg via [`invoke_push`], the env
/// leg via `pipeline::python_push::invoke_env_push` — so the ladder, the
/// transience predicate and the operator-facing wording exist once. `label` is
/// the mirror name that prefixes every line; `target_ref` and `platform` name
/// the leg in them.
///
/// Returns the parsed [`PushReport`] on success, or a descriptive error string
/// (caller records it as `push_error` without aborting the run).
pub(crate) async fn push_with_retry(
    ocx_binary: &Path,
    args: &[String],
    budget: u32,
    label: &str,
    target_ref: &str,
    platform: &str,
) -> Result<PushReport, String> {
    // The budget, named in every line this loop emits: an operator reading a
    // give-up message has to be able to tell an exhausted ladder from an exit
    // code that was never going to be retried, and to find the knob either way.
    let total = budget.saturating_add(1);
    let mut attempt = 1u32;
    loop {
        match push_once(ocx_binary, args, PUSH_TIMEOUT).await {
            Ok(report) => return Ok(report),
            Err(failure) => {
                if !failure.transient {
                    return Err(format!(
                        "{} — this exit code is not retried, whatever concurrency.max_retries ({budget}) grants",
                        failure.message,
                    ));
                }
                if attempt >= total {
                    return Err(format!(
                        "{} — gave up after {total} attempt(s); raise concurrency.max_retries ({budget}) to grant more",
                        failure.message,
                    ));
                }
                let backoff = push_retry_delay(attempt);
                // `{:?}` rather than whole seconds: the delay is jittered, so
                // the first retry lands just under or over a second and
                // `as_secs()` reported half of them as "0s".
                log::warn!(
                    "[{label}] push attempt {attempt}/{total} for {target_ref} ({platform}) failed, retrying in {backoff:?}: {}",
                    failure.message,
                );
                tokio::time::sleep(backoff).await;
                attempt += 1;
            }
        }
    }
}

#[cfg(test)]
#[path = "push/tests.rs"]
mod tests;
