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

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::json;
use uv_distribution_filename::WheelFilename;

use ocx_lib::oci::{LayerLayoutSpec, Platform};
use ocx_lib::package::metadata::Metadata;
use ocx_lib::package::metadata::bundle::{Bundle, Version as BundleVersion};
use ocx_lib::package::metadata::dependency::Dependencies;
use ocx_lib::package::metadata::entrypoint::{Entrypoint, EntrypointName, Entrypoints};
use ocx_lib::package::metadata::env::EnvBuilder;

use crate::platform::{PythonTarget, encode_l2};
use crate::repack::RepackedWheel;

/// Consumer-declared inputs to composition.
#[derive(Debug, Clone)]
pub struct EnvSpec {
    /// The extras requested for this env (e.g. `full` for `app[full]`).
    ///
    /// Drives extras-gated entrypoint synthesis and is validated against
    /// [`declared_extras`](Self::declared_extras) — a requested extra the lock
    /// does not declare is a [`ComposeError::UnknownExtra`].
    pub requested_extras: Vec<String>,
    /// The extras the lock declares (its top-level `extras` key), supplied by
    /// the consumer.
    ///
    /// The validation floor for [`requested_extras`](Self::requested_extras):
    /// any requested extra absent here is a [`ComposeError::UnknownExtra`]. Kept
    /// distinct from the requested set so an unknown-extra typo fails closed
    /// rather than silently synthesizing (or dropping) a launcher.
    pub declared_extras: Vec<String>,
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
        ocx_lib::package::info::Info {
            identifier,
            metadata: self.metadata,
            platform: self.platform,
        }
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
    // 1. Every requested extra must be one the lock declares. A typo fails
    //    closed here rather than silently registering an unresolvable launcher.
    for extra in &spec.requested_extras {
        if !spec.declared_extras.contains(extra) {
            return Err(ComposeError::UnknownExtra { extra: extra.clone() });
        }
    }

    // 2. ABI consistency: every wheel must match the interpreter pin (fail
    //    closed) before any layer or entrypoint is emitted.
    let interpreter_abi = spec.target.interpreter.abi.as_str();
    for wheel in wheels {
        check_abi(&wheel.filename, interpreter_abi)?;
    }

    // 3. Entrypoint synthesis: one entrypoint per gated console script.
    let mut entries: BTreeMap<EntrypointName, Entrypoint> = BTreeMap::new();
    for wheel in wheels {
        for script in &wheel.entry_points {
            // Extras gating: synthesize only when every extra the script is
            // gated on was requested (empty = always). Never inferred from
            // dependency presence.
            if !script.extras.iter().all(|extra| spec.requested_extras.contains(extra)) {
                continue;
            }
            let shim = synthesize_shim(&script.reference).ok_or_else(|| ComposeError::InvalidEntryPoint {
                name: script.name.clone(),
                reference: script.reference.clone(),
            })?;
            let name = EntrypointName::try_from(script.name.as_str()).map_err(|_| ComposeError::InvalidEntryPoint {
                name: script.name.clone(),
                reference: script.reference.clone(),
            })?;
            // `command` is the fixed, compile-time-valid slug `python3` and the
            // args are plain strings, so this Entrypoint always deserializes.
            let entrypoint: Entrypoint = serde_json::from_value(json!({
                "command": "python3",
                "args": ["-c", shim],
            }))
            .expect("python3 is a valid entrypoint command and the shim args are strings");
            entries.insert(name, entrypoint);
        }
    }
    let entrypoints = Entrypoints::new(entries);

    // 4. Env block: expose the site-packages tree, prepend the launcher bin
    //    dir to PATH, and disable bytecode writes into read-only package
    //    content (design spec, "Runtime-write mitigation").
    let env = EnvBuilder::new()
        .with_path("PYTHONPATH", "${installPath}/lib/site-packages", true)
        .with_path("PATH", "${installPath}/bin", true)
        .with_constant("PYTHONDONTWRITEBYTECODE", "1")
        .build();

