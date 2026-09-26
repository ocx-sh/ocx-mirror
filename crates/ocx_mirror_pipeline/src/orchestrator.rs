// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use ocx_console::progress::{ProgressManager, Spinner};
use ocx_oci::Platform;
use ocx_package::metadata::Metadata;
use ocx_package::metadata::authoring::{AuthoringDependencies, AuthoringMetadata};
use ocx_package::publisher::Publisher;
use ocx_package::version::Version;
use serde::Serialize;
use tokio::sync::Semaphore;

use super::download;
use super::mirror_result::MirrorResult;
use super::mirror_task::MirrorTask;
use super::package;
use super::progress;
use super::push;
use super::verify;
use crate::ocx_cli::create;
use crate::ocx_cli::resolve_ocx_binary;
use crate::ocx_cli::sign::{ResolvedSign, invoke_sign_reference};
use ocx_mirror_error::MirrorError;
use ocx_mirror_spec::version_platform_map::VersionPlatformMap;
use ocx_mirror_spec::{BinScanMode, MetadataConfig, MirrorSpec};

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
pub struct ExpectedMetadata {
    /// The authoring form both projections below are rendered from.
    ///
    /// Retained rather than discarded after rendering because two things can
    /// only be supplied from outside the spec — `binaries` under a `bin_scan`,
    /// and the digest of a dependency the spec names by tag — and re-rendering
    /// both projections around them is the only way to keep them agreeing with
    /// each other. See [`adopting_binaries_from`](Self::adopting_binaries_from)
    /// and [`adopting_pins_from`](Self::adopting_pins_from).
    authoring: AuthoringMetadata,
    /// Published projection — byte-for-byte the config blob a push writes.
    ///
    /// `None` while a dependency is still tag-only: the published form has no
    /// digest-less dependency, and only `ocx package create` resolves one. An
    /// expectation in that state is incomplete, never current.
    pub published: Option<Metadata>,
    /// `-metadata.json` sidecar written beside the bundle.
    pub sidecar_json: String,
}

impl ExpectedMetadata {
    /// Renders both projections from one authoring document.
    ///
    /// The sidecar goes through [`package::sidecar_json`] so both projections
    /// stay rendered from one authoring document.
    ///
    /// The published projection runs even while a dependency is tag-only,
    /// over a copy pinning each such tag to a placeholder digest. A pin is
    /// the one thing the spec cannot supply, so whatever else the projection
    /// refuses surfaces here as a spec error — not later, behind a pin
    /// adoption it has nothing to do with. The placeholder never leaves this
    /// function.
    pub fn render(authoring: AuthoringMetadata, platform: &Platform) -> Result<Self> {
        let sidecar_json = package::sidecar_json(&authoring, platform)?;
        let projected = with_placeholder_pins(&authoring)?.to_published()?;
        let published = authoring
            .dependencies()
            .iter()
            .all(|dependency| dependency.is_pinned())
            .then_some(projected);
        Ok(Self {
            authoring,
            published,
            sidecar_json,
        })
    }

