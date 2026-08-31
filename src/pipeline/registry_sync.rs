// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! The orchestrator: bootstrap → whole-run pre-flight → per-source copy.
//!
//! Every module below this one is a decision in isolation; this file is the
//! order they run in, and the order **is** the contract. Contracts C-040 and
//! C-044 of `plan_registry_mirror_sync.md`.
//!
//! # The three phases, and why the split is load-bearing
//!
//! **Phase 0** creates `output:` before any cache path is derived. C-037 and
//! C-038 canonicalize that directory, and `std::fs::canonicalize` needs it to
//! exist; `IndexStore`'s writers create it lazily, long after
//! `with_locks_root` has to be computed. Without the `create_dir_all` the
//! *first* sync against a fresh destination fails having done nothing.
//!
//! **Phase 1** pre-flights **every** source before a single byte is copied.
//! Collision detection (C-015) is a whole-run property: folded into a
//! per-source loop, source 1 would be fully mirrored — hours, gigabytes, roots
//! written — before source 2's expansion revealed the clash. The same hoist
//! makes C-018's [`MirrorError::IndexFormatUnsupported`] a whole-run gate,
//! matching its own "writes nothing" contract.
//!
//! **Phase 2** copies, per source in spec order, with up to
//! `concurrency.max_packages` packages in flight (`buffer_unordered`, then the
//! outcomes are re-sorted into spec order for the report). C-026's memory
//! ceiling is `max_blobs × largest_blob` and it holds *regardless* of that
//! package fan-out, because the one run-scoped `Arc<Semaphore>` this file owns
//! gates every blob pull across every package and source — concurrent packages
//! contend for the same permits rather than each getting their own.
//!
//! # The two error classes (C-040)
//!
//! The rule is a class, not a list: **only a read whose answer is not
//! authoritative aborts the run; every write failure fails its package.** Both
//! classes travel in the types themselves, so this file never enumerates
//! variants:
//!
//! - [`CopyError`] carries [`CopyError::is_whole_run_abort`] —
//!   [`tag_failure`] branches on that predicate.
//! - [`super::registry_sync::index_write`] returns
//!   [`MirrorError::ExecutionFailed`] for a refusal of *upstream* bytes and
//!   [`MirrorError::IndexWriteError`] for a *local* filesystem failure —
//!   [`write_failure`] splits on exactly that.
//! - Everything [`catalog`] returns is a source-side read that did not answer,
//!   so it propagates with `?` and aborts.
//!
//! `on_error` (and `--fail-fast`) governs the aggregating class only.

pub mod cache;
pub mod catalog;
pub mod destination;
pub mod glob;
pub mod index_write;
pub mod plan;
pub mod report;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::{StreamExt as _, TryStreamExt as _};
use ocx_lib::file_structure::IndexStore;
use ocx_lib::oci::index::{CatalogIndex, OciIndex, OciIndexConfig, parse_physical_repository, serialize_root};
use ocx_lib::oci::{Algorithm, ClientBuilder, Digest, Index, Reference};
use tokio::sync::Semaphore;

use self::plan::{PackageWork, PlannedDestination, SourcePlan};
use self::report::{PackageOutcome, PackageReport, RegistrySyncReport, RunCounters, SourceReport};
use crate::command::registry::options::RegistrySyncOptions;
use crate::error::MirrorError;
use crate::pipeline::registry_copy::{self, CopyContext, CopyError, DESCRIPTION_TAG};
use crate::spec::{OnError, RegistrySource, RegistrySpec};

/// Run one `registry sync` (C-044).
///
/// Returns the report even for a run with failures — the caller renders it and
/// *then* maps `has_failures()` to an exit code, so an operator sees which
/// packages failed rather than only that some did. An `Err` here is C-040's
/// whole-run abort class and there is no report to render, because the run
/// stopped before it could count anything.
///
/// # Errors
///
/// Every whole-run abort: [`MirrorError::SourceError`] (69) for a source index
/// or registry that will not answer and for an SSRF refusal,
/// [`MirrorError::IndexFormatUnsupported`] (65) for a source format this ocx
/// does not implement, [`MirrorError::SpecInvalid`] (65) for a catalog key
/// that cannot be expanded and for a destination collision,
/// [`MirrorError::IndexWriteError`] (74) for a local filesystem failure, and
/// [`MirrorError::TargetError`] (69) for a destination read with no
/// authoritative answer.
pub async fn execute_registry_sync(
    spec: &RegistrySpec,
    options: &RegistrySyncOptions,
) -> Result<RegistrySyncReport, MirrorError> {
    // ── Phase 0 — bootstrap ─────────────────────────────────────────────────
    let (cache_root, store) = bootstrap(spec, options).await?;

    repair_catalogs(spec, options, &store).await?;

    // ── Phase 1 — pre-flight, over ALL sources ──────────────────────────────
    //
    // Announced before each source, not after: this loop is several network
    // round trips per source and writes nothing, so a silent one is
    // indistinguishable from a hang.
    let mut sources = Vec::with_capacity(spec.sources.len());
    for (position, source) in spec.sources.iter().enumerate() {
        tracing::info!(
            "[{}/{}] pre-flight source '{}'",
            position + 1,
            spec.sources.len(),
            source.as_name()
        );
        sources.push(prepare_source(spec, source, &store, &cache_root).await?);
    }

    // Over the union, including sources that short-circuited: a short-circuited
    // source is one already mirrored to these repositories, so dropping it lets
    // another source silently overwrite what a previous run published (C-015).
    let expansions: Vec<destination::Expansion> = sources
        .iter()
        .flat_map(|prepared| plan::expansions(&prepared.plan.as_name, &prepared.plan.work))
        .collect();
    destination::detect_collisions(&expansions)?;

    // ── Phase 2 — copy, per source in spec order ────────────────────────────
    copy_sources(spec, options, &sources, &store).await
}

