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
        };
        let mut errors = Vec::new();
        config.validate(&mut errors);
        assert!(errors.iter().any(|e| e.contains("not a valid CPython ABI tag")));
    }
}
