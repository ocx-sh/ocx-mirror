// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Rolling aliases, and completing the cascade for platforms an earlier run
//! published.
//!
//! `--cascade` merges only the pushed leg's own platform entry into each
//! rolling tag, so a version completed across two runs ends up with `X.Y`,
//! `X` and `latest` holding the backfilled platform alone. The repair re-emits
//! each such entry from the registry's own descriptors and skips any whose
//! config bytes this build would not reproduce exactly.

use std::collections::BTreeMap;
use std::path::Path;

use ocx_lib::log;
use ocx_lib::publisher::Publisher;

use crate::command::package::pipeline::patch::patch_push_args;
use crate::command::package::pipeline::plan;
use crate::error::MirrorError;
use crate::filter::pep440_sort_key;
use crate::pipeline::ocx_cli::push::{PUSH_TIMEOUT, push_once};
use crate::pipeline::ocx_cli::resolve_ocx_binary;
use crate::pipeline::python_push;
use crate::pipeline::target_registry;
use crate::spec::{self, MirrorSpec};

/// Point `:latest` at one already-published env entry, returning whether the
/// alias landed.
///
/// `ocx package push --cascade` derives the rolling tags by parsing the version
/// as `X.Y.Z`, so a PEP 440 version it cannot parse (`0.0.0.2`) never gets
/// `latest` and a bare reference (`repo` → `repo:latest`) stays unresolvable.
/// This re-pushes the newest version's green entries under the literal tag —
/// content-addressed, so it costs a verify plus a tag write, and each entry
/// merges into the one `latest` image index.
///
/// Best-effort by construction: the primary publish already succeeded, so a
/// failed alias warns instead of redding the version. For the same reason it
/// gets a SINGLE attempt through [`push_once`] rather than the retry ladder —
/// same precedent as `patch::republish`. A missed alias is corrected by the
/// next run.
pub async fn alias_newest_as_latest(
    spec: &MirrorSpec,
    env_entry: &crate::pipeline::python_prepare::EnvEntry,
    platform: &str,
    version: &str,
    annotations: &BTreeMap<String, String>,
) -> bool {
    let latest_ref = format!("{}:latest", spec.target.reference());
    let attempt = async {
        let args = python_push::build_env_push_args(
            platform,
            &latest_ref,
            &env_entry.metadata_path,
            &env_entry.layers,
            annotations,
            false,
            // Unsigned, and already signed: this re-pushes a manifest the
            // primary publish just wrote, so the digest is identical and the
            // signature referrer attached to it already covers this tag. A
            // second `--sign` would attach a duplicate referrer for nothing.
            None,
        )?;
        let ocx_binary = resolve_ocx_binary()?;
        push_once(&ocx_binary, &args, PUSH_TIMEOUT, None)
            .await
            .map_err(|failure| failure.message)
    }
    .await;

    match attempt {
        Ok(_) => true,
        Err(message) => {
            log::warn!(
                "[{}] latest alias push failed for {version}/{platform}: {message}",
                spec.name,
            );
            false
        }
    }
}

/// Whether `version` — the newest version of THIS run — is also the newest
/// version the target repository holds, i.e. whether `:latest` may be moved
/// onto it.
///
/// [`alias_newest_as_latest`] otherwise only knows newest-in-run, so a
/// backfill run (`versions.backfill: oldest-first`, or an `--exact-version`
/// republish of an old release) would re-point `:latest` at a version older
/// than what is already published — silently downgrading every consumer
/// resolving the bare reference.
///
/// Fail-safe in the direction that cannot break the registry: a tag-list read
/// that fails answers `false`, so the alias is skipped and the next run
/// corrects it. Nothing here may fail the push job — the packages are already
/// published either way.
pub async fn run_newest_is_registry_newest(publisher: &Publisher, spec: &MirrorSpec, version: &str) -> bool {
    let identifier = ocx_lib::oci::Identifier::new_registry(&spec.target.repository, &spec.target.registry);
    let tags = match fetch_published_tags(publisher, &identifier).await {
        Ok(tags) => tags,
        Err(error) => {
            log::warn!(
                "[{}] skipping the latest alias for {version}: could not read the published tags of {identifier}: {error}",
                spec.name,
            );
            return false;
        }
    };

    match registry_tag_newer_than(&tags, version) {
        Some(newer) => {
            log::warn!(
                "[{}] skipping the latest alias for {version}: {identifier} already holds the newer tag '{newer}'",
                spec.name,
            );
            false
        }
        None => true,
    }
}

/// The latest-alias gate's tag listing, with a test-only injection seam.
///
/// The fake-`ocx` test harness fakes the subprocess, not the in-process
/// [`Publisher`], so without the seam the alias tests would read LIVE
/// registry state — passing only while the fixture's repository happens to
/// hold nothing newer, and breaking the day it does. Tests set
/// [`LATEST_TAGS_OVERRIDE`] under [`ocx_env_lock`], same discipline as every
/// other process-global test knob.
pub async fn fetch_published_tags(
    publisher: &Publisher,
    identifier: &ocx_lib::oci::Identifier,
) -> Result<Vec<String>, MirrorError> {
    #[cfg(test)]
    if let Some(tags) = LATEST_TAGS_OVERRIDE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
    {
        return Ok(tags);
    }
    target_registry::list_target_tags(publisher, identifier).await
}

