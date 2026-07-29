// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use ocx_lib::cli::progress::{ProgressManager, Spinner};
use ocx_lib::log;
use ocx_lib::oci::Platform;
use ocx_lib::package::bin_scan;
use ocx_lib::package::metadata::Metadata;
use ocx_lib::package::metadata::authoring::AuthoringMetadata;
use ocx_lib::package::version::Version;
use ocx_lib::publisher::Publisher;
use serde::Serialize;
use tokio::sync::Semaphore;

use super::download;
use super::mirror_result::MirrorResult;
use super::mirror_task::MirrorTask;
use super::package;
use super::progress;
use super::push;
use super::verify;
use crate::error::MirrorError;
use crate::spec::{BinScanMode, MetadataConfig, MirrorSpec};
use crate::version_platform_map::VersionPlatformMap;

/// A task that completed the prepare phase (download + verify + bundle).
struct PreparedTask {
    task: MirrorTask,
    task_dir: PathBuf,
    bundle_path: PathBuf,
    metadata: Metadata,
}

/// Outcome of the prepare phase for a single task.
enum PrepareOutcome {
    Ready(Box<PreparedTask>),
    Failed(MirrorResult),
}

/// Concurrency parameters for the mirror pipeline.
pub struct ConcurrencyParams {
    pub max_downloads: usize,
    pub max_bundles: usize,
    pub compression_threads: u32,
}

/// Per-bundle entry in a version manifest.
#[derive(Debug, Clone, Serialize)]
pub struct BundleEntry {
    /// Platform slug (e.g. `linux_amd64`).
    pub platform_slug: String,
    /// Absolute path to `bundle.tar.xz`.
    pub bundle_path: PathBuf,
    /// File size in bytes.
    pub size_bytes: u64,
    /// SHA-256 hex digest of the bundle file.
    pub sha256: String,
}

/// Output of `prepare_version`: per-version manifest listing all prepared bundles.
///
/// Written to `{work_dir}/{version}/manifest.json` on success.
#[derive(Debug, Clone, Serialize)]
pub struct VersionManifest {
    pub version: String,
    pub bundles: Vec<BundleEntry>,
}

/// The metadata a fresh publish would record for one `(version, platform)`.
///
/// Both forms come from the same resolved spec file, and both are needed
/// because they land in different places: [`published`](Self::published) is the
/// projection that becomes the OCI config blob, while
/// [`sidecar_json`](Self::sidecar_json) is the platform-stamped file that
/// `ocx package push --metadata` reads.
#[derive(Clone)]
pub(crate) struct ExpectedMetadata {
    /// The authoring form both projections below are rendered from.
    ///
    /// Retained rather than discarded after rendering because one field —
    /// `binaries` under a `bin_scan` — can only be supplied from outside the
    /// spec, and re-rendering both projections around it is the only way to
    /// keep them agreeing with each other. See
    /// [`adopting_binaries_from`](Self::adopting_binaries_from).
    authoring: AuthoringMetadata,
    /// Published projection — byte-for-byte the config blob a push writes.
    pub published: Metadata,
    /// `-metadata.json` sidecar, with the platform recorded.
    pub sidecar_json: String,
}

impl ExpectedMetadata {
    /// Renders both projections from one authoring document.
    ///
    /// The sidecar goes through [`package::sidecar_json`] rather than a plain
    /// serialization: it is what stamps the `platform` field, and `ocx package
    /// push --metadata` exits 65 on a sidecar that lacks one.
    pub(crate) fn render(authoring: AuthoringMetadata, platform: &Platform) -> Result<Self> {
        let sidecar_json = package::sidecar_json(&authoring, platform)?;
        // The published projection is also where a dependency declared only as
        // a `platforms` pin map collapses to this platform's pin.
        let published = authoring.to_published(platform)?;
        Ok(Self {
            authoring,
            published,
            sidecar_json,
        })
    }

    /// The same expectation, carrying `published`'s `binaries` claim.
    ///
    /// Under `bin_scan` the claim is derived from the extracted content tree,
    /// which no download-free path has — so the expectation this module can
    /// compute always says "no binaries declared". Left alone that reads as
    /// drift on every scanned mirror forever, and `pipeline patch` acting on it
    /// would republish the claim *away*, silently deleting a correct published
    /// `binaries` list. Adopting what the registry already records makes the
    /// comparison see the one field it cannot recompute as unchanged, while
    /// every other field still drifts normally.
    pub(crate) fn adopting_binaries_from(&self, published: &Metadata, platform: &Platform) -> Result<Self> {
        match published.binaries() {
            Some(binaries) => Self::render(self.authoring.clone().with_binaries(binaries.clone()), platform),
            None => Ok(self.clone()),
        }
    }
}