/// Phase 0 (C-044): make `output:` exist, then derive everything keyed on it.
///
/// The `create_dir_all` is **first**, and that is the whole point of the
/// phase. [`cache::locks_dir`] canonicalizes `output:` and
/// `std::fs::canonicalize` fails on a path that does not exist, while
/// `IndexStore`'s own writers create the tree lazily on first write — long
/// after `with_locks_root` has to be computed. Without this line the *first*
/// sync against a fresh destination fails having done nothing at all, and only
/// a test that supplies a genuinely absent `output:` catches it.
///
/// Returns the cache root alongside the store because phase 1 needs it for
/// [`cache::digest_file`], and deriving it twice invites the two call sites to
/// disagree about `--cache-dir`.
///
/// # Errors
///
/// [`MirrorError::IndexWriteError`] (74) when the output directory cannot be
/// created or canonicalized.
async fn bootstrap(spec: &RegistrySpec, options: &RegistrySyncOptions) -> Result<(PathBuf, IndexStore), MirrorError> {
    tokio::fs::create_dir_all(&spec.output).await.map_err(|error| {
        MirrorError::IndexWriteError(format!(
            "cannot create the output directory '{}': {error}",
            spec.output.display()
        ))
    })?;
    let cache_root = cache::cache_root(options.cache_dir.as_deref());
    let locks_dir = cache::locks_dir(&cache_root, &spec.output).await?;
    // `with_locks_root` is mandatory and forgetting it is silent — the default
    // puts the lock directory inside the tree the operator serves and commits.
    Ok((cache_root, index_write::build_index_store(&spec.output, &locks_dir)))
}

/// `--repair-catalog`, over every source — **ahead of the pre-flight** (C-034).
///
/// Ahead of it, not after: a repaired `c/index.json` is what `should_skip`'s
/// third condition compares against, so repairing later would leave this run
/// re-copying every package the corrupt catalog disagreed about.
///
/// Two things the flag does *not* do:
///
/// - **Nothing at all under `--dry-run`** (C-043, S-019). `regenerate_catalog`
///   rewrites `c/index.json` wholesale, dropping every entry whose root the tree
///   lacks — the single most destructive write this binary makes into a served,
///   committed tree, and `--dry-run`'s own promise is that it writes nothing.
/// - **Nothing for a source whose subtree does not exist yet.** Repair is
///   per-source, so an absent one is not a failure of the *run*: `bootstrap`
///   creates `output:` alone, which makes absence the ordinary state of a first
///   run and of every run that adds a source. `regenerate_catalog` refuses it —
///   correctly, for `ocx index regenerate`, where a repair verb pointed at
///   nothing is a user error — and that refusal is `IndexWriteError` (74), which
///   aborts the whole run having repaired nothing, including the sources that
///   were fine.
///
/// Only the absence is tolerated: the probe fails *closed*, and every other
/// failure of the repair itself still aborts (C-040).
///
/// # Errors
///
/// [`MirrorError::IndexWriteError`] (74) for a local filesystem failure while
/// re-deriving a catalog.
async fn repair_catalogs(
    spec: &RegistrySpec,
    options: &RegistrySyncOptions,
    store: &IndexStore,
) -> Result<(), MirrorError> {
    if !options.repair_catalog {
        return Ok(());
    }
    if options.dry_run {
        // Said, not silent: the operator asked for the repair and is not
        // getting it, and a silent skip is indistinguishable from a repair that
        // found nothing to do.
        tracing::info!("--dry-run: not repairing any catalog, because a dry run writes nothing");
        return Ok(());
    }

    for source in &spec.sources {
        let as_name = source.as_name();
        // The subtree, derived the way `regenerate_catalog` derives it —
        // through `source_config_path`, the one public accessor that exposes
        // it. `<subtree>/config.json` always has a parent; the fallback keeps
        // the impossible branch on the fail-closed side rather than skipping.
        let config_path = store.source_config_path(as_name);
        let subtree = config_path.parent().unwrap_or(&config_path);
        // `unwrap_or(true)` deliberately: a probe that could not answer is not
        // an absence, so the repair runs and raises the real I/O failure.
        if !tokio::fs::try_exists(subtree).await.unwrap_or(true) {
            tracing::info!(
                "nothing to repair for source '{as_name}': '{}' does not exist yet",
                subtree.display()
            );
            continue;
        }

        let outcome = index_write::repair_catalog(store, as_name).await?;
        tracing::info!(
            "repaired the catalog for source '{}': {} roots, {} added, {} corrected, {} removed",
            outcome.source,
            outcome.roots,
            outcome.added.len(),
            outcome.corrected.len(),
            outcome.removed.len()
        );
    }
    Ok(())
}

