// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Fail-safe target-registry state loading shared by `sync` and
//! `pipeline plan`.
//!
//! Discover classifies a version as `New` when the target registry reports
//! no platforms for it — so a transient read failure must never masquerade
//! as "nothing published". Treating the two alike re-flags already-published
//! versions, and the subsequent republish re-points the version tag and
//! orphans the previously published digest (GC/pin hazard — issue #157).
//! Only an authoritative registry not-found response may classify a
//! repository or tag as absent; every other error aborts the run.

use std::collections::{BTreeMap, HashSet};

use ocx_lib::oci::client::error::ClientError;
use ocx_lib::oci::{Descriptor, Digest, Identifier, ImageManifest, PinnedIdentifier, Platform};
use ocx_lib::package::metadata::Metadata;
use ocx_lib::package::version::Version;
use ocx_lib::publisher::Publisher;

use crate::error::MirrorError;

/// Extract platform entries from an OCI manifest.
pub(crate) fn extract_platforms(manifest: &ocx_lib::oci::Manifest) -> Vec<Platform> {
    match manifest {
        ocx_lib::oci::Manifest::ImageIndex(index) => index
            .manifests
            .iter()
            .filter_map(|entry| entry.platform.as_ref().and_then(|p| Platform::try_from(p.clone()).ok()))
            .collect(),
        _ => vec![],
    }
}

/// Lists all tags on the target repository.
///
/// An authoritative "repository not found" (first publish of a new mirror)
/// yields an empty list. Any other error aborts with
/// [`MirrorError::TargetError`].
pub(crate) async fn list_target_tags(
    publisher: &Publisher,
    identifier: &Identifier,
) -> Result<Vec<String>, MirrorError> {
    tags_from_result(publisher.list_tags(identifier.clone()).await, identifier)
}

/// Fetches the already-published platform set for each version tag.
///
/// An authoritative "manifest not found" (tag listed but manifest deleted
/// since) skips the tag. Any other error aborts with
/// [`MirrorError::TargetError`].
pub(crate) async fn fetch_published_platforms(
    publisher: &Publisher,
    identifier: &Identifier,
    tags: &[&str],
) -> Result<BTreeMap<Version, HashSet<Platform>>, MirrorError> {
    let mut platform_info: BTreeMap<Version, HashSet<Platform>> = BTreeMap::new();
    for tag in tags {
        let tag_identifier = identifier.clone_with_tag((*tag).to_string());
        let result = publisher.client().fetch_manifest(&tag_identifier).await;
        merge_manifest_result(tag, result, &mut platform_info)?;
    }
    Ok(platform_info)
}

/// One published `(version, platform)` image, as the registry holds it.
///
/// Carries everything a metadata-only re-push must reuse verbatim: the child
/// manifest's own digest and its layer descriptors. A metadata patch rewrites
/// the config blob only — the payload layers are already in the registry and
/// are re-referenced by digest, never re-uploaded.
///
/// The recorded metadata itself is deliberately *not* here: reading it costs a
/// second registry round trip per pair, and [`PublishedImage::config`] already
/// identifies the recorded bytes by digest. Fetch it with
/// [`fetch_published_metadata`] only once that digest rules out a match.
#[derive(Debug, Clone)]
pub(crate) struct PublishedImage {
    /// Version tag the image index was reached through.
    pub version: Version,
    /// Platform this child manifest was listed under in the index.
    pub platform: Platform,
    /// Digest of the child (platform) manifest itself.
    #[allow(dead_code)] // Read by `pipeline patch`, which re-points the tag at the rewritten manifest.
    pub manifest_digest: Digest,
    /// Config-blob descriptor — media type, digest, and size of the recorded
    /// package metadata.
    pub config: Descriptor,
    /// Payload layer descriptors, in manifest order.
    #[allow(dead_code)] // Read by `pipeline patch`, which re-references them by digest instead of re-uploading.
    pub layers: Vec<Descriptor>,
}

