// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! The pre-flight phase: filter → expand → collide → short-circuit → per-root
//! fallback → work list, plus the dry-run byte estimate.
//!
//! Everything here runs **before a single byte is copied**, over *all* sources,
//! so a collision or an unsupported source format refuses the run instead of
//! being discovered after the first source is fully mirrored. Contracts C-039
//! and C-043 of `plan_registry_mirror_sync.md`.
//!
//! # What the orchestrator does with this
//!
//! ```text
//! for source in &spec.sources {                       // Phase 1, ALL sources
//!     plans.push(plan_source(spec, source, source.as_name(), …)?);
//! }
//! detect_collisions(&plans.iter().flat_map(|p| expansions(&p.as_name, &p.work)).collect::<Vec<_>>())?;
//!
//! for plan in &plans {                                // Phase 2, per source
//!     if plan.short_circuited { continue; }
//!     // per-root fallback: fetch each root, then
//!     //   index_write::should_skip(&package.name, &source_root, local.as_ref(), local_catalog)
//! }
//! ```
//!
//! Two orderings in there are load-bearing:
//!
//! - **Collision detection sees every source's full expansion, including a
//!   source that short-circuited.** A short-circuited source is one already
//!   mirrored to its repositories; dropping it from the check lets a second
//!   source silently overwrite packages a previous run published. That is why
//!   [`SourcePlan::work`] stays the whole expanded set and
//!   [`SourcePlan::short_circuited`] is a separate flag rather than an empty
//!   list.
//! - **The per-root fallback is [`super::index_write::should_skip`], not a
//!   second predicate.** C-039 and C-032 are one comparison in the design, and
//!   the pairs-not-key-sets rule has to hold in both or a re-pointed tag is
//!   skipped forever.

use std::collections::BTreeSet;

use ocx_lib::oci::Digest;
use ocx_lib::oci::index::CatalogIndex;
use sha2::{Digest as _, Sha256};

use super::destination::{DestinationTemplate, Expansion, physical_repository, wire_pointer};
use super::glob::{Glob, package_selected};
use crate::error::MirrorError;
use crate::spec::{RegistrySource, RegistrySpec};

/// One package this run intends to copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageWork {
    /// The catalog key — the **logical** `<ns>/<pkg>` name, never the physical
    /// repository.
    pub name: String,
    /// Where its content lands: `target.repository` + the expanded template,
    /// validated and contained.
    pub physical_repository: String,
    /// The `oci://` pointer written into the mirrored root.
    pub pointer: String,
}

/// One source's pre-flight outcome.
#[derive(Debug, Clone)]
pub struct SourcePlan {
    /// The source's `as:` value — its output subtree and `{registry}`
    /// expansion.
    pub as_name: String,
    /// The exact bytes to hand [`super::cache::record_digest`] after a fully
    /// successful run — the source catalog's digest **and** a fingerprint of
    /// the mirrored catalog this run read.
    ///
    /// Opaque on purpose: nothing outside [`short_circuit`] takes it apart, and
    /// the two halves are only ever compared as a unit.
    pub cache_record: String,
    /// **Every** package this source's filters selected, in catalog order —
    /// not only the ones needing a copy.
    ///
    /// It stays the full set even when [`Self::short_circuited`] fired, because
    /// collision detection runs over it: a short-circuited source is one whose
    /// packages are *already* mirrored to these repositories, so omitting them
    /// would let another source claim the same repository unnoticed.
    pub work: Vec<PackageWork>,
    /// Whether the short-circuit fired — reported so a no-op run is never
    /// silent about *why* it did nothing, and consulted by the copy phase to
    /// skip this source's per-root pass entirely.
    pub short_circuited: bool,
}