    // 5. Interpreter dependency: `python3` on the composed PATH is the dispatch
    //    target for every synthesized entrypoint. A single dependency cannot
    //    duplicate an identifier or name, so `Dependencies::new` is infallible.
    let dependencies = Dependencies::new(vec![spec.interpreter.clone()])
        .expect("a single interpreter dependency cannot duplicate an identifier or name");

    // 6. L2 platform encoding (os/arch → OCX platform). The variant prefix is
    //    the consumer's tag concern, not baked into the Image Index platform.
    let platform = encode_l2(&spec.target)?.platform;

    // 7. Layers: one per wheel, applied at the content root with an empty
    //    layout — `repack` already emitted the final relocated tree.
    let layers = wheels
        .iter()
        .map(|wheel| WheelLayer {
            source: wheel.layer_path.clone(),
            layout: LayerLayoutSpec::default(),
        })
        .collect();

    let bundle = Bundle {
        version: BundleVersion::V1,
        strip_components: None,
        env,
        dependencies,
        entrypoints,
    };

    Ok(EnvComposition {
        metadata: Metadata::Bundle(bundle),
        platform,
        layers,
    })
}

/// Synthesizes the `python3 -c` shim for a `[console_scripts]` object reference
/// `module[:attr[.attr…]]`.
///
/// The shim imports the module via `importlib.import_module`, walks the attr
/// chain with `getattr`, and calls the resolved object under `sys.exit` — never
/// a `from … import …` template, which cannot express a dotted attribute chain.
/// Returns `None` when the reference is malformed (empty module, empty attr
/// segment, or more than one `:`), which the caller turns into a
/// [`ComposeError::InvalidEntryPoint`].
fn synthesize_shim(reference: &str) -> Option<String> {
    let (module, attrs) = parse_object_reference(reference)?;
    let mut lines = vec![
        "import importlib, sys".to_string(),
        format!("_obj = importlib.import_module(\"{module}\")"),
    ];
    for attr in attrs {
        lines.push(format!("_obj = getattr(_obj, \"{attr}\")"));
    }
    lines.push("sys.exit(_obj())".to_string());
    Some(lines.join("\n"))
}

/// Parses an object reference `module[:attr[.attr…]]` into its module and
/// (possibly empty) attribute chain. Returns `None` for a malformed reference.
///
/// The module and every attribute segment must be a non-empty Python
/// identifier; a second `:` (more than one colon) is malformed. Segments are
/// validated Python identifiers, so they contain no characters needing escaping
/// when embedded into the shim's string literals.
fn parse_object_reference(reference: &str) -> Option<(&str, Vec<&str>)> {
    let (module, attr_chain) = match reference.split_once(':') {
        Some((module, attrs)) => (module, Some(attrs)),
        None => (reference, None),
    };
    if !is_valid_dotted_identifier(module) {
        return None;
    }
    let attrs = match attr_chain {
        None => Vec::new(),
        // A remaining `:` in the chain means the reference had more than one
        // colon — malformed.
        Some(chain) if chain.contains(':') || !is_valid_dotted_identifier(chain) => return None,
        Some(chain) => chain.split('.').collect(),
    };
    Some((module, attrs))
}

/// Returns `true` when `value` is a non-empty dot-separated chain of Python
/// identifiers (e.g. `pkg.mod`, `Class.method`).
fn is_valid_dotted_identifier(value: &str) -> bool {
    !value.is_empty() && value.split('.').all(is_valid_python_identifier)
}

/// Returns `true` when `value` is a valid Python identifier: a leading letter or
/// underscore followed by ASCII alphanumerics or underscores.
fn is_valid_python_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) if first == '_' || first.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

