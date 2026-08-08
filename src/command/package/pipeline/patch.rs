// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `ocx-mirror package pipeline patch` — correct the metadata of versions the
//! registry already holds, without re-downloading or re-uploading anything.
//!
//! Package metadata lives in the OCI **config blob**, never in a layer. So
//! correcting it is a manifest re-emission that re-references the published
//! layers by digest: the only bytes uploaded are a config blob the size of a
//! `metadata.json`. The alternative — deleting the tags and re-mirroring — costs
//! hours of upstream download and orphans everyone pinned to `@sha256:`.
//!
//! Only leaf tags are scanned. `3.29.0_20260610` cascades to `3.29.0`, `3.29`
//! and `3` over one set of child manifests, so patching the leaf re-cascades the
//! aliases; scanning them too would schedule the same work four times.
//!
//! Idempotent by construction: a `(version, platform)` whose published config
//! already records what the spec would publish today is skipped. There is no
//! ledger and no stored range — the comparison against the registry is the
//! entire mechanism.
//!
//! A run that changed anything ends by announcing, because re-emitting a
//! manifest changes its digest and leaves the index root pointing at the digests
//! it replaced. A patched mirror whose index is stale is worse than an unpatched
//! one, so a failed announce fails the command — but an *absent*
//! `OCX_ANNOUNCE_TOKEN` does not: a repository without the secret is a valid
//! configuration, and it degrades to a notice exactly as `pipeline push` does.
//!
//! # Errors
//!
//! - [`MirrorError::SpecNotFound`] / [`MirrorError::SpecInvalid`] from
//!   `load_spec`, and `SpecInvalid` when the spec's metadata no longer resolves.
//! - [`MirrorError::SpecUsageError`] (exit 64) for an unparseable version bound
//!   or a `--version` the target registry does not publish.
//! - [`MirrorError::TargetError`] (exit 69) from the fail-safe registry reads.
//! - [`MirrorError::ExecutionFailed`] when a re-push or the closing announce
//!   fails.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use ocx_lib::cli::DataInterface;
use ocx_lib::log;
use ocx_lib::oci::{ClientBuilder, Descriptor, Identifier};
use ocx_lib::package::version::Version;
use ocx_lib::publisher::{ArchiveMediaType, Publisher};

use crate::command::package::pipeline::announce;
use crate::command::package::pipeline::plan::{image_drift, leaf_versions};
use crate::error::MirrorError;
use crate::pipeline::ocx_cli::announce::{
    ANNOUNCE_TIMEOUT, ENV_ANNOUNCE_TOKEN, TagSource, announce_token, invoke_announce,
};
use crate::pipeline::ocx_cli::push::{PUSH_TIMEOUT, build_push_args, push_once};
use crate::pipeline::ocx_cli::resolve_ocx_binary;
use crate::pipeline::orchestrator;
use crate::pipeline::target_registry::{self, PublishedImage};
use crate::spec::{self, MirrorSpec, strip_build};

/// `ocx-mirror package pipeline patch` subcommand.
#[derive(clap::Parser)]
pub struct Patch {
    /// Path to the mirror spec file.
    #[arg(long, default_value = "./mirror.yml")]
    pub spec: PathBuf,

    /// Republish the metadata blob and nothing else.
    ///
    /// Required, and currently the only mode: every published layer is
    /// re-referenced by digest, so no upstream asset is fetched and no layer is
    /// re-uploaded. Naming the mode keeps the grammar stable if a patch ever
    /// learns to rewrite something other than metadata.
    #[arg(long, required = true)]
    pub metadata_only: bool,

    /// Patch this published version. Repeatable, and composes with the range
    /// flags — the run patches the union.
    ///
    /// Matches the leaf tag, either verbatim or by its version core: on a
    /// build-stamped mirror `--version 3.29.0` selects `3.29.0_20260610`. A
    /// version the registry does not publish is an error, not a silent no-op.
    #[arg(long = "version", value_name = "VERSION")]
    pub versions: Vec<String>,

    /// Lowest published version to patch, inclusive.
    ///
    /// Compared on the version core, so a bound covers every build stamp of the
    /// version it names. Variant tags sort apart from default ones, so a bound
    /// must carry the variant prefix (`--min-version slim-3.13.0`) to select
    /// that variant's tags. Omit for an open lower end.
    #[arg(long, value_name = "VERSION")]
    pub min_version: Option<String>,

    /// Highest published version to patch, exclusive.
    ///
    /// Omit for an open upper end. Omit both bounds and pass no `--version` to
    /// patch every published version.
    #[arg(long, value_name = "VERSION")]
    pub max_version: Option<String>,
}

