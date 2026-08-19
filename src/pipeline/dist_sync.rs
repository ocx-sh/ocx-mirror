// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `dist sync` — mirror the OCX distribution (release archives + `dist.json`)
//! into a store a restricted network can reach.
//!
//! # The two invariants
//!
//! **Clobber-safety.** A run that cannot place every selected archive writes
//! **no manifest at all**. The destination keeps the previous, internally
//! consistent manifest rather than gaining one that promises archives the
//! store does not hold — the same rule `www-setup/scripts/gen-dist.sh`
//! enforces on the generating side, for the same reason. Its second half: a
//! run that selected *nothing* publishes nothing either. That case satisfies
//! "every selected archive landed" trivially, so the partial-run guard alone
//! would read a mistyped `select.min_version` as a success and overwrite a
//! working manifest with one naming no releases.
//!
//! **Publish order.** Archives first, then the content-addressed snapshot, then
//! the rolling `dist.json` last. A consumer reading mid-run therefore resolves
//! either the old manifest or the new one, and both are fully backed by bytes
//! already in the store.
//!
//! **One pass, not two.** Each row is probed, fetched, uploaded and cleaned up
//! on its own ([`mirror_row`]), so one archive's PUT overlaps another's GET —
//! a link is full duplex, and the two-phase shape this replaced left one
//! direction idle throughout each phase. The destination is asked *before*
//! anything is fetched, so an archive the store already holds costs neither
//! transfer; on a CI runner with an empty `output:` that is the difference
//! between pulling the whole mirror and pulling nothing.
//!
//! **Rolling versus immutable.** Archives and the snapshot are content-addressed
//! by name and may be skipped when the store already holds them. `dist.json` is
//! rolling — the path outlives its contents — and is republished every run
//! ([`upload::Precheck`]).
//!
//! There is deliberately **no `dist.json.sha256` sidecar.** Nothing read it:
//! `install.sh` verifies each archive against the manifest's own inline
//! `sha256` and says so in as many words, and pinning is `dist/<sha256>.json`.
//! It also could not be served faithfully — Artifactory reads a PUT to a
//! `*.sha256` path as a checksum declaration about the sibling artifact rather
//! than as a file, 404ing when the sibling does not exist yet and synthesising
//! its own body when it does.

pub mod layout;
pub mod manifest;
pub mod report;
pub mod upload;

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::{StreamExt as _, TryStreamExt as _};
use sha2::{Digest as _, Sha256};
use tokio::sync::Semaphore;
use url::Url;

use self::layout::{LayoutTemplate, RowValues};
use self::manifest::{DistManifest, ReleaseRow, SUPPORTED_SCHEMA};
use self::report::{ArchiveOutcome, ArchiveReport, DistSyncReport, RunCounters};
use self::upload::{Precheck, Probe, UploadOutcome, Uploader};
use crate::error::MirrorError;
use crate::pipeline::download::download;
use crate::pipeline::verify::verify_digest;
use crate::spec::DistSpec;

/// Fixed name of the rolling manifest, relative to `output:` and to
/// `publish.base_url`.
const MANIFEST_NAME: &str = "dist.json";
/// Directory the content-addressed snapshots live in.
const SNAPSHOT_DIR: &str = "dist";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// Ceiling on the upstream manifest body (CWE-400).
///
/// The same guard `registry_sync::catalog` applies to an index document, for
/// the same reason: this body arrives from the network before anything about
/// it has been checked, and `Response::text` would buffer whatever a hostile
/// or broken endpoint chose to send. The bound is far tighter than that one's
/// 32 MiB because the shape is known — a `dist.json` is one row per release
/// and target, a few hundred bytes each, so even fifteen years of releases
/// sits three orders of magnitude below this.
const MANIFEST_FETCH_CEILING: usize = 8 * 1024 * 1024;

