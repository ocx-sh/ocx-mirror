// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct ConcurrencyConfig {
    #[serde(default = "default_max_downloads")]
    pub max_downloads: usize,
    #[serde(default = "default_max_bundles")]
    pub max_bundles: usize,
    /// Delay in milliseconds between paged source-listing requests
    /// (GitHub Releases). `0` (default) = no delay.
    #[serde(default)]
    pub rate_limit_ms: u64,
    /// Extra attempts a transient push failure gets on top of the first.
    /// `0` = a single attempt.
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// Number of compression threads per bundle task.
    /// `0` (default) = auto: `max(1, available_parallelism / max_bundles)`.
    /// `1` = single-threaded (no block overhead).
    #[serde(default)]
    pub compression_threads: usize,
}

impl Default for ConcurrencyConfig {
    fn default() -> Self {
        Self {
            max_downloads: default_max_downloads(),
            max_bundles: default_max_bundles(),
            rate_limit_ms: 0,
            max_retries: default_max_retries(),
            compression_threads: 0,
        }
    }
}

fn default_max_downloads() -> usize {
    8
}

fn default_max_bundles() -> usize {
    std::thread::available_parallelism()
        .map(|p| (p.get() / 2).max(1))
        .unwrap_or(2)
}

fn default_max_retries() -> u32 {
    3
}

/// Resolve `compression_threads = 0` (auto) to a concrete value based on available parallelism
/// and the number of concurrent bundle tasks.
///
/// - `compression_threads > 0`: returns it directly (explicit override).
/// - `compression_threads == 0` with `max_bundles <= 1`: returns `0` (auto-detect at library level).
/// - `compression_threads == 0` with `max_bundles > 1`: divides available cores across bundles.
pub fn resolve_compression_threads(compression_threads: usize, max_bundles: usize) -> u32 {
    if compression_threads > 0 {
        return compression_threads as u32;
    }
    if max_bundles <= 1 {
        return 0;
    }
    (default_compression_threads() / max_bundles as u32).max(1)
}

/// The core count one bundle would compress with on its own.
///
/// A three-line copy of `ocx_util::compression::default_threads`, which is
/// `pub(crate)` there: the same all-cores-capped-at-16 rule, so dividing it
/// across `max_bundles` yields the share of the budget ocx itself would have
/// spent on a single bundle. Kept in step by value, not by import — a change
/// to the cap upstream is a change to make here.
fn default_compression_threads() -> u32 {
    std::thread::available_parallelism()
        .map(|n| (n.get() as u32).min(16))
        .unwrap_or(1)
}
