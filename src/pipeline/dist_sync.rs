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
//! **Publish order.** Archives first, then the content-addressed snapshot,
//! then the sidecar, and the rolling `dist.json` **last**. A consumer reading
//! mid-run therefore resolves either the old manifest or the new one, and both
//! are fully backed by bytes already in the store.

pub mod layout;
pub mod manifest;
pub mod report;
pub mod upload;

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use sha2::{Digest as _, Sha256};

use self::layout::{LayoutTemplate, RowValues};
use self::manifest::{DistManifest, ReleaseRow, SUPPORTED_SCHEMA};
use self::report::{ArchiveOutcome, ArchiveReport, DistSyncReport, RunCounters};
use self::upload::{UploadOutcome, Uploader};
use crate::error::MirrorError;
use crate::pipeline::download::download;
use crate::pipeline::verify::verify_digest;
use crate::spec::DistSpec;

/// Fixed name of the rolling manifest, relative to `output:` and to
/// `publish.base_url`.
const MANIFEST_NAME: &str = "dist.json";
/// Fixed name of the sidecar carrying the rolling manifest's sha256.
const MANIFEST_SIDECAR: &str = "dist.json.sha256";
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

    // What the upload pass republishes, in publish order: the rendered path
    // and the digest header that goes with it.
    //
    // Carried as a pair rather than as a `Vec` zipped against `releases` later:
    // a failed row pushes nothing, so a parallel vector is only aligned while
    // the clobber-safety return below is exactly where it is. `zip` truncates
    // silently rather than failing, so that arrangement would upload an archive
    // under a neighbouring row's checksum the day the guard moved.
    let mut published: Vec<(String, String)> = Vec::with_capacity(manifest.releases.len());

    // Two rows rendering to one path would have the second silently overwrite
    // the first, and the manifest would then name one file for two targets —
    // a wrong-binary install with nothing in the output to show for it.
    let mut claimed: HashMap<String, String> = HashMap::with_capacity(manifest.releases.len());

    for row in &mut manifest.releases {
        let name = row.label();

        match mirror_row(&client, spec, &template, row, dry_run, &mut claimed).await {
            Ok((relative, outcome)) => {
                published.push((relative, row.sha256.to_ascii_lowercase()));
                report.archives.push(ArchiveReport {
                    name,
                    outcome,
                    detail: None,
                });
            }
            Err(detail) => report.archives.push(ArchiveReport {
                name,
                outcome: ArchiveOutcome::Failed,
                detail: Some(detail),
            }),
        }
    }
    report.counters = RunCounters::from_archives(&report.archives, 0, 0);

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
        upload_tree(uploader, spec, &published, &snapshot, &mut report).await?;
    }

    Ok(report)
}

/// Render the manifest and write the three documents it is served as.
///
/// Returns the manifest's sha256 and the snapshot's relative path, which the
/// upload pass needs and the report carries.
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
    write_output(
        &spec.output.join(MANIFEST_SIDECAR),
        format!("{digest}  {MANIFEST_NAME}\n").as_bytes(),
    )
    .await?;
    write_output(&spec.output.join(MANIFEST_NAME), rendered.as_bytes()).await?;

    Ok((digest, snapshot))
}

/// PUT the emitted tree in publish order.
///
/// Archives first, then the content-addressed snapshot, then the sidecar, then
/// the rolling manifest — so every byte a manifest names is in the store
/// before any manifest naming it is, and a consumer reading mid-run resolves
/// either the old manifest or the new one.
///
/// # Errors
///
/// [`MirrorError::ExecutionFailed`] on the first rejected upload; the
/// remaining files include the manifest, and continuing would publish one
/// naming an archive that never landed.
async fn upload_tree(
    uploader: &Uploader,
    spec: &DistSpec,
    published: &[(String, String)],
    snapshot: &str,
    report: &mut DistSyncReport,
) -> Result<(), MirrorError> {
    for (relative, sha256) in published {
        put(uploader, relative, &spec.output.join(relative), Some(sha256), report).await?;
    }
    for relative in [snapshot, MANIFEST_SIDECAR, MANIFEST_NAME] {
        put(uploader, relative, &spec.output.join(relative), None, report).await?;
    }
    Ok(())
}

/// Place one archive under `output:` and re-point its manifest row at the
/// mirror.
///
/// Returns the rendered relative path alongside the outcome. `Err` carries the
/// per-row failure message; the caller counts it.
async fn mirror_row(
    client: &reqwest::Client,
    spec: &DistSpec,
    template: &LayoutTemplate,
    row: &mut ReleaseRow,
    dry_run: bool,
    claimed: &mut HashMap<String, String>,
) -> Result<(String, ArchiveOutcome), String> {
    let name = row.label();
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
    if let Some(first) = claimed.insert(relative.clone(), name) {
        return Err(format!(
            "layout path {relative:?} is already claimed by release {first}; \
             `publish.layout` must render one path per release and target"
        ));
    }

    // The row now names the mirror. Done before the download so a `--dry-run`
    // report and a real run agree about where a byte will live.
    row.url = mirrored_url(&spec.publish.base_url, &relative)?.to_string();

    if dry_run {
        return Ok((relative, ArchiveOutcome::Planned));
    }

    let destination = spec.output.join(&relative);
    if tokio::fs::try_exists(&destination).await.unwrap_or(false) && verify_digest(&destination, &digest).await.is_ok()
    {
        return Ok((relative, ArchiveOutcome::Skipped));
    }

    if let Some(parent) = destination.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }

    download(client, &source, &destination)
        .await
        .map_err(|error| format!("download failed: {error:#}"))?;

    // Upstream's assertion about the bytes, never recomputed from what
    // arrived — re-deriving it here would verify the copy against itself.
    verify_digest(&destination, &digest)
        .await
        .map_err(|error| format!("{error:#}"))?;

    Ok((relative, ArchiveOutcome::Copied))
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
    sha256: Option<&str>,
    report: &mut DistSyncReport,
) -> Result<(), MirrorError> {
    match uploader.put_file(relative, file, sha256).await {
        Ok(UploadOutcome::Uploaded) => {
            report.counters.uploaded += 1;
            Ok(())
        }
        Ok(UploadOutcome::Skipped) => {
            report.counters.already_present += 1;
            Ok(())
        }
        // An upload failure aborts the run rather than being counted: the
        // remaining files include the manifest, and continuing past a failed
        // archive would publish a manifest naming it.
        Err(error) => Err(MirrorError::ExecutionFailed(vec![format!(
            "upload of {relative} failed: {error:#}"
        )])),
    }
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
    let mut response = client
        .get(spec.source.clone())
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|error| MirrorError::SourceError(format!("cannot fetch {}: {error}", spec.source)))?;

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
    reqwest::Client::builder()
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
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|error| MirrorError::ExecutionFailed(vec![format!("cannot build an HTTP client: {error}")]))
}

#[cfg(test)]
#[path = "dist_sync/tests.rs"]
mod tests;