/// Fetch, filter, mirror, and optionally upload.
///
/// # Errors
///
/// [`MirrorError::SourceError`] when the upstream manifest cannot be fetched
/// or parsed, [`MirrorError::DistSchemaUnsupported`] for a schema this binary
/// cannot re-emit, and [`MirrorError::IndexWriteError`] for a failed write
/// into `output:`. Per-archive failures are counted into the report and
/// classified by the caller.
///
/// Takes `dry_run` as a bare `bool` rather than the CLI options struct: it is
/// the only field this layer reads, and borrowing the whole struct would put an
/// upward edge on `command::` for nothing. (`registry_sync` does take its
/// options struct — it reads four of that one's five fields.)
pub async fn execute_dist_sync(spec: &DistSpec, dry_run: bool) -> Result<DistSyncReport, MirrorError> {
    let client = build_client()?;

    // Before the first byte moves. Resolving credentials here rather than at
    // the upload pass means a spec naming an unset variable fails in seconds,
    // not after a multi-gigabyte download the operator then has to repeat —
    // and it is what `docs/reference/dist-yml.md` promises.
    //
    // Skipped entirely under `--dry-run`, which promises to report what would
    // be mirrored without touching anything: a fork-PR CI job has no secrets,
    // and demanding them to plan a run that uploads nothing would make the
    // flag useless exactly where it is most useful.
    let uploader = match &spec.upload {
        Some(config) if !dry_run => Some(
            Uploader::new(build_upload_client()?, &spec.publish.base_url, config)
                .map_err(|error| MirrorError::ExecutionFailed(vec![format!("{error:#}")]))?,
        ),
        _ => None,
    };

    let mut manifest = fetch_manifest(&client, spec).await?;
    manifest.apply_select(&spec.select);

    // Clobber-safety's other half. The gate below refuses to publish a manifest
    // naming an archive that did not land; this refuses to publish one naming
    // *nothing*, which that gate reads as a run where everything landed.
    //
    // The reachable case is a `select.min_version` typo — a bound above every
    // release upstream holds. Left alone it would overwrite a working
    // `dist.json` with an empty one and strand every consumer, and nothing in
    // the report would look wrong.
    if manifest.releases.is_empty() {
        return Err(MirrorError::ExecutionFailed(vec![format!(
            "no release survived `select:`; publishing an empty {MANIFEST_NAME} would strand every consumer \
             of this mirror, so the run stops instead"
        )]));
    }

    // Already validated at spec load; parsed again here because the template
    // is a value, not a validation result.
    let template = LayoutTemplate::parse(&spec.publish.layout)
        .map_err(|error| MirrorError::SpecInvalid(vec![format!("publish.layout: {error}")]))?;

    let mut report = DistSyncReport {
        dry_run,
        ..DistSyncReport::default()
    };

    // ── Pass 1 — plan every row, serially ───────────────────────────────────
    //
    // Everything here is pure and cheap, and every part of it needs exclusive
    // access to something the fetch pass cannot share: the row itself (its
    // `url` is rewritten to point at the mirror) and the `claimed` map. Hoisting
    // it also makes collision detection a whole-run property decided before the
    // first byte moves, the same shape `registry_sync`'s phase 1 already has.
    //
    // Two rows rendering to one path would have the second silently overwrite
    // the first, and the manifest would then name one file for two targets —
    // a wrong-binary install with nothing in the output to show for it.
    let mut claimed: HashMap<String, String> = HashMap::with_capacity(manifest.releases.len());
    let mut planned: Vec<PlannedRow> = Vec::with_capacity(manifest.releases.len());
    // Indexed rather than pushed, so the report keeps manifest order whatever
    // order the fetches below finish in.
    let mut reports: Vec<Option<ArchiveReport>> = (0..manifest.releases.len()).map(|_| None).collect();

    for (index, row) in manifest.releases.iter_mut().enumerate() {
        let name = row.label();
        match plan_row(spec, &template, row, index, name.clone(), &mut claimed) {
            Ok(plan) => planned.push(plan),
            Err(detail) => {
                reports[index] = Some(ArchiveReport {
                    name,
                    outcome: ArchiveOutcome::Failed,
                    detail: Some(detail),
                });
            }
        }
    }

    // ── Pass 2 — probe, fetch and upload each row, concurrently ─────────────
    //
    // One pass, not two. Each row runs probe → download → verify → upload → and
    // its own cleanup, so one archive's PUT overlaps another's GET instead of
    // every download finishing before the first upload starts.
    let total = planned.len();
    let retain = spec.retain_archives_resolved();
    let upload_permits = Semaphore::new(spec.concurrency.max_uploads);
    // Set by the first rejected write, read by every row before its own PUT.
    // Plain `Relaxed`: `buffered` polls every row from one task, so the flag
    // passes between futures that never run at the same instant — it is here
    // for shared interior mutability, not to order anything.
    let upload_failed = AtomicBool::new(false);
    let fetched: Vec<(PlannedRow, RowStep)> = if dry_run {
        // `--dry-run` promises a report of what *would* be mirrored, so the
        // plan above is the whole of the work.
        planned
            .into_iter()
            .map(|plan| {
                (
                    plan,
                    RowStep::Done(RowOutcome {
                        archive: ArchiveOutcome::Planned,
                        upload: None,
                    }),
                )
            })
            .collect()
    } else {
        // `buffered`, not `buffer_unordered`: it runs the same number
        // concurrently but yields in input order, which is the determinism the
        // report needs without an index sort afterwards.
        //
        // `try_collect` stops polling at the first *abort-class* error, but not
        // instantly: it only sees the error once that future returns, and
        // `buffered` keeps polling the rest until then. `upload_failed` closes
        // that window, so a store answering `401` sees at most the writes
        // already in flight — never one per archive. A row that merely failed
        // to fetch is not in that class: it comes back as `RowStep::Failed`
        // inside `Ok` and the pass runs on, so the operator sees every bad row
        // in one report.
        futures::stream::iter(planned.into_iter().enumerate().map(|(position, plan)| {
            let client = &client;
            let uploader = uploader.as_ref();
            let upload_permits = &upload_permits;
            let upload_failed = &upload_failed;
            async move {
                tracing::info!("[{}/{total}] {} → {}", position + 1, plan.name, plan.relative);
                let step = mirror_row(client, spec, uploader, upload_permits, upload_failed, &plan, retain).await?;
                Ok::<_, MirrorError>((plan, step))
            }
        }))
        .buffered(spec.concurrency.max_downloads)
        .try_collect()
        .await?
    };

    let mut uploaded = 0usize;
    let mut already_present = 0usize;

    for (plan, step) in fetched {
        let report_entry = match step {
            RowStep::Done(RowOutcome { archive, upload }) => {
                match upload {
                    Some(UploadOutcome::Uploaded) => uploaded += 1,
                    Some(UploadOutcome::Skipped) => already_present += 1,
                    None => {}
                }
                ArchiveReport {
                    name: plan.name,
                    outcome: archive,
                    detail: None,
                }
            }
            RowStep::Failed(detail) => ArchiveReport {
                name: plan.name,
                outcome: ArchiveOutcome::Failed,
                detail: Some(detail),
            },
        };
        reports[plan.index] = Some(report_entry);
    }
    report.archives = reports.into_iter().flatten().collect();
    report.counters = RunCounters::from_archives(&report.archives, uploaded, already_present);

    // Two reasons to stop here, both ending in "publish nothing".
    //
    // Clobber-safety: a partial run must not leave a manifest promising
    // archives the store does not hold. The ones that did land stay on disk,
    // which makes the corrected re-run cheap.
    //
    // `--dry-run`: the flag promises a report of what *would* be mirrored,
    // so writing the manifest would be the one side effect it rules out.
    if report.has_failures() || dry_run {
        return Ok(report);
    }

    let (digest, snapshot) = publish_manifest(spec, &manifest).await?;
    report.manifest_sha256 = Some(digest);

    if let Some(uploader) = &uploader {
        upload_manifest(uploader, spec, &snapshot, &mut report).await?;
    }

    Ok(report)
}

