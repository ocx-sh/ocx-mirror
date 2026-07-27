// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Provenance of the `ocx-mirror` behind a generated plan.
//!
//! [`OcxMirrorConfig`] pins nothing. The `ocx-mirror` a generated workflow runs
//! comes from the repository's own `ocx.toml` / `ocx.lock`, which `setup-ocx`
//! activates, and the `ocx` release the container test legs download is a
//! renderer constant (`OCX_CONTAINER_CLI_TAG`) so the whole fleet tests against
//! one binary. Neither is a per-spec choice, so neither is a field here.

use serde::Deserialize;

/// Records the git revision of `ocx-mirror` behind a plan.
///
/// `deny_unknown_fields` deliberately: a leftover `release_tag:` from the
/// version of this struct that had one should say so, not be quietly dropped.
/// A silently-ignored key is what teaches people the config is decorative.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OcxMirrorConfig {
    /// 40-hex git SHA, surfaced as `ocx_mirror_rev` in `pipeline plan` output
    /// for traceability. Must match `^[0-9a-f]{40}$`.
    #[serde(default)]
    pub rev: Option<String>,
}
