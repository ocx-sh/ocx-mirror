// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::Result;
use ocx_lib::oci::LayerLayoutSpec;
use ocx_lib::package::info::Info;
use ocx_lib::package::version::Version;
use ocx_lib::publisher::{LayerRef, Publisher};

use super::mirror_result::MirrorResult;
use super::mirror_task::VariantContext;

/// Push a bundled package to the registry and optionally cascade to rolling tags.
///
/// `cascade_versions` is the set of build-tagged versions used to compute
/// cascade blockers. Rolling tags are excluded — build-tagged versions already
/// provide correct blocking semantics.
///
/// When `variant` indicates a default variant, a second cascade pass generates
/// unadorned alias tags (e.g., `3.12.5`, `3.12`, `3`, `latest`) pointing to
/// the same manifest as the variant-prefixed tags.
///
/// `annotations` are the OCI annotations for this run (see [`crate::annotations`]),
/// written onto the image index of every tag the push touches.
pub async fn push_and_cascade(
    publisher: &Publisher,
    info: Info,
    bundle_path: &Path,
    cascade: bool,
    cascade_versions: &BTreeSet<Version>,
    variant: Option<&VariantContext>,
    annotations: &BTreeMap<String, String>,
) -> Result<MirrorResult> {
    let version_str = info.identifier.tag_or_latest().to_string();
    let platform = info.platform.clone();
    // Mirror publishes each bundle as a whole archive layer — no per-layer
    // strip/prefix rewriting, so the layout stays at its default (none). The
    // bundle was just built from freshly downloaded upstream assets, so no
    // repository in the target registry already holds the blob: there is no
    // cross-repository mount source to try.
    let layers = [LayerRef::File {
        path: bundle_path.to_path_buf(),
        layout: LayerLayoutSpec::default(),
        mount_from: None,
    }];

    // `true` matches the `ocx package push` default, which is what the pipeline
    // push path (`command::package::pipeline::push`) already gets by shelling
    // out — both mirror publish paths write the digest-named safety-net tag.
    let canonical_tag = true;

    if cascade {
        publisher
            .push_cascade(
                vec![info.clone()],
                &layers,
                cascade_versions.clone(),
                None,
                canonical_tag,
                annotations,
            )
            .await?;

        // Default variant aliasing: generate unadorned tags for the default variant.
        // e.g., pushing `pgo.lto-3.12.5_b1` also cascades `3.12.5`, `3.12`, `3`, `latest`.
        if let Some(ctx) = variant
            && ctx.is_default
            && let Some(version) = Version::parse(&version_str)
            && version.variant().is_some()
        {
            let bare = version.without_variant();
            let bare_tag = bare.to_string();
            let bare_id = info.identifier.clone_with_tag(bare_tag);
            let bare_info = Info {
                identifier: bare_id,
                metadata: info.metadata.clone(),
                platform: info.platform,
            };
            publisher
                .push_cascade(
                    vec![bare_info],
                    &layers,
                    cascade_versions.clone(),
                    None,
                    canonical_tag,
                    annotations,
                )
                .await?;
        }

        return Ok(MirrorResult::Pushed {
            version: version_str,
            platform,
            digest: String::new(),
        });
    }

    publisher
        .push(vec![info], &layers, None, canonical_tag, annotations)
        .await?;

    Ok(MirrorResult::Pushed {
        version: version_str,
        platform,
        digest: String::new(),
    })
}