/// Render the manifest and write the two documents it is served as.
///
/// Returns the manifest's sha256 and the snapshot's relative path, which the
/// upload pass needs and the report carries. The digest is still returned and
/// still reported — it names the snapshot, and it is what an operator pins.
///
/// # Errors
///
/// [`MirrorError::IndexWriteError`] for a render failure or any failed write
/// into `output:`.
async fn publish_manifest(spec: &DistSpec, manifest: &DistManifest) -> Result<(String, String), MirrorError> {
    let rendered = manifest
        .render()
        .map_err(|error| MirrorError::IndexWriteError(format!("cannot render {MANIFEST_NAME}: {error}")))?;
    let digest = hex::encode(Sha256::digest(rendered.as_bytes()));
    let snapshot = format!("{SNAPSHOT_DIR}/{digest}.json");

    write_output(&spec.output.join(&snapshot), rendered.as_bytes()).await?;
    write_output(&spec.output.join(MANIFEST_NAME), rendered.as_bytes()).await?;

    Ok((digest, snapshot))
}

/// PUT the two manifest documents, in publish order.
///
/// Every archive is already in the store by the time this runs — each row
/// uploaded its own in [`mirror_row`] — so this is only the tail: the
/// content-addressed snapshot, then the rolling manifest **last**. A consumer
/// reading mid-run therefore resolves either the old manifest or the new one,
/// and both are fully backed by bytes already in the store.
///
/// Strictly sequential, and deliberately so: this ordering *is* the invariant,
/// and there are two files, so there is nothing to gain by racing them.
///
/// # Errors
///
/// [`MirrorError::ExecutionFailed`] on a rejected upload.
async fn upload_manifest(
    uploader: &Uploader,
    spec: &DistSpec,
    snapshot: &str,
    report: &mut DistSyncReport,
) -> Result<(), MirrorError> {
    for (relative, precheck) in [
        // Content-addressed: a re-run whose manifest did not change writes the
        // same name, and the store already holds it.
        (snapshot, Precheck::HeadFirst),
        // Rolling: "already there" says nothing about which version is there.
        (MANIFEST_NAME, Precheck::Unconditional),
    ] {
        tracing::info!("PUT {relative}");
        report
            .counters
            .record(put(uploader, relative, &spec.output.join(relative), precheck).await?);
    }
    Ok(())
}