/// Fetches the child manifest of every published `(tag, platform)` pair.
///
/// One index fetch per tag plus one manifest fetch per platform beneath it.
///
/// Fail-safe (issue #157), with the same asymmetry the module doctrine sets:
/// an authoritative "manifest not found" for a *tag* (listed but deleted
/// since) skips that tag, because absence there is a fact the registry
/// asserted. A failure to read a child the index just named is not absence —
/// it is a failed read of published state — so every other error, on either
/// hop, aborts with [`MirrorError::TargetError`].
pub(crate) async fn fetch_published_images(
    publisher: &Publisher,
    identifier: &Identifier,
    tags: &[&str],
) -> Result<Vec<PublishedImage>, MirrorError> {
    let mut images = Vec::new();
    for tag in tags {
        let tag_identifier = identifier.clone_with_tag((*tag).to_string());
        let index = publisher.client().fetch_manifest(&tag_identifier).await;
        for (version, platform, child_digest) in index_children(tag, index)? {
            let child_identifier = pinned(identifier, child_digest.clone())?;
            let manifest = publisher.client().pull_manifest(&child_identifier).await;
            let manifest = child_manifest(tag, &platform, manifest)?;
            images.push(PublishedImage {
                version,
                platform,
                manifest_digest: child_digest,
                config: manifest.config,
                layers: manifest.layers,
            });
        }
    }
    Ok(images)
}

/// Fetches and parses the package metadata recorded in an image's config blob.
///
/// Every failure aborts with [`MirrorError::TargetError`] — including a blob
/// that does not parse as [`Metadata`]. An unreadable config is not evidence
/// that the published metadata is stale, and treating it as such would
/// republish every version of every mirror the moment the format moved.
pub(crate) async fn fetch_published_metadata(
    publisher: &Publisher,
    identifier: &Identifier,
    image: &PublishedImage,
) -> Result<Metadata, MirrorError> {
    let config_digest = Digest::try_from(&image.config.digest).map_err(|error| {
        MirrorError::TargetError(format!(
            "config descriptor of {} ({}) has an unusable digest '{}': {error}",
            image.version, image.platform, image.config.digest
        ))
    })?;
    let blob_identifier = pinned(identifier, config_digest)?;
    let blob = publisher.client().pull_blob(&blob_identifier).await;
    parse_metadata(&image.version, &image.platform, blob)
}

/// Pins `identifier` to a blob/manifest digest.
///
/// The tag is dropped first: an identifier carrying both would address the
/// tag, and the digest-addressed fetch would silently read whatever the tag
/// currently points at.
fn pinned(identifier: &Identifier, digest: Digest) -> Result<PinnedIdentifier, MirrorError> {
    PinnedIdentifier::try_from(identifier.without_tag().clone_with_digest(digest))
        .map_err(|error| MirrorError::TargetError(format!("failed to pin {identifier}: {error}")))
}

/// Classifies a per-tag index fetch into its `(version, platform, digest)`
/// children — fail-safe (issue #157).
///
/// A tag whose manifest is not an image index yields no children: that is what
/// [`extract_platforms`] already reports for the platform map, and a
/// single-manifest tag has no per-platform child to patch. An entry whose
/// platform or digest this build cannot read is skipped for the same reason
/// [`extract_platforms`] drops it — the pair goes unpatched, which is the safe
/// direction; only a *read failure* aborts.
fn index_children(
    tag: &str,
    result: ocx_lib::Result<(Digest, ocx_lib::oci::Manifest)>,
) -> Result<Vec<(Version, Platform, Digest)>, MirrorError> {
    let manifest = match result {
        Ok((_, manifest)) => manifest,
        Err(ocx_lib::Error::OciClient(ClientError::ManifestNotFound(_))) => return Ok(Vec::new()),
        Err(error) => {
            return Err(MirrorError::TargetError(format!(
                "failed to fetch manifest for tag '{tag}': {error}"
            )));
        }
    };

    let (Some(version), ocx_lib::oci::Manifest::ImageIndex(index)) = (Version::parse(tag), &manifest) else {
        return Ok(Vec::new());
    };

    let children = index
        .manifests
        .iter()
        .filter_map(|entry| {
            let platform = Platform::try_from(entry.platform.clone()?).ok()?;
            let digest = Digest::try_from(&entry.digest).ok()?;
            Some((version.clone(), platform, digest))
        })
        .collect();
    Ok(children)
}