/// One source after pre-flight — everything phase 2 needs, fetched once.
struct PreparedSource<'spec> {
    source: &'spec RegistrySource,
    plan: SourcePlan,
    /// The pre-flighted index-tree HTTP client (C-016). One per source: it
    /// carries that source's `trusted_hosts` exemptions.
    index_client: reqwest::Client,
    /// Where this source's catalog digest is recorded on full success (C-038).
    digest_path: PathBuf,
    /// The destination's own `c/index.json`, read **before** any write.
    ///
    /// Deliberately not re-read per package: `should_skip` consults one entry
    /// per package and each package is visited once, so the only entries this
    /// snapshot can be stale about are ones no later comparison reads.
    local_catalog: CatalogIndex,
}

/// Phase 1 for one source: client → `config.json` → catalog → plan (C-044).
///
/// Network work only; the filtering and expansion it feeds are pure, which is
/// what lets the whole pre-flight finish for every source before a byte moves.
async fn prepare_source<'spec>(
    spec: &RegistrySpec,
    source: &'spec RegistrySource,
    store: &IndexStore,
    cache_root: &Path,
) -> Result<PreparedSource<'spec>, MirrorError> {
    let as_name = source.as_name();
    let index_client = catalog::build_source_index_client(&source.index, &source.trusted_hosts).await?;

    // C-018's format gate fires here, in phase 1 — so a source declaring a
    // version this ocx cannot read refuses the WHOLE run before any other
    // source has been touched. The document's *bytes* are deliberately
    // dropped: `write_config_json` takes none, because `name_segments` is a
    // fleet-wide name-grammar declaration and copying an upstream's would let
    // it change how every client resolves names against this mirror.
    catalog::fetch_source_config(&index_client, &source.index).await?;
    let (catalog_bytes, catalog_document) = catalog::fetch_source_catalog(&index_client, &source.index).await?;
    // Over the raw bytes, never a re-serialization: the short-circuit compares
    // this against what a previous run recorded, and a round trip through
    // `CatalogDocument` would change the digest of an unchanged source.
    let catalog_digest = Algorithm::Sha256.hash(&catalog_bytes).to_string();

    let digest_path = cache::digest_file(cache_root, &spec.output, as_name).await?;
    let recorded_digest = cache::read_recorded_digest(&digest_path).await?;
    let local_catalog = store
        .read_source_catalog(as_name)
        .await
        .map_err(|error| {
            MirrorError::IndexWriteError(format!(
                "cannot read the mirrored catalog for source '{as_name}': {error}"
            ))
        })?
        .unwrap_or_default();

    let plan = plan::plan_source(
        spec,
        source,
        as_name,
        &catalog_document.packages,
        &catalog_digest,
        recorded_digest.as_deref(),
        &local_catalog,
    )?;

    Ok(PreparedSource {
        source,
        plan,
        index_client,
        digest_path,
        local_catalog,
    })
}