/// One release row that survived the serial pre-pass and is ready to fetch.
///
/// Carries its own `index` and `name` so [`mirror_row`]'s results can be folded
/// back into manifest order and reported without borrowing the row again — the
/// row's `url` has already been rewritten by then, and the fetch pass runs
/// several of these at once.
struct PlannedRow {
    /// Position in `manifest.releases`.
    index: usize,
    /// Human-readable label for the report and the log line.
    name: String,
    /// Rendered path below `output:` and below `publish.base_url`.
    relative: String,
    /// Upstream's assertion about the bytes, as an OCI digest string, for
    /// [`verify_digest`].
    digest: String,
    /// The same assertion as bare lowercase hex, for comparison against what a
    /// destination reports about an object it already holds.
    sha256_hex: String,
    /// Where the archive is fetched from.
    source: Url,
}

/// Decide where one archive lands and re-point its manifest row at the mirror.
///
/// Everything here is pure, cheap, and needs exclusive access to either the row
/// or `claimed` — which is exactly why it is separated from [`fetch_row`]
/// rather than sharing one function with it. `Err` carries the per-row failure
/// message; the caller counts it.
fn plan_row(
    spec: &DistSpec,
    template: &LayoutTemplate,
    row: &mut ReleaseRow,
    index: usize,
    name: String,
    claimed: &mut HashMap<String, String>,
) -> Result<PlannedRow, String> {
    let digest = row.digest()?;
    let source = row.check_url()?;
    let relative = template
        .expand(&RowValues {
            version: &row.version,
            tag: &row.tag,
            target: &row.target,
            filename: &row.filename,
            channel: &row.channel,
        })
        .map_err(|error| error.to_string())?;

    // Refused, not resolved: the second row cannot know whether the first was
    // the one the operator meant, and overwriting leaves no trace in the tree.
    //
    // Every row is visited exactly once, so a repeat claim is always a genuine
    // duplicate — including two rows sharing `(version, target)`, whose
    // digests may differ and whose collision is exactly what this catches.
    if let Some(first) = claimed.insert(relative.clone(), name.clone()) {
        return Err(format!(
            "layout path {relative:?} is already claimed by release {first}; \
             `publish.layout` must render one path per release and target"
        ));
    }

    // The row now names the mirror. Done before the download so a `--dry-run`
    // report and a real run agree about where a byte will live.
    row.url = mirrored_url(&spec.publish.base_url, &relative)?.to_string();

    Ok(PlannedRow {
        index,
        name,
        relative,
        sha256_hex: row.sha256.to_ascii_lowercase(),
        digest,
        source,
    })
}

