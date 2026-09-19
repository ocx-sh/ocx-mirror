// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::Result;
use ocx_oci::LayerLayoutSpec;
use ocx_oci::LayerRef;
use ocx_package::info::Info;
use ocx_package::publisher::Publisher;
use ocx_package::version::Version;

use super::mirror_result::MirrorResult;
use super::mirror_task::VariantContext;
use super::ocx_cli::sign::{ResolvedSign, invoke_sign_reference};

/// Push a bundled package to the registry and optionally cascade to rolling tags.
///
/// `cascade_versions` is the set of build-tagged versions used to compute
/// cascade blockers. Rolling tags are excluded — build-tagged versions already
/// provide correct blocking semantics.
///
/// When `variant` indicates a default variant, the cascade push also writes
/// the unadorned alias tags (e.g., `3.12.5`, `3.12`, `3`, `latest`) pointing
/// to the same manifest as the variant-prefixed tags — `Publisher`'s own
/// `default` track, re-tagged through the index, no second upload.
///
/// `annotations` are the OCI annotations for this run (see [`crate::annotations`]),
/// written onto the image index of every tag the push touches.
///
/// `sign` is the run's resolved `sign:` block. This leg publishes through
/// `ocx_package`'s [`Publisher`] rather than an `ocx package push` subprocess, so
/// there is no `--sign` to pass: the platform manifest is signed afterwards,
/// by `ocx package sign -p` (C-059). The enclosing index is signed once the
/// version's last platform has landed — by `orchestrator::execute_mirror`,
/// which is the only caller that knows where a version ends. Same division of
/// labour as `push --sign` plus the closing sweep on the subprocess legs (D2).
#[expect(
    clippy::too_many_arguments,
    reason = "the push identity (publisher, info, bundle, cascade, versions, variant, annotations) plus the signing block; grouping them would name a struct nothing else constructs"
)]
pub async fn push_and_cascade(
    publisher: &Publisher,
    info: Info,
    bundle_path: &Path,
    cascade: bool,
    cascade_versions: &BTreeSet<Version>,
    variant: Option<&VariantContext>,
    annotations: &BTreeMap<String, String>,
    sign: Option<&ResolvedSign>,
) -> Result<MirrorResult> {
    let version_str = info.identifier.tag_or_latest().to_string();
    let platform = info.platform.clone();
    // `Display` on an `Identifier` is `registry/repository:tag` — the exact
    // reference `ocx package sign` takes. Captured before `info` is moved into
    // the push.
    let signed_ref = info.identifier.to_string();
    // ponytail: default layout (no strip/prefix) preserves pre-bump behavior
    // exactly. Archive/binary pushes never cross-repository mount — only the
    // pylock env-push path's wheel layers carry `mount_from`.
    let layers = [LayerRef::File {
        path: bundle_path.to_path_buf(),
        layout: LayerLayoutSpec::default(),
        mount_from: None,
    }];

    // `true` matches the `ocx package push` default, which is what the pipeline
    // push path (`command::package::pipeline::push`) already gets by shelling
    // out — both mirror publish paths write the digest-named safety-net tag.
    let canonical_tag = true;

    // Every push below discards its `PushOutcome` deliberately: this leg pushes
    // one self-contained bundle layer that can never be mounted, so
    // `layer_counts` carries nothing to report, and the sync path it serves
    // writes no run-summary. Layer reuse is reported only on the env-push leg
    // (`python_push`), whose layers do carry `:from=` mount tails.

    if cascade {
        // Default variant aliasing: `default` re-tags the pushed manifest onto
        // the bare track too — e.g. `pgo.lto-3.12.5_b1` also cascades `3.12.5`,
        // `3.12`, `3`, `latest`. A version carrying no variant writes no alias.
        // Nothing extra is signed for it: the bare alias is the same manifest
        // under a second tag (`test_default_variant_aliases_the_bare_tags_to_its_own_manifest`
        // asserts every bare tag resolves to the default variant's own digest,
        // and a non-default variant gets none). A signature is a referrer
        // against the subject digest, not the tag, so the one call below
        // covers the alias — a second would spend another candidate against
        // the verifier's cap.
        let default = variant.is_some_and(|ctx| ctx.is_default);
        publisher
            .push_cascade(
                vec![info],
                &layers,
                cascade_versions.clone(),
                None,
                canonical_tag,
                default,
                annotations,
            )
            .await?;

        sign_platform(sign, &signed_ref, &platform.to_string()).await?;
        return Ok(MirrorResult::Pushed {
            version: version_str,
            platform,
            digest: String::new(),
        });
    }

    publisher
        .push(vec![info], &layers, None, canonical_tag, false, annotations)
        .await?;

    sign_platform(sign, &signed_ref, &platform.to_string()).await?;

    Ok(MirrorResult::Pushed {
        version: version_str,
        platform,
        digest: String::new(),
    })
}

/// Sign one platform manifest of `reference`, or do nothing without `sign:`.
///
/// A failed signature fails the leg, exactly as a failed `push --sign` fails
/// the subprocess legs: a package that published and did not sign must never
/// read as a clean publish (S-050). The manifest is already in the registry by
/// then — the exit code is how that partial outcome reaches the operator.
async fn sign_platform(sign: Option<&ResolvedSign>, reference: &str, platform: &str) -> Result<()> {
    let Some(resolved) = sign else { return Ok(()) };
    Ok(invoke_sign_reference(resolved, reference, Some(platform)).await?)
}

#[cfg(test)]
#[path = "push/tests.rs"]
mod tests;
