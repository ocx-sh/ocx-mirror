// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Shared helpers for driving the co-located `ocx` binary as a subprocess.
//!
//! Both the archive push/describe legs (`command::package::pipeline`) and the
//! pylock env-push leg (`pipeline::python_push`) shell out to `ocx package …`.
//! These two helpers — binary resolution and `OCX_*` env forwarding — live at
//! the pipeline layer so every subprocess caller shares one implementation
//! (rather than reaching across the module tree into a single command's file).

use std::path::PathBuf;

/// Resolve the path to the `ocx` binary.
///
/// Preference order:
/// 1. `OCX_BINARY_PIN` env var (per CLAUDE.md env table — set by ocx itself).
/// 2. Current executable path (`std::env::current_exe()`).
/// 3. `"ocx"` on `PATH` as final fallback.
pub(crate) fn resolve_ocx_binary() -> Result<PathBuf, String> {
    if let Ok(pin) = std::env::var("OCX_BINARY_PIN")
        && !pin.is_empty()
    {
        return Ok(PathBuf::from(pin));
    }

    // The current binary is `ocx-mirror`. We want the co-located `ocx` binary.
    if let Ok(current) = std::env::current_exe()
        && let Some(dir) = current.parent()
    {
        let candidate = dir.join("ocx");
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    // Fallback: hope `ocx` is on PATH.
    Ok(PathBuf::from("ocx"))
}

/// Forward all `OCX_*` environment variables from the current process into a
/// child command. This ensures offline mode, remote mode, registry config, and
/// index paths are inherited by the subprocess.
pub(crate) fn forward_ocx_env(cmd: &mut tokio::process::Command) {
    const OCX_VARS: &[&str] = &[
        "OCX_HOME",
        "OCX_DEFAULT_REGISTRY",
        "OCX_INSECURE_REGISTRIES",
        "OCX_OFFLINE",
        "OCX_REMOTE",
        "OCX_CONFIG",
        "OCX_NO_CONFIG",
        "OCX_PROJECT",
        "OCX_NO_PROJECT",
        "OCX_INDEX",
        "OCX_BINARY_PIN",
        "OCX_NO_UPDATE_CHECK",
        "OCX_NO_MODIFY_PATH",
    ];

    for var in OCX_VARS {
        if let Ok(val) = std::env::var(var) {
            cmd.env(var, val);
        }
    }
}