/// What one row's pass did, both halves.
struct RowOutcome {
    archive: ArchiveOutcome,
    /// `None` when the run has no uploader at all; otherwise what the
    /// destination did, so the counters can be folded in row order.
    upload: Option<UploadOutcome>,
}

/// One row's result, split by which of the two error classes it can be in.
///
/// The same split `registry_sync` makes for the same reason (C-040), and the
/// reason the row pass cannot simply return `Result<_, String>`: a source that
/// will not serve one archive is a fact about that archive, and the operator
/// should see *every* such row in one report rather than fixing them one run at
/// a time. A destination that refuses a write is a fact about the run — usually
/// a credential — and continuing past it would attempt the same rejected write
/// once per remaining archive, which is what trips account-lockout policy on
/// the stores this targets.
enum RowStep {
    Done(RowOutcome),
    /// Counted into the report; the run continues.
    Failed(String),
}

/// Mirror one planned archive: destination probe, download, upload, cleanup.
///
/// Borrows nothing mutable, which is what lets several of these run at once
/// under `concurrency.max_downloads` — and, because the upload happens *here*
/// rather than in a second pass, what lets one row's PUT overlap another's GET.
/// A link is full duplex; the two-phase shape this replaces left one direction
/// idle throughout each phase.
///
/// **The destination is asked before anything is fetched.** An archive the
/// store already holds costs neither transfer. That is worth far more than it
/// looks: a CI runner starts with an empty `output:`, so the previous shape
/// pulled the entire mirror — ~1.9 GB — on every run only to discover it had
/// nothing to upload.
///
/// The probe is also a **stronger** check than the upload-time one it replaces.
/// A bare HEAD proves a path is occupied, not that it is occupied by the right
/// bytes; comparing the store's reported sha256 against the manifest's turns a
/// blind skip into a verified one. A store that reports no checksum degrades to
/// the old existence-only trust rather than to a re-download, because that is
/// exactly the behaviour it had before.
///
/// `Err` carries the per-row failure message; the caller counts it.
async fn mirror_row(
    client: &reqwest::Client,
    spec: &DistSpec,
    uploader: Option<&Uploader>,
    upload_permits: &Semaphore,
    upload_failed: &AtomicBool,
    plan: &PlannedRow,
    retain: bool,
) -> Result<RowStep, MirrorError> {
    let destination = spec.output.join(&plan.relative);

    if let Some(uploader) = uploader {
        match uploader.probe(&plan.relative).await {
            Ok(Probe::Present { sha256 }) => {
                // `None` — a store that reports no checksum — is treated as
                // present, matching what the upload pass already did on its own
                // HEAD. Only a *reported and different* digest falls through to
                // a re-transfer, which is the one case that was silently wrong
                // before: a wrong object at the right path used to be trusted
                // for ever.
                if sha256.as_deref().is_none_or(|reported| reported == plan.sha256_hex) {
                    // Nothing here is needed any more, including a local copy a
                    // previous run staged.
                    discard_staged(&destination, retain).await;
                    return Ok(RowStep::Done(RowOutcome {
                        archive: ArchiveOutcome::Skipped,
                        upload: Some(UploadOutcome::Skipped),
                    }));
                }
                tracing::warn!(
                    "{} holds a different object than the manifest declares; re-uploading",
                    plan.relative
                );
            }
            Ok(Probe::Absent) => {}
            // The destination did not answer. That is a fact about the run, not
            // about this archive — every other probe is about to fail the same
            // way — so it aborts rather than reddening one row.
            Err(error) => {
                return Err(MirrorError::ExecutionFailed(vec![format!(
                    "cannot ask the destination about {}: {error:#}",
                    plan.relative
                )]));
            }
        }
    }

    let archive = match fetch_archive(client, &destination, plan).await {
        Ok(archive) => archive,
        // A source that will not serve this archive reds this row and no other.
        Err(detail) => return Ok(RowStep::Failed(detail)),
    };

    let Some(uploader) = uploader else {
        return Ok(RowStep::Done(RowOutcome { archive, upload: None }));
    };

    // The row pass is sized by `max_downloads`; this permit is what keeps
    // `max_uploads` meaningful now that the PUT lives inside it. Two knobs
    // because they bound two different resources — the source is usually a CDN,
    // the destination one corporate store with a rate limit worth respecting.
    let upload = {
        let _permit = upload_permits.acquire().await.map_err(|_| {
            MirrorError::ExecutionFailed(vec!["upload concurrency semaphore was closed mid-run".to_string()])
        })?;
        // Checked here, holding the permit, so the decision is made as late as
        // it can be: a write rejected while this row queued must not become a
        // second attempt with the same bad credential. That is the account
        // lockout `retry_delays` refuses to cause, and the run is over either
        // way — the rejection below aborts it.
        if upload_failed.load(Ordering::Relaxed) {
            return Ok(RowStep::Failed(format!(
                "upload of {} was not attempted: an earlier upload was rejected",
                plan.relative
            )));
        }
        // `Unconditional`: the probe above already answered the question
        // `Precheck::HeadFirst` would ask, and it answered `Absent`.
        uploader
            .put_file(&plan.relative, &destination, Precheck::Unconditional)
            .await
            .map_err(|error| {
                upload_failed.store(true, Ordering::Relaxed);
                MirrorError::ExecutionFailed(vec![format!("upload of {} failed: {error:#}", plan.relative)])
            })?
    };

    // Only after the store has confirmed it. Losing the local copy of something
    // that did not land would turn a retryable run into a re-download.
    discard_staged(&destination, retain).await;

    Ok(RowStep::Done(RowOutcome {
        archive,
        upload: Some(upload),
    }))
}

