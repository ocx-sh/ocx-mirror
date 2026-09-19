// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Index announce configuration for the push pipeline.
//!
//! [`AnnounceConfig`] names the *logical* index package a mirror publishes
//! under, plus where and how the index request is written — the index
//! repository, an optional fork, the forge and the write transport. It is
//! opt-in: a spec without an `announce:` block never calls
//! `ocx package announce`.
//!
//! The logical index package (`<namespace>/<package>`) and the physical
//! registry path (`target.registry/target.repository`) are related by owner
//! convention, not by a rule the code can derive — a mirror publishing to
//! `ghcr.io/ocx-contrib/bazelbuild/bazelisk` announces the logical package
//! `bazelbuild/bazelisk`. So the logical name is spelled out here rather than
//! string-stripped off the target.

use clap::ValueEnum;
use ocx_announce::forge::{ForgeKind, WriteTransport};
use serde::Deserialize;

/// Default index repository the pull request targets — also `ocx package
/// announce`'s own default for `--index-repo`.
pub const DEFAULT_INDEX_REPO: &str = "ocx-sh/index";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnnounceConfig {
    /// Logical index package as `<namespace>/<package>`, e.g. `bazelbuild/bazelisk`.
    pub package: String,

    /// Fork the index request is opened from, as `[HOST/]NAMESPACE/PROJECT`.
    /// Absent, the branch is pushed to `index_repo` itself and the request
    /// opened from there — which needs push access on the index repository,
    /// and is the only shape a GitLab CI job token can write.
    ///
    /// Either way the run needs an announce credential (see
    /// `pipeline::ocx_cli::announce`); without one it publishes normally and
    /// records the announce as skipped.
    #[serde(default)]
    pub fork: Option<String>,

    /// Index repository the request targets, as `[HOST/]NAMESPACE/PROJECT`.
    /// The host names a self-hosted GitHub Enterprise or GitLab instance;
    /// the namespace may be a nested GitLab group path.
    #[serde(default = "default_index_repo")]
    pub index_repo: String,

    /// Which forge hosts `index_repo`: `github` or `gitlab`. `ocx` infers it
    /// for github.com and gitlab.com; a self-hosted host says nothing about
    /// what runs there, so it is required then.
    #[serde(default)]
    pub forge: Option<String>,

    /// How the request is written: `api` (the default — the forge's REST
    /// API) or `git` (clone, commit, one authenticated push carrying the
    /// merge-request options — GitLab only, and the only transport a CI job
    /// token can open a merge request through).
    #[serde(default)]
    pub transport: Option<String>,

    /// Cron expression for the generated `announce-from-registry.yml`'s
    /// `schedule:` trigger. Absent → that workflow is dispatch-only.
    /// Charset-checked by `spec::validate_announce_config`; GitHub validates
    /// the semantics.
    #[serde(default)]
    pub schedule: Option<String>,
}

fn default_index_repo() -> String {
    DEFAULT_INDEX_REPO.to_string()
}

impl AnnounceConfig {
    /// The declared forge, read through `ocx`'s own `--forge` spelling
    /// (`clap::ValueEnum`) so the two vocabularies cannot drift. `None` when
    /// the block names none — `ocx` infers it from the index host — or names
    /// a spelling `validate_announce_config` has already refused.
    pub(crate) fn forge_kind(&self) -> Option<ForgeKind> {
        self.forge
            .as_deref()
            .and_then(|forge| <ForgeKind as ValueEnum>::from_str(forge, false).ok())
    }

    /// The write transport `ocx package announce` runs under, read through
    /// its own `--transport` spelling (`clap::ValueEnum`) — `ocx`'s default
    /// when the block names none. `validate_announce_config` has already
    /// refused any other spelling, so an unparsable value is the default.
    pub fn transport(&self) -> WriteTransport {
        self.transport
            .as_deref()
            .and_then(|transport| <WriteTransport as ValueEnum>::from_str(transport, false).ok())
            .unwrap_or_default()
    }
}