/// Computes the metadata a fresh publish of `platform` would produce today.
///
/// Download-free: reads the spec's metadata files and nothing else. `pipeline
/// plan` compares this against what the registry currently records to detect
/// drift, and `pipeline patch` re-publishes against it — so a spec fix reaches
/// already-published versions without anyone deleting a tag.
///
/// Under a `bin_scan` the result is therefore incomplete by construction, and
/// callers must run it through
/// [`ExpectedMetadata::adopting_binaries_from`] before comparing.
pub(crate) fn expected_metadata(
    config: &MetadataConfig,
    platform: &Platform,
    spec_dir: &Path,
) -> Result<ExpectedMetadata> {
    ExpectedMetadata::render(
        package::resolve_metadata(config, &platform.to_string(), spec_dir)?,
        platform,
    )
}

/// How a variant produces its published metadata: the spec files it reads, and
/// whether a `bin_scan` derives the `binaries` claim from the content tree.
#[derive(Debug, Clone)]
pub(crate) struct MetadataPlan {
    pub config: MetadataConfig,
    pub bin_scan: BinScanMode,
}

/// How `version`'s variant produces its published metadata.
///
/// A variant-prefixed tag (`slim-3.13.9`) is published from its own variant's
/// `metadata:` and `bin_scan:` where it has them, falling back to the
/// spec-level values — the same resolution [`MirrorSpec::effective_variants`]
/// performs for a sync run. Returns `None` when the tag names a variant the
/// spec no longer declares, or when neither the variant nor the spec declares
/// metadata.
pub(crate) fn metadata_plan_for(spec: &MirrorSpec, version: &Version) -> Option<MetadataPlan> {
    let variant = spec
        .effective_variants()
        .into_iter()
        .find(|variant| variant.name.as_deref() == version.variant())?;
    Some(MetadataPlan {
        config: variant.metadata?,
        bin_scan: variant.bin_scan,
    })
}

/// Prepare all platforms for a single version: download, verify, and bundle.
///
/// Runs platform tasks concurrently with `max_downloads` and `max_bundles`
/// semaphore slots. On success, writes `{work_dir}/{version}/manifest.json`
/// and returns the populated manifest.
///
/// Call sites:
/// - `execute_mirror` — drives the existing sync pipeline
/// - `command::package::pipeline::prepare` — standalone `ocx-mirror package pipeline prepare` subcommand
pub(crate) async fn prepare_version(
    version: &str,
    tasks: &[MirrorTask],
    work_dir: &Path,
    http_client: &reqwest::Client,
    concurrency: &ConcurrencyParams,
) -> Result<VersionManifest, MirrorError> {
    let download_sem = Arc::new(Semaphore::new(concurrency.max_downloads));
    let bundle_sem = Arc::new(Semaphore::new(concurrency.max_bundles));
    let compression_threads = concurrency.compression_threads;
    let progress = ProgressManager::hidden();

    let mut join_set = tokio::task::JoinSet::<(usize, Result<(PathBuf, Metadata)>)>::new();

    for (i, task) in tasks.iter().enumerate() {
        let task = task.clone();
        let task_dir = task_dir(work_dir, &task.normalized_version, &task.platform);
        let dl_sem = download_sem.clone();
        let bd_sem = bundle_sem.clone();
        let client = http_client.clone();
        let progress = progress.clone();

        join_set.spawn(async move {
            let spinner = progress.spinner(format!("{} {}", task.normalized_version, task.platform));
            let result = spinner
                .scope(prepare_task(
                    &task,
                    &task_dir,
                    &client,
                    &spinner,
                    &dl_sem,
                    &bd_sem,
                    compression_threads,
                ))
                .await;
            (i, result)
        });
    }

    // Collect in completion order, then sort by index for deterministic output.
    let mut outcomes: Vec<(usize, Result<(PathBuf, Metadata)>)> = Vec::with_capacity(tasks.len());
    while let Some(join_result) = join_set.join_next().await {
        match join_result {
            Ok(outcome) => outcomes.push(outcome),
            Err(e) => {
                return Err(MirrorError::ExecutionFailed(vec![format!(
                    "prepare task panicked: {e}"
                )]));
            }
        }
    }
    outcomes.sort_by_key(|(i, _)| *i);

    // Convert outcomes to bundle entries; propagate the first failure.
    let mut bundles = Vec::with_capacity(tasks.len());
    for (i, result) in outcomes {
        let (bundle_path, _metadata) = result.map_err(|e| {
            MirrorError::ExecutionFailed(vec![format!("prepare failed for {}: {e:#}", tasks[i].platform)])
        })?;

        let size_bytes = tokio::fs::metadata(&bundle_path).await.map(|m| m.len()).unwrap_or(0);

        let sha256 = compute_sha256(&bundle_path).await?;
        let platform_slug = tasks[i].platform.ascii_segments().join("_");

        bundles.push(BundleEntry {
            platform_slug,
            bundle_path,
            size_bytes,
            sha256,
        });
    }

    let manifest = VersionManifest {
        version: version.to_owned(),
        bundles,
    };

    // Write manifest.json to {work_dir}/{version}/
    let version_dir = work_dir.join(version);
    tokio::fs::create_dir_all(&version_dir)
        .await
        .map_err(|e| MirrorError::ExecutionFailed(vec![format!("failed to create version dir: {e}")]))?;

    let manifest_path = version_dir.join("manifest.json");
    let json = serde_json::to_string_pretty(&manifest)
        .map_err(|e| MirrorError::ExecutionFailed(vec![format!("failed to serialize manifest: {e}")]))?;
    tokio::fs::write(&manifest_path, json)
        .await
        .map_err(|e| MirrorError::ExecutionFailed(vec![format!("failed to write manifest.json: {e}")]))?;

    log::debug!("Wrote manifest to {}", manifest_path.display());
    Ok(manifest)
}