/// Phase 2 (C-044): every source in spec order; within a source, up to
/// `concurrency.max_packages` packages at once (`fail_fast` forces one), with
/// the outcomes re-sorted into catalog order for the report.
async fn copy_sources(
    spec: &RegistrySpec,
    options: &RegistrySyncOptions,
    sources: &[PreparedSource<'_>],
    store: &IndexStore,
) -> Result<RegistrySyncReport, MirrorError> {
    // Run-scoped, all three. The semaphore because C-026's memory ceiling is
    // `max_blobs × largest_blob` only while one pool covers the whole run;
    // `mounted_from` because a blob already at the destination is mountable
    // from whichever repository holds it, whatever source put it there; and
    // `mount_warnings` because the suppression is worthless if it resets.
    let blob_semaphore = Arc::new(Semaphore::new(spec.concurrency.max_blobs));
    let mounted_from = Arc::default();
    let mount_warnings = Arc::default();
    // One credential resolver for the run: clones share the `Arc<RwLock>` cache
    // (`ensure_source_auth`), so a physical host resolved once is not resolved
    // again by a later package naming the same host.
    let source_auth = ocx_lib::auth::Auth::new();
    // One destination client for the run: `native::Client` clones share the
    // token cache, so per-source clones re-use the destination's bearer token
    // instead of re-running the challenge per source.
    let destination_client = registry_copy::build_destination_client(&spec.target).await;

    // `--fail-fast` overrides the spec rather than combining with it (C-045),
    // and there is no flag that turns `fail_fast:` back off.
    let fail_fast = options.fail_fast || spec.on_error == OnError::FailFast;

    let mut report = RegistrySyncReport::default();
    let mut missing: Vec<(Digest, u64)> = Vec::new();

    'sources: for prepared in sources {
        let mut source_report = SourceReport {
            as_name: prepared.plan.as_name.clone(),
            short_circuited: prepared.plan.short_circuited,
            packages: Vec::new(),
        };
        if prepared.plan.short_circuited {
            report.sources.push(source_report);
            continue;
        }

        let context = CopyContext {
            source_index: source_read_seam(&prepared.source.trusted_hosts),
            source_client: registry_copy::build_source_client(prepared.source).await,
            source_auth: source_auth.clone(),
            destination_client: destination_client.clone(),
            blob_semaphore: Arc::clone(&blob_semaphore),
            max_retries: spec.concurrency.max_retries,
            canonical_tags: spec.canonical_tags,
            mounted_from: Arc::clone(&mounted_from),
            mount_warnings: Arc::clone(&mount_warnings),
        };

        let mut source_failed = false;
        let package_count = prepared.plan.work.len();
        // `fail_fast` promises that nothing after the first failure is copied,
        // and no fan-out can keep that promise: whatever is in flight has
        // already been sent. Width 1 *is* the promise, so the flag chooses the
        // sequential path rather than being checked inside a concurrent one.
        let width = if fail_fast { 1 } else { spec.concurrency.max_packages };

        let mut outcomes: Vec<PackageOutcomeRow> = Vec::new();
        let mut stopped_early = false;
        if width == 1 {
            for (position, package) in prepared.plan.work.iter().enumerate() {
                let row = copy_one_package(
                    spec,
                    options,
                    prepared,
                    package,
                    store,
                    &context,
                    Progress {
                        position,
                        count: package_count,
                    },
                )
                .await?;
                let failed = matches!(row.step, PackageStep::Failed(_));
                outcomes.push(row);
                if failed && fail_fast {
                    stopped_early = true;
                    break;
                }
            }
        } else {
            // `buffer_unordered`, not `buffered`: `buffered` holds a slot for a
            // package that finished but has not been yielded, so on the ordinary
            // incremental sweep — where most packages short-circuit in one root
            // fetch and a few copy for minutes — the finished skips pin the head
            // slots and the effective width collapses toward 1, which is the one
            // workload `max_packages` exists to speed up. Completion-order
            // yielding is fine because the fold sorts on `position` below; a
            // package's cost is round trips against two registries, and this
            // moves no memory ceiling, because the blob pool above is run-scoped.
            outcomes = futures::stream::iter(prepared.plan.work.iter().enumerate().map(|(position, package)| {
                let context = &context;
                async move {
                    copy_one_package(
                        spec,
                        options,
                        prepared,
                        package,
                        store,
                        context,
                        Progress {
                            position,
                            count: package_count,
                        },
                    )
                    .await
                }
            }))
            .buffer_unordered(width)
            .try_collect()
            .await?;
            // Completion order → catalog order, so the report reads the way the
            // catalog does regardless of which package finished first.
            outcomes.sort_by_key(|row| row.position);
        }

        for row in outcomes {
            source_failed |= matches!(row.step, PackageStep::Failed(_));
            missing.extend(row.missing);
            source_report.packages.push(package_report(&row.name, row.step));
        }

        if stopped_early {
            report.sources.push(source_report);
            break 'sources;
        }

        // C-039: the recorded digest means "a fully successful run mirrored
        // this exact catalog", so a source with any failure leaves the previous
        // value in place and the next run re-compares. Per source, not per run:
        // a source that fully succeeded is fully mirrored whatever another
        // source did, and `--dry-run` records nothing at all (C-043).
        if !source_failed && !options.dry_run {
            cache::record_digest(&prepared.digest_path, &prepared.plan.cache_record).await?;
        }
        report.sources.push(source_report);
    }

    report.counters = counters(&report.sources);
    if options.dry_run {
        // De-duplicated across the whole run, not per package: a blob two
        // packages share is uploaded once and mounted thereafter.
        report.estimated_bytes = Some(plan::dry_run_byte_estimate(&missing));
    }
    Ok(report)
}

/// One package's result as the fold below consumes it.
///
/// Carries its catalog `position` because the concurrent path yields in
/// completion order (`buffer_unordered`), and the report must read back in
/// catalog order — the fold sorts on this. The dry-run `missing` list is per
/// package rather than one shared `Vec`: with packages in flight together there
/// is no shared `&mut` to write into, and folding after the sort keeps the
/// estimate independent of which package finished first.
struct PackageOutcomeRow {
    /// Index into `prepared.plan.work` — the catalog order to restore.
    position: usize,
    /// The catalog key.
    name: String,
    /// What the pass did.
    step: PackageStep,
    /// Descriptors a `--dry-run` measured for this package.
    missing: Vec<(Digest, u64)>,
}

/// One package's copy, with the progress line that announces it.
///
/// Extracted from the loop so the sequential and concurrent paths run the
/// same body — the only difference between them is how many are in flight.
/// Where a package sits in its source's work list — `position` of `count`,
/// zero-based — carried as one value because the two are always passed and read
/// together (the progress line, and the report re-sort key).
#[derive(Clone, Copy)]
struct Progress {
    position: usize,
    count: usize,
}

