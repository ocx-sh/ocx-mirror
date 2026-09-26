// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! The `ocx package create` subprocess: compiles a prepared content tree and
//! its authoring metadata into a bundle plus the published-form sidecar.
//!
//! Everything create does to the metadata — pinning tag-only dependencies to
//! the platform's manifest digest, the `binaries` scan, the publish-time
//! validation, the libc check — is `ocx`'s, and the mirror drives it rather
//! than linking it. A second in-process copy is how the mirror ended up unable
//! to prepare a spec whose dependency named a tag (issue #90): the in-process
//! projection refused what `create` resolves.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use ocx_mirror_spec::BinScanMode;
use ocx_oci::Platform;

use super::forward_ocx_env;

/// How long one `ocx package create` may run before it is killed.
///
/// A backstop against a wedged child, sized like [`super::push::PUSH_TIMEOUT`]
/// and for the same reason: xz-compressing a multi-hundred-megabyte tree on a
/// small runner takes minutes, and a bound tuned to throughput kills healthy
/// runs. What it catches is a child that stopped making progress.
pub const CREATE_TIMEOUT: Duration = Duration::from_secs(3600);

/// One `ocx package create` invocation's inputs.
pub struct CreateRequest<'a> {
    /// The extracted tree to bundle.
    pub content_dir: &'a Path,
    pub platform: &'a Platform,
    /// The authoring-form metadata file `--metadata` reads.
    pub metadata: &'a Path,
    /// The bundle to write; create derives its sidecars from this path.
    pub output: &'a Path,
    pub compression_threads: u32,
    pub bin_scan: BinScanMode,
    pub libc_lint: bool,
    /// Resolve tag-only dependencies against the registry (`--remote`) rather
    /// than the local index; see [`resolves_tags_remotely`].
    pub remote: bool,
}

/// Whether create should resolve dependency tags against the registry.
///
/// Each build pins a tag-only dependency to what the tag points at *now*. The
/// local index is what ocx consults first otherwise, and a tag pointer cached
/// there months ago would pin every new version to a stale leaf. The operator's
/// `OCX_OFFLINE` or `OCX_FROZEN` is the exception: both ask for local
/// resolution, and `--frozen` refuses `--remote` outright. The child inherits
/// both and applies them itself.
pub fn resolves_tags_remotely() -> bool {
    use ocx_config::env::keys;
    !(ocx_util::env::flag(keys::OCX_OFFLINE, false) || ocx_util::env::flag(keys::OCX_FROZEN, false))
}

impl CreateRequest<'_> {
    /// The argv after the binary.
    ///
    /// `bin_scan` maps onto create's three modes one-for-one: `off` is
    /// `--no-bin-scan`, `auto` is create's default (no flag), `verify` is
    /// `--bin-scan`. `--force` because a resume that found a bundle without its
    /// sidecar re-creates over the stale bundle rather than trusting it.
    /// `--remote` is a root option, so it precedes the subcommand.
    pub fn args(&self) -> Vec<String> {
        let mut args: Vec<String> = self.remote.then(|| "--remote".to_string()).into_iter().collect();
        args.extend([
            "package".to_string(),
            "create".to_string(),
            self.content_dir.display().to_string(),
            "--platform".to_string(),
            self.platform.to_string(),
            "--metadata".to_string(),
            self.metadata.display().to_string(),
            "--output".to_string(),
            self.output.display().to_string(),
            "--force".to_string(),
            "--threads".to_string(),
            self.compression_threads.to_string(),
        ]);
        match self.bin_scan {
            BinScanMode::Off => args.push("--no-bin-scan".to_string()),
            BinScanMode::Auto => {}
            BinScanMode::Verify => args.push("--bin-scan".to_string()),
        }
        if !self.libc_lint {
            args.push("--no-libc-lint".to_string());
        }
        args
    }
}

/// Where create writes the compiled sidecar for `output`.
///
/// ocx's `conventions::infer_metadata_file`: the file stem with a trailing
/// `.tar` dropped, plus `-metadata.json` — `bundle.tar.xz` becomes
/// `bundle-metadata.json`. Copied rather than linked because it lives in
/// `ocx_cli`, which a satellite may not name.
pub fn compiled_sidecar_path(output: &Path) -> PathBuf {
    let stem = output.file_stem().and_then(|stem| stem.to_str()).unwrap_or_default();
    let stem = stem.strip_suffix(".tar").unwrap_or(stem);
    output.with_file_name(format!("{stem}-metadata.json"))
}

/// Runs `ocx package create` once, bounded by `timeout`.
///
/// A non-zero exit carries create's own stderr, which is where every refusal
/// it makes explains itself (an unresolvable dependency tag, a `binaries`
/// mismatch, a libc claim the binaries contradict). A zero exit's stderr is
/// logged as a warning: it is where create reports a skipped libc check.
///
/// # Errors
///
/// When the binary cannot be spawned, the run exceeds `timeout`, or it exits
/// non-zero.
pub async fn create(ocx_binary: &Path, args: &[String], timeout: Duration) -> Result<()> {
    let mut cmd = tokio::process::Command::new(ocx_binary);
    cmd.args(args);
    forward_ocx_env(&mut cmd);
    // A timed-out create would otherwise keep writing a bundle the retry, or
    // the next run's resume, is about to read.
    cmd.kill_on_drop(true);

    let output = tokio::time::timeout(timeout, cmd.output())
        .await
        .map_err(|_| anyhow::anyhow!("ocx package create timed out after {}s", timeout.as_secs()))?
        .with_context(|| format!("failed to spawn {}", ocx_binary.display()))?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        anyhow::bail!("ocx package create exited {}: {}", output.status, stderr.trim());
    }
    if !stderr.trim().is_empty() {
        log::warn!("ocx package create: {}", stderr.trim());
    }
    Ok(())
}

#[cfg(test)]
#[path = "create/tests.rs"]
mod tests;