/// Reuse a verified local copy, or fetch and verify one.
///
/// Split out so [`mirror_row`] reads as the two-error-class decision it is:
/// every failure in here reds one row, never the run.
async fn fetch_archive(
    client: &reqwest::Client,
    destination: &Path,
    plan: &PlannedRow,
) -> Result<ArchiveOutcome, String> {
    if tokio::fs::try_exists(destination).await.unwrap_or(false)
        && verify_digest(destination, &plan.digest).await.is_ok()
    {
        return Ok(ArchiveOutcome::Skipped);
    }

    if let Some(parent) = destination.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }

    download(client, &plan.source, destination)
        .await
        .map_err(|error| format!("download failed: {error:#}"))?;

    // Upstream's assertion about the bytes, never recomputed from what
    // arrived — re-deriving it here would verify the copy against itself.
    verify_digest(destination, &plan.digest)
        .await
        .map_err(|error| format!("{error:#}"))?;

    Ok(ArchiveOutcome::Copied)
}

/// Remove a staged archive once the store holds it, unless `retain` says keep.
///
/// Best-effort and deliberately silent on failure: the bytes are already at the
/// destination, so a file that will not delete is wasted disk rather than a
/// broken run, and failing the row here would undo an upload that succeeded.
async fn discard_staged(path: &Path, retain: bool) {
    if retain {
        return;
    }
    match tokio::fs::remove_file(path).await {
        Ok(()) => tracing::debug!("discarded staged {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::debug!("could not discard staged {}: {error}", path.display()),
    }
}

/// `base_url` with `relative` appended as path segments, keeping any path the
/// base carries and tolerating a trailing slash on either side.
///
/// **The single composition.** Two consumers need this URL — the `url` stamped
/// into every published manifest row, and the uploader's PUT target — and they
/// must name the same object or the store holds bytes nothing points at. They
/// were two functions once, and the two disagreed twice: on a query string
/// (which `spec::DistSpec::validate_publish_base` now refuses outright, since
/// an Azure SAS there is a credential either way) and on percent-encoding,
/// which no validation can rule out because `filename` is foreign data.
/// Composing through `Url` means every reserved character is escaped once, in
/// one place, and the two cannot drift again.
///
/// # Errors
///
/// A base that cannot be a base (`mailto:`), which spec validation has already
/// refused by requiring an http(s) scheme.
fn mirrored_url(base: &url::Url, relative: &str) -> Result<url::Url, String> {
    let mut composed = base.clone();
    {
        let mut segments = composed
            .path_segments_mut()
            .map_err(|()| "publish.base_url cannot be a base URL".to_string())?;
        // Drop the empty segment a trailing slash leaves behind, so
        // `https://host/ocx/` and `https://host/ocx` compose identically.
        segments.pop_if_empty();
        for part in relative.split('/').filter(|part| !part.is_empty()) {
            segments.push(part);
        }
    }
    Ok(composed)
}

/// PUT one file and fold the outcome into the counters.
///
/// # Errors
///
/// [`MirrorError::ExecutionFailed`] naming the file, on any rejected upload.
async fn put(
    uploader: &Uploader,
    relative: &str,
    file: &Path,
    precheck: Precheck,
) -> Result<UploadOutcome, MirrorError> {
    // An upload failure aborts the run rather than being counted: the manifest
    // is the file after this one, and publishing it past a failed write would
    // promise bytes the store does not hold.
    uploader
        .put_file(relative, file, precheck)
        .await
        .map_err(|error| MirrorError::ExecutionFailed(vec![format!("upload of {relative} failed: {error:#}")]))
}

/// Write a file under `output:`, creating parents.
///
/// # Errors
///
/// [`MirrorError::IndexWriteError`] when the parent cannot be created or the
/// write fails.
async fn write_output(path: &Path, bytes: &[u8]) -> Result<(), MirrorError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| MirrorError::IndexWriteError(format!("cannot create {}: {error}", parent.display())))?;
    }
    tokio::fs::write(path, bytes)
        .await
        .map_err(|error| MirrorError::IndexWriteError(format!("cannot write {}: {error}", path.display())))
}

