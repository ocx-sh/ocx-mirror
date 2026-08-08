// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Detecting published tiles whose metadata no longer matches what this build
//! would render.
//!
//! The config digest settles it without fetching a blob whenever the tile
//! carries a complete `bin_scan` claim; an incomplete one has to be compared
//! field by field, because the scan result is not recomputable from the spec.

use super::*;

/// Compares every published version's recorded metadata against what the spec
/// would produce today, and emits a `metadata-drift` entry per version that no
/// longer matches.
///
/// This is what makes a metadata fix retroactive: correct `metadata.json` in
/// the mirror repo, and the next cron run re-publishes the affected config
/// blobs against the layers already in the registry. Nothing is deleted and no
/// asset is downloaded.
///
/// A version already scheduled as `new` or `backfill-partial` is skipped — its
/// push writes current metadata anyway for the platforms it touches, and a
/// second entry under the same version string would collide in the workflow's
/// version matrix. A backfilled version whose *existing* platforms drifted is
/// picked up on the following run, once it is fully published.
pub async fn detect_metadata_drift(
    publisher: &Publisher,
    identifier: &Identifier,
    spec: &MirrorSpec,
    spec_dir: &Path,
    all_tags: &[String],
    scheduled: &[PlanVersionEntry],
) -> Result<Vec<PlanVersionEntry>, MirrorError> {
    let scheduled: HashSet<&str> = scheduled.iter().map(|entry| entry.version.as_str()).collect();

    // Tags worth scanning: a leaf, not already scheduled, and belonging to a
    // variant the spec still declares metadata for. A spec that declares none
    // cannot publish at all, so there is nothing to compare against.
    let leaves = leaf_versions(all_tags);
    let candidates: Vec<(Version, String, MetadataPlan)> = leaves
        .iter()
        .filter(|(_, tag)| !scheduled.contains(tag.as_str()))
        .filter_map(|(version, tag)| {
            let plan = orchestrator::metadata_plan_for(spec, version)?;
            Some((version.clone(), tag.clone(), plan))
        })
        .collect();

    // Read every candidate tag's child manifests concurrently — this is the
    // bulk of the scan, and it is all latency.
    let images: Vec<PublishedImage> = stream::iter(candidates.iter().map(|(_, tag, _)| tag.clone()))
        .map(|tag| async move { target_registry::fetch_published_images(publisher, identifier, &[tag.as_str()]).await })
        .buffer_unordered(DRIFT_SCAN_CONCURRENCY)
        .try_concat()
        .await?;
    log::info!(
        "Scanned {} published (version, platform) tiles across {} tags for metadata drift",
        images.len(),
        candidates.len(),
    );

    // The expectation depends on the variant and the platform, never on the
    // version — one spec file serves every version of a variant. Memoized so a
    // thousand-version mirror reads those files once per `(variant, platform)`
    // instead of once per published tile.
    let plans: HashMap<&Version, &MetadataPlan> = candidates.iter().map(|(version, _, plan)| (version, plan)).collect();
    let mut expected_metadata: HashMap<(Option<String>, Platform), ExpectedMetadata> = HashMap::new();
    let mut suspects: Vec<(PublishedImage, ExpectedMetadata, BinScanMode)> = Vec::new();

    for image in images {
        let Some(plan) = plans.get(&image.version) else {
            continue;
        };
        let key = (image.version.variant().map(str::to_string), image.platform.clone());
        let expected = match expected_metadata.entry(key) {
            Entry::Occupied(cached) => cached.into_mut(),
            Entry::Vacant(slot) => {
                let resolved =
                    orchestrator::expected_metadata(&plan.config, &image.platform, spec_dir).map_err(|error| {
                        MirrorError::SpecInvalid(vec![format!(
                            "failed to resolve the metadata {} would publish for {}: {error:#}",
                            image.version, image.platform
                        )])
                    })?;
                slot.insert(resolved)
            }
        };
        // Local and free, and the answer for every tile of a healthy mirror —
        // used here only to avoid spawning a network task per clean tile.
        // `image_drifted` below re-runs it as part of the actual decision.
        if !settled_by_digest(&image, &expected.published, plan.bin_scan)? {
            let expected = expected.clone();
            suspects.push((image, expected, plan.bin_scan));
        }
    }

    // Only the tiles the digest could not clear read their config blob, and that
    // read is concurrent too: when every published version predates a metadata
    // field — the case this whole feature exists for — every tile falls through
    // to here, so a sequential phase would hand back exactly the cost the
    // concurrent scan above removes.
    log::info!("{} tiles need a config-blob read to settle", suspects.len());
    let drifted: Vec<Option<(Version, String)>> = stream::iter(suspects)
        .map(|(image, expected, bin_scan)| async move {
            let drifted = image_drift(publisher, identifier, &image, &expected, bin_scan)
                .await?
                .is_some();
            Ok::<_, MirrorError>(drifted.then(|| (image.version, image.platform.to_string())))
        })
        .buffer_unordered(DRIFT_SCAN_CONCURRENCY)
        .try_collect()
        .await?;

    // Regroup: `buffer_unordered` yields out of order, and the plan is
    // oldest-first (BTreeMap keys are Version-ordered).
    let mut by_version: BTreeMap<Version, Vec<String>> = BTreeMap::new();
    for (version, platform) in drifted.into_iter().flatten() {
        by_version.entry(version).or_default().push(platform);
    }

    Ok(by_version
        .into_iter()
        .filter_map(|(version, mut platforms)| {
            platforms.sort();
            let tag = leaves.get(&version)?;
            Some(drift_entry(tag, version.variant(), platforms))
        })
        .collect())
}

