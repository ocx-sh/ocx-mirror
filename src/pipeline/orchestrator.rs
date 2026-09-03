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
use ocx_lib::package::metadata::Metadata;
use ocx_lib::package::metadata::authoring::AuthoringMetadata;
use ocx_lib::package::version::Version;
use ocx_lib::package::{bin_scan, libc_lint};
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
use crate::pipeline::ocx_cli::sign::{ResolvedSign, invoke_sign_reference};
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
/// [`sidecar_json`](Self::sidecar_json) is the file that
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
    /// `-metadata.json` sidecar written beside the bundle.
    pub sidecar_json: String,
}

impl ExpectedMetadata {
    /// Renders both projections from one authoring document.
    ///
    /// The sidecar goes through [`package::sidecar_json`] so both projections
    /// stay rendered from one authoring document.
    pub(crate) fn render(authoring: AuthoringMetadata, platform: &Platform) -> Result<Self> {
        let sidecar_json = package::sidecar_json(&authoring, platform)?;
        let published = authoring.to_published()?;
        Ok(Self {
            authoring,
            published,
            sidecar_json,
        })
    }

    /// The same expectation, carrying `published`'s `binaries` claim — but only
    /// when this expectation could not compute one itself.
    ///
    /// The mode is not the question; whether the *spec declares the field* is.
    /// A metadata file that declares `binaries` computes the whole document
    /// download-free, scanning or not — `verify` checks the declaration against
    /// the tree rather than replacing it, so the published claim is the declared
    /// one either way. Adopting there would rewrite the expectation to whatever
    /// is already published, and a maintainer correcting a wrong hand-written
    /// list would get silence: the field could never drift and the fix never
    /// lands.
    ///
    /// Only when the file declares nothing and a scan filled it is the claim
    /// genuinely uncomputable here. Left unadopted it reads as drift on every
    /// such tile forever, and `pipeline patch` acting on that would republish
    /// the claim *away*, silently deleting a correct published list.
    pub(crate) fn adopting_binaries_from(&self, published: &Metadata, platform: &Platform) -> Result<Self> {
        match published.binaries() {
            Some(binaries) if self.authoring.binaries().is_none() => {
                Self::render(self.authoring.clone().with_binaries(binaries.clone()), platform)
            }
            _ => Ok(self.clone()),
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
///
/// A bare tag that matches no variant by name belongs to the default variant.
/// A *named* default (`name: pgo.lto, default: true`) publishes `3.13.9` beside
/// `pgo.lto-3.13.9`, and the bare alias carries no variant name — so matching on
/// name alone found nothing and drift detection and `patch` skipped every one of
/// those tags silently.
pub(crate) fn metadata_plan_for(spec: &MirrorSpec, version: &Version) -> Option<MetadataPlan> {
    let variants = spec.effective_variants();
    let variant = match variants.iter().find(|v| v.name.as_deref() == version.variant()) {
        Some(variant) => variant,
        None if version.variant().is_none() => variants.iter().find(|v| v.is_default)?,
        None => return None,
    };
    Some(MetadataPlan {
        config: variant.metadata.clone()?,
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
    sign: Option<&ResolvedSign>,
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

        // The reference every push in this range writes into, and the one
        // whose image index is signed once the range is done.
        let mut version_ref: Option<String> = None;
        let mut version_pushed = false;

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
                            sign,
                        ))
                        .await;

                    version_ref.get_or_insert_with(|| {
                        format!(
                            "{}/{}:{}",
                            prep.task.target.registry, prep.task.target.repository, prep.task.normalized_version,
                        )
                    });

                    match push_result {
                        Ok(result) => {
                            if matches!(&result, MirrorResult::Pushed { .. }) {
                                version_pushed = true;
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

        // ── The version's image index (D2, C-059) ────────────────────────────
        //
        // After the range, never inside it: an index is only whole once its
        // last platform has merged in, and signing it per platform would attach
        // one referrer per push to a subject that then changes. This is the
        // in-process leg's counterpart to `pipeline push`'s closing
        // `--tags-file` sweep.
        //
        // A failed index signature is reported as this version's failure rather
        // than raised: the platform manifests are signed and in the registry,
        // and `execute_mirror` returns outcomes rather than aborting.
        if let (Some(resolved), Some(reference), true) = (sign, version_ref.as_deref(), version_pushed)
            && let Err(error) = invoke_sign_reference(resolved, reference, None).await
        {
            results.push(MirrorResult::Failed {
                version: version_keys[range_idx].clone(),
                platform: ocx_lib::oci::Platform::default(),
                error: format!("{error}"),
            });
            if fail_fast {
                abort = true;
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
/// earlier point in this function can know what `binaries` will say. The libc
/// check reads the same tree in the same window, giving the sequence
/// `resolve → download → verify → extract → scan → chmod declared binaries →
/// libc check → sidecar → compress → drop tree`.
///
/// A resume skips every step of that window, because the tree is gone. They are
/// not equally covered afterwards. `bin_scan` re-runs its own guard on the
/// resumed path ([`reject_empty_scan`] below), so an unusable claim still reds.
/// The libc check has no such equivalent: it is reachable only from the bundle
/// block, and the early return above happens first. So:
///
/// - a bundle on disk is **not** evidence the check passed — it may have been
///   written under `libc_lint: false`, or by a binary predating the check;
/// - flipping `libc_lint` back to `true` does not reach a work dir that already
///   holds a bundle. The operator must discard the bundle to re-check it.
///
/// The declared-binaries chmod sits in the same block and is uncovered the same
/// way: a bundle written by a binary predating it keeps its 0644 members, and no
/// resume will fix them — discard the bundle to re-prepare it.
///
/// No warning is emitted for this. A resume with `libc_lint` on — the default,
/// and the overwhelmingly common case — would fire it every time, which is
/// noise, not signal.
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
        // and discarded it, so a `bin_scan` has nothing left to read. Everything
        // the *spec* owns is re-resolved from the spec, so a spec-side fix — an
        // env var, an entrypoint — still reaches a resumed run; only `binaries`,
        // the one field no download-free path can recompute, is adopted from the
        // sidecar that run wrote. Carrying the whole sidecar instead discarded
        // every unrelated metadata fix on any resumed run.
        let authoring = match task.bin_scan.scans() {
            true => adopt_resumed_binaries(authoring, &sidecar_path).await?,
            false => authoring,
        };
        // The same guard the fresh path runs. A resume that adopts an empty or
        // absent claim would publish it, and `adopting_binaries_from` then reads
        // the absent field as "nothing to adopt" — so plan reports no drift and
        // the mistake is permanent.
        reject_empty_scan(&authoring, task)?;
        let metadata = finalize_metadata(&authoring, &task.platform, &sidecar_path).await?;
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
        reject_empty_scan(&scanned, task)?;
        // Issue #51: a tar or zip member keeps whatever mode upstream shipped, and
        // some upstreams ship their interface binary at 0644 (PowerShell's `pwsh`).
        // `asset_type: binary` never had the problem — `place_binary` chmods 0755 —
        // so this closes the asymmetry on the archive path, triggered by the one
        // list that says which files are commands. Both asset types route through
        // here; on the binary path it is a no-op.
        if let Some(binaries) = scanned.binaries() {
            package::ensure_declared_binaries_executable(&content_dir, binaries).await?;
        }
        // The second reader of the same tree, in the same window and for the
        // same reason: what a binary demands of a host's C library is only
        // knowable from the binary. After the scan because it inspects the
        // `PATH` directories the scanned metadata names, and before the sidecar
        // is written so a refused version leaves nothing on disk claiming to be
        // publishable.
        check_declared_libc(&content_dir, &scanned, task).await?;
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

/// Fails a scanning task that ends up with no usable `binaries` claim.
///
/// `binaries: []` is not "undeclared" — it is a positive published claim that
/// the package exposes no executables, and it is what a *fill* yields whenever
/// its target directories are absent from the extracted tree: a typo in the
/// metadata or an upstream that renamed `bin/`. Under `verify` with nothing
/// declared the same state passes silently, because nothing found makes the
/// one-directional diff trivially empty — so the check is on the result, not
/// the diff. An absent field is rejected too: a scanning task that reaches here
/// without one came off a sidecar that disagrees with its own bundle.
///
/// **This observes the tree only when the scan filled the field.** For
/// `(verify, declared)` `resolve_binaries` returns the metadata unchanged, so
/// what arrives here is the hand-written list and nothing about the archive has
/// been established. Catching a vanished directory in that case needs ocx to
/// stop treating a declared-but-absent name as legal (ADR §2) — recorded as a
/// follow-up, not fixable here.
///
/// ponytail: an empty result stands in for "the target directories are
/// missing", which avoids re-deriving ocx's `strip_components` wildcard walk. A
/// package whose interface directory exists but holds no executables lands in
/// the same error, and that is also a spec worth failing.
fn reject_empty_scan(scanned: &AuthoringMetadata, task: &MirrorTask) -> Result<()> {
    if task.bin_scan.scans() && scanned.binaries().is_none_or(|binaries| binaries.is_empty()) {
        anyhow::bail!(
            "bin_scan for {} {} found no executables — the metadata's ${{installPath}} PATH \
             directories are missing from the extracted archive, or hold nothing executable. \
             Publishing would claim this package exposes no commands. Check the PATH entries \
             against the archive layout, or set bin_scan: off and list binaries by hand.",
            task.normalized_version,
            task.platform,
        );
    }
    Ok(())
}

/// Checks what the packaged binaries demand of a host's C library against what
/// the platform key claims they demand, and fails this task if the claim is
/// false.
///
/// `os.features` subset matching reads an omitted `libc.glibc` as "needs no
/// particular libc", which resolves onto every host — so a glibc-linked mirror
/// tile published under a bare `linux/amd64` lands on Alpine and dies with a
/// bare `No such file or directory` naming a file that is plainly there. That
/// has already happened to a bazel mirror. Nothing else in this pipeline reads
/// the artifact, so nothing else can catch it.
///
/// `libc_lint: false` skips the whole call — refusals and unresolvable-scope
/// failures alike, exactly as `ocx package create --no-libc-lint` does. A
/// partial bypass would leave a bug in the un-bypassed half still able to stop
/// a mirror, which is the availability failure the key exists to prevent.
///
/// The context names the repository the run publishes to (one spec per target,
/// so it names which of a repo's specs to fix), the version and the platform;
/// the offending file, its dynamic loader, the libc it needs and the corrected
/// platform key all come from [`LibcLintError`](ocx_lib::package::libc_lint::LibcLintError).
async fn check_declared_libc(content_dir: &Path, metadata: &AuthoringMetadata, task: &MirrorTask) -> Result<()> {
    if !task.libc_lint {
        // Gated on the lint's own scope predicate, not the key alone: the check
        // inspects nothing on darwin/* or windows/*, so on those legs of a
        // matrix the two runs are identical and a warning would name a
        // verification that was never going to happen.
        if libc_lint::checks_declared_libc(&task.platform) {
            log::warn!(
                "[{}] libc_lint: false — {} {} publishes without checking its declared os.features \
                 against the packaged binaries",
                task.target.repository,
                task.normalized_version,
                task.platform,
            );
        }
        return Ok(());
    }
    libc_lint::check_declared_libc(content_dir, metadata, &task.platform)
        .await
        .with_context(|| {
            format!(
                "libc check refused {} {} {} — declare the libc its binaries need, or set \
                 `libc_lint: false` in the spec to publish without the check",
                task.target.repository, task.normalized_version, task.platform,
            )
        })
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

/// `authoring` carrying the `binaries` claim an earlier run scanned, read back
/// off the sidecar it left behind.
///
/// Only reached on a resume under `bin_scan`, where the content tree is gone and
/// a re-scan is impossible. Everything else in `authoring` is the freshly
/// re-resolved spec, so a spec-side fix still lands; only this one field comes
/// from the sidecar. A spec that declares `binaries` itself owns the field and
/// is left alone — the same rule
/// [`ExpectedMetadata::adopting_binaries_from`] applies on the download-free
/// paths.
async fn adopt_resumed_binaries(authoring: AuthoringMetadata, sidecar: &Path) -> Result<AuthoringMetadata> {
    if authoring.binaries().is_some() {
        return Ok(authoring);
    }
    let json = tokio::fs::read_to_string(sidecar).await.with_context(|| {
        format!(
            "cannot resume a bin_scan bundle without its sidecar {} — remove the bundle to re-scan",
            sidecar.display()
        )
    })?;
    let recorded: AuthoringMetadata =
        serde_json::from_str(&json).with_context(|| format!("failed to parse {}", sidecar.display()))?;
    Ok(match recorded.binaries() {
        Some(binaries) => authoring.with_binaries(binaries.clone()),
        None => authoring,
    })
}

/// Phase 2: Push a prepared bundle to the registry with optional cascade.
async fn push_task(
    task: &MirrorTask,
    bundle_path: &Path,
    metadata: &Metadata,
    publisher: &Publisher,
    cascade_versions: &std::collections::BTreeSet<Version>,
    annotations: &std::collections::BTreeMap<String, String>,
    sign: Option<&ResolvedSign>,
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
        sign,
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
#[path = "orchestrator/tests.rs"]
mod tests;