/// Fetch and parse the upstream manifest.
///
/// # Errors
///
/// [`MirrorError::SourceError`] when the manifest cannot be fetched, read, or
/// parsed, and [`MirrorError::DistSchemaUnsupported`] for a `schema` this
/// binary cannot re-emit.
async fn fetch_manifest(client: &reqwest::Client, spec: &DistSpec) -> Result<DistManifest, MirrorError> {
    // The source half of an authenticated store. `upload.identity` covers
    // writes; a store that also gates reads used to be unreachable, which
    // `docs/reference/dist-yml.md` recorded as a follow-up. Host-keyed like
    // every other read leg, so the same variables serve the manifest fetch and
    // the archive downloads `mirror_row` performs through `download`.
    let mut request = client.get(spec.source.clone());
    if let Some(credential) = crate::auth::resolve(&spec.source)? {
        request = credential.apply(request);
    }
    let mut response = request
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        // `{:#}` over an `anyhow` wrapper, not reqwest's own `Display`: that
        // one prints "error sending request for url (...)" and stops, leaving
        // the cause — an untrusted proxy CA, a refused connect, a DNS failure —
        // reachable only through `Error::source`. On a restricted network that
        // is the entire content of the message.
        .map_err(|error| {
            MirrorError::SourceError(format!("cannot fetch {}: {:#}", spec.source, anyhow::Error::new(error)))
        })?;

    // Refuse a declared oversize body before reading a byte of it.
    if let Some(declared) = response.content_length()
        && declared > MANIFEST_FETCH_CEILING as u64
    {
        return Err(MirrorError::SourceError(format!(
            "{} declares {declared} bytes, over the {MANIFEST_FETCH_CEILING}-byte cap",
            spec.source
        )));
    }

    // An endpoint that omits or lies about `Content-Length` — chunked transfer,
    // or a hostile host — still cannot stream more than the cap into memory,
    // because the running total is checked before each chunk is appended.
    let mut bytes: Vec<u8> = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| MirrorError::SourceError(format!("cannot read {}: {error}", spec.source)))?
    {
        if bytes.len() + chunk.len() > MANIFEST_FETCH_CEILING {
            return Err(MirrorError::SourceError(format!(
                "{} exceeds the {MANIFEST_FETCH_CEILING}-byte cap",
                spec.source
            )));
        }
        bytes.extend_from_slice(&chunk);
    }

    let manifest: DistManifest = serde_json::from_slice(&bytes)
        .map_err(|error| MirrorError::SourceError(format!("cannot parse {}: {error}", spec.source)))?;

    if manifest.schema != SUPPORTED_SCHEMA {
        return Err(MirrorError::DistSchemaUnsupported(manifest.schema));
    }
    Ok(manifest)
}

