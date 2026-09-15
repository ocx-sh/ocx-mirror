// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! OCI annotations recorded on every image index this mirror publishes.
//!
//! The CI half is `ocx_lib`'s: [`ocx_lib::ci::annotations::for_flavor`] reads
//! the same GitHub Actions / GitLab CI variables `ocx package push
//! --ci-annotations` does — `image.source` (the mirror's own repository,
//! which is what GHCR uses to link a package to a repository and inherit its
//! permissions), `image.revision` (the commit that produced the build) and
//! `image.created` — so a mirror and a hand push stamp the same keys from the
//! same names. The spec's `annotations:` block supplies further keys and
//! overrides any auto-detected one, the precedence `--ci-annotations` itself
//! promises.
//!
//! A published index is public, permanent and readable without
//! authentication, so the environment surface is the fixed, pinned read set
//! `ocx_lib::ci::annotations` tests against and nothing else: the `ocx`
//! subprocess inherits the runner's full environment (including `GH_TOKEN`),
//! and anything resembling iteration over it would put a live token on the
//! wire.

use std::collections::BTreeMap;

use ocx_lib::ci::CiFlavor;

/// Build the annotation set for a publish, merging CI auto-detection with the
/// spec's `annotations:` block. A configured key wins over the auto-detected
/// value for that key; every other auto-detected key still applies.
///
/// Outside CI (`CiFlavor::detect()` finds neither provider) nothing is
/// auto-detected — `ocx package push` leaves the index's `annotations` field
/// as it found it, so a local push never clears a link an earlier CI push
/// made.
///
/// No `image.version`: this map is built once per run and reused for every
/// version the run pushes, and the tag already names the version.
// ponytail: thread the per-push `Version` through `build_push_args` if an
// SBOM consumer ever needs the version key on the index.
pub fn build_annotations(configured: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    build_annotations_for(CiFlavor::detect(), configured)
}

/// [`build_annotations`] with the CI detection already made — the seam a
/// test reaches to state which provider's variables the map is read from.
pub(crate) fn build_annotations_for(
    flavor: Option<CiFlavor>,
    configured: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let auto = flavor
        .map(|flavor| ocx_lib::ci::annotations::for_flavor(flavor, None))
        .unwrap_or_default();
    overlay(auto, configured)
}

/// The pure half of [`build_annotations`]: the spec's `annotations:` laid over
/// the auto-detected set, configured winning per key.
pub(crate) fn overlay(
    mut auto: BTreeMap<String, String>,
    configured: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    auto.extend(configured.iter().map(|(k, v)| (k.clone(), v.clone())));
    auto
}

/// Render an annotation map as repeated `--annotation KEY=VALUE` arguments.
///
/// Ordering is the map's (lexicographic by key), so an assembled argv is
/// reproducible across runs.
pub fn push_args(annotations: &BTreeMap<String, String>) -> Vec<String> {
    annotations
        .iter()
        .flat_map(|(key, value)| ["--annotation".to_string(), format!("{key}={value}")])
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
    use ocx_lib::oci::annotations;

    fn configured(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn auto() -> BTreeMap<String, String> {
        configured(&[
            (annotations::SOURCE, "https://github.com/ocx-sh/mirror-shfmt"),
            (annotations::REVISION, "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2"),
        ])
    }

    #[test]
    fn configured_key_overrides_auto_detected_source() {
        let result = overlay(
            auto(),
            &configured(&[(annotations::SOURCE, "https://github.com/upstream/project")]),
        );
        assert_eq!(result[annotations::SOURCE], "https://github.com/upstream/project");
        // The other auto-detected key survives the override.
        assert_eq!(
            result[annotations::REVISION],
            "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2"
        );
    }

    #[test]
    fn configured_keys_beyond_the_auto_detected_ones_pass_through() {
        let result = overlay(auto(), &configured(&[(annotations::LICENSES, "Apache-2.0")]));
        assert_eq!(result[annotations::LICENSES], "Apache-2.0");
        assert_eq!(result.len(), 3);
    }

    /// The CI read set is `ocx_lib`'s, pinned there (`ci::annotations::tests`):
    /// GitHub's three names and GitLab's three, nothing that could carry a
    /// token. Whatever the runner looks like, the configured keys always win
    /// and always arrive.
    #[test]
    fn configured_keys_always_reach_the_built_map() {
        let configured = configured(&[
            (annotations::LICENSES, "Apache-2.0"),
            (annotations::SOURCE, "https://github.com/upstream/project"),
        ]);
        let result = build_annotations(&configured);
        for (key, value) in &configured {
            assert_eq!(result.get(key), Some(value));
        }
    }

    /// Outside CI nothing is auto-detected: the map is the spec's block and
    /// nothing else, whatever the developer's own `GITHUB_*` say.
    #[test]
    fn outside_ci_the_built_map_is_exactly_the_configured_one() {
        let configured = configured(&[(annotations::LICENSES, "Apache-2.0")]);
        assert_eq!(build_annotations_for(None, &configured), configured);
    }

    /// The GitLab wiring end to end: `for_flavor` reads the three `CI_*`
    /// names, `created` comes from the pipeline clock, and the configured
    /// `image.source` still wins over the job's own.
    #[test]
    fn a_gitlab_job_stamps_source_revision_and_created_with_configured_source_winning() {
        let _guard = crate::test_support::ocx_env_lock();
        let _restore = crate::test_support::EnvRestore::set(&[
            ("GITLAB_CI", Some("true")),
            ("CI_PROJECT_URL", Some("https://gitlab.example/tools/mirror-shfmt")),
            ("CI_COMMIT_SHA", Some("a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2")),
            ("CI_PIPELINE_CREATED_AT", Some("2026-09-15T18:59:14Z")),
            ("SOURCE_DATE_EPOCH", None),
        ]);
        let result = build_annotations_for(
            Some(CiFlavor::GitLab),
            &configured(&[(annotations::SOURCE, "https://github.com/upstream/project")]),
        );

        assert_eq!(result[annotations::SOURCE], "https://github.com/upstream/project");
        assert_eq!(
            result[annotations::REVISION],
            "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2"
        );
        assert_eq!(result[annotations::CREATED], "2026-09-15T18:59:14Z");
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn push_args_render_repeated_flag_pairs_in_key_order() {
        let args = push_args(&configured(&[
            (annotations::SOURCE, "https://github.com/ocx-sh/mirror-shfmt"),
            (annotations::REVISION, "a1b2c3d4"),
        ]));
        assert_eq!(
            args,
            vec![
                "--annotation",
                "org.opencontainers.image.revision=a1b2c3d4",
                "--annotation",
                "org.opencontainers.image.source=https://github.com/ocx-sh/mirror-shfmt",
            ]
        );
    }

    #[test]
    fn push_args_are_empty_for_an_empty_map() {
        assert!(push_args(&BTreeMap::new()).is_empty());
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