/// Classifies a child-manifest fetch — every failure aborts (issue #157).
///
/// The index named this child a moment ago, so a not-found here is registry
/// inconsistency, not absence.
fn child_manifest(
    tag: &str,
    platform: &Platform,
    result: std::result::Result<ImageManifest, ClientError>,
) -> Result<ImageManifest, MirrorError> {
    result.map_err(|error| {
        MirrorError::TargetError(format!(
            "failed to fetch the {platform} manifest of tag '{tag}': {error}"
        ))
    })
}

/// Classifies a config-blob fetch and parse — every failure aborts (issue #157).
fn parse_metadata(
    version: &Version,
    platform: &Platform,
    result: std::result::Result<Vec<u8>, ClientError>,
) -> Result<Metadata, MirrorError> {
    let blob = result.map_err(|error| {
        MirrorError::TargetError(format!(
            "failed to fetch the metadata blob of {version} ({platform}): {error}"
        ))
    })?;
    serde_json::from_slice(&blob).map_err(|error| {
        MirrorError::TargetError(format!(
            "published metadata of {version} ({platform}) does not parse: {error}"
        ))
    })
}

/// Classifies a `list_tags` result — fail-safe (issue #157).
fn tags_from_result(result: ocx_lib::Result<Vec<String>>, identifier: &Identifier) -> Result<Vec<String>, MirrorError> {
    match result {
        Ok(tags) => Ok(tags),
        Err(ocx_lib::Error::OciClient(ClientError::RepositoryNotFound(_))) => Ok(Vec::new()),
        Err(error) => Err(MirrorError::TargetError(format!(
            "failed to list tags for {identifier}: {error}"
        ))),
    }
}

/// Classifies a per-tag `fetch_manifest` result — fail-safe (issue #157).
fn merge_manifest_result(
    tag: &str,
    result: ocx_lib::Result<(ocx_lib::oci::Digest, ocx_lib::oci::Manifest)>,
    platform_info: &mut BTreeMap<Version, HashSet<Platform>>,
) -> Result<(), MirrorError> {
    match result {
        Ok((_, manifest)) => {
            if let Some(version) = Version::parse(tag) {
                let platforms = extract_platforms(&manifest);
                if !platforms.is_empty() {
                    platform_info.entry(version).or_default().extend(platforms);
                }
            }
            Ok(())
        }
        Err(ocx_lib::Error::OciClient(ClientError::ManifestNotFound(_))) => Ok(()),
        Err(error) => Err(MirrorError::TargetError(format!(
            "failed to fetch manifest for tag '{tag}': {error}"
        ))),
    }
}

/// Fetches what a signature would be attached to, for every tag.
///
/// [`fetch_published_images`]'s sibling for the backfill, and deliberately not
/// a reuse of it: that walk drops every tag `Version::parse` refuses — which is
/// every rolling alias, `latest` included — and never reports the *index*
/// digest, which is the subject the index-level signature attaches to. Both
/// omissions are correct there and fatal here.
///
/// One manifest fetch per tag, and none per child: an image index already
/// names its children's digests, and the backfill signs a child by narrowing
/// its parent with `-p` rather than by addressing it.
///
/// Fail-safe on the same asymmetry the module doctrine sets (issue #157): an
/// authoritative "manifest not found" for a listed tag skips it, because
/// absence there is a fact the registry asserted. Every other failure aborts —
/// reading a failed manifest read as "this tag has no subjects" would report a
/// green run over content nothing signed.
pub(crate) async fn fetch_signing_subjects(
    publisher: &Publisher,
    identifier: &Identifier,
    tags: &[&str],
) -> Result<Vec<crate::pipeline::sign_backfill::PublishedTag>, MirrorError> {
    let mut published = Vec::new();
    for tag in signing_tags(tags) {
        let tag_identifier = identifier.clone_with_tag(tag.to_string());
        let result = publisher.client().fetch_manifest(&tag_identifier).await;
        if let Some(entry) = signing_subject(tag, result)? {
            published.push(entry);
        }
    }
    Ok(published)
}