impl Patch {
    pub async fn execute(&self, _printer: &DataInterface) -> Result<(), MirrorError> {
        let spec = spec::load_spec(&self.spec).await?;
        let spec_dir = self.spec.parent().unwrap_or(Path::new("."));

        let client = ClientBuilder::from_env().map_err(|e| MirrorError::ExecutionFailed(vec![e.to_string()]))?;
        let publisher = Publisher::new(client);
        let identifier = Identifier::new_registry(&spec.target.repository, &spec.target.registry);

        // Fail-safe (issue #157): a failed tag list must not read as "nothing
        // published", which here would silently patch nothing at all.
        let all_tags = target_registry::list_target_tags(&publisher, &identifier).await?;
        let selection = Selection::parse(&self.versions, self.min_version.as_deref(), self.max_version.as_deref())?;
        let selected = selection.apply(&all_tags)?;

        if selected.is_empty() {
            log::info!("[patch] {} — no published version matched the selection", spec.name);
            return Ok(());
        }

        let ocx_binary = resolve_ocx_binary().map_err(|e| MirrorError::ExecutionFailed(vec![e]))?;
        let annotations = crate::annotations::build_annotations(&spec.annotations);
        // Sidecars go under the pipeline's own work dir rather than the shared
        // system temp dir, for the reason `pipeline announce` gives: a
        // predictable `/tmp/<fixed-name>` is a path any other local user can
        // pre-create as a symlink.
        let work_dir = PathBuf::from(".ocx-mirror").join(format!("patch-{}", std::process::id()));

        let mut republished = 0usize;
        let mut current = 0usize;
        let mut failures: Vec<String> = Vec::new();

        for (version, tag) in &selected {
            // A tag whose variant the spec no longer declares has nothing to be
            // compared against, and a spec that declares no metadata at all
            // cannot publish in the first place.
            let Some(plan) = orchestrator::metadata_plan_for(&spec, version) else {
                log::debug!("[patch] {tag} — the spec declares no metadata for this variant");
                continue;
            };

            let images = target_registry::fetch_published_images(&publisher, &identifier, &[tag.as_str()]).await?;
            for image in &images {
                let expected =
                    orchestrator::expected_metadata(&plan.config, &image.platform, spec_dir).map_err(|error| {
                        MirrorError::SpecInvalid(vec![format!(
                            "failed to resolve the metadata {tag} would publish for {}: {error:#}",
                            image.platform
                        )])
                    })?;

                // The expectation this republishes against is the one the drift
                // comparison settled on, never the one resolved above: under a
                // `bin_scan` they differ by the `binaries` claim, which patch
                // cannot recompute because it never downloads. Pushing the
                // unadopted sidecar would delete a correct published claim on
                // the first unrelated metadata fix.
                let Some(expected) = image_drift(&publisher, &identifier, image, &expected, plan.bin_scan).await?
                else {
                    current += 1;
                    log::debug!("[patch] {tag} ({}) — already current", image.platform);
                    continue;
                };

                if let Err(refusal) = layout_unchanged(&publisher, &identifier, image, &expected.published).await? {
                    failures.push(format!("{tag} ({}): {refusal}", image.platform));
                    continue;
                }

                let sidecar = work_dir.join(format!("{tag}-{}-metadata.json", spec::platform_slug(&image.platform)));
                let outcome = republish(
                    &spec,
                    tag,
                    image,
                    &expected.sidecar_json,
                    &sidecar,
                    &annotations,
                    &ocx_binary,
                )
                .await;

                match outcome {
                    Ok(()) => {
                        republished += 1;
                        log::info!("[patch] {tag} ({}) — metadata republished", image.platform);
                    }
                    Err(error) => failures.push(format!("{tag} ({}): {error}", image.platform)),
                }
            }
        }

        let _ = tokio::fs::remove_dir_all(&work_dir).await;

        log::info!(
            "[patch] {} — {republished} manifest(s) republished, {current} already current",
            spec.name,
        );

        // Announce whatever landed, even alongside failures: the manifests that
        // WERE re-emitted are live under digests the index does not know, and
        // leaving them unannounced is the stale-index state this chain exists to
        // prevent. An absent `announce:` block means there is no index package
        // to announce into, which is not a failure.
        //
        // No `OCX_ANNOUNCE_TOKEN` is a valid configuration — forks and test
        // repositories — and degrades exactly as the push job's announce does:
        // recorded, not fatal. Failing here would red a run whose manifests
        // already landed, over an announce that was never attempted.
        if republished > 0
            && let Some(config) = spec.announce.as_ref()
        {
            if announce_token().is_none() {
                println!(
                    "::notice title=Index announce skipped::No {ENV_ANNOUNCE_TOKEN} secret — \
                     {} republished {republished} manifest(s) but the index was not updated.",
                    config.package,
                );
            }
            // `--tags-from-registry`, the same path `pipeline announce` drives:
            // a cascade re-points aliases the patch never named, so the tag set
            // to re-observe is the repository's, not the run's.
            else {
                match invoke_announce(config, &TagSource::FromRegistry, None, &ocx_binary, ANNOUNCE_TIMEOUT).await {
                    // Always a real run (`out: None`), and reported through the
                    // same formatter `pipeline announce` uses — a patch-driven
                    // announce that curated nothing must not read differently
                    // from one that did just because a different command drove it.
                    Ok(report) => {
                        let target = format!("{}/{}", spec.target.registry, spec.target.repository);
                        announce::log_report(false, &report, config, &target);
                    }
                    Err(error) => failures.push(format!(
                        "index announce for {} failed: {error} — {republished} republished manifest(s) are live \
                         and the index still points at the digests they replaced",
                        config.package,
                    )),
                }
            }
        }

        if !failures.is_empty() {
            return Err(MirrorError::ExecutionFailed(failures));
        }
        Ok(())
    }
}