    /// The dependencies still named by tag alone — what makes
    /// [`published`](Self::published) `None`.
    pub fn unpinned_dependencies(&self) -> Vec<&ocx_oci::PackageRef> {
        self.authoring
            .dependencies()
            .iter()
            .filter(|dependency| !dependency.is_pinned())
            .map(|dependency| &dependency.identifier)
            .collect()
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
    pub fn adopting_binaries_from(&self, published: &Metadata, platform: &Platform) -> Result<Self> {
        match published.binaries() {
            Some(binaries) if self.authoring.binaries().is_none() => {
                Self::render(self.authoring.clone().with_binaries(binaries.clone()), platform)
            }
            _ => Ok(self.clone()),
        }
    }

    /// The same expectation, with each tag-only dependency pinned to the digest
    /// `published` records for the same identifier.
    ///
    /// The same rule as [`adopting_binaries_from`](Self::adopting_binaries_from),
    /// for the other field a download-free path cannot compute: the digest a
    /// tag resolved to at `ocx package create` time. A published dependency
    /// matches when registry, repository and tag are equal — the digest is
    /// exactly what the spec leaves open — so a tile published from this spec
    /// reads as current. A spec dependency with no such match (the tag moved to
    /// a different name, or the dependency is new) stays unpinned, and the
    /// expectation stays incomplete: that is drift only `create` can resolve.
    ///
    /// A dependency the spec pins itself is left alone, so a hand-edited digest
    /// still drifts and patches.
    pub fn adopting_pins_from(&self, published: &Metadata, platform: &Platform) -> Result<Self> {
        if self.published.is_some() {
            return Ok(self.clone());
        }
        let AuthoringMetadata::Bundle(mut bundle) = self.authoring.clone();
        let dependencies = bundle
            .dependencies
            .iter()
            .map(|dependency| {
                let mut dependency = dependency.clone();
                if !dependency.is_pinned()
                    && let Some(recorded) = published
                        .dependencies()
                        .iter()
                        .find(|recorded| recorded.identifier.without_digest() == dependency.identifier)
                {
                    dependency.identifier = recorded.identifier.clone().into();
                }
                dependency
            })
            .collect();
        bundle.dependencies = AuthoringDependencies::new(dependencies)?;
        Self::render(AuthoringMetadata::Bundle(bundle), platform)
    }
}

/// `authoring` with every tag-only dependency pinned to an all-zero digest.
fn with_placeholder_pins(authoring: &AuthoringMetadata) -> Result<AuthoringMetadata> {
    let placeholder = ocx_oci::Digest::Sha256("0".repeat(64));
    let AuthoringMetadata::Bundle(mut bundle) = authoring.clone();
    let dependencies = bundle
        .dependencies
        .iter()
        .map(|dependency| {
            let mut dependency = dependency.clone();
            if !dependency.is_pinned() {
                dependency.identifier = dependency.identifier.clone_with_digest(placeholder.clone());
            }
            dependency
        })
        .collect();
    bundle.dependencies = AuthoringDependencies::new(dependencies)?;
    Ok(AuthoringMetadata::Bundle(bundle))
}

/// Computes the metadata a fresh publish of `platform` would produce today.
///
/// Download-free: reads the spec's metadata files and nothing else. `pipeline
/// plan` compares this against what the registry currently records to detect
/// drift, and `pipeline patch` re-publishes against it — so a spec fix reaches
/// already-published versions without anyone deleting a tag.
///
/// Under a `bin_scan`, or with a dependency named by tag alone, the result is
/// therefore incomplete by construction, and callers must run it through
/// [`ExpectedMetadata::adopting_binaries_from`] and
/// [`ExpectedMetadata::adopting_pins_from`] before comparing.
pub fn expected_metadata(config: &MetadataConfig, platform: &Platform, spec_dir: &Path) -> Result<ExpectedMetadata> {
    ExpectedMetadata::render(
        package::resolve_metadata(config, &platform.to_string(), spec_dir)?,
        platform,
    )
}

/// How a variant produces its published metadata: the spec files it reads, and
/// whether a `bin_scan` derives the `binaries` claim from the content tree.
#[derive(Debug, Clone)]
pub struct MetadataPlan {
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
pub fn metadata_plan_for(spec: &MirrorSpec, version: &Version) -> Option<MetadataPlan> {
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

/// The single tag `tasks` names, or a refusal when they name more than one.
///
/// [`prepare_version`] writes one manifest and names one directory, so the
/// slice it is handed has to agree on the tag. It does not always: a bare
/// `--version 3.7.0` with no `--plan` is matched against *every* effective
/// variant (`build_tasks_for_version`'s `version_info.version == version`),
/// so a spec declaring `variants:` produces tasks carrying `3.7.0_<stamp>`
/// **and** `slim-3.7.0_<stamp>`. Reading the first tag put the manifest under
/// whichever variant came first, listed every variant's bundles in it —
/// colliding two `linux_amd64` rows in one `bundles` array — and left the
/// other variant's directory without a manifest. Reordering `variants:`
/// moved the file, which is issue #35 wearing a different hat.
///
/// Refusing is what the `--plan` path already does for the same input
/// (`version '{v}' names N plan entries`), so the two paths now answer an
/// ambiguous `--version` the same way: name the tag you meant.
fn one_version_per_run(tasks: &[MirrorTask]) -> Result<String, MirrorError> {
    let Some(first) = tasks.first() else {
        return Err(MirrorError::ExecutionFailed(vec![
            "no prepare tasks to run".to_string(),
        ]));
    };

    let mut tags: Vec<&str> = tasks.iter().map(|task| task.normalized_version.as_str()).collect();
    tags.sort_unstable();
    tags.dedup();
    if let [_, _, ..] = tags.as_slice() {
        return Err(MirrorError::SpecUsageError(format!(
            "these prepare tasks name {} versions, and a run publishes one version per run — \
             pass one of these tags as `--version`: {}",
            tags.len(),
            tags.join(", ")
        )));
    }

    Ok(first.normalized_version.clone())
}

/// Prepare all platforms for a single version: download, verify, and bundle.
///
/// Runs platform tasks concurrently with `max_downloads` and `max_bundles`
/// semaphore slots. On success, writes
/// `{work_dir}/{normalized_version}/manifest.json` and returns the populated
/// manifest.
///
/// The version is **read off the tasks**, never passed in. It used to be a
/// parameter, and `prepare` passed its raw `--version` argument while
/// [`task_dir`] below named each bundle directory from
/// `task.normalized_version`: with any `build_timestamp` but `none`, a bare
/// `--version 3.7.0` wrote `3.7.0/manifest.json` beside
/// `3.7.0_20260727160931/<slug>/bundle.tar.xz`, and the manifest described a
/// version it was not stored next to (issue #35). Two names for one thing is
/// the whole defect, so there is now one.
///
/// # Errors
///
/// [`MirrorError::SpecUsageError`] when the tasks do not all carry the same
/// `normalized_version` — see the guard below.
/// [`MirrorError::ExecutionFailed`] when `tasks` is empty — there is no
/// version to name the run after — when a task panics, or when the manifest
/// cannot be written.
pub async fn prepare_version(
    tasks: &[MirrorTask],
    work_dir: &Path,
    http_client: &reqwest::Client,
    concurrency: &ConcurrencyParams,
) -> Result<VersionManifest, MirrorError> {
    let version = one_version_per_run(tasks)?;
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
        version: version.clone(),
        bundles,
    };

    // Beside the bundles, because both directories are now named by the same
    // string.
    let version_dir = work_dir.join(&version);
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
                platform: ocx_oci::Platform::default(),
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
/// The basename is [`ocx_mirror_spec::platform_slug`] — the same slug the CI
/// renderer stamps into `bundle-{V}-{slug}.tar.xz` and `pipeline push` reads
/// back. Computing it locally is how a libc-bearing platform's bundle became
/// invisible to the leg that was supposed to test it.
pub fn task_dir(work_dir: &Path, version: &str, platform: &ocx_oci::Platform) -> PathBuf {
    work_dir.join(version).join(ocx_mirror_spec::platform_slug(platform))
}

/// Phase 1: Download, verify, and bundle a single task.
///
/// Acquires `download_sem` for the download+verify phase, then releases it and
/// acquires `bundle_sem` for the CPU-bound bundling phase. This lets downloads
/// and compression run independently.
///
/// The sequence is `resolve → download → verify → extract → chmod declared
/// binaries → ocx package create → drop tree`. Create owns everything the
/// published metadata needs from the tree and the index: it pins tag-only
/// dependencies to this platform's manifest digest, runs the `bin_scan`, the
/// publish-time validation and the libc check, and writes the bundle and the
/// compiled sidecar. The mirror lays the tree out (asset types, strip, binary
/// renaming, decompression) and keeps the two guards create does not make:
/// the #51 chmod and [`reject_empty_scan`].
///
/// A resume — the bundle **and** the compiled sidecar on disk, compiled from
/// the authoring metadata the spec renders today — reuses both verbatim,
/// because the tree create read is gone and nothing short of a re-create can
/// recompile them. `authoring-metadata.json`, create's `--metadata` input, is
/// the resume key: when the spec's rendering differs from it byte for byte, or
/// it is missing, the task is created again from the archive still on disk, so
/// a spec metadata fix reaches a work dir `package sync` kept across a failed
/// push. What the key does not cover is the create flags: a bundle on disk is
/// **not** evidence the libc check passed — it may have been written under
/// `libc_lint: false`.
///
/// A bundle whose sidecar is missing is an interrupted create, and is simply
/// created again. The publisher-declared digest is the exception to all of
/// this: it reads the downloaded archive rather than the tree, so the resume
/// re-checks it and refuses outright when the archive is gone.
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
    // Resolved before the download so a spec that does not parse fails before
    // a byte moves.
    let authoring = package::resolve_metadata(config, &task.platform.to_string(), &task.spec_dir)?;
    // Create's `--metadata` input, and the resume key: the compiled sidecar is
    // only this run's answer when create compiled it from these exact bytes.
    let authoring_path = task_dir.join("authoring-metadata.json");
    let authoring_json = package::sidecar_json(&authoring, &task.platform)?;

    if bundle_path.exists()
        && sidecar_path.exists()
        && tokio::fs::read_to_string(&authoring_path)
            .await
            .is_ok_and(|compiled_from| compiled_from == authoring_json)
    {
        // The one download-window check a resume can still run: the declared
        // digest reads the downloaded archive, not the content tree that run
        // discarded. It is the control #75's whole safety argument rests on —
        // the proof a proxy served the bytes the publisher declared — so a
        // bundle is never adopted on the strength of existing. Fail-closed when
        // the archive is gone: an unverifiable bundle is not evidence.
        match (task.asset_digest.as_deref(), archive_path.exists()) {
            (Some(digest), true) => verify::verify_digest(&archive_path, digest).await?,
            (Some(_), false) => anyhow::bail!(
                "cannot re-check the declared digest of '{}': {} is present but the downloaded \
                 asset is gone — delete the bundle to re-download and re-verify",
                task.asset_name,
                bundle_path.display(),
            ),
            // Same refusal the fresh path makes, in the same place in the
            // sequence: `require` with nothing declared fails whether or not a
            // bundle happens to be lying around.
            (None, _) if task.require_digest => anyhow::bail!(
                "no publisher digest declared for '{}' and the verify policy is 'require'",
                task.asset_name,
            ),
            (None, _) => {}
        }
        // The same guard the fresh path runs, over the same compiled file: a
        // sidecar edited or written by an older binary may carry an empty or
        // absent claim, and republishing it makes the mistake permanent.
        let metadata = read_compiled_sidecar(&sidecar_path).await?;
        reject_empty_scan(&metadata, task)?;
        return Ok((bundle_path, metadata));
    }

    // Whatever sidecar is here was compiled from other input, or its create was
    // interrupted. Dropped before create runs, so a create that fails now
    // cannot leave it beside a matching authoring file for the next run to
    // adopt.
    match tokio::fs::remove_file(&sidecar_path).await {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err(error)
                .with_context(|| format!("failed to remove the stale sidecar {}", sidecar_path.display()));
        }
        _ => {}
    }

