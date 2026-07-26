// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Index announce configuration for the push pipeline.
//!
//! [`AnnounceConfig`] names the *logical* index package a mirror publishes
//! under, plus the fork the index pull request is opened from. It is opt-in:
//! a spec without an `announce:` block never calls `ocx package announce`.
//!
//! The logical index package (`<namespace>/<package>`) and the physical
//! registry path (`target.registry/target.repository`) are related by owner
//! convention, not by a rule the code can derive — a mirror publishing to
//! `ghcr.io/ocx-contrib/bazelbuild/bazelisk` announces the logical package
//! `bazelbuild/bazelisk`. So the logical name is spelled out here rather than
//! string-stripped off the target.

use serde::Deserialize;

/// Default index repository the pull request targets — also `ocx package
/// announce`'s own default for `--index-repo`.
pub const DEFAULT_INDEX_REPO: &str = "ocx-sh/index";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnnounceConfig {
    /// Logical index package as `<namespace>/<package>`, e.g. `bazelbuild/bazelisk`.
    pub package: String,

    /// Fork the index pull request is opened from, as `<owner>/<repo>`.
    /// Requires `OCX_ANNOUNCE_TOKEN` in the push job's environment; without it
    /// the run publishes normally and records the announce as skipped.
    pub fork: String,

    /// Index repository the pull request targets, as `<owner>/<repo>`.
    #[serde(default = "default_index_repo")]
    pub index_repo: String,
}

fn default_index_repo() -> String {
    DEFAULT_INDEX_REPO.to_string()
}