/// Whether `expected` still describes the layers `image` already published.
///
/// `strip_components` is the whole mapping from a layer's archive paths onto
/// `${installPath}`, and it is fixed when the layer is *built*. Patch never
/// downloads: it re-references those exact layers by digest, so republishing
/// metadata that strips differently describes a tree the layer does not contain
/// — every `${installPath}`-rooted PATH entry would name a directory absent
/// from the bundle, and the package would install broken while its digest still
/// verifies.
///
/// This is reachable in production, not theoretical: `mirror-astral-sh`
/// published windows/amd64 as `strip_components: 1` with `PATH=${installPath}`,
/// then had to move to `strip_components: 0` with `PATH=${installPath}/python`
/// because the `bin_scan` load gate rejects the bare form. Every leaf tag reads
/// as drifted, and patching them would have shipped exactly that broken state.
///
/// ponytail: compares the one field that is decidable without the layer bytes.
/// A PATH change under an unchanged `strip_components` is still unverified —
/// deciding that needs the layer listing, which is the download patch exists to
/// avoid. Upgrade to a blob listing if a mirror ever hits that shape.
async fn layout_unchanged(
    publisher: &Publisher,
    identifier: &Identifier,
    image: &PublishedImage,
    expected: &ocx_lib::package::metadata::Metadata,
) -> Result<Result<(), String>, MirrorError> {
    let published = target_registry::fetch_published_metadata(publisher, identifier, image).await?;
    Ok(match layout_refusal(&published, expected) {
        Some(refusal) => Err(refusal),
        None => Ok(()),
    })
}

/// The decidable half of [`layout_unchanged`], split out so it is testable
/// without a registry.
fn layout_refusal(
    published: &ocx_lib::package::metadata::Metadata,
    expected: &ocx_lib::package::metadata::Metadata,
) -> Option<String> {
    let (was, now) = (published.strip_components(), expected.strip_components());
    if was == now {
        return None;
    }
    Some(format!(
        "refusing to patch: the published layers were built with strip_components {was:?} and the spec \
         now says {now:?}. Patch re-references those layers by digest, so the new metadata would describe \
         a tree they do not contain. Re-mirror this version instead of patching it.",
    ))
}

/// Re-emits one `(version, platform)` manifest against `sidecar_json`.
///
/// Drives `ocx package push` rather than writing manifests here: the layer
/// digests are unchanged, so `ocx` HEAD-checks each blob and uploads nothing but
/// the config.
///
/// Through [`push_once`], so a hung registry connection is killed on
/// [`PUSH_TIMEOUT`] instead of parking the patch job until GitHub's job cap.
/// The retry ladder is not shared — a config-blob-only republish costs nothing
/// to run again from a dispatch, so it takes the single attempt and none of what
/// `pipeline push` needs for a 350 MB upload.
///
/// [`push_once`]'s JSON contract *is* shared, and it is stricter than what this
/// used to accept: the argv carries `--format json`, and an exit 0 whose stdout
/// does not parse as a `PushReport` fails the patch rather than counting as a
/// republished manifest. Every `PushReport` field defaults, so `{}` satisfies
/// it; silence does not.
async fn republish(
    spec: &MirrorSpec,
    tag: &str,
    image: &PublishedImage,
    sidecar_json: &str,
    sidecar: &Path,
    annotations: &BTreeMap<String, String>,
    ocx_binary: &Path,
) -> Result<(), String> {
    if let Some(parent) = sidecar.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }
    tokio::fs::write(sidecar, sidecar_json)
        .await
        .map_err(|e| format!("failed to write {}: {e}", sidecar.display()))?;

    let target_ref = format!("{}:{}", spec.target.reference(), tag);
    let args = patch_push_args(&target_ref, image, sidecar, annotations, spec.cascade.enabled)?;

    push_once(ocx_binary, &args, PUSH_TIMEOUT)
        .await
        .map(|_report| ())
        .map_err(|failure| failure.message)
}