/// See [`fetch_published_tags`]. `Some(tags)` is consumed by the next fetch.
#[cfg(test)]
pub static LATEST_TAGS_OVERRIDE: std::sync::Mutex<Option<Vec<String>>> = std::sync::Mutex::new(None);

/// The first published tag that is strictly newer than `version` under
/// [`pep440_sort_key`], if any.
///
/// Rolling and reserved tags are not versions and are skipped: `latest` is the
/// very tag being decided, and the reserved set is the digest-named keep-tag
/// safety net plus the OCI referrers/cosign sidecars. `Tag::is_reserved_str` is
/// ocx's own classifier rather than a prefix test of ours, so the two cannot
/// drift: ocx 0.6.0 writes `__ocx.keep.<alg>-<hex>` and still reads the frozen
/// legacy `<alg>.<hex>` form, and one call covers both. An unparseable version
/// on either side leaves the two unordered, which counts as "newer" — the
/// caller then declines to move the alias, which is the safe direction.
pub fn registry_tag_newer_than<'a>(tags: &'a [String], version: &str) -> Option<&'a str> {
    let own_key = pep440_sort_key(version);
    tags.iter()
        .map(String::as_str)
        .filter(|tag| *tag != "latest" && !ocx_lib::package::tag::Tag::is_reserved_str(tag))
        .find(|tag| {
            let key = pep440_sort_key(tag);
            key.0.is_some() && key > own_key
        })
}

/// The entries of a version's published index whose cascade never ran.
///
/// `ocx package push --cascade` merges only the pushed leg's OWN platform into
/// each rolling tag, so both phase-2 loops give it to every leg of a whole
/// version. That is complete for a version published in one run, and silently
/// incomplete for one completed across two: the run that first published the
/// version withheld `--cascade` from its green legs precisely because the
/// version was still partial, and the run that backfills the missing platform
/// no longer carries those legs at all — `pipeline plan` trims already-published
/// `(version, platform)` tiles (`filter::filter_versions`,
/// `plan::build_env_plan_entries`). Nothing ever cascades them, so `X.Y`, `X`
/// and `latest` end up holding the backfilled platform alone while the exact
/// version tag is correct.
///
/// A pushed platform string that does not parse excludes nothing: re-pushing an
/// entry this run already pushed is idempotent, while dropping one that still
/// needs the cascade is the bug.
pub fn entries_awaiting_cascade<'a>(
    published: &'a [target_registry::PublishedImage],
    platforms_pushed: &[String],
) -> Vec<&'a target_registry::PublishedImage> {
    let pushed: Vec<ocx_lib::oci::Platform> = platforms_pushed
        .iter()
        .filter_map(|platform| platform.parse().ok())
        .collect();
    published
        .iter()
        .filter(|image| !pushed.contains(&image.platform))
        .collect()
}

/// Run the cascade for every entry of `version`'s merged index this run did not
/// push, so the rolling tags reflect the whole version rather than this run's
/// legs (see [`entries_awaiting_cascade`]).
///
/// Each entry is re-emitted from the registry's own descriptors — the published
/// layers by digest, the published config metadata verbatim — so the manifest
/// written is byte-identical to the one already there and the push costs a
/// config blob plus the cascade tag writes. Nothing is downloaded and no layer
/// is re-uploaded, the same mechanism `pipeline patch` publishes through.
///
/// An entry whose config bytes this build would not reproduce exactly is
/// SKIPPED rather than re-pushed: a differing config blob yields a new platform
/// manifest digest, which would orphan the digest a consumer's lock pins — a
/// worse outcome than the rolling tag this repairs.
///
/// Best-effort by construction, on the same reasoning as
/// [`alias_newest_as_latest`]: every package of the version is already
/// published, so a failed repair warns instead of redding the version. Single
/// attempt per entry — the upload is a config blob, and the retry ladder
/// `pipeline push` needs for a 350 MB tile buys nothing here.
///
/// Returns the cascade tags written, for the run summary and the announce union.
pub async fn cascade_backfilled_entries(
    publisher: &Publisher,
    spec: &MirrorSpec,
    version: &str,
    platforms_pushed: &[String],
    annotations: &BTreeMap<String, String>,
) -> Vec<String> {
    let identifier = ocx_lib::oci::Identifier::new_registry(&spec.target.repository, &spec.target.registry);
    let published = match published_images_for(publisher, &identifier, version).await {
        Ok(images) => images,
        Err(error) => {
            log::warn!(
                "[{}] {version}: could not read the published index to complete its cascade, so the rolling tags \
                 may carry only this run's platforms: {error}",
                spec.name,
            );
            return Vec::new();
        }
    };

    let awaiting = entries_awaiting_cascade(&published, platforms_pushed);
    if awaiting.is_empty() {
        return Vec::new();
    }

    let work_dir = match tempfile::TempDir::new() {
        Ok(dir) => dir,
        Err(error) => {
            log::warn!(
                "[{}] {version}: could not create a sidecar directory: {error}",
                spec.name
            );
            return Vec::new();
        }
    };
    let ocx_binary = match resolve_ocx_binary() {
        Ok(binary) => binary,
        Err(error) => {
            log::warn!("[{}] {version}: {error}", spec.name);
            return Vec::new();
        }
    };

    let mut tags = Vec::new();
    for image in awaiting {
        log::info!(
            "[{}] {version} ({}): re-cascading a platform an earlier run published",
            spec.name,
            image.platform,
        );
        match re_cascade_entry(
            publisher,
            &identifier,
            spec,
            image,
            annotations,
            &ocx_binary,
            work_dir.path(),
        )
        .await
        {
            Ok(written) => tags.extend(written),
            Err(message) => log::warn!(
                "[{}] {version} ({}): the rolling tags do not carry this platform — {message}",
                spec.name,
                image.platform,
            ),
        }
    }
    tags
}

