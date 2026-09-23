// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! OCI annotations recorded on every image index this mirror publishes.
//!
//! The CI half is `ocx`'s own: `ocx package push --ci-annotations=<provider>`
//! stamps `image.source` (the mirror's own repository, which is what GHCR uses
//! to link a package to a repository and inherit its permissions),
//! `image.revision` (the commit that produced the build), `image.created` and
//! `image.version` — so a mirror and a hand push stamp the same keys from the
//! same names, because they are one implementation rather than two. The
//! spec's `annotations:` block rides along as `--annotation KEY=VALUE` and
//! overrides any auto-detected key, the precedence `--ci-annotations` itself
//! promises.
//!
//! The provider is named rather than autodetected: a bare `--ci-annotations`
//! outside a detectable CI is a usage error (exit 64), and a local
//! `ocx-mirror` run must not fail on one. Outside CI the flag is simply not
//! emitted — `ocx package push` then leaves the index's `annotations` field as
//! it found it, so a local push never clears a link an earlier CI push made.
//!
//! A published index is public, permanent and readable without
//! authentication, so the environment surface stays the fixed read set `ocx`
//! documents for the flag and nothing else: the `ocx` subprocess inherits the
//! runner's full environment (including `GH_TOKEN`), and anything resembling
//! iteration over it would put a live token on the wire.

use std::collections::BTreeMap;

/// Build the annotation overlay for a run: the spec's `annotations:` block,
/// plus the run's own `image.created` on GitHub Actions.
///
/// **Why `created` is pinned here.** `--ci-annotations` takes it from
/// `SOURCE_DATE_EPOCH`, else the provider's pipeline clock, else the wall
/// clock — and GitHub Actions exposes no pipeline-creation variable. A run
/// pushes each platform of a version as its own `ocx package push`, so the
/// wall-clock fallback would date one version's platforms minutes apart.
/// Resolved once per run instead, through `ocx`'s own reader and spelling, so
/// a pinned `SOURCE_DATE_EPOCH` still wins and the value is the one `ocx`
/// would have written. GitLab needs nothing: `CI_PIPELINE_CREATED_AT` is
/// already one instant for the whole pipeline.
///
/// A configured key wins over the pinned one, the same precedence `ocx`
/// applies to every other auto-detected key.
///
/// No `image.version`: that key is `--ci-annotations`' own, resolved per push
/// from the tag it is writing, which is the one value a per-run map cannot
/// hold.
pub fn build_annotations(configured: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    build_annotations_for(ci_provider(), configured)
}

/// [`build_annotations`] with the CI detection already made — the seam a test
/// reaches to state which provider the run is built for.
// `pub` only because the root crate's push tests reach it across the crate
// boundary; not part of the documented surface.
#[doc(hidden)]
pub fn build_annotations_for(
    provider: Option<&str>,
    configured: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut annotations = BTreeMap::new();
    if provider == Some(GITHUB) {
        annotations.insert(
            ocx_oci::annotations::CREATED.to_string(),
            ocx_oci::referrer::manifest::bundle_created(
                ocx_oci::referrer::manifest::pinned_instant().unwrap_or_else(chrono::Utc::now),
            ),
        );
    }
    annotations.extend(configured.iter().map(|(key, value)| (key.clone(), value.clone())));
    annotations
}

/// `--ci-annotations`' spelling for GitHub Actions.
const GITHUB: &str = "github";

/// The `--ci-annotations` provider for this process, or `None` outside CI —
/// the two markers `ocx`'s own detection reads, under the spelling its flag
/// accepts.
fn ci_provider() -> Option<&'static str> {
    if ocx_util::env::var("GITHUB_ACTIONS").as_deref() == Some("true") {
        return Some(GITHUB);
    }
    if ocx_util::env::var("GITLAB_CI").as_deref() == Some("true") {
        return Some("gitlab");
    }
    None
}

/// Render a run's annotations as `ocx package push` arguments: the CI provider
/// flag when there is one, then the overlay as repeated
/// `--annotation KEY=VALUE`.
///
/// Ordering is the map's (lexicographic by key), so an assembled argv is
/// reproducible across runs.
pub fn push_args(annotations: &BTreeMap<String, String>) -> Vec<String> {
    push_args_for(ci_provider(), annotations)
}

/// [`push_args`] with the CI detection already made.
pub(crate) fn push_args_for(provider: Option<&str>, annotations: &BTreeMap<String, String>) -> Vec<String> {
    provider
        .map(|provider| format!("--ci-annotations={provider}"))
        .into_iter()
        .chain(
            annotations
                .iter()
                .flat_map(|(key, value)| ["--annotation".to_string(), format!("{key}={value}")]),
        )
        .collect()
}