/// Compute the SHA-256 hex digest of a file.
async fn compute_sha256(path: &Path) -> Result<String, MirrorError> {
    use sha2::{Digest, Sha256};

    let data = tokio::fs::read(path).await.map_err(|e| {
        MirrorError::ExecutionFailed(vec![format!(
            "failed to read bundle for sha256 {}: {e}",
            path.display()
        )])
    })?;

    let mut hasher = Sha256::new();
    hasher.update(&data);
    let hash = hasher.finalize();
    Ok(hex::encode(hash))
}

/// Execute all mirror tasks with concurrent preparation and sequential pushing.
///
/// All artifacts (downloads, bundles) live under `work_dir/{version}/{platform}/`.
/// On successful push the task directory is removed. On failure it persists so the
/// next run can resume from whatever stage completed.
///
/// **Phases:**
/// 1. *Prepare* (concurrent) — Download and bundle all tasks in parallel.
///    Downloads are gated by `concurrency.max_downloads`, bundling by
///    `concurrency.max_bundles`. The two semaphores are independent so slow
///    downloads don't block idle CPU cores and vice versa.
/// 2. *Push* (sequential) — Push tasks in version order (oldest first) for correct
///    cascade tag ordering. Each successful `(version, platform)` push is immediately
///    registered in the version map so subsequent cascade computations see it.
// Pipeline entrypoint: orthogonal services + policy (tasks, registry
// client, HTTP client, work dir, version map, progress, fail-fast,
// concurrency). A params struct would relocate the list without
// improving clarity, so the lint is allowed here.
#[allow(clippy::too_many_arguments)]
pub async fn execute_mirror(
    tasks: Vec<MirrorTask>,
    publisher: &Publisher,
    http_client: &reqwest::Client,
    work_dir: &Path,
    mut version_map: VersionPlatformMap,
    progress: &ProgressManager,
    fail_fast: bool,
    concurrency: ConcurrencyParams,
    annotations: &std::collections::BTreeMap<String, String>,
) -> Vec<MirrorResult> {
    // Group tasks by version
    let mut by_version: HashMap<String, Vec<MirrorTask>> = HashMap::new();
    for task in tasks {
        by_version
            .entry(task.normalized_version.clone())
            .or_default()
            .push(task);
    }

    // Sort versions oldest first (cascade ordering)
    let mut version_keys: Vec<String> = by_version.keys().cloned().collect();
    version_keys.sort_by(|a, b| {
        let va = Version::parse(a);
        let vb = Version::parse(b);
        match (va, vb) {
            (Some(a), Some(b)) => a.cmp(&b),
            _ => a.cmp(b),
        }
    });

    // Build ordered task list with version boundaries
    let mut entries: Vec<(MirrorTask, PathBuf)> = Vec::new();
    let mut version_ranges: Vec<Range<usize>> = Vec::new();

    for version_key in &version_keys {
        let start = entries.len();
        for task in by_version.remove(version_key).expect("key from version_keys") {
            let task_dir = task_dir(work_dir, &task.normalized_version, &task.platform);
            entries.push((task, task_dir));
        }
        version_ranges.push(start..entries.len());
    }

    let n = entries.len();
    log::debug!(
        "Executing {n} tasks across {} versions (downloads: {}, bundles: {}, compression threads: {})",
        version_keys.len(),
        concurrency.max_downloads,
        concurrency.max_bundles,
        concurrency.compression_threads,
    );

    // Phase 1: Prepare all tasks concurrently (download + verify + bundle)
    // Two independent semaphores: downloads are I/O-bound, bundles are CPU-bound.
    // Spans are created on-demand after acquiring the first semaphore, so only
    // actively-worked-on tasks show progress bars.
    let download_sem = Arc::new(Semaphore::new(concurrency.max_downloads));
    let bundle_sem = Arc::new(Semaphore::new(concurrency.max_bundles));
    let compression_threads = concurrency.compression_threads;
    let mut join_set = tokio::task::JoinSet::<(usize, PrepareOutcome)>::new();

    for (i, (task, task_dir)) in entries.into_iter().enumerate() {
        let dl_sem = download_sem.clone();
        let bd_sem = bundle_sem.clone();
        let client = http_client.clone();
        let progress = progress.clone();

        join_set.spawn(async move {
            let spinner = progress.spinner(format!("{} {}", task.normalized_version, task.platform));

            match spinner
                .scope(prepare_task(
                    &task,
                    &task_dir,
                    &client,
                    &spinner,
                    &dl_sem,
                    &bd_sem,
                    compression_threads,
                ))
                .await
            {
                Ok((bundle_path, metadata)) => (
                    i,
                    PrepareOutcome::Ready(Box::new(PreparedTask {
                        task,
                        task_dir,
                        bundle_path,
                        metadata,
                    })),
                ),
                Err(e) => (
                    i,
                    PrepareOutcome::Failed(MirrorResult::Failed {
                        version: task.normalized_version.clone(),
                        platform: task.platform.clone(),
                        error: format!("{e:#}"),
                    }),
                ),
            }
        });
    }

    // Collect prepare results into index-ordered slots
    let mut prepared: Vec<Option<PrepareOutcome>> = (0..n).map(|_| None).collect();
    while let Some(join_result) = join_set.join_next().await {
        match join_result {
            Ok((idx, outcome)) => {
                prepared[idx] = Some(outcome);
            }
            Err(e) => {
                log::error!("Task panicked: {e}");
            }
        }
    }

    // Phase 2: Push sequentially by version (oldest first).
    // Each successful (version, platform) push is immediately registered in the
    // version map so subsequent cascade computations see it as existing.
    let mut results = Vec::new();
    let mut abort = false;

    for (range_idx, range) in version_ranges.iter().enumerate() {
        if abort {
            break;
        }

        for idx in range.clone() {
            let Some(outcome) = prepared[idx].take() else {
                continue;
            };

            match outcome {
                PrepareOutcome::Ready(prep) => {
                    let spinner = progress.spinner(format!("{} {}", prep.task.normalized_version, prep.task.platform));
                    progress::set_stage(&spinner, "Pushing", &prep.task.normalized_version, &prep.task.platform);

                    let cascade_versions = version_map.versions_for_cascade();
                    let push_result = spinner
                        .scope(push_task(
                            &prep.task,
                            &prep.bundle_path,
                            &prep.metadata,
                            publisher,
                            &cascade_versions,
                            annotations,
                        ))
                        .await;

                    match push_result {
                        Ok(result) => {
                            if matches!(&result, MirrorResult::Pushed { .. }) {
                                // Register this (version, platform) immediately so
                                // the next platform's cascade sees it.
                                if let Some(v) = Version::parse(&version_keys[range_idx]) {
                                    // Register bare alias for default variants so subsequent
                                    // bare cascades in this run see correct blockers.
                                    if prep.task.variant.as_ref().is_some_and(|ctx| ctx.is_default)
                                        && v.variant().is_some()
                                    {
                                        version_map.add(v.without_variant(), prep.task.platform.clone());
                                    }
                                    version_map.add(v, prep.task.platform.clone());
                                }
                                clean_task_dir(&prep.task_dir).await;
                            }
                            results.push(result);
                        }
                        Err(e) => {
                            results.push(MirrorResult::Failed {
                                version: prep.task.normalized_version.clone(),
                                platform: prep.task.platform.clone(),
                                error: format!("{e:#}"),
                            });
                            if fail_fast {
                                abort = true;
                                break;
                            }
                        }
                    }
                }
                PrepareOutcome::Failed(result) => {
                    results.push(result);
                    if fail_fast {
                        abort = true;
                        break;
                    }
                }
            }
        }
    }

    results
}

