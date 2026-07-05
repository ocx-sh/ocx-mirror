// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use serde::Deserialize;

/// Interpreter configuration for `source.type: pylock` mirrors.
///
/// Selects the CPython version/ABI the mirrored wheels target and the
/// python-build-standalone package that provides the pinned interpreter at
/// compose time (env-package composition, W2.2/ocx_python). Required
/// whenever `source.type` is `pylock` — see `MirrorSpec::validate`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PythonConfig {
    /// Interpreter version, e.g. `"3.13.1"`.
    pub version: String,
    /// Interpreter ABI tag, e.g. `"cp313"` or `"cp313t"` (free-threaded).
    pub abi: String,
    /// OCX package reference for the pinned python-build-standalone interpreter.
    pub interpreter_package: String,
    /// Lock derivation options for `source.type: pypi` mirrors — a committed
    /// `source.type: pylock` already resolves its own lock, so this is only
    /// meaningful (and only accepted, see `MirrorSpec::validate`) alongside
    /// `pypi`.
    #[serde(default)]
    pub lock: Option<LockOptions>,
}

/// How `pipeline plan`/`prepare` derive the per-version PEP 751 lock for a
/// `source.type: pypi` mirror (discovery + lock derivation land in
/// plan_python_mirror_v2 W1/W2 — this struct only carries the parsed config
/// today).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)] // read from YAML spec; consumed once lock derivation (W1/W2) wires it up
pub struct LockOptions {
    /// Resolve a platform/interpreter-agnostic universal lock rather than one
    /// pinned to the resolving host. Default: `true`.
    #[serde(default = "default_true")]
    pub universal: bool,
    /// Extras to include when resolving the lock.
    #[serde(default)]
    pub extras: Vec<String>,
    /// Package names to exclude from resolution.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Timeout, in seconds, for the lock resolution subprocess. Default: 300.
    #[serde(default = "default_lock_timeout")]
    pub timeout_seconds: u64,
}

fn default_true() -> bool {
    true
}

fn default_lock_timeout() -> u64 {
    300
}

static ABI_TAG_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"^cp\d+t?$").unwrap());

impl PythonConfig {
    pub fn validate(&self, errors: &mut Vec<String>) {
        if self.version.trim().is_empty() {
            errors.push("python.version must not be empty".to_string());
        }
        if !ABI_TAG_RE.is_match(&self.abi) {
            errors.push(format!(
                "python.abi '{}' is not a valid CPython ABI tag (expected e.g. 'cp313' or 'cp313t')",
                self.abi
            ));
        }
        if self.interpreter_package.trim().is_empty() {
            errors.push("python.interpreter_package must not be empty".to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_accepts_well_formed_config() {
        let config = PythonConfig {
            version: "3.13.1".to_string(),
            abi: "cp313t".to_string(),
            interpreter_package: "ocx.sh/python/cpython:3.13.1".to_string(),
            lock: None,
        };
        let mut errors = Vec::new();
        config.validate(&mut errors);
        assert!(errors.is_empty(), "expected no errors, got: {errors:?}");
    }

    #[test]
    fn validate_rejects_malformed_abi() {
        let config = PythonConfig {
            version: "3.13.1".to_string(),
            abi: "python3".to_string(),
            interpreter_package: "ocx.sh/python/cpython:3.13.1".to_string(),
            lock: None,
        };
        let mut errors = Vec::new();
        config.validate(&mut errors);
        assert!(errors.iter().any(|e| e.contains("not a valid CPython ABI tag")));
    }
}
