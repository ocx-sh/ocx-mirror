// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Upstream version discovery for ocx-mirror: the source adapters
//! (`github_release`, `url_index`, `pylock`, `pypi`), the spec-declared
//! generator runner, and the per-platform asset [`resolver`].
//!
//! A generic crate (adr_bazel_crate_split.md § C1, C3): it names no mirror
//! error or spec type. Adapters return `anyhow` chains the root classifies;
//! the one typed error is [`github_release::GithubClientError`].

pub mod generator;
pub mod github_release;
pub mod pylock;
pub mod pypi;
pub mod resolver;
pub mod url_index;

use std::collections::HashMap;

use url::Url;

/// Information about a single upstream version, produced by source adapters.
#[derive(Debug, Clone)]
pub struct VersionInfo {
    pub version: String,
    pub assets: HashMap<String, Url>,
    /// Publisher-declared content digests keyed by asset name, normalised to
    /// `sha256:<hex>`. Sparse — an asset whose source declares none is absent.
    pub asset_digests: HashMap<String, String>,
    pub is_prerelease: bool,
}
