// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Resolving `sign:` into what an `ocx` child needs, and the two
//! `ocx package sign` invocations that finish the job.
//!
//! Division of labour (`adr_mirror_signing.md` D2): `ocx package push --sign`
//! signs each **platform manifest** inline, so every push leg carries
//! [`sign_push_args`]'s tail; the enclosing **image index** is only whole once
//! its last platform has landed, so [`invoke_sign_sweep`] signs the indexes
//! afterwards from the tag list the run already writes. The in-process
//! `Publisher` leg has no `--sign` to pass, so it signs through
//! [`invoke_sign_reference`] once its push returns.
//!
//! The mirror resolves every `Ref` itself rather than handing ocx a
//! `[trust.sigstore]` table: that table is one global per machine and
//! consumer-side, so a host that consumes public `ocx.sh` packages could not
//! also publish against a corporate Sigstore. Endpoints therefore always
//! travel as explicit flags, and the two secret-class values
//! (`identity_token`, `passphrase`) travel in the child's **environment** —
//! never argv, never a log line (C-054).

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::Path;
use std::time::Duration;

use ocx_lib::log;

use super::{forward_ocx_env, resolve_ocx_binary};
use crate::error::MirrorError;
use crate::spec::{KeyConfig, KeylessConfig, Ref, SignConfig};

/// Public Sigstore, emitted as `--fulcio-url` when `sign.keyless.fulcio` is
/// omitted.
///
/// Mirror-owned rather than left to ocx's own default: C-052 requires both
/// endpoints on every keyless argv, so that a fleet `[trust.sigstore]` naming
/// a different Fulcio cannot change what this mirror publishes against.
pub(crate) const DEFAULT_FULCIO_URL: &str = "https://fulcio.sigstore.dev";

/// Public Rekor, emitted as `--rekor-url` when `sign.keyless.rekor` is
/// omitted. Same reasoning as [`DEFAULT_FULCIO_URL`].
pub(crate) const DEFAULT_REKOR_URL: &str = "https://rekor.sigstore.dev";

/// Resolve every tag against the registry, never the local index.
///
/// `ocx package push` writes a tag -> digest pin into `$OCX_HOME/index` and
/// **does not update the digest when a later push moves that tag** — it
/// refreshes only the `observed` timestamp. A sign child sharing a warm
/// `OCX_HOME` therefore resolves a re-pushed tag to the digest it held before
/// the run, and signs nothing: `manifest not found: <repo>@sha256:<old>`,
/// exit 79, for a manifest the registry is serving right now. Reproduced on
/// `pipeline push`'s own sweep by any second run that moves an index, not just
/// on `pipeline patch`.
///
/// Signing is always about what the registry holds *now*, so a local pin has
/// nothing to offer here; `--remote` routes exactly the tag -> manifest lookup
/// to the registry and leaves digest-addressed reads local-first.
const REMOTE_RESOLUTION: &str = "--remote";

/// The largest a `file://` ref's file may be.
///
/// The values behind one are an OIDC token or a key passphrase — hundreds of
/// bytes each. The cap is not a size expectation, it is a bound on reading a
/// path an operator names: without it, `file:///dev/zero` is an OOM
/// (PKG-04/PKG-07). Matches ocx's own `--identity-token-file` ceiling.
pub(crate) const MAX_SECRET_FILE_BYTES: u64 = 64 * 1024;

/// How long one `ocx package sign` child may run before it is killed.
///
/// A backstop against a wedged child, not a throughput expectation: signing is
/// a Fulcio round trip, a Rekor round trip and one referrer push per subject,
/// and `ocx` bounds each registry request itself. A sweep does that once per
/// tag, so the bound is generous enough for a large cascade and still far
/// inside a CI job's own limit.
pub(crate) const SIGN_TIMEOUT: Duration = Duration::from_secs(900);

/// The child environment variable carrying the OIDC identity token.
const ENV_IDENTITY_TOKEN: &str = "OCX_IDENTITY_TOKEN";

/// The child environment variable carrying a private key's passphrase.
const ENV_KEY_PASSWORD: &str = "OCX_KEY_PASSWORD";

/// A resolved secret, held so it cannot be printed by accident.
///
/// No `Display`, and a `Debug` that redacts (API-02): the only way to reach
/// the value is [`Secret::expose`], which has exactly one call site —
/// [`ocx_child_env`]. `secrecy` would give the same property plus zeroization,
/// but the intermediate heap copies its own docs disclaim make that a
/// dependency for the half we already have.
struct Secret(String);

