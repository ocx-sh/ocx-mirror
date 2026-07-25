// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! OCI annotations recorded on every image index this mirror publishes.
//!
//! Two keys are auto-detected from the CI environment — `image.source` (the
//! mirror's own repository, which is what GHCR uses to link a package to a
//! repository and inherit its permissions) and `image.revision` (the commit
//! that produced the build). The spec's `annotations:` block supplies further
//! keys and overrides any auto-detected one.
//!
//! A published index is public, permanent and readable without
//! authentication, so the environment surface is a fixed allowlist of three
//! named variables and nothing else: the `ocx` subprocess inherits the
//! runner's full environment (including `GH_TOKEN`), and anything resembling
//! iteration over it would put a live token on the wire.

use std::collections::BTreeMap;

use ocx_lib::oci::annotations;

/// Forge base URL, e.g. `https://github.com`.
const ENV_SERVER_URL: &str = "GITHUB_SERVER_URL";
/// `owner/repo` of the mirror repository running the publish.
const ENV_REPOSITORY: &str = "GITHUB_REPOSITORY";
/// Commit SHA the workflow is running against.
const ENV_REVISION: &str = "GITHUB_SHA";

/// Build the annotation set for a publish, merging CI auto-detection with the
/// spec's `annotations:` block. A configured key wins over the auto-detected
/// value for that key; every other auto-detected key still applies.
///
/// Outside CI the variables are absent and their annotations are simply not
/// emitted — `ocx package push` leaves the index's `annotations` field as it
/// found it, so a local push never clears a link an earlier CI push made.
pub fn build_annotations(configured: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    assemble(configured, |name| std::env::var(name).ok())
}

/// The pure half of [`build_annotations`]: `lookup` resolves an environment
/// variable name. Split out so tests can record exactly which names are read —
/// the allowlist is a security boundary, which wants a test rather than a
/// comment. `pub(crate)` for the argv-boundary test in
/// `command::package::pipeline::push`, which needs the same seam.
pub(crate) fn assemble<F>(configured: &BTreeMap<String, String>, mut lookup: F) -> BTreeMap<String, String>
where
    F: FnMut(&str) -> Option<String>,
{
    let mut nonempty = |name: &str| lookup(name).filter(|value| !value.trim().is_empty());

    let mut map = BTreeMap::new();

    // Both halves are required: half a URL is not a repository.
    if let (Some(server), Some(repository)) = (nonempty(ENV_SERVER_URL), nonempty(ENV_REPOSITORY)) {
        map.insert(
            annotations::SOURCE.to_string(),
            format!("{}/{}", server.trim_end_matches('/'), repository),
        );
    }
    if let Some(revision) = nonempty(ENV_REVISION) {
        map.insert(annotations::REVISION.to_string(), revision);
    }

    map.extend(configured.iter().map(|(k, v)| (k.clone(), v.clone())));
    map
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

    /// The complete environment surface `assemble` is allowed to read. Pinned
    /// by `reads_only_the_named_allowlist`: widening it needs a deliberate
    /// edit here, and the module doc explains why that is a security decision.
    const ENV_ALLOWLIST: [&str; 3] = [ENV_SERVER_URL, ENV_REPOSITORY, ENV_REVISION];

    /// A runner environment carrying a token, as the generated workflow's push
    /// job does (`GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}`).
    const TOKEN: &str = "ghs_liveTokenFromTheRunnerEnvironment";

    fn ci_env(name: &str) -> Option<String> {
        match name {
            ENV_SERVER_URL => Some("https://github.com".to_string()),
            ENV_REPOSITORY => Some("ocx-sh/mirror-shfmt".to_string()),
            ENV_REVISION => Some("a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2".to_string()),
            _ => None,
        }
    }

    fn configured(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn full_ci_env_yields_source_and_revision() {
        let result = assemble(&BTreeMap::new(), ci_env);
        assert_eq!(result[annotations::SOURCE], "https://github.com/ocx-sh/mirror-shfmt");
        assert_eq!(
            result[annotations::REVISION],
            "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2"
        );
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn server_url_trailing_slash_does_not_double() {
        let result = assemble(&BTreeMap::new(), |name| match name {
            ENV_SERVER_URL => Some("https://github.example.com/".to_string()),
            other => ci_env(other),
        });
        assert_eq!(
            result[annotations::SOURCE],
            "https://github.example.com/ocx-sh/mirror-shfmt"
        );
    }

    #[test]
    fn missing_server_url_drops_source_but_keeps_revision() {
        let result = assemble(&BTreeMap::new(), |name| match name {
            ENV_SERVER_URL => None,
            other => ci_env(other),
        });
        assert!(!result.contains_key(annotations::SOURCE));
        assert!(result.contains_key(annotations::REVISION));
    }

    #[test]
    fn missing_repository_drops_source_but_keeps_revision() {
        let result = assemble(&BTreeMap::new(), |name| match name {
            ENV_REPOSITORY => None,
            other => ci_env(other),
        });
        assert!(!result.contains_key(annotations::SOURCE));
        assert!(result.contains_key(annotations::REVISION));
    }

    #[test]
    fn missing_revision_drops_revision_but_keeps_source() {
        let result = assemble(&BTreeMap::new(), |name| match name {
            ENV_REVISION => None,
            other => ci_env(other),
        });
        assert!(result.contains_key(annotations::SOURCE));
        assert!(!result.contains_key(annotations::REVISION));
    }

    #[test]
    fn blank_env_value_counts_as_absent() {
        let result = assemble(&BTreeMap::new(), |name| match name {
            ENV_REVISION => Some("   ".to_string()),
            other => ci_env(other),
        });
        assert!(!result.contains_key(annotations::REVISION));
    }

    #[test]
    fn outside_ci_nothing_is_emitted() {
        let result = assemble(&BTreeMap::new(), |_| None);
        assert!(result.is_empty());
    }

    #[test]
    fn configured_key_overrides_auto_detected_source() {
        let result = assemble(
            &configured(&[(annotations::SOURCE, "https://github.com/upstream/project")]),
            ci_env,
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
        let result = assemble(&configured(&[(annotations::LICENSES, "Apache-2.0")]), ci_env);
        assert_eq!(result[annotations::LICENSES], "Apache-2.0");
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn reads_only_the_named_allowlist() {
        let mut seen: Vec<String> = Vec::new();
        let _ = assemble(&BTreeMap::new(), |name| {
            seen.push(name.to_string());
            None
        });
        seen.sort();
        seen.dedup();
        let mut expected: Vec<String> = ENV_ALLOWLIST.iter().map(|s| (*s).to_string()).collect();
        expected.sort();
        assert_eq!(seen, expected);
    }

    #[test]
    fn secret_shaped_env_never_reaches_an_annotation() {
        // The `ocx` subprocess inherits the runner environment, so a lookup
        // outside the allowlist is the leak path. Answer every non-allowlisted
        // name with a token: any that reaches the map is a hole.
        let result = assemble(&configured(&[("sh.ocx.mirror.note", "no secrets here")]), |name| {
            if ENV_ALLOWLIST.contains(&name) {
                ci_env(name)
            } else {
                Some(TOKEN.to_string())
            }
        });
        assert!(
            result.values().all(|value| !value.contains(TOKEN)),
            "token leaked into annotations: {result:?}"
        );
        // Same guarantee on the wire form the subprocess actually receives.
        assert!(push_args(&result).iter().all(|arg| !arg.contains(TOKEN)));
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
