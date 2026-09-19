// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use std::fmt;
use std::path::Path;

use ocx_package::version::Version;
use serde::{Deserialize, Serialize};

use crate::error::MirrorError;

/// Controls the order in which non-mirrored versions are selected when
/// `new_per_run` caps the batch size.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackfillOrder {
    /// Prioritise the most recent versions first (default). New mirrors get
    /// the latest releases immediately; older versions trickle in over
    /// subsequent runs.
    #[default]
    NewestFirst,
    /// Start from the oldest non-mirrored version and work forward. Useful
    /// when chronological completeness matters more than freshness.
    OldestFirst,
}

impl fmt::Display for BackfillOrder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NewestFirst => write!(f, "newest_first"),
            Self::OldestFirst => write!(f, "oldest_first"),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct VersionsConfig {
    pub min: Option<String>,
    pub max: Option<String>,
    pub new_per_run: Option<usize>,
    #[serde(default)]
    pub backfill: BackfillOrder,
    /// Cron schedule expression (e.g. `"0 */6 * * *"`) used to generate a
    /// `schedule:` trigger in the rendered GHA workflow. Omitting this field
    /// produces a workflow with only `workflow_dispatch` + `push:` triggers.
    pub poll_interval: Option<String>,
}

impl VersionsConfig {
    pub fn validate(&self, errors: &mut Vec<String>) {
        if let Some(min) = &self.min
            && Version::parse(min).is_none()
        {
            errors.push(format!("versions.min: invalid version '{min}'"));
        }
        if let Some(max) = &self.max
            && Version::parse(max).is_none()
        {
            errors.push(format!("versions.max: invalid version '{max}'"));
        }
        if let Some(cron) = &self.poll_interval {
            super::validate_cron("versions.poll_interval", cron, errors);
        }
    }
}

/// The version window a run actually filtered by, after resolving both edges.
///
/// Reported in `plan.json` because a moving vendor pointer makes the spec alone
/// insufficient to reproduce a run. An unset edge emits no key; its inclusivity
/// flag is always present and carries the shorthand default, so a plan written
/// before the object form existed — or by hand — still reads correctly.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "jsonschema", derive(schemars::JsonSchema))]
pub struct ResolvedBounds {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<String>,
    #[serde(default = "inclusive_min_default")]
    pub min_inclusive: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<String>,
    #[serde(default)]
    pub max_inclusive: bool,
    /// Where each edge came from. Not serialized: `plan.json`'s shape is the
    /// four fields above, and the plain plan renderer is the only reader.
    #[serde(skip)]
    pub min_origin: BoundOrigin,
    #[serde(skip)]
    pub max_origin: BoundOrigin,
}

fn inclusive_min_default() -> bool {
    true
}

/// `Default` is hand-written, not derived: `min_inclusive` defaults to `true`,
/// and a derived `false` would silently narrow the window of every spec that
/// reaches the no-`versions:` path.
impl Default for ResolvedBounds {
    fn default() -> Self {
        Self {
            min: None,
            min_inclusive: true,
            max: None,
            max_inclusive: false,
            min_origin: BoundOrigin::default(),
            max_origin: BoundOrigin::default(),
        }
    }
}

impl ResolvedBounds {
    /// Whether `candidate` falls inside the window.
    pub(crate) fn admits(&self, candidate: &str) -> bool {
        crate::filter::within_bounds_ex(
            candidate,
            self.min.as_deref(),
            self.min_inclusive,
            self.max.as_deref(),
            self.max_inclusive,
        )
    }
}

/// Where a resolved edge's value came from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BoundOrigin {
    /// Written in the spec.
    #[default]
    Spec,
    /// Fetched from a URL.
    Url,
    /// Produced by a generator command.
    Generator,
}

impl fmt::Display for BoundOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spec => write!(f, "spec"),
            Self::Url => write!(f, "url"),
            Self::Generator => write!(f, "generator"),
        }
    }
}

/// Resolve `versions:`'s two edges once per run.
///
/// Stub: today both edges are literal strings in the spec, so this copies them
/// with the shorthand edges (`min` inclusive, `max` exclusive). #78 replaces
/// the body with the fail-closed resolver for `{url}` / `{generator}` bounds —
/// the signature, the call sites and the `plan.json` field are already here.
pub(crate) async fn resolve_version_bounds(
    versions: Option<&VersionsConfig>,
    _spec_dir: &Path,
) -> Result<ResolvedBounds, MirrorError> {
    let Some(config) = versions else {
        return Ok(ResolvedBounds::default());
    };
    Ok(ResolvedBounds {
        min: config.min.clone(),
        max: config.max.clone(),
        ..ResolvedBounds::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_window_admits_everything() {
        // The value every spec without a `versions:` block is filtered by.
        // Reds if someone re-derives `Default` and flips `min_inclusive`.
        let bounds = ResolvedBounds::default();
        assert!(bounds.admits("0.0.1"));
        assert!(bounds.admits("99.0.0"));
        assert!(bounds.min_inclusive);
        assert!(!bounds.max_inclusive);
    }

    #[tokio::test]
    async fn the_stub_copies_the_spec_edges_with_todays_inclusivity() {
        let config = VersionsConfig {
            min: Some("1.0.0".to_string()),
            max: Some("3.0.0".to_string()),
            ..Default::default()
        };
        let bounds = resolve_version_bounds(Some(&config), Path::new("."))
            .await
            .expect("the stub never fails");

        assert!(bounds.admits("1.0.0"), "min is inclusive");
        assert!(!bounds.admits("3.0.0"), "max is exclusive");
        assert!(!bounds.admits("0.9.0"));
        assert!(bounds.admits("2.9.9"));
    }
}