/// Pre-flight one source: filter, expand, and evaluate the short-circuit
/// (C-039).
///
/// `as_name` is the source's resolved `as:` value. It is threaded rather than
/// read back off `source` so the `as:` ▸ `registry` fallback has exactly one
/// implementation ([`RegistrySource::as_name`]) and one lookup per source; it
/// is the `{registry}` expansion, the output subtree, and
/// [`Expansion::source`], and all three must be the same string.
///
/// Network work belongs to the caller: this is pure over an already-fetched
/// catalog, so the whole pre-flight can run for every source before a byte
/// moves.
///
/// # Errors
///
/// [`MirrorError::SpecInvalid`] (exit 65) — see [`expand_source`].
pub fn plan_source(
    spec: &RegistrySpec,
    source: &RegistrySource,
    as_name: &str,
    catalog: &CatalogIndex,
    catalog_digest: &str,
    recorded_digest: Option<&str>,
    local_catalog: &CatalogIndex,
) -> Result<SourcePlan, MirrorError> {
    let work = expand_source(spec, source, as_name, catalog)?;
    // The short-circuit's second condition is over the *filtered* set, which is
    // exactly what expansion just produced — deriving it here is what stops the
    // two from drifting apart.
    let filtered: BTreeSet<String> = work.iter().map(|package| package.name.clone()).collect();
    Ok(SourcePlan {
        as_name: as_name.to_string(),
        // ponytail: fingerprints the catalog as pre-flight FOUND it, not as the
        // copy phase leaves it, so the short-circuit starts firing one run after
        // a change rather than immediately. Steady state — the case the no-op
        // NFR is about — is unaffected: a run that changes nothing records what
        // it read, and the next run matches. Recording the post-run catalog
        // would close the one-run lag, at the cost of the orchestrator re-reading
        // `c/index.json` after the copy phase; not worth a cross-module read for
        // one extra comparison pass per upstream change.
        cache_record: cache_record(catalog_digest, local_catalog),
        short_circuited: short_circuit(catalog_digest, recorded_digest, &filtered, local_catalog),
        work,
    })
}

/// Filter and expand one source's catalog into its destination set.
///
/// Applies the source's `include:`/`exclude:` globs, expands the
/// `destination:` template per surviving key, validates and contains each
/// composed repository, and composes each `oci://` pointer. Runs for every
/// source **before** any copy, so the union can be collision-checked.
///
/// Catalog order is the `CatalogIndex` map's own key order, so a run's work
/// list is deterministic without sorting anything.
///
/// # Errors
///
/// [`MirrorError::SpecInvalid`] (exit 65) for a catalog key that fails the OCI
/// repository grammar, escapes the configured prefix, or is not
/// `<namespace>/<package>` — and for an `include:`/`exclude:` pattern outside
/// the glob grammar, which `RegistrySpec::validate` should already have
/// refused at load time.
//
// ponytail: the first bad key ends the expansion rather than every bad key
// being collected. The remedy is an `exclude:` glob and the message names the
// offending key; aggregate them if a real catalog ever produces more than one.
pub fn expand_source(
    spec: &RegistrySpec,
    source: &RegistrySource,
    as_name: &str,
    catalog: &CatalogIndex,
) -> Result<Vec<PackageWork>, MirrorError> {
    let template = DestinationTemplate::parse(&spec.destination).map_err(spec_invalid)?;
    let include = compile_globs(&source.include)?;
    let exclude = compile_globs(&source.exclude)?;

    let mut work = Vec::new();
    for name in catalog.keys() {
        if !package_selected(name, &include, &exclude) {
            continue;
        }
        let expanded = template.expand(as_name, name).map_err(spec_invalid)?;
        let repository = physical_repository(&spec.target, &expanded)?;
        let pointer = wire_pointer(&spec.target, &repository)?;
        work.push(PackageWork {
            name: name.clone(),
            physical_repository: repository,
            pointer,
        });
    }
    Ok(work)
}

/// One [`Expansion`] per unit of package work, for whole-run collision
/// detection (C-015).
///
/// `as_name` is a **required** parameter rather than something the caller may
/// fill in later: `source` participates in C-015's distinctness comparison, so
/// an `Expansion` built without it reads two sources publishing the same
/// catalog key as one package listed twice — no collision is reported and the
/// second source silently overwrites the first.
pub fn expansions(as_name: &str, work: &[PackageWork]) -> Vec<Expansion> {
    work.iter()
        .map(|package| Expansion {
            source: as_name.to_string(),
            catalog_key: package.name.clone(),
            repository: package.physical_repository.clone(),
        })
        .collect()
}