/// The HTTP client for both the manifest fetch and the archive downloads.
///
/// Redirects are followed, unlike the source-index client in `registry sync`:
/// GitHub Releases answers every asset URL with a redirect to its object host,
/// so refusing them would break the default `source:`. The manifest's inline
/// sha256 is the control here — wherever the bytes came from, they must hash
/// to what upstream published.
///
/// # Errors
///
/// [`MirrorError::ExecutionFailed`] when the TLS backend cannot be built.
fn build_client() -> Result<reqwest::Client, MirrorError> {
    crate::http::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|error| MirrorError::ExecutionFailed(vec![format!("cannot build an HTTP client: {error}")]))
}

/// The HTTP client for the upload pass — a separate client because it must
/// **not** follow redirects.
///
/// `reqwest` strips `Authorization`, `Cookie` and `Proxy-Authorization` when a
/// redirect crosses origins, but it cannot know that `upload.headers` carries
/// a credential too: `JOB-TOKEN` (GitLab) and `X-JFrog-Art-Api` (Artifactory)
/// are ordinary headers to it, and would be replayed verbatim to whatever host
/// a `Location` names. The threat model puts a redirect to an attacker host
/// squarely in scope, and a store legitimately redirecting a `PUT` is not a
/// deployment this supports — so the redirect is refused rather than followed.
///
/// The download client keeps the default policy on purpose: GitHub Releases
/// answers every asset URL with a redirect to its object host, no credential is
/// ever attached to those requests, and the manifest digest is what makes the
/// bytes trustworthy wherever they came from.
///
/// # Errors
///
/// [`MirrorError::ExecutionFailed`] when the TLS backend cannot be built.
fn build_upload_client() -> Result<reqwest::Client, MirrorError> {
    crate::http::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|error| MirrorError::ExecutionFailed(vec![format!("cannot build an HTTP client: {error}")]))
}

#[cfg(test)]
#[path = "dist_sync/tests.rs"]
mod tests;