/// Build the task directory path: `{work_dir}/{version}/{platform_slug}/`
///
/// The basename is [`crate::spec::platform_slug`] — the same slug the CI
/// renderer stamps into `bundle-{V}-{slug}.tar.xz` and `pipeline push` reads
/// back. Computing it locally is how a libc-bearing platform's bundle became
/// invisible to the leg that was supposed to test it.
pub(crate) fn task_dir(work_dir: &Path, version: &str, platform: &ocx_lib::oci::Platform) -> PathBuf {
    work_dir.join(version).join(crate::spec::platform_slug(platform))
}

/// Phase 1: Download, verify, and bundle a single task.
///
/// Acquires `download_sem` for the download+verify phase, then releases it and
/// acquires `bundle_sem` for the CPU-bound bundling phase. This lets downloads
/// and compression run independently.
///
/// The published metadata is finalised **between** extraction and compression,
/// not before the download: a `bin_scan` reads the extracted tree, so no
/// earlier point in this function can know what `binaries` will say.
pub(crate) async fn prepare_task(
    task: &MirrorTask,
    task_dir: &Path,
    http_client: &reqwest::Client,
    spinner: &Spinner,
    download_sem: &Semaphore,
    bundle_sem: &Semaphore,
    compression_threads: u32,
) -> Result<(PathBuf, Metadata)> {
    tokio::fs::create_dir_all(task_dir).await?;

    let archive_path = task_dir.join(&task.asset_name);
    let content_dir = task_dir.join("content");
    let bundle_path = task_dir.join("bundle.tar.xz");
    // The per-platform metadata the generated CI workflow's `cp` step copies
    // beside the bundle (not the spec-level default metadata.json from the
    // working directory), and that `ocx package push --metadata` then reads.
    let sidecar_path = task_dir.join("metadata.json");

    let Some(config) = &task.metadata_config else {
        anyhow::bail!("no metadata configuration provided in spec");
    };
    let authoring = package::resolve_metadata(config, &task.platform.to_string(), &task.spec_dir)?;

    if bundle_path.exists() {
        // Resume. An earlier run already extracted this bundle's content tree
        // and discarded it, so a `bin_scan` has nothing left to read — the
        // sidecar that run wrote is the record of what it found, and it is
        // written before the bundle exists precisely so this readback cannot
        // miss. Without a scan nothing downstream of the download reaches the
        // metadata, so it is re-resolved from the spec instead and a spec-side
        // fix reaches the resumed run.
        let metadata = if task.bin_scan.scans() {
            resume_scanned_metadata(&sidecar_path, &task.platform).await?
        } else {
            finalize_metadata(&authoring, &task.platform, &sidecar_path).await?
        };
        return Ok((bundle_path, metadata));
    }

    // --- Download phase (I/O-bound) ---
    {
        let _permit = download_sem.acquire().await.expect("semaphore closed");

        // Download
        if !archive_path.exists() {
            progress::set_stage(spinner, "Downloading", &task.normalized_version, &task.platform);
            download::download(http_client, &task.download_url, &archive_path).await?;
        }

        // Verify (only if configured)
        if let Some(verify_config) = &task.verify_config {
            progress::set_stage(spinner, "Verifying", &task.normalized_version, &task.platform);
            verify::verify(
                verify_config,
                http_client,
                &archive_path,
                &task.asset_name,
                &HashMap::new(),
                &task.download_url,
            )
            .await?;
        }
    } // download permit released

    // --- Bundle phase (CPU-bound) ---
    //
    // Extraction, the bin-scan and compression share one permit: the scan reads
    // the tree extraction just wrote and compression consumes the same tree, so
    // releasing in between would only widen the window in which an extracted
    // tree occupies disk without letting any other task make progress.
    let metadata = {
        let _permit = bundle_sem.acquire().await.expect("semaphore closed");

        progress::set_stage(spinner, "Bundling", &task.normalized_version, &task.platform);

        let ap = archive_path.clone();
        let cd = content_dir.clone();
        let asset_type = task.asset_type.clone();
        let an = task.asset_name.clone();
        tokio::task::spawn_blocking(move || {
            tokio::runtime::Handle::current().block_on(async {
                if cd.exists() {
                    let _ = tokio::fs::remove_dir_all(&cd).await;
                }
                tokio::fs::create_dir_all(&cd).await?;
                package::extract(&ap, &cd, &asset_type, &an).await
            })
        })
        .await??;

        // The one metadata input that does not exist before the download. Every
        // other one is resolvable from the spec alone, which is what lets
        // `expected_metadata` stay download-free for `plan` and `patch`.
        let scanned = bin_scan::resolve_binaries(&content_dir, authoring, &task.platform, task.bin_scan.into())
            .await
            .with_context(|| format!("bin_scan failed for {} {}", task.normalized_version, task.platform))?;
        let metadata = finalize_metadata(&scanned, &task.platform, &sidecar_path).await?;

        let cd = content_dir.clone();
        let bp = bundle_path.clone();
        tokio::task::spawn_blocking(move || {
            tokio::runtime::Handle::current().block_on(async {
                package::bundle(&cd, &bp, compression_threads).await?;
                let _ = tokio::fs::remove_dir_all(&cd).await;
                Ok::<_, anyhow::Error>(())
            })
        })
        .await??;

        metadata
    }; // bundle permit released

    Ok((bundle_path, metadata))
}