    // --- Download phase (I/O-bound) ---
    {
        let _permit = download_sem.acquire().await.expect("semaphore closed");

        // Download
        if !archive_path.exists() {
            progress::set_stage(spinner, "Downloading", &task.normalized_version, &task.platform);
            download::download(http_client, &task.download_url, &archive_path).await?;
        }

        // Verify. Unconditional since #76: an absent `verify:` block means the
        // *default* policy (`if_present` on both digest axes), not "no
        // verification" — a source that declares a digest is checked against
        // it whether or not the spec spells the block out. The sidecar
        // `checksums_file` leg still needs the block, and defaults to absent.
        let verify_config = task.verify_config.clone().unwrap_or_default();
        progress::set_stage(spinner, "Verifying", &task.normalized_version, &task.platform);
        verify::verify(
            &verify_config,
            http_client,
            &archive_path,
            &task.asset_name,
            task.asset_digest.as_deref(),
            task.require_digest,
        )
        .await?;
    } // download permit released

    // --- Bundle phase (CPU-bound) ---
    //
    // Extraction and create share one permit: create reads the tree extraction
    // just wrote, so releasing in between would only widen the window in which
    // an extracted tree occupies disk without letting any other task progress.
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

        // Issue #51: a tar or zip member keeps whatever mode upstream shipped, and
        // some upstreams ship their interface binary at 0644 (PowerShell's `pwsh`).
        // `asset_type: binary` never had the problem — `place_binary` chmods 0755 —
        // so this closes the asymmetry on the archive path, triggered by the one
        // list that says which files are commands.
        //
        // Skipped under `verify`, which exists to refuse exactly this state: a
        // declared name present but not executable fails create's scan, and
        // fixing the mode first would make that refusal unreachable.
        //
        // ponytail: keyed on the *declared* list, because create fills a scanned
        // one only after this runs. The gap is empty in practice — a fill claims
        // only files it found executable — so a scanned claim never names a file
        // this would have had to fix.
        if task.bin_scan != BinScanMode::Verify
            && let Some(binaries) = authoring.binaries()
        {
            package::ensure_declared_binaries_executable(&content_dir, binaries).await?;
        }