/// Validates a wheel's ABI against the interpreter pin, failing closed.
///
/// A wheel is ABI-consistent when it carries a universal ABI (`none` for
/// pure-Python, `abi3` for the stable ABI) or a concrete CPython ABI equal to
/// the interpreter's. A concrete `cpXY`/`cpXYt` that differs is a
/// [`ComposeError::AbiMismatch`] (e.g. a `cp313` wheel against a free-threaded
/// `cp313t` interpreter), as is a wheel filename that fails to parse — an
/// unverifiable ABI is rejected rather than admitted.
fn check_abi(filename: &str, interpreter_abi: &str) -> Result<(), ComposeError> {
    let wheel_abis: Vec<String> = match filename.parse::<WheelFilename>() {
        Ok(wheel) => wheel.abi_tags().iter().map(ToString::to_string).collect(),
        Err(_) => {
            return Err(ComposeError::AbiMismatch {
                filename: filename.to_string(),
                wheel_abi: "unparseable".to_string(),
                interpreter_abi: interpreter_abi.to_string(),
            });
        }
    };
    let compatible = wheel_abis
        .iter()
        .any(|abi| abi == "none" || abi == "abi3" || abi == interpreter_abi);
    if compatible {
        Ok(())
    } else {
        Err(ComposeError::AbiMismatch {
            filename: filename.to_string(),
            wheel_abi: wheel_abis.join("."),
            interpreter_abi: interpreter_abi.to_string(),
        })
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use ocx_lib::package::metadata::dependency::Dependency;

    use crate::platform::{
        Implementation, InterpreterPin, TargetArchitecture, TargetOperatingSystem, TargetPlatform, VariantConstraints,
    };
    use crate::repack::ConsoleScript;

    // ── Inline construction helpers (no fixtures) ───────────────────────────

    fn interpreter_dependency() -> Dependency {
        let json = format!(r#"{{"identifier":"ocx.sh/python:3.13@sha256:{}"}}"#, "a".repeat(64));
        serde_json::from_str(&json).expect("interpreter dependency parses")
    }

    fn python_target(abi: &str) -> PythonTarget {
        PythonTarget {
            platform: TargetPlatform {
                operating_system: TargetOperatingSystem::Linux,
                architecture: TargetArchitecture::Amd64,
            },
            variant: VariantConstraints::default(),
            interpreter: InterpreterPin {
                python_version: "3.13".to_string(),
                python_full_version: "3.13.1".to_string(),
                abi: abi.to_string(),
                implementation: Implementation::CPython,
            },
        }
    }

    fn env_spec(requested: &[&str], declared: &[&str], abi: &str) -> EnvSpec {
        EnvSpec {
            requested_extras: requested.iter().map(ToString::to_string).collect(),
            declared_extras: declared.iter().map(ToString::to_string).collect(),
            interpreter: interpreter_dependency(),
            target: python_target(abi),
        }
    }

    fn console_script(name: &str, reference: &str, extras: &[&str]) -> ConsoleScript {
        ConsoleScript {
            name: name.to_string(),
            reference: reference.to_string(),
            extras: extras.iter().map(ToString::to_string).collect(),
        }
    }

    fn wheel(filename: &str, scripts: Vec<ConsoleScript>) -> RepackedWheel {
        RepackedWheel {
            filename: filename.to_string(),
            layer_path: PathBuf::from(format!("/layers/{filename}.tar.zst")),
            layer_digest: format!("sha256:{}", "b".repeat(64)),
            wheel_sha256: "c".repeat(64),
            entry_points: scripts,
            record_paths: Vec::new(),
            locked_extras: Vec::new(),
        }
    }

    /// A pure-Python wheel (`none` ABI), compatible with any interpreter — used
    /// where a test isolates entrypoint/env behaviour from the ABI check.
    const PURE_WHEEL: &str = "foo-1.0-py3-none-any.whl";

    /// Returns the sole entrypoint's args (`["-c", shim]`), asserting exactly one
    /// entrypoint named `name` dispatching `python3`.
    fn sole_entrypoint_args(composition: &EnvComposition, name: &str) -> Vec<String> {
        let entrypoints = composition
            .metadata
            .entrypoints()
            .expect("bundle metadata carries entrypoints");
        assert_eq!(entrypoints.len(), 1, "expected exactly one entrypoint");
        let (entry_name, entry) = entrypoints.iter().next().expect("one entrypoint present");
        assert_eq!(entry_name.as_str(), name);
        assert_eq!(
            entry.command().expect("dispatch command set").as_str(),
            "python3",
            "every synthesized entrypoint dispatches python3"
        );
        let args = entry.args();
        assert_eq!(args[0], "-c", "shim runs via python3 -c, no shell");
        args.to_vec()
    }

    // ── Entrypoint synthesis: shim grammar ──────────────────────────────────

    #[test]
    fn simple_module_func_reference_builds_importlib_getattr_call_shim() {
        let spec = env_spec(&[], &[], "cp313");
        let wheels = vec![wheel(PURE_WHEEL, vec![console_script("mytool", "mod:func", &[])])];

        let composition = compose_env(&spec, &wheels).expect("composition succeeds");
        let args = sole_entrypoint_args(&composition, "mytool");
        let shim = &args[1];

        assert!(shim.contains("importlib.import_module(\"mod\")"), "shim: {shim}");
        assert!(shim.contains("getattr(_obj, \"func\")"), "shim: {shim}");
        assert!(
            shim.contains("sys.exit(_obj())"),
            "shim calls the resolved object: {shim}"
        );
        assert!(
            !shim.contains("import func"),
            "must not use a from-import template: {shim}"
        );
    }

    #[test]
    fn dotted_attr_reference_walks_each_getattr_in_order() {
        let spec = env_spec(&[], &[], "cp313");
        let wheels = vec![wheel(
            PURE_WHEEL,
            vec![console_script("tool", "pkg.mod:Class.method", &[])],
        )];

        let composition = compose_env(&spec, &wheels).expect("composition succeeds");
        let args = sole_entrypoint_args(&composition, "tool");
        let shim = &args[1];

        assert!(shim.contains("importlib.import_module(\"pkg.mod\")"), "shim: {shim}");
        let class_at = shim.find("getattr(_obj, \"Class\")").expect("Class getattr present");
        let method_at = shim.find("getattr(_obj, \"method\")").expect("method getattr present");
        assert!(class_at < method_at, "attr chain must walk Class before method: {shim}");
    }

    #[test]
    fn module_only_reference_imports_without_getattr() {
        let spec = env_spec(&[], &[], "cp313");
        let wheels = vec![wheel(PURE_WHEEL, vec![console_script("flask", "flask.cli", &[])])];

        let composition = compose_env(&spec, &wheels).expect("composition succeeds");
        let args = sole_entrypoint_args(&composition, "flask");
        let shim = &args[1];

        assert!(shim.contains("importlib.import_module(\"flask.cli\")"), "shim: {shim}");
        assert!(!shim.contains("getattr"), "a module-only ref has no attr walk: {shim}");
        assert!(shim.contains("sys.exit(_obj())"), "shim: {shim}");
    }

    #[test]
    fn malformed_reference_is_invalid_entry_point() {
        let spec = env_spec(&[], &[], "cp313");
        let wheels = vec![wheel(PURE_WHEEL, vec![console_script("bad", "a:b:c", &[])])];

        let error = compose_env(&spec, &wheels).expect_err("a two-colon reference is malformed");
        assert!(
            matches!(error, ComposeError::InvalidEntryPoint { ref name, ref reference } if name == "bad" && reference == "a:b:c"),
            "got {error:?}"
        );
    }

    // ── Extras gating ───────────────────────────────────────────────────────

    #[test]
    fn extras_gated_script_is_skipped_when_extra_not_requested() {
        // `d` is declared but not requested → the blackd launcher is not synthesized.
        let spec = env_spec(&[], &["d"], "cp313");
        let wheels = vec![wheel(PURE_WHEEL, vec![console_script("blackd", "blackd:main", &["d"])])];

        let composition = compose_env(&spec, &wheels).expect("composition succeeds");
        let entrypoints = composition.metadata.entrypoints().expect("entrypoints present");
        assert!(
            entrypoints.is_empty(),
            "an unrequested extra must not synthesize its launcher"
        );
    }

    #[test]
    fn extras_gated_script_is_synthesized_when_extra_requested() {
        let spec = env_spec(&["d"], &["d"], "cp313");
        let wheels = vec![wheel(PURE_WHEEL, vec![console_script("blackd", "blackd:main", &["d"])])];

        let composition = compose_env(&spec, &wheels).expect("composition succeeds");
        let args = sole_entrypoint_args(&composition, "blackd");
        assert!(
            args[1].contains("importlib.import_module(\"blackd\")"),
            "shim: {}",
            args[1]
        );
    }

    #[test]
    fn requested_extra_absent_from_declared_is_unknown_extra() {
        let spec = env_spec(&["full"], &["d"], "cp313");
        let wheels = vec![wheel(PURE_WHEEL, Vec::new())];

        let error = compose_env(&spec, &wheels).expect_err("a requested extra not declared must fail");
        assert!(
            matches!(error, ComposeError::UnknownExtra { ref extra } if extra == "full"),
            "got {error:?}"
        );
    }

    // ── ABI consistency ─────────────────────────────────────────────────────

    #[test]
    fn cp313_wheel_against_free_threaded_interpreter_is_abi_mismatch() {
        // A concrete cp313 wheel must not compose against a cp313t interpreter.
        let spec = env_spec(&[], &[], "cp313t");
        let wheels = vec![wheel("numpy-2.1.3-cp313-cp313-manylinux_2_28_x86_64.whl", Vec::new())];

        let error = compose_env(&spec, &wheels).expect_err("cp313 vs cp313t must fail closed");
        match error {
            ComposeError::AbiMismatch {
                filename,
                wheel_abi,
                interpreter_abi,
            } => {
                assert_eq!(filename, "numpy-2.1.3-cp313-cp313-manylinux_2_28_x86_64.whl");
                assert_eq!(wheel_abi, "cp313");
                assert_eq!(interpreter_abi, "cp313t");
            }
            other => panic!("expected AbiMismatch, got {other:?}"),
        }
    }

    #[test]
    fn matching_cpython_abi_composes() {
        let spec = env_spec(&[], &[], "cp313");
        let wheels = vec![wheel("numpy-2.1.3-cp313-cp313-manylinux_2_28_x86_64.whl", Vec::new())];
        assert!(
            compose_env(&spec, &wheels).is_ok(),
            "a cp313 wheel matches a cp313 interpreter"
        );
    }

    // ── Env block, layers, platform ─────────────────────────────────────────

    #[test]
    fn env_block_carries_pythonpath_path_and_dontwritebytecode() {
        let spec = env_spec(&[], &[], "cp313");
        let wheels = vec![wheel(PURE_WHEEL, Vec::new())];

        let composition = compose_env(&spec, &wheels).expect("composition succeeds");
        let env = composition.metadata.env().expect("bundle metadata carries env");
        let env_json = serde_json::to_string(env).expect("env serializes");

        assert!(env_json.contains("PYTHONPATH"), "env: {env_json}");
        assert!(env_json.contains("lib/site-packages"), "env: {env_json}");
        assert!(env_json.contains("PATH"), "env: {env_json}");
        assert!(
            env_json.contains("PYTHONDONTWRITEBYTECODE"),
            "runtime-write mitigation must be present: {env_json}"
        );
    }

    #[test]
    fn each_wheel_becomes_a_content_root_layer_with_empty_layout() {
        let spec = env_spec(&[], &[], "cp313");
        let wheels = vec![
            wheel("foo-1.0-py3-none-any.whl", Vec::new()),
            wheel("bar-2.0-py3-none-any.whl", Vec::new()),
        ];

        let composition = compose_env(&spec, &wheels).expect("composition succeeds");
        assert_eq!(composition.layers.len(), 2, "one layer per wheel");
        for layer in &composition.layers {
            assert!(
                layer.layout.is_empty(),
                "repack emits the final tree; the layer applies at the content root"
            );
        }
        assert_eq!(
            composition.layers[0].source,
            PathBuf::from("/layers/foo-1.0-py3-none-any.whl.tar.zst")
        );
    }

    #[test]
    fn platform_is_the_l2_os_arch_encoding() {
        let spec = env_spec(&[], &[], "cp313");
        let wheels = vec![wheel(PURE_WHEEL, Vec::new())];

        let composition = compose_env(&spec, &wheels).expect("composition succeeds");
        assert_eq!(composition.platform.to_string(), "linux/amd64");
    }
}
