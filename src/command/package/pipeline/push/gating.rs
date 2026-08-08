// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Which container legs gate a given platform entry.
//!
//! An entry carrying a `+libc.<flavor>` feature is judged only by containers
//! running that libc — a musl entry is not held back by a glibc leg's result,
//! and vice versa. A featureless entry is gated by all of them.

use crate::run_summary::ExcludedPlatform;
use crate::spec::{self, MirrorSpec, PlatformConfig, Severity};

/// The base `os/arch` half of an env entry's full wheels-key platform string
/// (`linux/amd64+libc.glibc` → `linux/amd64`). Spec lookups (`platforms:`
/// order, containers, tests) are keyed by base — a full-key lookup would miss
/// silently and fall back to `_native_`/`usize::MAX`.
pub fn base_platform_str(platform: &str) -> &str {
    platform.split('+').next().unwrap_or(platform)
}

/// The `libc.*` os_feature declared on an env entry's platform string, if any.
pub fn entry_libc_feature(platform: &str) -> Option<&str> {
    let (_, features) = platform.split_once('+')?;
    features.split('+').find(|feature| feature.starts_with("libc."))
}

/// The container IDs whose JUnit files gate one env entry, filtered by libc
/// compatibility: a featureless entry is gated by EVERY container of its base
/// platform (it claims to run on any libc, so all legs must be green); a
/// `libc.glibc` entry only by gnu containers; a `libc.musl` entry only by
/// musl (alpine) containers. A native leg (`_native_`) counts as gnu — GHA
/// runners are glibc. An empty result means no test leg covers the entry's
/// declared libc; the caller fails closed.
pub fn gating_container_ids_for_entry(spec: &MirrorSpec, base_platform: &str, libc: Option<&str>) -> Vec<String> {
    let containers = spec
        .platforms
        .as_ref()
        .and_then(|platforms| platforms.get(base_platform))
        .and_then(|config| config.containers.as_deref())
        .filter(|containers| !containers.is_empty());

    let Some(containers) = containers else {
        // Native leg: a glibc runner — gates featureless and glibc entries.
        return match libc {
            None | Some("libc.glibc") => vec!["_native_".to_string()],
            _ => Vec::new(),
        };
    };

    let wanted = match libc {
        None => None, // featureless: every container gates
        Some("libc.musl") => Some("musl"),
        // `libc.glibc` (any other feature namespace is rejected at spec
        // validation) gates on gnu containers.
        Some(_) => Some("gnu"),
    };
    containers
        .iter()
        .filter(|container| wanted.is_none_or(|libc| spec::infer_libc_from_image(&container.image) == libc))
        .map(|container| {
            container
                .id
                .clone()
                .unwrap_or_else(|| spec::image_to_container_id(&container.image))
        })
        .collect()
}

/// Returns platforms in spec declaration order.
pub fn spec_platform_order(spec: &MirrorSpec) -> Vec<String> {
    // IndexMap preserves insertion order; HashMap does not. The spec `platforms`
    // field is a `HashMap<String, PlatformConfig>`. We sort alphabetically as a
    // deterministic fallback when declaration order is not preserved.
    let Some(platforms) = &spec.platforms else {
        return Vec::new();
    };
    let mut keys: Vec<String> = platforms.keys().cloned().collect();
    keys.sort();
    keys
}

/// Returns the container IDs expected for a platform.
///
/// Container mode → slugified image names (`:` and `/` replaced by `_`).
/// Native mode → single entry `_native_`.
pub fn container_ids_for_platform(spec: &MirrorSpec, platform_str: &str) -> Vec<String> {
    let Some(platforms) = &spec.platforms else {
        return vec!["_native_".to_string()];
    };

    let Some(config) = platforms.get(platform_str) else {
        return vec!["_native_".to_string()];
    };

    container_ids_from_config(config)
}

pub fn container_ids_from_config(config: &PlatformConfig) -> Vec<String> {
    match &config.containers {
        None => vec!["_native_".to_string()],
        Some(containers) if containers.is_empty() => vec!["_native_".to_string()],
        Some(containers) => containers
            .iter()
            .map(|c| {
                c.id.clone().unwrap_or_else(|| {
                    // Default slug: image with `:` and `/` replaced by `_`.
                    crate::spec::image_to_container_id(&c.image)
                })
            })
            .collect(),
    }
}

/// Returns the test names declared for a platform (platform-level override or top-level).
pub fn test_names_for_platform(spec: &MirrorSpec, platform_str: &str) -> Vec<String> {
    // Check for platform-level test override first.
    if let Some(platforms) = &spec.platforms
        && let Some(config) = platforms.get(platform_str)
        && let Some(platform_tests) = &config.tests
    {
        return platform_tests.iter().map(|t| t.name.clone()).collect();
    }

    // Fall back to top-level tests.
    spec.tests
        .as_ref()
        .map(|tests| tests.iter().map(|t| t.name.clone()).collect())
        .unwrap_or_default()
}

/// Collect declared platforms whose `broken`-severity exclude entry matches
/// `version`, for visibility (🔒 rows in the Discord report).
///
/// `skip`-severity excludes — and `min_version`/`max_version` windows — stay
/// silent (they never reach this point with a matching entry). Sorted by
/// platform for deterministic output. The excluded pairs were never built, so
/// they never overlap with `platforms_pushed` / `platforms_failed`.
pub fn collect_excluded_platforms(spec: &MirrorSpec, version: &str) -> Vec<ExcludedPlatform> {
    let Some(platforms) = &spec.platforms else {
        return Vec::new();
    };
    let mut excluded: Vec<ExcludedPlatform> = platforms
        .keys()
        .filter_map(|platform| {
            let entry = spec.exclude_hit(version, platform)?;
            (entry.severity == Severity::Broken).then(|| ExcludedPlatform {
                platform: platform.clone(),
                reason: entry.reason.clone(),
            })
        })
        .collect();
    excluded.sort_by(|a, b| a.platform.cmp(&b.platform));
    excluded
}
