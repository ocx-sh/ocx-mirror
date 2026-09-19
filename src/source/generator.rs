// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Spawning a spec-declared generator command and collecting its stdout.
//!
//! Lifted out of [`crate::source::url_index::from_generator`] so a second
//! caller — a `versions:` bound resolved from a vendor pointer — runs the
//! command under exactly the same timeout, kill-on-drop and
//! non-zero-exit / empty-output classification. The only behavioural
//! difference from the original is the `ensure!` below.

use std::process::Stdio;
use std::time::Duration;

use std::path::Path;

use crate::spec::GeneratorConfig;

/// Run a generator command under its timeout and return its raw stdout.
///
/// Empty output and a non-zero exit are both hard errors: a generator that
/// says nothing is indistinguishable from one that silently failed, and
/// treating either as "no versions" is how a mirror quietly stops mirroring.
pub async fn run(config: &GeneratorConfig, spec_dir: &Path) -> anyhow::Result<Vec<u8>> {
    // `command[0]` below is index-safe only because `Source::validate` guards
    // the url_index caller. This helper now has a second caller, so it guards
    // itself rather than inheriting someone else's precondition.
    anyhow::ensure!(!config.command.is_empty(), "generator command is empty");

    let working_dir = config.resolve_working_directory(spec_dir);

    let timeout = Duration::from_secs(config.timeout_seconds);
    let result = tokio::time::timeout(timeout, async {
        let output = tokio::process::Command::new(&config.command[0])
            .args(&config.command[1..])
            .current_dir(&working_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("failed to run generator '{}': {e}", config.command[0]))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "generator '{}' failed (exit {}): {}",
                config.command.join(" "),
                output.status,
                stderr.trim()
            );
        }

        if output.stdout.is_empty() {
            anyhow::bail!("generator '{}' produced no output", config.command.join(" "));
        }

        Ok(output.stdout)
    })
    .await;

    match result {
        Ok(inner) => inner,
        Err(_) => anyhow::bail!(
            "generator '{}' timed out after {}s",
            config.command.join(" "),
            config.timeout_seconds
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn empty_command_is_refused_before_any_spawn() {
        // The one behaviour the lift adds: the url_index caller is guarded by
        // `Source::validate`, the bounds caller is not.
        let config = GeneratorConfig {
            command: Vec::new(),
            working_directory: None,
            timeout_seconds: 10,
        };

        let err = run(&config, Path::new(".")).await.unwrap_err();
        assert!(err.to_string().contains("generator command is empty"), "got: {err}");
    }
}
