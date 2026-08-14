// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Shared flags for the `ocx-mirror registry` verbs.

use std::path::PathBuf;

use crate::command::package::options::OutputFormat;

/// Flags for `registry sync` (C-045).
#[derive(Clone, clap::Args)]
pub struct RegistrySyncOptions {
    /// Report what would be copied, and how many bytes, without copying or
    /// writing anything — the cache digest included
    #[arg(long)]
    pub dry_run: bool,

    /// Stop at the first per-package failure, overriding the spec's `on_error:`
    #[arg(long)]
    pub fail_fast: bool,

    /// Re-derive `c/index.json` from the root documents already on disk.
    /// Wholesale — it covers every root under `p/`, including ones the current
    /// filter excludes
    #[arg(long)]
    pub repair_catalog: bool,

    /// Directory for the source-catalog digest and the index lock files.
    /// Defaults to `${XDG_CACHE_HOME:-~/.cache}/ocx-mirror`. Never inside
    /// `output:`
    #[arg(long)]
    pub cache_dir: Option<PathBuf>,

    /// Output format
    #[arg(long, value_enum, default_value = "plain")]
    pub format: OutputFormat,
}