async fn copy_one_package(
    spec: &RegistrySpec,
    options: &RegistrySyncOptions,
    prepared: &PreparedSource<'_>,
    package: &PackageWork,
    store: &IndexStore,
    context: &CopyContext,
    progress: Progress,
) -> Result<PackageOutcomeRow, MirrorError> {
    // Before the copy, not after it: the existing per-tag line at
    // `tag_failure` reports a package that already finished, and a
    // multi-gigabyte one spends its whole transfer between the two.
    // `package.name` is `{:?}`: it is a catalog key straight off the source's
    // index, and under a `{upstream_*}` template it never reaches the OCI
    // grammar guard, so a key carrying a newline or an escape would otherwise
    // forge lines in the CI log an operator reads (CWE-117).
    tracing::info!(
        "[{}/{}] {}/{:?}",
        progress.position + 1,
        progress.count,
        prepared.plan.as_name,
        package.name
    );
    let mut missing = Vec::new();
    let step = sync_package(spec, options, prepared, package, store, context, &mut missing).await?;
    Ok(PackageOutcomeRow {
        position: progress.position,
        name: package.name.clone(),
        step,
        missing,
    })
}

/// What one package's pass did.
enum PackageStep {
    /// Content transferred (or, under `--dry-run`, measured) and the root
    /// written.
    Copied,
    /// Already at the destination with matching tag→digest pairs (C-032).
    Skipped,
    /// One or more aggregating failures; the message names each.
    Failed(String),
}

