// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Driving the `ocx` binary as a subprocess.
//!
//! Both the archive push/describe legs (`command::package::pipeline`) and the
//! pylock env-push leg (`pipeline::python_push`) shell out to `ocx package …`.
//! The whole subprocess boundary lives here at the pipeline layer so every
//! caller shares one implementation: this module owns binary resolution and
//! `OCX_*` env forwarding, [`push`] owns the `ocx package push` invocation and
//! its retry ladder, [`create`] owns `ocx package create`, and [`announce`]
//! owns `ocx package announce`.
//!
//! It was previously split — the two helpers here, the invocations inside
//! `command::package::pipeline::push` — which made a command module the owner
//! of plumbing the layer below it needed, and `pipeline::python_push` had to
//! reach upward to get at it.

pub mod announce;
pub mod create;
pub mod push;
pub mod sign;

use std::path::PathBuf;

/// Resolve the path to the `ocx` binary.
///
/// Preference order:
/// 1. `OCX_BINARY_PIN` env var (set by ocx itself when running under `ocx exec`).
/// 2. `"ocx"` on `PATH`.
pub fn resolve_ocx_binary() -> Result<PathBuf, String> {
    if let Ok(pin) = std::env::var("OCX_BINARY_PIN")
        && !pin.is_empty()
    {
        return Ok(PathBuf::from(pin));
    }

    Ok(PathBuf::from("ocx"))
}

/// The `OCX_*` inputs the mirror re-exports into its ocx children.
const FORWARDED_OCX_VARS: &[&str] = &[
    "OCX_HOME",
    "OCX_DEFAULT_REGISTRY",
    "OCX_INSECURE_REGISTRIES",
    "OCX_NO_CONFIG",
    "OCX_PROJECT",
    "OCX_NO_PROJECT",
    "OCX_BINARY_PIN",
    "OCX_NO_UPDATE_CHECK",
    "OCX_NO_MODIFY_PATH",
    // Deliberately NOT `OCX_IDENTITY_TOKEN` / `OCX_KEY_PASSWORD`
    // (adr_mirror_signing.md, C-054). This list is a *forwarding*
    // whitelist for names inherited from this process; the two signing
    // secrets are resolved from `sign:` refs by
    // [`sign::resolve_sign`] — which may name a variable this process
    // never had — and applied per child by [`sign::ocx_child_env`].
    // Adding them here would forward whatever the ambient environment
    // happens to hold under the ocx-owned names, silently overriding the
    // spec on a runner that already sets one.
];

/// Forward the [`FORWARDED_OCX_VARS`] set into a child command. Offline, frozen
/// and the other root-flag narrowings are not in it: they travel in
/// `__OCX_ENV_STATE`, which the child inherits because the mirror never clears
/// a child's environment.
pub fn forward_ocx_env(cmd: &mut tokio::process::Command) {
    for var in FORWARDED_OCX_VARS {
        if let Ok(val) = std::env::var(var) {
            cmd.env(var, val);
        }
    }
}

#[cfg(test)]
mod tests;