/// Writes the `-metadata.json` sidecar beside the bundle and returns the
/// published projection the in-process push carries in `Info`.
async fn finalize_metadata(authoring: &AuthoringMetadata, platform: &Platform, sidecar: &Path) -> Result<Metadata> {
    let expected = ExpectedMetadata::render(authoring.clone(), platform)?;
    tokio::fs::write(sidecar, &expected.sidecar_json)
        .await
        .with_context(|| format!("failed to write {}", sidecar.display()))?;
    Ok(expected.published)
}

/// The metadata an earlier run recorded for this task, read back off the
/// sidecar it left behind.
///
/// Only reached on a resume under `bin_scan`, where re-resolving from the spec
/// would silently drop the scanned `binaries` claim and publish a bundle whose
/// metadata contradicts the one already prepared beside it.
async fn resume_scanned_metadata(sidecar: &Path, platform: &Platform) -> Result<Metadata> {
    let json = tokio::fs::read_to_string(sidecar).await.with_context(|| {
        format!(
            "cannot resume a bin_scan bundle without its sidecar {} — remove the bundle to re-scan",
            sidecar.display()
        )
    })?;
    let authoring: AuthoringMetadata =
        serde_json::from_str(&json).with_context(|| format!("failed to parse {}", sidecar.display()))?;
    Ok(authoring.to_published(platform)?)
}