/// Mirror one package (C-044's phase 2 body).
///
/// The write order is C-030's visibility guarantee and is not negotiable:
/// dispatch objects → merge → rewrite → open the transaction → root → commit →
/// `config.json`. `root exists ⇒ its objects exist`, and `config.json` after
/// the commit because it re-acquires the same source lock rather than
/// inheriting the transaction's — the inverted order self-deadlocks.
///
/// # Errors
///
/// Only C-040's whole-run abort class. An aggregating failure comes back as
/// [`PackageStep::Failed`].
async fn sync_package(
    spec: &RegistrySpec,
    options: &RegistrySyncOptions,
    prepared: &PreparedSource<'_>,
    package: &PackageWork,
    store: &IndexStore,
    context: &CopyContext,
    missing: &mut Vec<(Digest, u64)>,
) -> Result<PackageStep, MirrorError> {
    let as_name = &prepared.plan.as_name;
    let source = prepared.source;

    let (root_bytes, source_root) =
        catalog::fetch_source_root(&prepared.index_client, &source.index, &package.name).await?;
    // C-017, before any registry request for this package.
    catalog::validate_root_host(&source_root, &source.trusted_hosts).await?;

    // `read_root_uncatalogued`, never `read_root`: the latter opens its own
    // `begin_catalog_transaction` on a straddle, which self-deadlocks against
    // the one this function is about to take.
    //
    // `Ok(None)` (a first publish) and `Err` are **not** collapsed (C-047):
    // reading a corrupt root as "absent" would degrade the tag union to this
    // run's confirmed set and delete every tag a previous run mirrored.
    let local_root = match store
        .read_root_uncatalogued(as_name, &package.name, |root| {
            parse_physical_repository(&root.repository).map(|_| ())
        })
        .await
    {
        Ok(local_root) => local_root,
        Err(error) => {
            return Ok(PackageStep::Failed(format!(
                "cannot read the mirrored root document: {error}"
            )));
        }
    };

    if index_write::should_skip(
        &package.name,
        &source_root,
        local_root.as_ref(),
        &prepared.local_catalog,
    ) {
        return Ok(PackageStep::Skipped);
    }

    let destination_root = local_root.as_ref().map(|read| read.bytes.as_slice());

    // Re-parsed rather than threaded out of `validate_root_host`, so that
    // function stays the single SSRF guard on this path instead of doubling as
    // an accessor.
    let (source_registry, source_repository) = parse_physical_repository(&source_root.repository).map_err(|error| {
        MirrorError::SourceError(format!(
            "source root for '{}' has an unusable repository pointer: {error}",
            package.name
        ))
    })?;

    // A `{upstream_host}`/`{upstream_repository}` template expands here and
    // nowhere else — the plan phase holds the catalog key alone, and this is
    // the first line at which the upstream reference exists. Its refusal is one
    // package's failure (C-040): an upstream pointer that cannot be composed
    // into a path is that upstream's fault, not the spec's, and aborting would
    // let any source end a whole run.
    let destination = match &package.destination {
        PlannedDestination::Resolved(resolved) => resolved.clone(),
        PlannedDestination::FromUpstream => {
            let upstream = destination::Upstream {
                host: &source_registry,
                repository: &source_repository,
            };
            match plan::resolve_upstream(spec, as_name, &package.name, upstream) {
                Ok(resolved) => resolved,
                // `resolve_upstream` composes the upstream reference into a path
                // and reaches no registry or disk, so every failure it can
                // return is that one pointer's fault — a per-package failure
                // (C-040), never a run abort. Classified by the call rather than
                // the variant so a future change to its error type cannot
                // silently promote one bad root into a whole-run abort.
                Err(MirrorError::SpecInvalid(messages)) => return Ok(PackageStep::Failed(messages.join("; "))),
                Err(other) => return Ok(PackageStep::Failed(other.to_string())),
            }
        }
    };

    if let Some(pointer) = destination.pointer.as_deref() {
        index_write::warn_on_pointer_drift(destination_root, &package.name, pointer);
    }

    // Seed the source client for the host the pointer names, before the first
    // request addressed to it. `build_source_client` seeded the *logical*
    // `registry:`; everything below dials this one. A real credential is
    // resolved only for a host the operator named — the source's `registry:`
    // or a `trusted_hosts` entry — because the physical host comes from the
    // upstream root and a lookalike could otherwise slug-collide onto the
    // operator's credential (see `ensure_source_auth`).
    let credentialed_hosts: Vec<&str> = std::iter::once(source.registry.as_str())
        .chain(source.trusted_hosts.iter().map(String::as_str))
        .collect();
    registry_copy::ensure_source_auth(context, &source_registry, &credentialed_hosts).await;

    // The mirror image of the drift warning above, and mutually exclusive with
    // it: under a rewrite the published pointer is ours and drift is the risk,
    // under preserve it is upstream's and reachability is. Placed here rather
    // than in the plan phase because the upstream repository only exists once
    // the root has been fetched — plan time sees the catalog key alone.
    if destination.pointer.is_none() {
        destination::warn_on_mirror_path_mismatch(
            &package.name,
            &source_registry,
            &source_repository,
            &destination.physical_repository,
        );
    }

    let mut confirmed = BTreeSet::new();
    let mut objects: BTreeMap<Digest, Vec<u8>> = BTreeMap::new();
    let mut failures: Vec<String> = Vec::new();

    let policy = if spec.publish_tags {
        registry_copy::TagPolicy::Publish
    } else {
        registry_copy::TagPolicy::DigestOnly
    };

    for entry in registry_copy::tag_copy_plan(&source_root.tags) {
        let source_reference =
            Reference::with_tag(source_registry.clone(), source_repository.clone(), entry.tag.clone());
        let destination_reference = policy.address(
            &spec.target.registry,
            &destination.physical_repository,
            &entry.tag,
            &entry.content,
        );

        if options.dry_run {
            match registry_copy::missing_descriptors(&source_reference, &destination_reference, &entry.content, context)
                .await
            {
                Ok(descriptors) => missing.extend(descriptors),
                Err(error) => failures.push(tag_failure(&entry.tag, error)?),
            }
            continue;
        }

        // `objects` holds one verified manifest body per distinct `content`
        // digest, so a hit means an earlier tag of this package already brought
        // every byte this one names over. All that is left is to point the tag
        // at it — and under `DigestOnly` not even that.
        if let Some(bytes) = objects.get(&entry.content) {
            let tagged = match policy {
                registry_copy::TagPolicy::Publish => {
                    registry_copy::tag_manifest(&destination_reference, bytes, context).await
                }
                registry_copy::TagPolicy::DigestOnly => Ok(()),
            };
            match tagged {
                Ok(()) => {
                    tracing::info!(
                        "copied {:?} tag {:?}: content already copied for an earlier tag",
                        package.name,
                        entry.tag
                    );
                    confirmed.insert(entry.tag.clone());
                }
                Err(error) => failures.push(tag_failure(&entry.tag, error)?),
            }
            continue;
        }

        let copied =
            registry_copy::copy_manifest_tree(&source_reference, &destination_reference, &entry.content, context).await;
        record_tag(
            &package.name,
            &entry,
            copied,
            &mut confirmed,
            &mut objects,
            &mut failures,
        )?;
    }

    if options.dry_run {
        // C-043 writes nothing at all — no objects, no root, no catalog entry,
        // no cache digest.
        return Ok(finish(failures));
    }

    if let Err(error) = registry_copy::copy_description(
        &Reference::with_tag(source_registry, source_repository, DESCRIPTION_TAG.to_string()),
        &Reference::with_tag(
            spec.target.registry.clone(),
            destination.physical_repository.clone(),
            DESCRIPTION_TAG.to_string(),
        ),
        policy,
        context,
    )
    .await
    {
        failures.push(tag_failure(DESCRIPTION_TAG, error)?);
    }

    // Nothing landed and nothing was published before, so there is no root to
    // write that would not be a lie: an empty `tags{}` root advertises a
    // package the mirror does not hold, and it satisfies `should_skip`'s
    // conditions 1 and 3 forever after. The next ordinary run retries.
    if !failures.is_empty() && confirmed.is_empty() && local_root.is_none() {
        return Ok(finish(failures));
    }

    if let Err(error) = write_package(
        store,
        PublishTarget {
            as_name,
            name: &package.name,
            pointer: destination.pointer.as_deref(),
        },
        &root_bytes,
        destination_root,
        &confirmed,
        &objects,
    )
    .await
    {
        failures.push(write_failure(error)?);
    }

    Ok(finish(failures))
}

