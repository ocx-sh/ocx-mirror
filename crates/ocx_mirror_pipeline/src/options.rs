// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Option types the pipeline legs take from their commands.
//!
//! They are clap types, but the pipeline reads them (`registry_sync` takes
//! the whole flag set, the `report_*` functions the format), so they live
//! here and `command` re-exports them at `command::package::options` and
//! `command::registry::options`.

use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat {
    Plain,
    Json,
}

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
