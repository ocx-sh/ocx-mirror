// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Env-package composition: layer layout, entrypoint synthesis, interpreter
//! dependency, env metadata.
//!
//! Turns a validated set of [`RepackedWheel`]s plus a consumer-declared
//! [`EnvSpec`] into an [`EnvComposition`]: the wheel layers (each applied at the
//! content root — `repack` already emitted the final relocated tree), a
//! synthesized entrypoint per `[console_scripts]` entry (extras-gated per
//! [`EnvSpec::requested_extras`]), the private interpreter dependency, and the
//! env metadata (`PYTHONPATH`, `PATH`, `PYTHONDONTWRITEBYTECODE=1`).
//!
//! # Target-agnostic
//!
//! The composition is **not** a fully-formed [`Info`](ocx_lib::package::info::Info):
//! `Info` requires a concrete [`Identifier`](ocx_lib::oci::Identifier) carrying
//! a registry host, which this crate never knows. Instead it emits the two
//! target-agnostic thirds of an `Info` — the composed [`Metadata`] and the L2
//! [`Platform`] — and [`EnvComposition::into_info`] assembles the final `Info`
//! once the consumer supplies the `Identifier`.
//!
//! # Entrypoint synthesis
//!
//! `compose` parses each [`ConsoleScript::reference`](crate::repack::ConsoleScript)
//! (the raw `module[:attr[.attr…]]` object reference extracted by `repack`) into
//! an entrypoint with `command: python3` and `args: ["-c", <shim>]`, where the
//! shim resolves the module via `importlib.import_module` and walks the attr
//! chain with `getattr` (never a literal `from … import …`, which breaks on
//! dotted attrs). A malformed reference is a [`ComposeError::InvalidEntryPoint`].
//! `python3` resolves via the private interpreter dependency on the composed
//! `PATH`; ABI mismatch (parsed from [`RepackedWheel::filename`](crate::repack::RepackedWheel))
//! fails here at compose, not at run.

use std::path::PathBuf;

use ocx_lib::oci::{LayerLayoutSpec, Platform};
use ocx_lib::package::metadata::Metadata;

use crate::platform::PythonTarget;
use crate::repack::RepackedWheel;

/// Consumer-declared inputs to composition.
#[derive(Debug, Clone)]
pub struct EnvSpec {
    /// The extras requested for this env (e.g. `full` for `app[full]`).
    ///
    /// Drives extras-gated entrypoint synthesis and is validated against the
    /// lock's top-level `extras` key — a requested extra the lock does not
    /// declare is a [`ComposeError::UnknownExtra`].
    pub requested_extras: Vec<String>,
    /// The private interpreter dependency, pinned by the consumer
    /// (python-build-standalone package). Its `python3` on the composed `PATH`
    /// is the dispatch target for every synthesized entrypoint.
    pub interpreter: ocx_lib::package::metadata::dependency::Dependency,
    /// The selection target — supplies the L2 platform encoding and the ABI
    /// the wheel set is checked against.
    pub target: PythonTarget,
}

/// A single wheel layer descriptor: its source layer plus placement.
#[derive(Debug, Clone)]
pub struct WheelLayer {
    /// Path to the repacked `tar.zst` layer (from [`RepackedWheel::layer_path`]).
    pub source: PathBuf,
    /// The per-layer strip + output prefix. Defaults **empty**: `repack` emits
    /// the final relocated tree (a wheel spans `lib/site-packages/`, `bin/`, and
    /// `share/…`, which a single layer prefix cannot express), so each wheel
    /// applies at the content root. The field exists because ocx_lib's layer-ref
    /// requires a [`LayerLayoutSpec`] and to leave room for a future
    /// strip/prefix edge case — not to relocate wheels.
    pub layout: LayerLayoutSpec,
}

/// The target-agnostic composition of an env package.
///
/// Carries the two registry-independent thirds of an
/// [`Info`](ocx_lib::package::info::Info) — [`metadata`](Self::metadata) and
/// [`platform`](Self::platform) — plus the layer descriptors. The consumer
/// supplies the registry-bearing [`Identifier`](ocx_lib::oci::Identifier) and
/// calls [`into_info`](Self::into_info) to obtain the final `Info`.
#[derive(Debug, Clone)]
pub struct EnvComposition {
    /// The composed bundle metadata: synthesized entrypoints, env vars
    /// (`PYTHONPATH`, `PATH`, `PYTHONDONTWRITEBYTECODE=1`), and the private
    /// interpreter dependency.
    pub metadata: Metadata,
    /// The L2-encoded OCX platform for the Image Index entry.
    pub platform: Platform,
    /// The ordered wheel layer descriptors (source + placement).
    pub layers: Vec<WheelLayer>,
}

impl EnvComposition {
    /// Assembles the final [`Info`](ocx_lib::package::info::Info) by attaching a
    /// consumer-supplied [`Identifier`](ocx_lib::oci::Identifier).
    ///
    /// This is the single seam where the registry host enters: the crate stays
    /// target-agnostic; the consumer (the mirror) owns the identifier.
    pub fn into_info(self, identifier: ocx_lib::oci::Identifier) -> ocx_lib::package::info::Info {
        let _ = identifier;
        unimplemented!("W1.7")
    }
}

/// Composes an env package from a validated wheel set and consumer inputs.
///
/// # Errors
///
/// Returns [`ComposeError::UnknownExtra`] for a requested extra absent from the
/// lock, [`ComposeError::AbiMismatch`] when a wheel's ABI is inconsistent with
/// the interpreter pin, [`ComposeError::InvalidEntryPoint`] for a malformed
/// `[console_scripts]` object reference, and [`ComposeError::Platform`] when L2
/// platform encoding fails.
pub fn compose_env(spec: &EnvSpec, wheels: &[RepackedWheel]) -> Result<EnvComposition, ComposeError> {
    let _ = (spec, wheels);
    unimplemented!("W1.7")
}

/// Errors from env-package composition.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ComposeError {
    /// A requested extra is not declared in the lock's top-level `extras`.
    #[error("requested extra '{extra}' is not declared in the lock")]
    UnknownExtra {
        /// The undeclared extra.
        extra: String,
    },
    /// A wheel's ABI is inconsistent with the interpreter pin.
    #[error("wheel '{filename}' ABI '{wheel_abi}' is incompatible with interpreter ABI '{interpreter_abi}'")]
    AbiMismatch {
        /// The offending wheel filename.
        filename: String,
        /// The wheel's ABI tag.
        wheel_abi: String,
        /// The interpreter's ABI tag.
        interpreter_abi: String,
    },
    /// A `[console_scripts]` object reference does not parse as
    /// `module[:attr[.attr…]]`.
    #[error("invalid entry point '{name}': '{reference}' is not a valid object reference")]
    InvalidEntryPoint {
        /// The entry-point name.
        name: String,
        /// The malformed object reference.
        reference: String,
    },
    /// L2 platform encoding failed for the target.
    #[error("platform encoding error during composition")]
    Platform(#[from] crate::platform::PlatformError),
}