/// Whether this source's whole pass can be skipped (C-039).
///
/// Fires iff **all three**:
///
/// - `sha256(source catalog bytes)` equals the value the previous **fully
///   successful** run recorded; **and**
/// - the mirrored `c/index.json` is byte-for-byte the map that run left
///   behind; **and**
/// - the filtered package-name set is a **subset (`⊆`) of** the local
///   `c/index.json` key set.
///
/// **The middle condition is the one that took a defect to find.** The subset
/// test asks whether each key is *present*; it says nothing about whether the
/// key's **value** is still right. An entry corrupted in place — key intact,
/// digest wrong — satisfies both of the other conditions, so the whole source
/// was skipped in one request and the per-package predicate that compares each
/// entry against `sha256(root bytes)` (C-032 condition 3) never ran. The
/// mirror reported "unchanged since the last run" and the drifted entry stayed
/// drifted forever, defeating the auto-repair the owner asked for.
///
/// Verifying entries against their roots would mean reading every root from
/// disk, which is exactly the O(n) the short-circuit exists to avoid. So the
/// check is on the map's own fingerprint instead: the previously unstated
/// assumption "our local catalog is intact" becomes an O(1) comparison against
/// what the last successful run recorded, with no extra requests and no
/// per-root reads. A catalog edited by anything other than this tool falls
/// through to the per-package pass, which already repairs correctly.
///
/// `⊆`, not `==`: the mirror is append-only, so the local catalog only ever
/// grows. Under `==` the first `include:` narrowing makes the sets unequal
/// **forever**, silently degrading the headline no-op behaviour from one
/// request to one per package every run.
///
/// The second condition costs zero extra requests — both sets are already in
/// memory — and closes the widened-`include:` blind spot: the source catalog is
/// unchanged, so a digest-only comparison reports zero deltas and never copies
/// the newly-requested packages.
///
/// When it does not fire, the fallback is the per-root pass, comparing each
/// filtered package's `tags{}` **key→digest pairs** — the *same* predicate
/// [`super::index_write::should_skip`] uses. One comparison in the design, not
/// two.
pub fn short_circuit(
    source_catalog_digest: &str,
    recorded_digest: Option<&str>,
    filtered: &BTreeSet<String>,
    local_catalog: &CatalogIndex,
) -> bool {
    recorded_digest == Some(cache_record(source_catalog_digest, local_catalog).as_str())
        && filtered.iter().all(|name| local_catalog.contains_key(name))
}

/// What a fully successful run records, and what [`short_circuit`] rebuilds to
/// compare against: the source catalog's digest and the mirrored catalog's
/// fingerprint, in one opaque token.
///
/// One string rather than two fields so [`super::cache`] needs no format of its
/// own — it stores whatever it is handed, and a record written by an older
/// build simply fails to match, which C-039 already defines as harmless (one
/// extra comparison pass, never a correctness loss).
fn cache_record(source_catalog_digest: &str, local_catalog: &CatalogIndex) -> String {
    format!("{source_catalog_digest} {}", catalog_fingerprint(local_catalog))
}

/// A fingerprint of the mirrored catalog's **contents**, keys and values both.
///
/// Over the parsed map rather than the file's bytes, which makes it immune to a
/// cosmetic reformat that changes no entry — the roots are what the catalog
/// describes, and a re-indented file describes the same ones. `CatalogIndex` is
/// a `BTreeMap`, so iteration is sorted and the fingerprint is stable across
/// runs without sorting anything here.
///
/// The `\0` separators matter: without them `{"a": "bc"}` and `{"ab": "c"}`
/// hash identically.
fn catalog_fingerprint(local_catalog: &CatalogIndex) -> String {
    let mut hasher = Sha256::new();
    for (name, entry) in local_catalog {
        hasher.update(name.as_bytes());
        hasher.update([0]);
        hasher.update(entry.as_bytes());
        hasher.update([0]);
    }
    hex::encode(hasher.finalize())
}

/// The exact byte count a real run would transfer, for `--dry-run` (C-043).
///
/// The sum of the `size` field of every descriptor whose destination HEAD
/// **missed**, counting each distinct digest once. Descriptor `size` is
/// authoritative in an OCI manifest, so this is an exact figure, not a
/// heuristic.
///
/// De-duplication is by digest, so a blob shared by six platform manifests is
/// counted once — the map keeps the last size seen for a digest, and a digest
/// is content, so two entries for it cannot disagree.
pub fn dry_run_byte_estimate(missing: &[(Digest, u64)]) -> u64 {
    missing
        .iter()
        .map(|(digest, size)| (digest, size))
        .collect::<std::collections::BTreeMap<_, _>>()
        .values()
        .copied()
        .sum()
}

/// Compile one source's `include:` or `exclude:` list.
fn compile_globs(patterns: &[String]) -> Result<Vec<Glob>, MirrorError> {
    patterns
        .iter()
        .map(|pattern| Glob::compile(pattern).map_err(spec_invalid))
        .collect()
}

/// A plan-time refusal (exit 65). Plan-time because nothing has been copied
/// yet, so refusing costs the operator nothing but a re-run.
fn spec_invalid(error: impl std::fmt::Display) -> MirrorError {
    MirrorError::SpecInvalid(vec![error.to_string()])
}

#[cfg(test)]
#[path = "plan/tests.rs"]
mod tests;
