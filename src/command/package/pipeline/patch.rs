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
use crate::command::package::pipeline::push::{
    ANNOUNCE_TIMEOUT, ENV_ANNOUNCE_TOKEN, PUSH_TIMEOUT, TagSource, announce_token, build_push_args, invoke_announce,
    push_once, resolve_ocx_binary,
};
use crate::command::package::target_registry::{self, PublishedImage};
use crate::error::MirrorError;
use crate::pipeline::orchestrator;
use crate::spec::{self, MirrorSpec};

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
fn patch_push_args(
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

/// A version with any build-metadata segment removed.
///
/// Bounds are written in the grammar a human reads off the spec (`3.29.0`)
/// while a build-stamped mirror publishes `3.29.0_20260610`, and those do not
/// compare the way the bound implies: `Version` sorts a build-stamped version
/// *before* its own core, so `--min-version 3.29.0` would exclude the very tag
/// it names. Comparing cores makes an inclusive lower bound cover every build of
/// that version and an exclusive upper bound exclude every build of it.
fn strip_build(version: &Version) -> Version {
    match version.has_build() {
        // A build segment implies an `X.Y.Z` core, so the parent always exists.
        true => version.parent().unwrap_or_else(|| version.clone()),
        false => version.clone(),
    }
}

#[cfg(test)]
mod tests {
    use ocx_lib::oci::{Algorithm, Descriptor, Platform};

    use super::*;
    use crate::pipeline::orchestrator::ExpectedMetadata;
    use crate::spec::BinScanMode;

    /// The wire media types, read off the same enum the production code maps
    /// from — a literal here would let the two drift and still pass.
    fn tar_gz() -> &'static str {
        ArchiveMediaType::TarGz.as_media_type()
    }
    fn tar_xz() -> &'static str {
        ArchiveMediaType::TarXz.as_media_type()
    }

    fn version(raw: &str) -> Version {
        Version::parse(raw).unwrap_or_else(|| panic!("'{raw}' is a version"))
    }

    /// The tag list as `list_target_tags` returns it — every test enters
    /// through `Selection::apply`, which is the production entry point, so the
    /// leaf reduction is under test rather than reproduced here.
    fn tag_list(tags: &[&str]) -> Vec<String> {
        tags.iter().map(|t| (*t).to_string()).collect()
    }

    fn selected_tags(selection: &Selection, tags: &[&str]) -> Vec<String> {
        selection
            .apply(&tag_list(tags))
            .expect("selection applies")
            .into_values()
            .collect()
    }

    // ── Version selection ─────────────────────────────────────────────────

    /// A build-stamped mirror is the case the bounds have to survive:
    /// `Version` sorts `3.29.0_20260610` BEFORE `3.29.0`, so comparing the
    /// published version directly would drop every stamped tag out of an
    /// inclusive lower bound naming its own core.
    #[test]
    fn min_version_is_inclusive_on_the_version_core() {
        let tags = ["3.28.0_20260101", "3.29.0_20260610", "3.30.0_20260701"];
        let selection = Selection::parse(&[], Some("3.29.0"), None).expect("bounds parse");

        assert_eq!(
            selected_tags(&selection, &tags),
            vec!["3.29.0_20260610", "3.30.0_20260701"],
        );
    }

    #[test]
    fn max_version_is_exclusive() {
        let tags = ["3.28.0", "3.29.0", "3.30.0"];
        let selection = Selection::parse(&[], None, Some("3.30.0")).expect("bounds parse");

        assert_eq!(selected_tags(&selection, &tags), vec!["3.28.0", "3.29.0"]);
    }

    /// The exclusive upper bound must exclude every build of the version it
    /// names, not just the bare tag — otherwise `--max-version 3.30.0` still
    /// patches `3.30.0_20260701` on exactly the mirrors that stamp builds.
    #[test]
    fn max_version_excludes_every_build_of_the_version_it_names() {
        let tags = ["3.29.0_20260610", "3.30.0_20260701"];
        let selection = Selection::parse(&[], None, Some("3.30.0")).expect("bounds parse");

        assert_eq!(selected_tags(&selection, &tags), vec!["3.29.0_20260610"]);
    }

    #[test]
    fn both_bounds_narrow_to_a_half_open_window() {
        let tags = ["3.28.0", "3.29.0", "3.30.0", "3.31.0"];
        let selection = Selection::parse(&[], Some("3.29.0"), Some("3.31.0")).expect("bounds parse");

        assert_eq!(selected_tags(&selection, &tags), vec!["3.29.0", "3.30.0"]);
    }

    #[test]
    fn no_selector_at_all_patches_every_published_version() {
        let tags = ["3.28.0", "3.29.0", "3.30.0"];
        let selection = Selection::parse(&[], None, None).expect("bounds parse");

        assert_eq!(selected_tags(&selection, &tags), vec!["3.28.0", "3.29.0", "3.30.0"]);
    }

    #[test]
    fn an_exact_version_selects_only_itself() {
        let tags = ["3.28.0", "3.29.0", "3.30.0"];
        let selection = Selection::parse(&["3.29.0".to_string()], None, None).expect("bounds parse");

        assert_eq!(selected_tags(&selection, &tags), vec!["3.29.0"]);
    }

    /// The version a human reads off the spec is the core; the leaf tag carries
    /// the stamp. Requiring the stamp would make `--version` unusable on every
    /// mirror that has one.
    #[test]
    fn an_exact_version_matches_a_build_stamped_leaf_by_its_core() {
        let tags = ["3.29.0_20260610", "3.30.0_20260701"];
        let selection = Selection::parse(&["3.29.0".to_string()], None, None).expect("bounds parse");

        assert_eq!(selected_tags(&selection, &tags), vec!["3.29.0_20260610"]);
    }

    #[test]
    fn an_exact_version_composes_with_a_range_as_a_union() {
        let tags = ["3.28.0", "3.29.0", "3.30.0", "3.31.0"];
        let selection = Selection::parse(&["3.28.0".to_string()], Some("3.30.0"), None).expect("bounds parse");

        assert_eq!(selected_tags(&selection, &tags), vec!["3.28.0", "3.30.0", "3.31.0"]);
    }

    /// Silently patching nothing is how a corrected spec stays unpublished
    /// while the run reports success.
    #[test]
    fn an_unpublished_exact_version_is_a_usage_error() {
        use ocx_lib::cli::ExitCode;

        let selection = Selection::parse(&["9.9.9".to_string()], None, None).expect("bounds parse");
        let error = selection.apply(&tag_list(&["3.29.0"])).expect_err("must reject");

        assert_eq!(error.kind_exit_code(), ExitCode::UsageError, "got: {error}");
    }

    #[test]
    fn an_unparseable_bound_is_a_usage_error() {
        use ocx_lib::cli::ExitCode;

        let error = Selection::parse(&[], Some("three"), None).expect_err("must reject");
        assert_eq!(error.kind_exit_code(), ExitCode::UsageError, "got: {error}");
    }

    /// Aliases share their leaf's child manifests, so scanning them would
    /// schedule the same patch four times — and patching the leaf re-cascades
    /// them anyway.
    #[test]
    fn only_leaf_tags_are_selectable_never_their_cascade_aliases() {
        let tags = ["3.29.0_20260610", "3.29.0", "3.29", "3", "latest"];
        let selection = Selection::parse(&[], None, None).expect("bounds parse");

        assert_eq!(selected_tags(&selection, &tags), vec!["3.29.0_20260610"]);
    }

    /// The alias is not silently skipped either — naming one is a usage error,
    /// so nobody concludes their patch ran.
    #[test]
    fn naming_a_cascade_alias_exactly_is_a_usage_error() {
        let selection = Selection::parse(&["3.29".to_string()], None, None).expect("bounds parse");
        let error = selection
            .apply(&tag_list(&["3.29.0_20260610", "3.29.0", "3.29", "3"]))
            .expect_err("must reject");

        assert!(error.to_string().contains("3.29"), "got: {error}");
    }

    // ── argv construction ─────────────────────────────────────────────────

    fn descriptor(media_type: &str) -> Descriptor {
        Descriptor {
            media_type: media_type.to_string(),
            digest: format!("sha256:{}", "a".repeat(64)),
            size: 42,
            urls: None,
            annotations: None,
            artifact_type: None,
        }
    }

    fn image(layers: Vec<Descriptor>) -> PublishedImage {
        PublishedImage {
            version: version("3.29.0"),
            platform: "linux/amd64".parse::<Platform>().expect("valid platform"),
            manifest_digest: ocx_lib::oci::Digest::Sha256("b".repeat(64)),
            config: descriptor("application/vnd.ocx.package.metadata.v1+json"),
            layers,
        }
    }

    #[test]
    fn a_published_layer_becomes_a_digest_reference_with_its_media_type_extension() {
        let args = patch_push_args(
            "ghcr.io/ocx-sh/cmake:3.29.0_20260610",
            &image(vec![descriptor(tar_xz())]),
            Path::new("/work/3.29.0-linux_amd64-metadata.json"),
            &BTreeMap::new(),
            true,
        )
        .expect("argv assembles");

        assert_eq!(
            args,
            vec![
                "--format",
                "json",
                "package",
                "push",
                "--cascade",
                "--new",
                "-p",
                "linux/amd64",
                "-i",
                "ghcr.io/ocx-sh/cmake:3.29.0_20260610",
                "--metadata",
                "/work/3.29.0-linux_amd64-metadata.json",
                &format!("sha256:{}.tar.xz", "a".repeat(64)),
            ],
        );
    }

    /// Order is the layer stack. A patch that reorders or drops one republishes
    /// a different package under the same tag.
    #[test]
    fn every_layer_is_re_referenced_in_manifest_order() {
        let mut base = descriptor(tar_gz());
        base.digest = format!("sha256:{}", "1".repeat(64));
        let mut top = descriptor(tar_xz());
        top.digest = format!("sha256:{}", "2".repeat(64));

        let args = patch_push_args(
            "ghcr.io/ocx-sh/cmake:3.29.0",
            &image(vec![base, top]),
            Path::new("/work/metadata.json"),
            &BTreeMap::new(),
            true,
        )
        .expect("argv assembles");

        let layers: Vec<&String> = args.iter().filter(|arg| arg.starts_with("sha256:")).collect();
        assert_eq!(
            layers,
            vec![
                &format!("sha256:{}.tar.gz", "1".repeat(64)),
                &format!("sha256:{}.tar.xz", "2".repeat(64)),
            ],
        );
    }

    /// `--cascade` is the spec's call, and it is what re-points `3.29.0`,
    /// `3.29`, `3` and `latest` onto the patched manifest.
    #[test]
    fn cascade_follows_the_spec_flag() {
        let cascading = patch_push_args(
            "ghcr.io/ocx-sh/cmake:3.29.0",
            &image(vec![descriptor(tar_xz())]),
            Path::new("/work/metadata.json"),
            &BTreeMap::new(),
            true,
        )
        .expect("argv assembles");
        let plain = patch_push_args(
            "ghcr.io/ocx-sh/cmake:3.29.0",
            &image(vec![descriptor(tar_xz())]),
            Path::new("/work/metadata.json"),
            &BTreeMap::new(),
            false,
        )
        .expect("argv assembles");

        assert!(cascading.iter().any(|arg| arg == "--cascade"), "got: {cascading:?}");
        assert!(!plain.iter().any(|arg| arg == "--cascade"), "got: {plain:?}");
    }

    /// A sidecar without the stamped `platform` field exits 65 on the `ocx`
    /// side, so the flag carrying it is load-bearing, not cosmetic.
    #[test]
    fn the_sidecar_is_always_passed_explicitly() {
        let args = patch_push_args(
            "ghcr.io/ocx-sh/cmake:3.29.0",
            &image(vec![descriptor(tar_xz())]),
            Path::new("/work/metadata.json"),
            &BTreeMap::new(),
            true,
        )
        .expect("argv assembles");

        let flag = args
            .iter()
            .position(|arg| arg == "--metadata")
            .expect("--metadata present");
        assert_eq!(args.get(flag + 1).map(String::as_str), Some("/work/metadata.json"));
    }

    #[test]
    fn spec_annotations_survive_onto_the_patched_index() {
        let annotations = BTreeMap::from([(
            "org.opencontainers.image.source".to_string(),
            "https://github.com/ocx-sh/mirror-cmake".to_string(),
        )]);
        let args = patch_push_args(
            "ghcr.io/ocx-sh/cmake:3.29.0",
            &image(vec![descriptor(tar_xz())]),
            Path::new("/work/metadata.json"),
            &annotations,
            true,
        )
        .expect("argv assembles");

        assert!(
            args.contains(&"org.opencontainers.image.source=https://github.com/ocx-sh/mirror-cmake".to_string()),
            "got: {args:?}",
        );
    }

    /// Defaulting the extension would re-emit the manifest declaring a format
    /// the layer's bytes do not have, and every consumer would fail to unpack
    /// it — silently, since the digest still verifies.
    #[test]
    fn an_unmappable_layer_media_type_errors_instead_of_guessing() {
        let error = patch_push_args(
            "ghcr.io/ocx-sh/cmake:3.29.0",
            &image(vec![descriptor("application/vnd.oci.image.layer.v1.tar")]),
            Path::new("/work/metadata.json"),
            &BTreeMap::new(),
            true,
        )
        .expect_err("must reject");

        assert!(error.contains("application/vnd.oci.image.layer.v1.tar"), "got: {error}");
    }

    /// The spec `republish` reads: the target reference it pushes to, and
    /// whether that push cascades.
    fn minimal_spec() -> MirrorSpec {
        serde_yaml_ng::from_str(
            r#"
name: cmake
target:
  registry: ghcr.io
  repository: ocx-sh/cmake
source:
  type: github_release
  owner: kitware
  repo: cmake
assets:
  linux/amd64:
    - "cmake\\.tar\\.xz"
"#,
        )
        .expect("the fixture parses")
    }

    /// The message a failed patch reports to its caller, verbatim.
    ///
    /// `republish` runs through `pipeline push`'s bounded `push_once` so a hung
    /// registry cannot park the patch job until GitHub's cap. That refactor must
    /// not have cost what the registry actually said — a patch run reds on this
    /// string and nothing else carries the reason.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_rejected_republish_reports_the_exit_code_and_the_stderr() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("fake-ocx");
        std::fs::write(&script, "#!/bin/sh\necho 'manifest rejected' >&2\nexit 65\n").expect("script writes");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let error = republish(
            &minimal_spec(),
            "3.29.0",
            &image(vec![descriptor(tar_xz())]),
            r#"{"type":"bundle","version":1}"#,
            &dir.path().join("3.29.0-linux_amd64-metadata.json"),
            &BTreeMap::new(),
            &script,
        )
        .await
        .expect_err("a rejected push must not read as a republished manifest");

        assert!(error.contains("ocx package push exited"), "got: {error}");
        assert!(error.contains("65"), "got: {error}");
        assert!(error.contains("manifest rejected"), "got: {error}");
    }

    /// The success half of the same contract, which routing through `push_once`
    /// changed and nothing covered: exit 0 used to be unconditional success,
    /// and is now success only when stdout parses as a `PushReport`. `{}` is the
    /// minimum that does — every field defaults — so this pins that a report
    /// carrying no fields is still a republished manifest, and that the
    /// stricter contract did not turn an ordinary patch run red.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_republish_that_exits_zero_with_a_parseable_report_succeeds() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("fake-ocx");
        std::fs::write(&script, "#!/bin/sh\necho '{}'\n").expect("script writes");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        republish(
            &minimal_spec(),
            "3.29.0",
            &image(vec![descriptor(tar_xz())]),
            r#"{"type":"bundle","version":1}"#,
            &dir.path().join("3.29.0-linux_amd64-metadata.json"),
            &BTreeMap::new(),
            &script,
        )
        .await
        .expect("exit 0 with a parseable report is a republished manifest");
    }

    // ── skip gate ─────────────────────────────────────────────────────────

    /// The expectation a fresh push of the fixture would record.
    fn expected_metadata() -> ExpectedMetadata {
        let authoring = serde_json::from_slice(br#"{"type":"bundle","version":1,"strip_components":1,"env":[]}"#)
            .expect("metadata fixture parses");
        ExpectedMetadata::render(authoring, &"linux/amd64".parse().expect("valid platform"))
            .expect("the fixture renders both projections")
    }

    fn image_recording(config_digest: &str) -> PublishedImage {
        let mut image = image(vec![descriptor(tar_xz())]);
        image.config.digest = config_digest.to_string();
        image
    }

    fn offline_publisher() -> Publisher {
        Publisher::new(ClientBuilder::from_env().expect("client builds"))
    }

    /// Patch must refuse when the layer layout changed under it.
    ///
    /// `strip_components` is the whole mapping from a layer's archive paths onto
    /// `${installPath}`, fixed when the layer is built. Patch re-references the
    /// published layers by digest, so republishing metadata that strips
    /// differently describes a tree they do not contain — every
    /// `${installPath}`-rooted PATH entry names a directory absent from the
    /// bundle, and the package installs broken while its digest still verifies.
    ///
    /// Live on `mirror-astral-sh`: windows/amd64 published as
    /// `strip_components: 1` with `PATH=${installPath}`, then had to move to
    /// `strip_components: 0` with `PATH=${installPath}/python` because the
    /// `bin_scan` load gate rejects the bare form. All six leaf tags read as
    /// drifted, and patching them would have shipped exactly that.
    #[test]
    fn a_strip_components_change_is_a_layout_change_not_a_metadata_fix() {
        let strip = |n: Option<u8>| -> ocx_lib::package::metadata::Metadata {
            let field = n.map(|n| format!(r#","strip_components":{n}"#)).unwrap_or_default();
            serde_json::from_str(&format!(r#"{{"type":"bundle","version":1{field},"env":[]}}"#))
                .expect("metadata fixture parses")
        };

        // Every published-vs-expected pair a patch run can see. Only an equal
        // pair may be republished; the rest describe a different tree.
        for (was, now, patchable) in [
            (Some(1), Some(1), true),
            (None, None, true),
            (Some(1), Some(0), false),
            (Some(0), Some(1), false),
            // `None` and `Some(0)` mean the same layout but are different
            // published bytes. Refusing costs one message a human resolves;
            // the opposite ships a broken package. Pinned so a future
            // "normalize None to 0" is a decision, not a silent drift.
            (None, Some(0), false),
        ] {
            let refusal = layout_refusal(&strip(was), &strip(now));
            assert_eq!(
                refusal.is_none(),
                patchable,
                "published strip {was:?} -> spec strip {now:?}, got: {refusal:?}",
            );
            if let Some(message) = refusal {
                assert!(
                    message.contains("Re-mirror"),
                    "a refusal must name the remedy, since the patch workflow is the signposted one: {message}",
                );
            }
        }
    }

    /// The idempotency mechanism, and the whole reason there is no patch
    /// ledger: a `(version, platform)` whose config blob already carries what
    /// the spec would publish is settled from the descriptor's digest alone —
    /// no blob fetch, no push.
    #[tokio::test]
    async fn matching_metadata_skips_without_a_registry_call() {
        let expected = expected_metadata();
        let bytes = serde_json::to_vec(&expected.published).expect("serializes");
        let image = image_recording(&Algorithm::Sha256.hash(&bytes).to_string());

        let drift = image_drift(
            &offline_publisher(),
            &Identifier::new_registry("mirror/cmake", "registry.test"),
            &image,
            &expected,
            BinScanMode::Off,
        )
        .await
        .expect("the config digest settles it without a fetch");

        assert!(drift.is_none(), "a matching config digest must not schedule a patch");
    }

    /// The red half of the test above: with a config digest that does NOT
    /// match, `Ok(None)` — the value that produces a skip — must be
    /// unreachable, whatever the registry then answers.
    #[tokio::test]
    async fn a_differing_config_digest_never_settles_as_a_skip() {
        let image = image_recording(&format!("sha256:{}", "f".repeat(64)));

        let result = image_drift(
            &offline_publisher(),
            &Identifier::new_registry("mirror/cmake", "registry.invalid"),
            &image,
            &expected_metadata(),
            BinScanMode::Off,
        )
        .await;

        assert!(
            !matches!(result, Ok(None)),
            "a config digest that differs must never settle as 'already current': {}",
            result.is_ok(),
        );
    }
}