impl Secret {
    /// The value, for handing to a child process's environment. Never for a
    /// log line, an error message, or an argv word.
    fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// `sign:` resolved once per run: the argv tail every ocx child shares, and
/// the secrets that reach the child through its environment instead.
///
/// Built by [`resolve_sign`] before the first push (C-054) so a missing
/// variable fails the run at one place, with one message, rather than once per
/// leg.
#[derive(Debug)]
pub(crate) struct ResolvedSign {
    /// The C-052 tail *without* `--sign`: `push` prepends it,
    /// `sign` does not take it.
    flags: Vec<String>,
    /// `OCX_IDENTITY_TOKEN` / `OCX_KEY_PASSWORD`. `BTreeMap` so the applied
    /// order is fixed and a test can assert on it.
    child_env: BTreeMap<&'static str, Secret>,
}

/// The `--sign` tail for one `ocx package push` argv (C-052).
///
/// `None` — no `sign:` block — yields an empty vector, which is what keeps an
/// unsigned mirror's argv byte-identical to what it was before signing existed.
pub(crate) fn sign_push_args(sign: Option<&ResolvedSign>) -> Vec<String> {
    match sign {
        None => Vec::new(),
        Some(resolved) => std::iter::once("--sign".to_string())
            .chain(resolved.flags.iter().cloned())
            .collect(),
    }
}

/// Apply the resolved secrets to an `ocx` child.
///
/// Called beside [`forward_ocx_env`] at every spawn site. The two are separate
/// because `OCX_VARS` is a *forwarding* list — names inherited from this
/// process — and these two values are resolved here, from refs, and may not
/// exist in this process's environment at all.
pub(crate) fn ocx_child_env(cmd: &mut tokio::process::Command, sign: Option<&ResolvedSign>) {
    let Some(resolved) = sign else { return };
    for (name, secret) in &resolved.child_env {
        cmd.env(name, secret.expose());
    }
}

/// Resolve `sign:` against the process environment and filesystem.
///
/// The production entry point; [`resolve_sign`] is the pure core it wraps
/// (ARCH-12). `None` in, `None` out — a mirror without `sign:` publishes
/// unsigned, unchanged.
///
/// # Errors
/// [`MirrorError::SignMaterialMissing`] when a ref names a variable that is
/// unset or a file that cannot be read; [`MirrorError::SpecUsageError`] when
/// the block names neither mode or both (already refused by `validate_sign_config`,
/// so unreachable through `load_spec`).
pub(crate) fn resolve_sign_from_env(config: Option<&SignConfig>) -> Result<Option<ResolvedSign>, MirrorError> {
    config
        .map(|config| resolve_sign(config, &|name| std::env::var_os(name), &read_bounded))
        .transpose()
}

/// Read a `file://` ref, bounded at [`MAX_SECRET_FILE_BYTES`].
///
/// `take(cap + 1)` rather than a metadata check then a read: the bound has to
/// stop the read itself, and `cap + 1` is what distinguishes "exactly at the
/// cap" from "over it" (PKG-04). A `/proc` file or a pipe reports no length to
/// pre-check against anyway.
fn read_bounded(path: &Path) -> io::Result<Vec<u8>> {
    use std::io::Read as _;

    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_SECRET_FILE_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Resolve every [`Ref`] under `sign:` into flags and child-environment values.
///
/// Pure over the injected readers so the unset-variable, unreadable-file and
/// oversized-file paths are unit-testable without touching either (ARCH-12).
///
/// Only `sign.key`'s reference passes through **verbatim**: ocx's `--key`
/// takes its own `KeyRef` grammar — a bare path, `file://PATH`, or `env://NAME`
/// whose variable holds the PEM itself — which [`Ref`]'s rendering is already a
/// subset of. Everything else is resolved here: endpoints because they must not
/// fall through to `[trust.sigstore]`, secrets because ocx offers no flag that
/// would keep them off argv.
///
/// # Errors
/// See [`resolve_sign_from_env`].
pub(crate) fn resolve_sign(
    config: &SignConfig,
    lookup: &dyn Fn(&str) -> Option<OsString>,
    read: &dyn Fn(&Path) -> io::Result<Vec<u8>>,
) -> Result<ResolvedSign, MirrorError> {
    let mut flags = Vec::new();
    let mut child_env = BTreeMap::new();

    match (&config.keyless, &config.key) {
        (Some(keyless), None) => {
            resolve_keyless(keyless, lookup, read, &mut flags, &mut child_env)?;
        }
        (None, Some(key)) => {
            resolve_key(key, lookup, read, &mut flags, &mut child_env)?;
        }
        // Both refusals belong to `validate_sign_config` (C-051), which runs
        // inside `load_spec` — so neither arm is reachable from a loaded spec.
        // Repeated rather than `unreachable!`: a panic here would be a crash
        // on a spec shape, and the exit code is the same 64 either way.
        (None, None) => {
            return Err(MirrorError::SpecUsageError(
                "sign: names neither `keyless` nor `key`".to_string(),
            ));
        }
        (Some(_), Some(_)) => {
            return Err(MirrorError::SpecUsageError(
                "sign: names both `keyless` and `key`; exactly one is allowed".to_string(),
            ));
        }
    }

    Ok(ResolvedSign { flags, child_env })
}

/// `--sign --fulcio-url <U> --rekor-url <U>`, both endpoints always present.
fn resolve_keyless(
    keyless: &KeylessConfig,
    lookup: &dyn Fn(&str) -> Option<OsString>,
    read: &dyn Fn(&Path) -> io::Result<Vec<u8>>,
    flags: &mut Vec<String>,
    child_env: &mut BTreeMap<&'static str, Secret>,
) -> Result<(), MirrorError> {
    let fulcio = match &keyless.fulcio {
        Some(reference) => resolve_ref(reference, "sign.keyless.fulcio", lookup, read)?,
        None => DEFAULT_FULCIO_URL.to_string(),
    };
    let rekor = match &keyless.rekor {
        Some(reference) => resolve_ref(reference, "sign.keyless.rekor", lookup, read)?,
        None => DEFAULT_REKOR_URL.to_string(),
    };
    flags.extend(["--fulcio-url".to_string(), fulcio, "--rekor-url".to_string(), rekor]);

    // `ocx package push` carries no `--identity-token-*` flag at all — the
    // deliberate design that keeps a token off argv — so `OCX_IDENTITY_TOKEN`
    // is the one channel that works on both the push leg and the sweep, for
    // every ref form. `package sign`'s `--identity-token-file` would work only
    // for the `file://` form, only on the sweep, and would put the token's
    // path in argv for nothing.
    if let Some(reference) = &keyless.identity_token {
        let token = resolve_ref(reference, "sign.keyless.identity_token", lookup, read)?;
        child_env.insert(ENV_IDENTITY_TOKEN, Secret(token));
    }
    Ok(())
}

/// `--sign --key <ref>` then `--rekor-upload --rekor-url <U>` or
/// `--no-rekor-upload`.
///
/// Silence means `--no-rekor-upload`, never a fleet
/// `[trust.sigstore].rekor_upload`: an omitted `rekor:` must not push a
/// private digest to the public transparency log (ADR D1).
fn resolve_key(
    key: &KeyConfig,
    lookup: &dyn Fn(&str) -> Option<OsString>,
    read: &dyn Fn(&Path) -> io::Result<Vec<u8>>,
    flags: &mut Vec<String>,
    child_env: &mut BTreeMap<&'static str, Secret>,
) -> Result<(), MirrorError> {
    let (reference, passphrase, rekor) = match key {
        KeyConfig::Reference(reference) => (reference, &None, &None),
        KeyConfig::Full(full) => (&full.reference, &full.passphrase, &full.rekor),
    };

    // Verbatim: `env://NAME` here means "the variable holds the PEM", which is
    // ocx's grammar, not a mirror indirection to resolve.
    flags.extend(["--key".to_string(), String::from(reference.clone())]);

    match rekor {
        Some(reference) => {
            let url = resolve_ref(reference, "sign.key.rekor", lookup, read)?;
            flags.extend(["--rekor-upload".to_string(), "--rekor-url".to_string(), url]);
        }
        None => flags.push("--no-rekor-upload".to_string()),
    }

    if let Some(reference) = passphrase {
        let secret = resolve_ref(reference, "sign.key.passphrase", lookup, read)?;
        child_env.insert(ENV_KEY_PASSWORD, Secret(secret));
    }
    Ok(())
}

/// One `Ref` to its value.
///
/// A `Literal` under a secret-class field is refused by `validate_sign_config`
/// (C-051) before this runs; reached anyway it is taken verbatim, so the value
/// still lands in the child's environment rather than in argv — the property
/// that matters here.
///
/// `field` is the dotted spec field, and it is the only thing besides the
/// variable name or path that ever reaches an error message.
fn resolve_ref(
    reference: &Ref,
    field: &str,
    lookup: &dyn Fn(&str) -> Option<OsString>,
    read: &dyn Fn(&Path) -> io::Result<Vec<u8>>,
) -> Result<String, MirrorError> {
    match reference {
        Ref::Literal(literal) => Ok(literal.clone()),
        Ref::Env(name) => match lookup(name) {
            None => Err(MirrorError::SignMaterialMissing {
                field: field.to_string(),
                source: format!("environment variable {name} is not set"),
            }),
            // The value is dropped, never rendered: `into_string` hands the
            // offending `OsString` back in its `Err`, and for a secret-class
            // field that is exactly what must not reach the message.
            Some(value) => value.into_string().map_err(|_| MirrorError::SignMaterialMissing {
                field: field.to_string(),
                source: format!("environment variable {name} is not valid UTF-8"),
            }),
        },
        Ref::File(path) => {
            let bytes = read(path).map_err(|e| MirrorError::SignMaterialMissing {
                field: field.to_string(),
                source: format!("{} ({e})", path.display()),
            })?;
            if bytes.len() as u64 > MAX_SECRET_FILE_BYTES {
                return Err(MirrorError::SignMaterialMissing {
                    field: field.to_string(),
                    source: format!("{} is larger than {MAX_SECRET_FILE_BYTES} bytes", path.display()),
                });
            }
            // Same `Err`-drops-the-value reasoning as the `env://` arm.
            let text = String::from_utf8(bytes).map_err(|_| MirrorError::SignMaterialMissing {
                field: field.to_string(),
                source: format!("{} is not valid UTF-8", path.display()),
            })?;
            // A token or passphrase written by `printf`/`echo` carries the
            // trailing newline; ocx trims its own file reads the same way.
            Ok(text.trim().to_string())
        }
    }
}

/// One swept tag's row, as much of `ocx package sign --tags-file`'s JSON as
/// this pipeline reads.
///
/// `skipped` is a bare-manifest tag the sweep left alone — `push --sign`
/// already signed it inline — and it is **not** a failure (C-058). Collected
/// so the run can say so; `ocx` itself already excludes it from its exit code.
#[derive(Debug, Default, serde::Deserialize)]
struct SweepReport {
    #[serde(default)]
    tags: Vec<SweptTagRow>,
}

/// ocx's `--format json` envelope, of which this pipeline reads only `data`.
///
/// The rows are nested — `{"schema_version":1,"command":…,"exit_code":…,
/// "data":{"tags":[…]}}` — so deserializing [`SweepReport`] from the top level
/// silently yielded an empty `tags` and narrated nothing, for every sweep this
/// pipeline has ever run.
#[derive(Debug, serde::Deserialize)]
struct SweepEnvelope {
    #[serde(default)]
    data: SweepReport,
}

#[derive(Debug, serde::Deserialize)]
struct SweptTagRow {
    tag: String,
    status: SweptOutcome,
}

#[derive(Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum SweptOutcome {
    Completed,
    /// `SkippedBareManifest` on ocx's side: nothing to sign at the index level.
    Skipped,
    /// The tag resolves to an index another tag in the same sweep already
    /// signed, so one referrer covers both. Not a failure — a cascade points
    /// several tags at one index, and a referrer is filed against the subject
    /// digest, so acting per tag would publish N identical referrers.
    Covered,
    Failed,
    /// A status this ocx emits and this mirror does not know. Keeps the
    /// envelope parsing — `report_sweep` narrates only `Skipped` and
    /// `Covered`, so an unrecognised row goes unmentioned rather than
    /// failing the whole report. Never counted as a failure either: the
    /// child's exit code is the verdict.
    #[serde(other)]
    Unknown,
}

/// The `ocx package sign --tags-file` argv (C-058).
///
/// Pure, so the word list is pinned by a unit test rather than by reading a
/// spawned child's `/proc/<pid>/cmdline`. The tail is [`sign_push_args`]'s
/// **minus `--sign`**: that flag is `package push`'s opt-in, and `package
/// sign` rejects it as an unknown flag (exit 64).
fn sweep_args(sign: &ResolvedSign, tags_file: &str, reference: &str) -> Vec<String> {
    let mut args: Vec<String> = [
        "--format",
        "json",
        REMOTE_RESOLUTION,
        "package",
        "sign",
        "--tags-file",
        tags_file,
        reference,
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    args.extend(sign.flags.iter().cloned());
    args
}

/// The single-reference `ocx package sign` argv (C-059).
///
/// `-p` appears exactly when a platform is given: an index-level sign must not
/// narrow into one, and `ocx` refuses `--platform` alongside a sweep for the
/// same reason.
fn sign_reference_args(sign: &ResolvedSign, reference: &str, platform: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = ["--format", "json", REMOTE_RESOLUTION, "package", "sign"]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    if let Some(platform) = platform {
        args.extend(["-p".to_string(), platform.to_string()]);
    }
    args.push(reference.to_string());
    args.extend(sign.flags.iter().cloned());
    args
}

/// Sign the indexes named by a `--tags-file` (C-058).
///
/// The closing half of D2: `push --sign` signed each platform manifest as it
/// landed, and this signs the image index each written tag now points at, once
/// the last platform of the run is in.
///
/// # Errors
/// [`MirrorError::SignFailed`] carrying the child's own exit code.
pub(crate) async fn invoke_sign_sweep(
    sign: &ResolvedSign,
    tags_file: &Path,
    reference: &str,
) -> Result<(), MirrorError> {
    let file = tags_file
        .to_str()
        .ok_or_else(|| MirrorError::SignFailed {
            target: reference.to_string(),
            code: ocx_lib::cli::ExitCode::DataError as i32,
        })
        .inspect_err(|_| {
            log::warn!("[sign] tags file path is not valid UTF-8: {}", tags_file.display());
        })?;

    let stdout = run_sign(sign, &sweep_args(sign, file, reference), reference, SIGN_TIMEOUT).await?;
    report_sweep(&stdout, reference);
    Ok(())
}

/// Write `tags` and sign the image index behind each of them (C-058).
///
/// The shared body of `pipeline push`'s closing sweep and `pipeline patch`'s:
/// both move tags onto an index `push --sign` never reaches — it signs the
/// platform manifest, and the index digest is still moving while the platforms
/// land — and both must end with exactly one referrer per distinct index.
///
/// A no-op without `sign:`, and a no-op when the caller wrote no tags: `ocx
/// package sign --tags-file` over an empty file has nothing to act on and the
/// child would only cost a process.
///
/// # Errors
/// [`MirrorError::SignFailed`] carrying the child's own exit code, so a Rekor
/// outage (83) stays distinguishable from a missing identity (78) and from a
/// registry without a Referrers API (84).
pub(crate) async fn sweep_index_tags(
    sign: Option<&ResolvedSign>,
    tags: &[String],
    reference: &str,
    tags_file: &Path,
) -> Result<(), MirrorError> {
    let Some(resolved) = sign else { return Ok(()) };
    if tags.is_empty() {
        return Ok(());
    }

    if let Some(parent) = tags_file.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|e| {
            MirrorError::TemplateError(format!(
                "failed to create sign tags directory {}: {e}",
                parent.display()
            ))
        })?;
    }
    tokio::fs::write(tags_file, tags.join("\n"))
        .await
        .map_err(|e| MirrorError::TemplateError(format!("failed to write {}: {e}", tags_file.display())))?;