/// The tags a backfill may address, with `ocx`'s reserved namespace removed.
///
/// Three kinds of tag in a listing are not published content and must never be
/// signed through:
///
/// - `__ocx.keep.<algorithm>-<hex>` pins a *platform manifest* against GC. It
///   resolves to a bare manifest, so signing through it files a second
///   signature against a subject the platform pass already covers — and does
///   it with no `-p`, through a reference no operator ever names.
/// - `__ocx.desc` and `__ocx.patch` address description and patch artifacts,
///   not a package.
/// - A bare `sha256-<hex>` tag **is the referrers fallback index** on a
///   registry without the Referrers API — which is to say, the signatures this
///   command just wrote. Signing those would grow without bound.
///
/// `Tag::is_reserved_str` is `ocx`'s own classifier for all three; a prefix
/// check here would be a fourth spelling of a rule that already exists.
fn signing_tags<'a>(tags: &[&'a str]) -> Vec<&'a str> {
    tags.iter()
        .copied()
        .filter(|tag| !ocx_lib::package::tag::Tag::is_reserved_str(tag))
        .collect()
}

/// Classifies one tag's manifest fetch into its signing subjects — the pure
/// half of [`fetch_signing_subjects`], so the fail-safe split is unit-testable
/// without a registry.
///
/// A child entry whose platform or digest this build cannot read is dropped
/// rather than aborting, matching [`index_children`]: the pair goes unsigned,
/// which is the safe direction — a re-run signs it once the build understands
/// it. A bare manifest yields no children and is itself the only subject.
fn signing_subject(
    tag: &str,
    result: ocx_lib::Result<(Digest, ocx_lib::oci::Manifest)>,
) -> Result<Option<crate::pipeline::sign_backfill::PublishedTag>, MirrorError> {
    let (digest, manifest) = match result {
        Ok(fetched) => fetched,
        Err(ocx_lib::Error::OciClient(ClientError::ManifestNotFound(_))) => return Ok(None),
        Err(error) => {
            return Err(MirrorError::TargetError(format!(
                "failed to fetch manifest for tag '{tag}': {error}"
            )));
        }
    };

    let children = match &manifest {
        ocx_lib::oci::Manifest::ImageIndex(index) => index
            .manifests
            .iter()
            .filter_map(|entry| {
                let platform = Platform::try_from(entry.platform.clone()?).ok()?;
                let child = Digest::try_from(&entry.digest).ok()?;
                Some((platform, child))
            })
            .collect(),
        ocx_lib::oci::Manifest::Image(_) => Vec::new(),
    };

    Ok(Some(crate::pipeline::sign_backfill::PublishedTag {
        tag: tag.to_string(),
        digest,
        children,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ocx`'s reserved namespace is not publishable content (WP 4).
    ///
    /// Found by the acceptance tier: a live repository carries a
    /// `__ocx.keep.sha256-<hex>` tag pinning its platform manifest, and a
    /// backfill that addressed it signed that manifest a second time — once
    /// through the keep tag with no `-p`, once through the platform pass.
    #[test]
    fn reserved_tags_are_not_signing_subjects() {
        let keep = format!("__ocx.keep.sha256-{}", "a".repeat(64));
        let fallback = format!("sha256-{}", "b".repeat(64));
        let tags = ["3.7.0", &keep, "__ocx.desc", "__ocx.patch", &fallback, "latest"];

        assert_eq!(signing_tags(&tags), vec!["3.7.0", "latest"]);
    }

    // ── Regression tests for issue #157 — fail-safe, not fail-open ────────
    //
    // The fail-open predecessors (`list_tags().unwrap_or_default()` and the
    // `if let Ok` / warn-and-continue manifest loops in plan.rs + sync.rs)
    // turned transient registry failures into "nothing published", re-flagging
    // published versions as New and re-pointing their tags on push.

    fn identifier() -> Identifier {
        Identifier::new_registry("mirror/cmake", "registry.test")
    }

    fn transient_error() -> ocx_lib::Error {
        ClientError::Registry("registry returned 503".into()).into()
    }

    // ── list_tags classification ──────────────────────────────────────────

    #[test]
    fn transient_list_tags_error_aborts() {
        // Fail-open would return an empty tag list here — every published
        // version would classify as New.
        let result = tags_from_result(Err(transient_error()), &identifier());
        assert!(
            matches!(result, Err(MirrorError::TargetError(_))),
            "transient list_tags failure must abort, got {result:?}"
        );
    }

    #[test]
    fn auth_error_aborts() {
        // Auth hiccups are not authoritative absence either — the catch-all
        // arm must abort for every non-not-found error class.
        let error = ClientError::Authentication("token exchange failed".into());
        let result = tags_from_result(Err(error.into()), &identifier());
        assert!(
            matches!(result, Err(MirrorError::TargetError(_))),
            "auth failure during list_tags must abort, got {result:?}"
        );
    }

    #[test]
    fn repository_not_found_means_no_tags() {
        // First publish of a new mirror repository: the registry
        // authoritatively reports the repository absent — legitimately empty.
        let error = ClientError::RepositoryNotFound("registry.test/mirror/cmake".to_string());
        let result = tags_from_result(Err(error.into()), &identifier());
        assert_eq!(result.expect("repository-absent is not an error"), Vec::<String>::new());
    }

    #[test]
    fn listed_tags_pass_through() {
        let tags = vec!["4.3.3".to_string(), "latest".to_string()];
        let result = tags_from_result(Ok(tags.clone()), &identifier());
        assert_eq!(result.expect("tags pass through"), tags);
    }

    // ── fetch_manifest classification ─────────────────────────────────────

    #[test]
    fn transient_manifest_error_aborts() {
        // Fail-open would skip the tag — the version's platform_info would
        // stay empty and the version would classify as New.
        let mut platform_info = BTreeMap::new();
        let result = merge_manifest_result("4.3.3", Err(transient_error()), &mut platform_info);
        assert!(
            matches!(result, Err(MirrorError::TargetError(_))),
            "transient fetch_manifest failure must abort, got {result:?}"
        );
    }

    #[test]
    fn manifest_not_found_skips_tag() {
        // Authoritative absence: tag listed but manifest deleted since —
        // safe to treat as not published.
        let error = ClientError::ManifestNotFound("registry.test/mirror/cmake:4.3.3".to_string());
        let mut platform_info = BTreeMap::new();
        let result = merge_manifest_result("4.3.3", Err(error.into()), &mut platform_info);
        result.expect("manifest-absent is not an error");
        assert!(platform_info.is_empty());
    }

    // ── child-manifest + config-blob classification ───────────────────────
    //
    // Same doctrine as above, one hop deeper: the drift scan reads a child
    // manifest and its config blob per published pair, and a failed read there
    // must not read as "this version's metadata is fine" (missed patch) nor as
    // "it differs" (fleet-wide republish).

    fn image_index(platform_digest: &str) -> ocx_lib::oci::Manifest {
        ocx_lib::oci::Manifest::ImageIndex(ocx_lib::oci::native::ImageIndex {
            schema_version: 2,
            media_type: Some("application/vnd.oci.image.index.v1+json".to_string()),
            artifact_type: None,
            manifests: vec![ocx_lib::oci::native::ImageIndexEntry {
                media_type: "application/vnd.oci.image.manifest.v1+json".to_string(),
                artifact_type: None,
                digest: platform_digest.to_string(),
                size: 1,
                platform: Some(ocx_lib::oci::native::Platform {
                    architecture: ocx_lib::oci::native::Arch::Amd64,
                    os: ocx_lib::oci::native::Os::Linux,
                    os_version: None,
                    os_features: None,
                    variant: None,
                    features: None,
                }),
                annotations: None,
            }],
            annotations: None,
        })
    }

    #[test]
    fn index_children_lists_platform_children() {
        let child = format!("sha256:{}", "c".repeat(64));
        let index = image_index(&child);
        let digest = Digest::Sha256("b".repeat(64));

        let children = index_children("4.3.3", Ok((digest, index))).expect("index classifies");

        assert_eq!(children.len(), 1);
        let (version, platform, child_digest) = &children[0];
        assert_eq!(version, &Version::parse("4.3.3").expect("valid version"));
        assert_eq!(platform, &"linux/amd64".parse::<Platform>().expect("valid platform"));
        assert_eq!(child_digest.to_string(), child);
    }

    #[test]
    fn transient_index_error_aborts_image_fetch() {
        // Fail-open would return no children — the version's published metadata
        // would never be compared and the drift would be reported as absent.
        let result = index_children("4.3.3", Err(transient_error()));
        assert!(
            matches!(result, Err(MirrorError::TargetError(_))),
            "transient index fetch failure must abort, got {result:?}"
        );
    }

    #[test]
    fn index_manifest_not_found_skips_tag_for_images() {
        let error = ClientError::ManifestNotFound("registry.test/mirror/cmake:4.3.3".to_string());
        let children = index_children("4.3.3", Err(error.into())).expect("manifest-absent is not an error");
        assert!(children.is_empty());
    }

    #[test]
    fn transient_child_manifest_error_aborts() {
        // The index named this child; a failed read of it is a failed read of
        // published state, never absence.
        let platform: Platform = "linux/amd64".parse().expect("valid platform");
        let error = ClientError::ManifestNotFound("registry.test/mirror/cmake@sha256:cc".to_string());
        let result = child_manifest("4.3.3", &platform, Err(error));
        assert!(
            matches!(result, Err(MirrorError::TargetError(_))),
            "a child manifest the index just listed must not read as absent, got {result:?}"
        );
    }

    #[test]
    fn published_config_blob_parses_to_metadata() {
        let version = Version::parse("4.3.3").expect("valid version");
        let platform: Platform = "linux/amd64".parse().expect("valid platform");
        let blob = br#"{"type":"bundle","version":1,"strip_components":1,"env":[]}"#.to_vec();

        let metadata = parse_metadata(&version, &platform, Ok(blob)).expect("config blob parses");
        assert_eq!(metadata.strip_components(), Some(1));
    }

    #[test]
    fn unparseable_config_blob_aborts() {
        // Treating an unreadable config as drift would republish every version
        // of every mirror the first time the metadata format moved.
        let version = Version::parse("4.3.3").expect("valid version");
        let platform: Platform = "linux/amd64".parse().expect("valid platform");
        let result = parse_metadata(&version, &platform, Ok(b"not json".to_vec()));
        assert!(
            matches!(result, Err(MirrorError::TargetError(_))),
            "a config blob that does not parse must abort, got {result:?}"
        );
    }

    #[test]
    fn transient_config_blob_error_aborts() {
        let version = Version::parse("4.3.3").expect("valid version");
        let platform: Platform = "linux/amd64".parse().expect("valid platform");
        let error = ClientError::Registry("registry returned 503".into());
        let result = parse_metadata(&version, &platform, Err(error));
        assert!(
            matches!(result, Err(MirrorError::TargetError(_))),
            "transient blob fetch failure must abort, got {result:?}"
        );
    }

    #[test]
    fn pinned_drops_the_tag() {
        // An identifier carrying both a tag and a digest addresses the tag, so
        // the digest-addressed fetch would read whatever the tag points at now.
        let digest = Digest::Sha256("c".repeat(64));
        let tagged = identifier().clone_with_tag("4.3.3");
        let pinned = pinned(&tagged, digest.clone()).expect("pins");
        assert!(pinned.tag().is_none(), "the tag must be dropped: {pinned}");
        assert_eq!(pinned.digest(), digest);
    }

    #[test]
    fn fetched_manifest_extends_platform_info() {
        let index = ocx_lib::oci::native::ImageIndex {
            schema_version: 2,
            media_type: Some("application/vnd.oci.image.index.v1+json".to_string()),
            artifact_type: None,
            manifests: vec![ocx_lib::oci::native::ImageIndexEntry {
                media_type: "application/vnd.oci.image.manifest.v1+json".to_string(),
                artifact_type: None,
                digest: format!("sha256:{}", "a".repeat(64)),
                size: 1,
                platform: Some(ocx_lib::oci::native::Platform {
                    architecture: ocx_lib::oci::native::Arch::Amd64,
                    os: ocx_lib::oci::native::Os::Linux,
                    os_version: None,
                    os_features: None,
                    variant: None,
                    features: None,
                }),
                annotations: None,
            }],
            annotations: None,
        };
        let manifest = ocx_lib::oci::Manifest::ImageIndex(index);
        let digest = ocx_lib::oci::Digest::Sha256("b".repeat(64));

        let mut platform_info = BTreeMap::new();
        merge_manifest_result("4.3.3", Ok((digest, manifest)), &mut platform_info).expect("manifest merges");

        let version = Version::parse("4.3.3").expect("valid version");
        let platforms = platform_info.get(&version).expect("version recorded");
        assert!(platforms.contains(&"linux/amd64".parse::<Platform>().expect("valid platform")));
    }
}