/// The repair's view of what the version tag holds, with a test-only stub.
///
/// Same hazard [`fetch_published_tags`] documents, one step worse: the
/// fake-`ocx` harness fakes the subprocess, not the in-process [`Publisher`],
/// and every green version of every push test reaches this call — so without a
/// stub the unit suite would read the LIVE `ocx.sh` state its fixtures name,
/// and then *re-push* against whatever it found. A test build therefore sees an
/// empty index and skips the repair; the mechanism itself is exercised by the
/// acceptance harness against the local registry.
#[cfg(not(test))]
pub async fn published_images_for(
    publisher: &Publisher,
    identifier: &ocx_lib::oci::Identifier,
    version: &str,
) -> Result<Vec<target_registry::PublishedImage>, MirrorError> {
    target_registry::fetch_published_images(publisher, identifier, &[version]).await
}

/// See [`published_images_for`] — the test build's registry-free stand-in.
#[cfg(test)]
pub async fn published_images_for(
    _publisher: &Publisher,
    _identifier: &ocx_lib::oci::Identifier,
    _version: &str,
) -> Result<Vec<target_registry::PublishedImage>, MirrorError> {
    Ok(Vec::new())
}

/// Re-emits one published `(version, platform)` manifest with `--cascade`, so
/// the rolling tags pick up an entry an earlier run left behind. See
/// [`cascade_backfilled_entries`] for why this is safe to run against live
/// published state.
pub async fn re_cascade_entry(
    publisher: &Publisher,
    identifier: &ocx_lib::oci::Identifier,
    spec: &MirrorSpec,
    image: &target_registry::PublishedImage,
    annotations: &BTreeMap<String, String>,
    ocx_binary: &Path,
    work_dir: &Path,
) -> Result<Vec<String>, String> {
    let published = target_registry::fetch_published_metadata(publisher, identifier, image)
        .await
        .map_err(|error| format!("the published metadata could not be read: {error}"))?;

    // The re-push must be a no-op on the manifest. `config_bytes_match` decides
    // that from the descriptor alone: it is false exactly when this build would
    // serialize the same document differently from whatever `ocx` published it,
    // and re-pushing then rewrites the platform manifest instead of repairing a
    // tag.
    if !plan::config_bytes_match(image, &published)
        .map_err(|error| format!("the published metadata could not be compared: {error}"))?
    {
        return Err(format!(
            "its published config blob is not what this build would write, and re-pushing it would replace the \
             manifest digest {} rather than only move the rolling tags",
            image.manifest_digest,
        ));
    }

    // The sidecar is the published metadata verbatim: `ocx package push -m`
    // reads the published form since 0.5.6, and this path must reproduce the
    // registry's config bytes exactly — no authoring round-trip, no platform
    // stamp (retired; the platform travels on `-p` alone).
    let sidecar_json = serde_json::to_string_pretty(&published)
        .map_err(|error| format!("the push sidecar could not be rendered: {error}"))?;

    let sidecar = work_dir.join(format!(
        "{}-{}-metadata.json",
        image.version,
        spec::platform_slug(&image.platform),
    ));
    tokio::fs::write(&sidecar, sidecar_json)
        .await
        .map_err(|error| format!("failed to write {}: {error}", sidecar.display()))?;

    let target_ref = format!("{}:{}", spec.target.reference(), image.version);
    // Same reasoning as `alias_newest_as_latest`: this re-pushes an
    // already-published manifest at its existing digest to move the rolling
    // tags, so whatever signature that digest carries still applies. A
    // platform published before `sign:` was added stays unsigned here and is
    // what `package pipeline sign` (the backfill) exists for.
    let args = patch_push_args(&target_ref, image, &sidecar, annotations, true, None)?;

    push_once(ocx_binary, &args, PUSH_TIMEOUT, None)
        .await
        .map(|report| report.cascade_tags_written)
        .map_err(|failure| failure.message)
}