        tokio::fs::write(&authoring_path, &authoring_json)
            .await
            .with_context(|| format!("failed to write {}", authoring_path.display()))?;
        let request = create::CreateRequest {
            content_dir: &content_dir,
            platform: &task.platform,
            metadata: &authoring_path,
            output: &bundle_path,
            compression_threads,
            bin_scan: task.bin_scan,
            libc_lint: task.libc_lint,
            remote: create::resolves_tags_remotely(),
        };
        let ocx_binary = resolve_ocx_binary().map_err(|error| anyhow::anyhow!(error))?;
        create::create(&ocx_binary, &request.args(), create::CREATE_TIMEOUT)
            .await
            .with_context(|| {
                format!(
                    "ocx package create refused {} {} {}",
                    task.target.repository, task.normalized_version, task.platform,
                )
            })?;

        // Checked before the sidecar takes the name CI and resume look for, so a
        // refused scan leaves nothing either of them would treat as publishable.
        let compiled_path = create::compiled_sidecar_path(&bundle_path);
        let metadata = read_compiled_sidecar(&compiled_path).await?;
        reject_empty_scan(&metadata, task)?;
        tokio::fs::rename(&compiled_path, &sidecar_path)
            .await
            .with_context(|| {
                format!(
                    "failed to move {} to {}",
                    compiled_path.display(),
                    sidecar_path.display()
                )
            })?;
        // Best-effort: the bundle is written, and a leftover tree is only disk.
        let _ = tokio::fs::remove_dir_all(&content_dir).await;

