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
//! **Phase 2** copies, per source in spec order, **packages sequentially**.
//! C-026's memory ceiling is `max_blobs × largest_blob` and it holds only
//! because of two things this file owns: one run-scoped `Arc<Semaphore>`, and
//! this loop not being parallel.
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

use ocx_lib::file_structure::IndexStore;
use ocx_lib::oci::index::{CatalogIndex, OciIndex, OciIndexConfig, parse_physical_repository, serialize_root};
use ocx_lib::oci::{Algorithm, ClientBuilder, Digest, Index, Reference};
use tokio::sync::Semaphore;

use self::plan::{PackageWork, SourcePlan};
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

    // Ahead of the pre-flight, not after it: a repaired `c/index.json` is what
    // `should_skip`'s third condition compares against, so repairing later
    // would leave this run re-copying every package the corrupt catalog
    // disagreed about (C-034).
    if options.repair_catalog {
        for source in &spec.sources {
            let outcome = index_write::repair_catalog(&store, source.as_name()).await?;
            tracing::info!(
                "repaired the catalog for source '{}': {} roots, {} added, {} corrected, {} removed",
                outcome.source,
                outcome.roots,
                outcome.added.len(),
                outcome.corrected.len(),
                outcome.removed.len()
            );
        }
    }

    // ── Phase 1 — pre-flight, over ALL sources ──────────────────────────────
    let mut sources = Vec::with_capacity(spec.sources.len());
    for source in &spec.sources {
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
    let locks_dir = cache::locks_dir(&cache_root, &spec.output)?;
    // `with_locks_root` is mandatory and forgetting it is silent — the default
    // puts the lock directory inside the tree the operator serves and commits.
    Ok((cache_root, index_write::build_index_store(&spec.output, &locks_dir)))
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

    let digest_path = cache::digest_file(cache_root, &spec.output, as_name)?;
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

/// Phase 2 (C-044): every source in spec order, every package sequentially.
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
            destination_client: destination_client.clone(),
            blob_semaphore: Arc::clone(&blob_semaphore),
            max_retries: spec.concurrency.max_retries,
            mounted_from: Arc::clone(&mounted_from),
            mount_warnings: Arc::clone(&mount_warnings),
        };

        let mut source_failed = false;
        for package in &prepared.plan.work {
            let step = sync_package(spec, options, prepared, package, store, &context, &mut missing).await?;
            let failed = matches!(step, PackageStep::Failed(_));
            source_failed |= failed;
            source_report.packages.push(package_report(&package.name, step));

            if failed && fail_fast {
                report.sources.push(source_report);
                break 'sources;
            }
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
    index_write::warn_on_pointer_drift(destination_root, &package.name, &package.pointer);

    // Re-parsed rather than threaded out of `validate_root_host`, so that
    // function stays the single SSRF guard on this path instead of doubling as
    // an accessor.
    let (source_registry, source_repository) = parse_physical_repository(&source_root.repository).map_err(|error| {
        MirrorError::SourceError(format!(
            "source root for '{}' has an unusable repository pointer: {error}",
            package.name
        ))
    })?;

    let mut confirmed = BTreeSet::new();
    let mut objects: BTreeMap<Digest, Vec<u8>> = BTreeMap::new();
    let mut failures: Vec<String> = Vec::new();

    for entry in registry_copy::tag_copy_plan(&source_root.tags) {
        let source_reference =
            Reference::with_tag(source_registry.clone(), source_repository.clone(), entry.tag.clone());
        // Tag-only, never digest-carrying: the fork addresses a manifest `PUT`
        // by digest whenever the reference has one, so a digest here would copy
        // the content and create no tag.
        let destination_reference = Reference::with_tag(
            spec.target.registry.clone(),
            package.physical_repository.clone(),
            entry.tag.clone(),
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
            package.physical_repository.clone(),
            DESCRIPTION_TAG.to_string(),
        ),
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
        as_name,
        package,
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
                "copied '{package}' tag '{}': {} manifest(s), {} blob(s) skipped, {} mounted, {} uploaded ({} bytes)",
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

/// The index-tree writes for one package, in C-030's order.
///
/// # Errors
///
/// [`MirrorError::ExecutionFailed`] for a refusal of upstream bytes
/// (aggregating) and [`MirrorError::IndexWriteError`] for a local filesystem
/// failure (whole-run abort) — [`write_failure`] is what tells them apart.
async fn write_package(
    store: &IndexStore,
    as_name: &str,
    package: &PackageWork,
    root_bytes: &[u8],
    destination_root: Option<&[u8]>,
    confirmed: &BTreeSet<String>,
    objects: &BTreeMap<Digest, Vec<u8>>,
) -> Result<(), MirrorError> {
    // First, unconditionally: `root exists ⇒ its dispatch objects exist`
    // (C-033). Content-addressed and idempotent, so the ordering is free.
    let dispatch: Vec<(Digest, Vec<u8>)> = objects
        .iter()
        .map(|(digest, bytes)| (digest.clone(), bytes.clone()))
        .collect();
    index_write::write_dispatch_objects(store, as_name, &package.name, &dispatch).await?;

    // `Value` end to end so forward-compat fields ride through both sides
    // (C-047), then one key mutated (C-028).
    let source_value: serde_json::Value = serde_json::from_slice(root_bytes)
        .map_err(|error| MirrorError::ExecutionFailed(vec![format!("source root is not valid JSON: {error}")]))?;
    let merged = index_write::merge_root_tags(&source_value, destination_root, confirmed)?;
    let rewritten = index_write::rewrite_root(&serialize_root(&merged), &package.pointer)?;

    // Opened only now: upstream's contract is that every network round trip
    // happens before the catalog lock is taken, and the transaction is per
    // package so each becomes visible atomically on its own.
    let mut transaction = store.begin_catalog_transaction(as_name).await.map_err(|error| {
        MirrorError::IndexWriteError(format!("cannot open the catalog transaction for '{as_name}': {error}"))
    })?;
    index_write::write_root(&mut transaction, &package.name, &rewritten).await?;
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
/// Built with a bare [`ClientBuilder`] rather than `from_env`, because
/// `from_env` installs the `OCX_MIRRORS` map: a client-side mirror rewrite
/// would dial a host other than the one [`catalog::validate_root_host`] just
/// approved, which is the SSRF check defeated by configuration. The guard
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
/// # Errors
///
/// The whole-run abort class.
fn tag_failure(tag: &str, error: CopyError) -> Result<String, MirrorError> {
    if !error.is_whole_run_abort() {
        return Ok(format!("tag '{tag}': {error}"));
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