/// Fold one tag's copy result into the confirmed set, the dispatch objects and
/// the failure list — **the only place C-047's `confirmed` gains a member**.
///
/// The whole of C-047's wiring is the one branch below: a tag joins
/// `confirmed` on `Ok` and on nothing else. Hand [`index_write::merge_root_tags`]
/// the source root's `tags{}` instead of this set and every one of WP-11's
/// unit tests still passes, while a partial run publishes tags whose content
/// was never copied — and the next run's union makes that permanent. It is
/// extracted rather than left inline in the loop precisely so the branch is
/// reachable from a test without a registry.
///
/// `objects` is keyed by digest, so two tags pointing at the same dispatch
/// object write it once.
///
/// Both `package` and `entry.tag` are `{:?}`-formatted (CWE-117): the key is a
/// catalog key and the tag comes straight out of the same upstream root's
/// `tags{}`, and neither has passed a charset guard on the way into a log an
/// operator reads.
///
/// # Errors
///
/// C-040's whole-run abort class, via [`tag_failure`].
fn record_tag(
    package: &str,
    entry: &registry_copy::TagCopyPlan,
    copied: Result<(registry_copy::CopyStats, Vec<u8>), CopyError>,
    confirmed: &mut BTreeSet<String>,
    objects: &mut BTreeMap<Digest, Vec<u8>>,
    failures: &mut Vec<String>,
) -> Result<(), MirrorError> {
    match copied {
        Ok((stats, bytes)) => {
            tracing::info!(
                "copied {package:?} tag {:?}: {} manifest(s), {} blob(s) skipped, {} mounted, {} uploaded ({} bytes)",
                entry.tag,
                stats.manifests,
                stats.blobs_skipped,
                stats.blobs_mounted,
                stats.blobs_uploaded,
                stats.bytes_uploaded
            );
            confirmed.insert(entry.tag.clone());
            objects.insert(entry.content.clone(), bytes);
        }
        Err(error) => failures.push(tag_failure(&entry.tag, error)?),
    }
    Ok(())
}

/// Which subtree, which package, and the pointer its root must carry — the
/// identity half of one publish.
///
/// A struct rather than three parameters: [`write_package`] already carries
/// four byte-level arguments, and these three travel together everywhere.
struct PublishTarget<'a> {
    /// The source's `as:` value — its output subtree.
    as_name: &'a str,
    /// The catalog key.
    name: &'a str,
    /// The `oci://` pointer to write, or `None` to republish the source's own.
    pointer: Option<&'a str>,
}

/// The index-tree writes for one package, in C-030's order.
///
/// # Errors
///
/// [`MirrorError::ExecutionFailed`] for a refusal of upstream bytes
/// (aggregating) and [`MirrorError::IndexWriteError`] for a local filesystem
/// failure (whole-run abort) — [`write_failure`] is what tells them apart.
async fn write_package(
    store: &IndexStore,
    target: PublishTarget<'_>,
    root_bytes: &[u8],
    destination_root: Option<&[u8]>,
    confirmed: &BTreeSet<String>,
    objects: &BTreeMap<Digest, Vec<u8>>,
) -> Result<(), MirrorError> {
    // First, unconditionally: `root exists ⇒ its dispatch objects exist`
    // (C-033). Content-addressed and idempotent, so the ordering is free.
    //
    // `objects` by reference, never rebuilt into a `Vec` of pairs: it holds one
    // verified manifest body per distinct `content` digest for the whole
    // package, and copying it to change container shape would double the one
    // buffer that sits outside C-026's `max_blobs × largest_blob` ceiling —
    // for a callee that only iterates it.
    let as_name = target.as_name;
    index_write::write_dispatch_objects(store, as_name, target.name, objects).await?;

    // `Value` end to end so forward-compat fields ride through both sides
    // (C-047), then one key mutated (C-028).
    let source_value: serde_json::Value = serde_json::from_slice(root_bytes)
        .map_err(|error| MirrorError::ExecutionFailed(vec![format!("source root is not valid JSON: {error}")]))?;
    let merged = index_write::merge_root_tags(&source_value, destination_root, confirmed)?;
    // `None` is `rewrite_pointers: false`, and the serialized merge *is* the
    // document to publish: `merge_root_tags` works over `Value` end to end, so
    // the source's own `repository` has already ridden through verbatim
    // alongside every forward-compat field. Preserving is the absence of the
    // mutation, never a second mutation writing the upstream value back.
    let serialized = serialize_root(&merged);
    let rewritten = match target.pointer {
        Some(pointer) => index_write::rewrite_root(&serialized, pointer)?,
        None => serialized,
    };

    // Opened only now: upstream's contract is that every network round trip
    // happens before the catalog lock is taken, and the transaction is per
    // package so each becomes visible atomically on its own.
    let mut transaction = store.begin_catalog_transaction(as_name).await.map_err(|error| {
        MirrorError::IndexWriteError(format!("cannot open the catalog transaction for '{as_name}': {error}"))
    })?;
    index_write::write_root(&mut transaction, target.name, &rewritten).await?;
    transaction.commit().await.map_err(|error| {
        MirrorError::IndexWriteError(format!("cannot commit the catalog for source '{as_name}': {error}"))
    })?;

    // After the commit, never before: this re-acquires the same source lock
    // rather than inheriting the transaction's, so the inverted order
    // self-deadlocks (C-031). Write-if-absent, so the per-package call is a
    // lock plus a stat after the source's first package.
    index_write::write_config_json(store, as_name).await
}