/// The `ocx package push` argv that re-emits `image`'s manifest.
///
/// Every published layer is passed as a digest reference in manifest order —
/// order is the layer stack, and dropping or reordering one would republish a
/// different package under the same tag.
pub(crate) fn patch_push_args(
    target_ref: &str,
    image: &PublishedImage,
    sidecar: &Path,
    annotations: &BTreeMap<String, String>,
    cascade: bool,
) -> Result<Vec<String>, String> {
    let layers: Vec<String> = image.layers.iter().map(layer_reference).collect::<Result<_, _>>()?;
    let layers: Vec<&str> = layers.iter().map(String::as_str).collect();

    build_push_args(
        &image.platform.to_string(),
        target_ref,
        &layers,
        Some(sidecar),
        annotations,
        cascade,
    )
}

/// Renders a published layer as the `sha256:<hex>.<ext>` reference
/// `ocx package push` accepts for a blob the registry already holds.
///
/// The extension is not decoration. The OCI distribution spec exposes no media
/// type on a blob HEAD, so `ocx` refuses to guess and the caller must re-declare
/// the archive format. A media type outside [`ArchiveMediaType`] therefore
/// errors: guessing would re-emit the manifest with a media type the layer's
/// bytes do not match, and every consumer would fail to unpack it.
fn layer_reference(layer: &Descriptor) -> Result<String, String> {
    let media_type = ArchiveMediaType::ALL
        .iter()
        .find(|candidate| candidate.as_media_type() == layer.media_type)
        .ok_or_else(|| {
            format!(
                "layer {} has media type '{}', which has no archive extension to re-declare it with",
                layer.digest, layer.media_type
            )
        })?;
    Ok(format!("{}.{}", layer.digest, media_type.canonical_extension()))
}

/// Which published versions a run patches.
///
/// The exact list and the range compose as a union: `--version 1.2.3
/// --min-version 2.0` patches `1.2.3` and everything from `2.0` up. Neither
/// stated at all patches every published version.
#[derive(Debug)]
struct Selection {
    exact: Vec<Version>,
    min: Option<Version>,
    max: Option<Version>,
}

impl Selection {
    fn parse(exact: &[String], min: Option<&str>, max: Option<&str>) -> Result<Self, MirrorError> {
        let parse_one = |flag: &str, raw: &str| {
            Version::parse(raw).ok_or_else(|| {
                MirrorError::SpecUsageError(format!(
                    "{flag} '{raw}' is not a version — expected `X`, `X.Y` or `X.Y.Z`"
                ))
            })
        };
        Ok(Self {
            exact: exact
                .iter()
                .map(|raw| parse_one("--version", raw))
                .collect::<Result<_, _>>()?,
            min: min.map(|raw| parse_one("--min-version", raw)).transpose()?,
            max: max.map(|raw| parse_one("--max-version", raw)).transpose()?,
        })
    }

    /// Narrows the target repository's tags to the set this run patches.
    ///
    /// The leaf reduction happens here rather than at the call site so that
    /// nothing can select a cascade alias by going around it: an alias shares
    /// its leaf's child manifests, so patching both would schedule the same work
    /// twice and the second run would find the first's result already current.
    fn apply(&self, all_tags: &[String]) -> Result<BTreeMap<Version, String>, MirrorError> {
        let leaves = leaf_versions(all_tags);
        if self.exact.is_empty() && self.min.is_none() && self.max.is_none() {
            return Ok(leaves);
        }

        let ranged = self.min.is_some() || self.max.is_some();
        let mut selected = BTreeMap::new();
        let mut matched: HashSet<&Version> = HashSet::new();

        for (version, tag) in leaves {
            let core = strip_build(&version);
            let named = self
                .exact
                .iter()
                .find(|wanted| **wanted == core || wanted.to_string() == tag);
            if let Some(wanted) = named {
                matched.insert(wanted);
            }
            if named.is_some() || (ranged && self.in_range(&core)) {
                selected.insert(version, tag);
            }
        }

        // A `--version` nobody published is a typo, and silently patching
        // nothing is how a corrected spec quietly stays unpublished.
        let missing: Vec<String> = self
            .exact
            .iter()
            .filter(|wanted| !matched.contains(wanted))
            .map(Version::to_string)
            .collect();
        if !missing.is_empty() {
            return Err(MirrorError::SpecUsageError(format!(
                "not published as a leaf tag: {} — an alias like `3.29` is patched by patching the leaf it cascades from",
                missing.join(", ")
            )));
        }

        Ok(selected)
    }

    fn in_range(&self, core: &Version) -> bool {
        self.min.as_ref().is_none_or(|min| core >= min) && self.max.as_ref().is_none_or(|max| core < max)
    }
}

#[cfg(test)]
#[path = "patch/tests.rs"]
mod tests;