    log::info!("[sign] {reference} — signing {} index(es)", tags.len());
    invoke_sign_sweep(resolved, tags_file, reference).await
}

/// Sign one reference, optionally narrowed into one platform (C-059).
///
/// The in-process `Publisher` leg's counterpart to `push --sign`: that leg
/// writes manifests through `ocx_lib` rather than a subprocess, so there is no
/// `--sign` to pass and the signature is attached afterwards.
///
/// # Errors
/// [`MirrorError::SignFailed`] carrying the child's own exit code.
pub(crate) async fn invoke_sign_reference(
    sign: &ResolvedSign,
    reference: &str,
    platform: Option<&str>,
) -> Result<(), MirrorError> {
    run_sign(
        sign,
        &sign_reference_args(sign, reference, platform),
        reference,
        SIGN_TIMEOUT,
    )
    .await
    .map(|_stdout| ())
}

/// One `ocx package sign` child, bounded by `timeout` and killed on drop,
/// returning its stdout.
///
/// `timeout` is a parameter rather than [`SIGN_TIMEOUT`] read directly, so the
/// bound itself can be tested without a fifteen-minute test — the same seam
/// `push_once` takes.
///
/// Nothing here logs a resolved secret: the argv carries none by construction
/// (C-054), and the child's stderr is reproduced only through the exit code.
async fn run_sign(
    sign: &ResolvedSign,
    args: &[String],
    reference: &str,
    timeout: Duration,
) -> Result<String, MirrorError> {
    let ocx_binary = resolve_ocx_binary().map_err(|e| {
        log::warn!("[sign] {e}");
        MirrorError::SignFailed {
            target: reference.to_string(),
            code: ocx_lib::cli::ExitCode::Unavailable as i32,
        }
    })?;

    let mut cmd = tokio::process::Command::new(&ocx_binary);
    cmd.args(args);
    forward_ocx_env(&mut cmd);
    ocx_child_env(&mut cmd, Some(sign));
    // Tokio leaves a child running when its future is dropped; on timeout that
    // would orphan a sign still pushing a referrer manifest at the registry.
    cmd.kill_on_drop(true);

    let output = match tokio::time::timeout(timeout, cmd.output()).await {
        Err(_) => {
            log::warn!("[sign] {reference} timed out after {}s", timeout.as_secs());
            // A stalled child is the transient class, and 75 is the code this
            // pipeline already retries on.
            return Err(MirrorError::SignFailed {
                target: reference.to_string(),
                code: ocx_lib::cli::ExitCode::TempFail as i32,
            });
        }
        Ok(Err(e)) => {
            log::warn!("[sign] failed to spawn ocx: {e}");
            return Err(MirrorError::SignFailed {
                target: reference.to_string(),
                code: ocx_lib::cli::ExitCode::Unavailable as i32,
            });
        }
        Ok(Ok(output)) => output,
    };

    if !output.status.success() {
        // `ocx`'s own output, forwarded verbatim so the operator sees the
        // absent-ambient-provider message rather than a bare exit code (C-055).
        //
        // stdout is the fallback, not an afterthought: under `--format json`
        // ocx reports a failure in its stdout envelope (CLI-04), so a real
        // failure — a sweep whose every tag failed — reaches here with stderr
        // empty, and logging stderr alone left the operator an exit code and
        // nothing else.
        let stderr = String::from_utf8_lossy(&output.stderr); // LOSSY-OK: display
        let stdout = String::from_utf8_lossy(&output.stdout); // LOSSY-OK: display
        log::warn!("[sign] {reference}: {}", failure_detail(&stderr, &stdout));
        return Err(MirrorError::SignFailed {
            target: reference.to_string(),
            // `None` (signal-killed) classifies as `Failure` like any
            // unrecognised code — the run did not sign either way.
            code: output.status.code().unwrap_or(ocx_lib::cli::ExitCode::Failure as i32),
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned()) // LOSSY-OK: display
}

/// What to show the operator when a sign child fails: its stderr, or its
/// stdout when stderr said nothing.
///
/// Split out only so the choice is reachable by a test — the branch itself is
/// three lines, but getting it wrong costs an operator an exit code with no
/// message at all, which is the failure this exists to prevent.
fn failure_detail<'a>(stderr: &'a str, stdout: &'a str) -> &'a str {
    match stderr.trim() {
        "" => stdout.trim(),
        text => text,
    }
}

/// Log what a sweep did, treating a `skipped` row as the ordinary outcome it is.
///
/// Unparseable stdout is not an error: the child exited 0, so the indexes are
/// signed, and this is only the narration. That is the opposite of `push`,
/// whose report carries the cascade tags the run then acts on.
fn report_sweep(stdout: &str, reference: &str) {
    let Ok(envelope) = serde_json::from_str::<SweepEnvelope>(stdout.trim()) else {
        return;
    };
    let report = envelope.data;
    let rows = |wanted: SweptOutcome| -> Vec<&str> {
        report
            .tags
            .iter()
            .filter(|row| row.status == wanted)
            .map(|row| row.tag.as_str())
            .collect()
    };
    let skipped = rows(SweptOutcome::Skipped);
    if !skipped.is_empty() {
        log::info!(
            "[sign] {reference}: {} bare-manifest tag(s) already signed by their push: {}",
            skipped.len(),
            skipped.join(", "),
        );
    }
    // Narrated beside the skipped line for the same reason: an operator
    // counting referrers against a cascade release sees fewer than there are
    // tags, and this is the row that says why.
    let covered = rows(SweptOutcome::Covered);
    if !covered.is_empty() {
        log::info!(
            "[sign] {reference}: {} tag(s) share an index another tag in this sweep signed: {}",
            covered.len(),
            covered.join(", "),
        );
    }
}

#[cfg(test)]
#[path = "sign/tests.rs"]
mod tests;