/// The ocx read seam for one source's registry (C-022's manifest fetches).
///
/// Built with a bare [`ClientBuilder`] rather than through `registry_client`,
/// because `registry_client` installs the `OCX_MIRRORS` map via `.mirrors(…)`:
/// a client-side mirror rewrite
/// would dial a host other than the one [`catalog::validate_root_host`] just
/// approved, which is the SSRF check defeated by configuration. (Before ocx
/// v0.6.0 the same map arrived via `ClientBuilder::from_env`, now deleted —
/// the hazard moved call sites, it did not go away.) The guard
/// itself is `ssrf_guard` — resolve → validate → **pin** — so the approved
/// address cannot rebind before the socket opens either.
fn source_read_seam(trusted_hosts: &[String]) -> Index {
    let client = ClientBuilder::new()
        .plain_http_registries(ocx_lib::env::insecure_registries())
        .ssrf_guard(trusted_hosts.to_vec())
        .build();
    Index::from_remote(OciIndex::new(OciIndexConfig { client }))
}

/// [`PackageStep::Copied`] for a clean pass, [`PackageStep::Failed`] carrying
/// every message otherwise.
fn finish(failures: Vec<String>) -> PackageStep {
    if failures.is_empty() {
        PackageStep::Copied
    } else {
        PackageStep::Failed(failures.join("; "))
    }
}

/// Classify a copy failure (C-040), prefixed with the tag it happened on.
///
/// Branches on [`CopyError::is_whole_run_abort`] — the predicate, never a
/// variant list, which would drift from C-040's table. The inner
/// [`MirrorError`] is propagated **verbatim**: re-wrapping it would change the
/// exit code of an abort that is not a destination-read failure.
///
/// **The tag is `{:?}`-formatted, never bare** (CWE-117). It comes straight out
/// of an upstream root's `tags{}` and passes no charset guard, and this string
/// is the one that becomes [`PackageReport::detail`] — which an operator reads
/// twice over, once in a CI log and once through `print_table` on their own
/// terminal. Escaping here rather than at either rendering site is what covers
/// both, and it stays necessary after the tag grammar is validated upstream:
/// a rejected tag's rejection message still names it.
///
/// [`PackageReport::detail`]: report::PackageReport::detail
///
/// # Errors
///
/// The whole-run abort class.
fn tag_failure(tag: &str, error: CopyError) -> Result<String, MirrorError> {
    if !error.is_whole_run_abort() {
        return Ok(format!("tag {tag:?}: {error}"));
    }
    match error {
        CopyError::Abort(inner) => Err(inner),
        // `is_whole_run_abort` answered yes, so this arm is reachable only if
        // `CopyError` grows a second aborting variant. Fail closed — the
        // alternative silently demotes a new abort to a package failure.
        other => Err(MirrorError::TargetError(other.to_string())),
    }
}

/// Split an index-write failure into C-040's two classes.
///
/// `index_write` encodes the class in the variant it chooses:
/// [`MirrorError::ExecutionFailed`] is its refusal of **upstream** bytes (a
/// root that is not an object, a `content` pointer that is not an image
/// index), which must fail one package — otherwise any upstream could deny a
/// 121-package mirror by publishing one malformed root. Everything else is
/// local and there is no meaningful way to continue writing a tree that cannot
/// be written.
///
/// # Errors
///
/// The whole-run abort class.
fn write_failure(error: MirrorError) -> Result<String, MirrorError> {
    match error {
        MirrorError::ExecutionFailed(messages) => Ok(messages.join("; ")),
        other => Err(other),
    }
}

/// One report row for a finished package.
fn package_report(name: &str, step: PackageStep) -> PackageReport {
    let (outcome, detail) = match step {
        PackageStep::Copied => (PackageOutcome::Copied, None),
        PackageStep::Skipped => (PackageOutcome::Skipped, None),
        PackageStep::Failed(detail) => (PackageOutcome::Failed, Some(detail)),
    };
    PackageReport {
        name: name.to_string(),
        outcome,
        detail,
    }
}

/// The run counters, derived from the rows rather than incremented alongside
/// them — two tallies of the same thing drift.
fn counters(sources: &[SourceReport]) -> RunCounters {
    let mut counters = RunCounters::default();
    for source in sources {
        for package in &source.packages {
            counters.total += 1;
            match package.outcome {
                PackageOutcome::Copied => counters.copied += 1,
                PackageOutcome::Skipped => counters.skipped += 1,
                PackageOutcome::Failed => counters.failed += 1,
            }
        }
    }
    counters
}

#[cfg(test)]
#[path = "registry_sync/tests.rs"]
mod tests;