        metadata
    }; // bundle permit released

    Ok((bundle_path, metadata))
}

/// The published-form sidecar `ocx package create` compiled.
async fn read_compiled_sidecar(path: &Path) -> Result<Metadata> {
    let json = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("failed to read the compiled sidecar {}", path.display()))?;
    serde_json::from_str(&json).with_context(|| {
        format!(
            "failed to parse the compiled sidecar {} — delete the bundle beside it to re-prepare",
            path.display()
        )
    })
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
/// `(verify, declared)` create passes the declaration through, so what arrives
/// here is the hand-written list and nothing about the archive has been
/// established. Catching a vanished directory in that case needs ocx to stop
/// treating a declared-but-absent name as legal (ADR §2) — recorded as a
/// follow-up, not fixable here.
///
/// ponytail: an empty result stands in for "the target directories are
/// missing", which avoids re-deriving ocx's `strip_components` wildcard walk. A
/// package whose interface directory exists but holds no executables lands in
/// the same error, and that is also a spec worth failing.
fn reject_empty_scan(metadata: &Metadata, task: &MirrorTask) -> Result<()> {
    if task.bin_scan.scans() && metadata.binaries().is_none_or(|binaries| binaries.is_empty()) {
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
    let target = ocx_oci::OciIdentifier::from_parts(&task.target.repository, &task.target.registry)
        .clone_with_tag(&task.normalized_version);

    let info = ocx_package::info::Info {
        metadata: metadata.clone(),
        platform: task.platform.clone(),
    };

    push::push_and_cascade(
        publisher,
        &target,
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