/// Reject annotation keys that cannot survive the `KEY=VALUE` wire form: an
/// empty key is rejected by `ocx package push`, and a key containing `=` would
/// be re-split at the wrong place and publish a different key than configured.
pub fn validate(configured: &BTreeMap<String, String>, errors: &mut Vec<String>) {
    for key in configured.keys() {
        if key.trim().is_empty() {
            errors.push("annotations: key must not be empty".to_string());
        } else if key.contains('=') {
            errors.push(format!("annotations: key '{key}' must not contain '='"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocx_oci::annotations;

    fn configured(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    /// Inside CI the provider is named, never left to `ocx`'s autodetect: the
    /// bare flag is a usage error wherever detection fails.
    #[test]
    fn a_ci_run_names_its_provider_before_the_configured_keys() {
        let args = push_args_for(
            Some("gitlab"),
            &configured(&[
                (annotations::SOURCE, "https://github.com/upstream/project"),
                (annotations::LICENSES, "Apache-2.0"),
            ]),
        );
        assert_eq!(
            args,
            vec![
                "--ci-annotations=gitlab",
                "--annotation",
                "org.opencontainers.image.licenses=Apache-2.0",
                "--annotation",
                "org.opencontainers.image.source=https://github.com/upstream/project",
            ]
        );
    }

    /// Outside CI the argv is the spec's block and nothing else, whatever the
    /// developer's own `GITHUB_*` say.
    #[test]
    fn outside_ci_no_provider_flag_is_emitted() {
        assert_eq!(
            push_args_for(None, &configured(&[(annotations::LICENSES, "Apache-2.0")])),
            vec!["--annotation", "org.opencontainers.image.licenses=Apache-2.0"]
        );
        assert!(push_args_for(None, &BTreeMap::new()).is_empty());
    }

    /// GitLab and a local run add nothing to the spec's block; GitHub adds the
    /// run's `created`, and a configured one still wins.
    #[test]
    fn only_github_pins_created_and_a_configured_key_still_wins() {
        let _guard = ocx_mirror_test_support::ocx_env_lock();
        let _restore = ocx_mirror_test_support::EnvRestore::set(&[("SOURCE_DATE_EPOCH", Some("1700000000"))]);
        let spec_block = configured(&[(annotations::LICENSES, "Apache-2.0")]);

        assert_eq!(build_annotations_for(None, &spec_block), spec_block);
        assert_eq!(build_annotations_for(Some("gitlab"), &spec_block), spec_block);

        let github = build_annotations_for(Some(GITHUB), &spec_block);
        assert_eq!(github[annotations::CREATED], "2023-11-14T22:13:20Z");
        assert_eq!(github[annotations::LICENSES], "Apache-2.0");

        let pinned_by_spec = build_annotations_for(
            Some(GITHUB),
            &configured(&[(annotations::CREATED, "2020-01-01T00:00:00Z")]),
        );
        assert_eq!(pinned_by_spec[annotations::CREATED], "2020-01-01T00:00:00Z");
    }

    /// The detection is `ocx`'s: the marker must read exactly `true`, and
    /// GitHub wins over GitLab when a runner sets both.
    #[test]
    fn the_provider_is_read_from_the_two_ci_markers() {
        let _guard = ocx_mirror_test_support::ocx_env_lock();

        let _restore = ocx_mirror_test_support::EnvRestore::set(&[("GITHUB_ACTIONS", None), ("GITLAB_CI", None)]);
        assert_eq!(ci_provider(), None);

        let _gitlab = ocx_mirror_test_support::EnvRestore::set(&[("GITLAB_CI", Some("true"))]);
        assert_eq!(ci_provider(), Some("gitlab"));

        let _github = ocx_mirror_test_support::EnvRestore::set(&[("GITHUB_ACTIONS", Some("true"))]);
        assert_eq!(ci_provider(), Some(GITHUB));

        let _not_true = ocx_mirror_test_support::EnvRestore::set(&[("GITHUB_ACTIONS", Some("1"))]);
        assert_eq!(ci_provider(), Some("gitlab"));
    }

    #[test]
    fn validate_rejects_empty_and_equals_bearing_keys() {
        let mut errors = Vec::new();
        validate(&configured(&[("", "v"), ("a=b", "v"), ("fine.key", "v")]), &mut errors);
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(errors.iter().any(|e| e.contains("must not be empty")));
        assert!(errors.iter().any(|e| e.contains("must not contain '='")));
    }
}