/// Phase 2: Push a prepared bundle to the registry with optional cascade.
async fn push_task(
    task: &MirrorTask,
    bundle_path: &Path,
    metadata: &Metadata,
    publisher: &Publisher,
    cascade_versions: &std::collections::BTreeSet<Version>,
    annotations: &std::collections::BTreeMap<String, String>,
) -> Result<MirrorResult> {
    let identifier = ocx_lib::oci::Identifier::new_registry(&task.target.repository, &task.target.registry)
        .clone_with_tag(&task.normalized_version);

    let info = ocx_lib::package::info::Info {
        identifier,
        metadata: metadata.clone(),
        platform: task.platform.clone(),
    };

    push::push_and_cascade(
        publisher,
        info,
        bundle_path,
        task.cascade,
        cascade_versions,
        task.variant.as_ref(),
        annotations,
    )
    .await
}

/// Remove the task directory after successful push.
async fn clean_task_dir(task_dir: &Path) {
    if let Err(e) = tokio::fs::remove_dir_all(task_dir).await {
        log::debug!("Failed to clean task dir {}: {e}", task_dir.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn platform(spec: &str) -> ocx_lib::oci::Platform {
        spec.parse().expect("valid platform")
    }

    // ── bin_scan: the claim that only exists after extraction ─────────────

    /// A spec directory holding one metadata file that declares an
    /// interface-visible `${installPath}/bin` PATH var — the shape a scan
    /// looks at — plus whatever `binaries` clause the caller wants in it.
    #[cfg(unix)]
    fn spec_dir_declaring(binaries: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("metadata.json"),
            format!(
                r#"{{"type":"bundle","version":1{binaries},
                "env":[{{"key":"PATH","type":"path","value":"${{installPath}}/bin","required":false,"visibility":"interface"}}]}}"#
            ),
        )
        .expect("write metadata fixture");
        dir
    }

    /// A `.tar.xz` holding `bin/tool` with the exec bit set — the upstream
    /// asset a mirror downloads, built here so the test needs no network.
    #[cfg(unix)]
    async fn staged_asset(at: &Path) {
        use std::os::unix::fs::PermissionsExt;

        let content = at.parent().expect("asset has a parent").join("upstream-content");
        std::fs::create_dir_all(content.join("bin")).expect("create fixture tree");
        let tool = content.join("bin").join("tool");
        std::fs::write(&tool, b"#!/bin/sh\n").expect("write fixture tool");
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).expect("chmod fixture tool");
        package::bundle(&content, at, 1).await.expect("build fixture asset");
        std::fs::remove_dir_all(&content).expect("drop fixture tree");
    }

    /// Runs the real prepare phase offline: the asset is staged where the
    /// download would have written it, so `prepare_task` skips the fetch and
    /// exercises extract → scan → sidecar → bundle exactly as in production.
    #[cfg(unix)]
    async fn prepare_scanned(spec_dir: &Path, task_dir: &Path, bin_scan: BinScanMode) -> Result<Metadata> {
        let task = MirrorTask {
            version: "1.0.0".into(),
            normalized_version: "1.0.0".into(),
            platform: platform("linux/amd64"),
            download_url: "https://example.invalid/asset.tar.xz".parse().expect("valid url"),
            asset_name: "asset.tar.xz".into(),
            target: crate::spec::Target {
                registry: "registry.test".into(),
                repository: "mirror/tool".into(),
            },
            metadata_config: Some(MetadataConfig {
                default: "metadata.json".into(),
                platforms: HashMap::new(),
            }),
            bin_scan,
            verify_config: None,
            cascade: false,
            spec_dir: spec_dir.to_path_buf(),
            asset_type: crate::spec::AssetType::Archive { strip_components: None },
            variant: None,
        };

        tokio::fs::create_dir_all(task_dir).await.expect("create task dir");
        let asset = task_dir.join(&task.asset_name);
        if !asset.exists() {
            staged_asset(&asset).await;
        }

        let progress = ProgressManager::hidden();
        let spinner = progress.spinner("test".to_string());
        let (_bundle, metadata) = prepare_task(
            &task,
            task_dir,
            &reqwest::Client::new(),
            &spinner,
            &Semaphore::new(1),
            &Semaphore::new(1),
            1,
        )
        .await?;
        Ok(metadata)
    }

    /// The names on disk, as the sidecar `ocx package push --metadata` reads
    /// records them — the file, not the in-memory value, because that file is
    /// what the CI push job actually publishes from.
    #[cfg(unix)]
    fn sidecar_binaries(task_dir: &Path) -> Vec<String> {
        let json = std::fs::read_to_string(task_dir.join("metadata.json")).expect("sidecar written");
        let sidecar: AuthoringMetadata = serde_json::from_str(&json).expect("sidecar parses");
        sidecar
            .binaries()
            .map(|binaries| binaries.iter().map(|name| name.as_str().to_string()).collect())
            .unwrap_or_default()
    }

    /// The ordering constraint this feature exists to satisfy: `binaries` is
    /// derived from the extracted content tree, so the metadata cannot be
    /// finalised before the download the way every other field is.
    ///
    /// Asserted against the sidecar as well as the published projection: the CI
    /// push job publishes from the file, so a claim that reached only the
    /// in-memory value would never leave the runner.
    #[cfg(unix)]
    #[tokio::test]
    async fn bin_scan_auto_fills_binaries_from_the_extracted_tree() {
        let spec = spec_dir_declaring("");
        let work = tempfile::tempdir().expect("tempdir");

        let off = work.path().join("off");
        let metadata = prepare_scanned(spec.path(), &off, BinScanMode::Off)
            .await
            .expect("prepare succeeds");
        assert!(
            metadata.binaries().is_none(),
            "control: without bin_scan nothing may invent a binaries claim",
        );
        assert!(sidecar_binaries(&off).is_empty(), "control: sidecar too");

        let auto = work.path().join("auto");
        let metadata = prepare_scanned(spec.path(), &auto, BinScanMode::Auto)
            .await
            .expect("prepare succeeds");
        assert_eq!(
            metadata.binaries().map(|binaries| binaries.len()),
            Some(1),
            "the executable under the interface PATH dir must reach the published metadata",
        );
        assert_eq!(sidecar_binaries(&auto), vec!["tool"], "and the sidecar the push reads");
    }

    /// `verify` is the mode a mirror wants once it hand-lists `binaries`: the
    /// list becomes a regression test against upstream rearranging its archive,
    /// and a disagreement fails the run instead of publishing quietly.
    #[cfg(unix)]
    #[tokio::test]
    async fn bin_scan_verify_fails_on_an_undeclared_binary() {
        let spec = spec_dir_declaring(r#","binaries":["other"]"#);
        let work = tempfile::tempdir().expect("tempdir");

        let error = prepare_scanned(spec.path(), &work.path().join("verify"), BinScanMode::Verify)
            .await
            .expect_err("an executable the spec does not declare must fail the run");
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("tool") && rendered.contains("not declared"),
            "the failure must name the undeclared binary: {rendered}",
        );

        // The same tree under `auto` passes a declared list through untouched —
        // otherwise the test above would prove nothing about `verify`.
        let metadata = prepare_scanned(spec.path(), &work.path().join("auto"), BinScanMode::Auto)
            .await
            .expect("auto passes a declared claim through unverified");
        assert_eq!(metadata.binaries().map(|binaries| binaries.len()), Some(1));
    }

    /// A resume arrives after the content tree is gone, so re-resolving the
    /// metadata from the spec would silently drop the scanned claim and publish
    /// a bundle whose metadata contradicts what was prepared beside it.
    ///
    /// The sidecar the first run wrote is the record, and it is written before
    /// the bundle exists precisely so this readback cannot miss.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_resumed_bin_scan_task_keeps_the_scanned_binaries() {
        let spec = spec_dir_declaring("");
        let work = tempfile::tempdir().expect("tempdir");
        let task_dir = work.path().join("resume");

        prepare_scanned(spec.path(), &task_dir, BinScanMode::Auto)
            .await
            .expect("first run succeeds");
        assert!(task_dir.join("bundle.tar.xz").exists(), "first run must leave a bundle");
        assert!(
            !task_dir.join("content").exists(),
            "and must have discarded the tree a re-scan would need",
        );

        let resumed = prepare_scanned(spec.path(), &task_dir, BinScanMode::Auto)
            .await
            .expect("resume succeeds");
        assert_eq!(
            resumed.binaries().map(|binaries| binaries.len()),
            Some(1),
            "a resumed run must republish the scanned claim, not drop it",
        );
        assert_eq!(sidecar_binaries(&task_dir), vec!["tool"]);
    }

    #[test]
    fn task_dir_distinguishes_libc_variants() {
        let work = Path::new("/work");
        let glibc = task_dir(work, "3.12.5", &platform("linux/amd64+libc.glibc"));
        let musl = task_dir(work, "3.12.5", &platform("linux/amd64+libc.musl"));

        // Same os/arch, different libc must not collide in one work directory.
        assert_ne!(glibc, musl);
        assert_eq!(glibc, Path::new("/work/3.12.5/linux_amd64_libc.glibc"));
        assert_eq!(musl, Path::new("/work/3.12.5/linux_amd64_libc.musl"));

        // Bare os/arch (no os_features) keeps its plain slug.
        assert_eq!(
            task_dir(work, "3.12.5", &platform("linux/amd64")),
            Path::new("/work/3.12.5/linux_amd64")
        );
    }
}