/// The metadata a fresh publish would write for `image`, or `None` when the
/// registry already records exactly that.
///
/// The config digest settles the common case without fetching the blob: equal
/// bytes are necessarily equal values. The converse does **not** hold — the
/// published bytes are whatever the `ocx` that wrote them serialized, and a
/// change in JSON key order alone would make every digest differ — so a digest
/// mismatch only promotes the pair to a value comparison. Reporting drift off
/// the digest would republish the entire fleet the first time serialization
/// moved.
///
/// Returns the expectation rather than a bare `bool` because under `bin_scan`
/// the expectation the caller passed in is not the one the comparison ran
/// against: it is missing `binaries`, and the answer is only meaningful after
/// the published claim has been carried into it. Handing that adopted
/// expectation back is what stops `pipeline patch` republishing against the
/// unadopted one and deleting the claim.
pub async fn image_drift(
    publisher: &Publisher,
    identifier: &Identifier,
    image: &PublishedImage,
    expected: &ExpectedMetadata,
    bin_scan: BinScanMode,
) -> Result<Option<ExpectedMetadata>, MirrorError> {
    if settled_by_digest(image, &expected.published, bin_scan)? {
        return Ok(None);
    }

    let published = target_registry::fetch_published_metadata(publisher, identifier, image).await?;
    let expected = match bin_scan.scans() {
        true => expected
            .adopting_binaries_from(&published, &image.platform)
            .map_err(|error| {
                MirrorError::SpecInvalid(vec![format!(
                    "failed to carry the published binaries of {} ({}) into the expected metadata: {error:#}",
                    image.version, image.platform
                )])
            })?,
        false => expected.clone(),
    };
    Ok(metadata_drifted(&published, &expected.published)?.then_some(expected))
}

/// Whether the config digest alone proves `image` is current.
///
/// Only an *incomplete* expectation has to skip it. A `bin_scan` whose spec
/// declares no `binaries` cannot compute the claim the registry records, so its
/// digest can never match a correctly published tile and trusting it would
/// report drift on every one of them; those go straight to the blob read that
/// adopts the published claim. A spec that declares `binaries` computes the
/// whole document download-free, scanning or not, and skipping the digest there
/// buys nothing: it costs a config-blob GET per tile per run, and turns one
/// unparseable published blob into a `TargetError` that aborts the discover job.
pub fn settled_by_digest(
    image: &PublishedImage,
    expected: &Metadata,
    bin_scan: BinScanMode,
) -> Result<bool, MirrorError> {
    let complete = !bin_scan.scans() || expected.binaries().is_some();
    Ok(complete && config_bytes_match(image, expected)?)
}

/// Whether the config blob the registry recorded is byte-identical to the one a
/// fresh publish would write, decided from the descriptor's digest alone.
///
/// `true` proves there is no drift, with no blob fetch. `false` proves nothing:
/// the published bytes are whatever the `ocx` that wrote them serialized, so a
/// key-order change alone flips this while the value is unchanged. The caller
/// must fall back to [`metadata_drifted`] rather than report drift here.
///
/// The push pipeline's backfill cascade repair asks the same question for the
/// opposite reason: it re-pushes a published tile only to move the rolling tags,
/// so it needs the re-push to be manifest-identical, and `false` — "this build
/// would write different bytes" — is exactly the condition under which it must
/// not run.
pub fn config_bytes_match(image: &PublishedImage, expected: &Metadata) -> Result<bool, MirrorError> {
    let bytes = serde_json::to_vec(expected)
        .map_err(|error| MirrorError::ExecutionFailed(vec![format!("failed to serialize metadata: {error}")]))?;
    Ok(image.config.digest == Algorithm::Sha256.hash(&bytes).to_string())
}

/// Compares two metadata documents by value.
///
/// Both sides are re-serialized from the deserialized form, so key order and
/// whitespace of the published bytes cannot register as a difference. A byte
/// comparison here would report drift for every version published by a
/// different `ocx` and republish the whole mirror on the next cron run.
pub fn metadata_drifted(published: &Metadata, expected: &Metadata) -> Result<bool, MirrorError> {
    let serialize = |metadata: &Metadata| {
        serde_json::to_value(metadata)
            .map_err(|error| MirrorError::ExecutionFailed(vec![format!("failed to serialize metadata: {error}")]))
    };
    Ok(serialize(published)? != serialize(expected)?)
}

/// Published version tags that are not a cascade alias of another published
/// tag, keyed by version so the result is oldest-first.
///
/// `3.29.0_20260610` cascades to `3.29.0`, `3.29`, and `3` — four tags over one
/// set of child manifests. Scanning the aliases too would schedule the same
/// patch up to four times; patching the leaf re-cascades them anyway.
pub fn leaf_versions(all_tags: &[String]) -> BTreeMap<Version, String> {
    let mut published: BTreeMap<Version, String> = BTreeMap::new();
    for tag in all_tags {
        if let Some(version) = Version::parse(tag) {
            published.insert(version, tag.clone());
        }
    }

    let aliases: HashSet<Version> = published
        .keys()
        .flat_map(|version| std::iter::successors(version.parent(), Version::parent))
        .collect();
    published.retain(|version, _| !aliases.contains(version));
    published
}

/// A `metadata-drift` plan entry for an already-published tag.
///
/// `source_version` and `assets` stay empty by construction: a metadata patch
/// re-references the published layers by digest and never touches the upstream
/// source, and a normalized tag cannot be reversed into the upstream version
/// string it was built from.
pub fn drift_entry(tag: &str, variant: Option<&str>, platforms: Vec<String>) -> PlanVersionEntry {
    PlanVersionEntry {
        version: tag.to_string(),
        platforms,
        kind: PlanVersionKind::MetadataDrift,
        source_version: String::new(),
        variant: variant.map(str::to_string),
        assets: Vec::new(),
        pylock: None,
    }
}
