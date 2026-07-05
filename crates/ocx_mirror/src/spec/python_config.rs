// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use ocx_lib::package::version::Version;
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
    /// Which console scripts synthesize as entrypoints (design decision C,
    /// `plan_python_mirror_v2`). Defaults to `auto` (root package only) when
    /// omitted — see [`resolve_entrypoint_selection`](Self::resolve_entrypoint_selection).
    #[serde(default = "default_entrypoints")]
    pub entrypoints: EntrypointsConfig,
}

/// `python.entrypoints:` — which console scripts synthesize as OCX
/// entrypoints.
///
/// Untagged: the bare strings `"auto"`/`"all"` parse as [`EntrypointMode`];
/// anything else parses as an explicit list of version-windowed names.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum EntrypointsConfig {
    /// `auto` or `all` (see [`EntrypointMode`]).
    Mode(EntrypointMode),
    /// An explicit list of entrypoint names, each optionally windowed to an
    /// app-version range.
    Explicit(Vec<EntrypointBound>),
}

/// The two bare-string `python.entrypoints:` modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntrypointMode {
    /// Only the root package's own console scripts synthesize (root =
    /// `source.package`/spec name). **The default** as of
    /// `plan_python_mirror_v2` — previously every wheel's scripts
    /// synthesized unconditionally (see [`All`](Self::All)).
    Auto,
    /// Every wheel's console scripts synthesize — the pre-`plan_python_mirror_v2`
    /// behavior. A dependency wheel's bundled CLI stays available under this
    /// mode; see `ocx_python::EntrypointSelection`'s spawn-parity note for why
    /// an app that spawns one needs this (or an explicit entry) instead of
    /// the `auto` default.
    All,
}

/// One `python.entrypoints:` explicit-list entry: a console-script name,
/// optionally windowed to an app-version range.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntrypointBound {
    /// The console-script name to admit.
    pub name: String,
    /// Inclusive lower bound: the entry applies from this app version on.
    /// Absent = unbounded below.
    #[serde(default)]
    pub min_version: Option<String>,
    /// Exclusive upper bound: the entry stops applying at this app version.
    /// Absent = unbounded above.
    #[serde(default)]
    pub max_version: Option<String>,
}

impl EntrypointBound {
    /// Returns `true` when `version` falls within this bound's window: `min`
    /// inclusive / `max` exclusive — the same convention as `versions:` and
    /// per-platform `min_version`/`max_version` (`filter.rs`,
    /// `platforms_config.rs`). An unset bound is open on that side; both
    /// unset always applies.
    fn applies(&self, version: &Version) -> bool {
        let min = self.min_version.as_ref().and_then(|s| Version::parse(s));
        let max = self.max_version.as_ref().and_then(|s| Version::parse(s));
        if let Some(min) = min
            && *version < min
        {
            return false;
        }
        if let Some(max) = max
            && *version >= max
        {
            return false;
        }
        true
    }
}

fn default_entrypoints() -> EntrypointsConfig {
    EntrypointsConfig::Mode(EntrypointMode::Auto)
}

/// How `pipeline plan`/`prepare` derive the per-version PEP 751 lock for a
/// `source.type: pypi` mirror. Its fields are read by `pipeline::lock_derive`
/// via `pipeline plan`'s per-candidate invocation (`--locks-dir`
/// persistence, plan_python_mirror_v2 W2.A3).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
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
        if let EntrypointsConfig::Explicit(bounds) = &self.entrypoints {
            for bound in bounds {
                if bound.name.trim().is_empty() {
                    errors.push("python.entrypoints[].name must not be empty".to_string());
                }
            }
        }
    }

    /// Resolves `entrypoints:` against a concrete app version into an
    /// [`ocx_python::EntrypointSelection`].
    ///
    /// Version-window resolution happens here, mirror-side, so
    /// `ocx_python::compose_env` stays version-agnostic (design decision C,
    /// `plan_python_mirror_v2`). `root_package` is the dist name `Auto`
    /// compares wheels against — the caller supplies
    /// `Source::pylock_app_name` (`source.package`/spec name); this crate
    /// does not normalize it (`ocx_python` does, before comparing).
    pub fn resolve_entrypoint_selection(
        &self,
        app_version: &str,
        root_package: &str,
    ) -> ocx_python::EntrypointSelection {
        match &self.entrypoints {
            EntrypointsConfig::Mode(EntrypointMode::Auto) => ocx_python::EntrypointSelection::RootOnly {
                root_package: root_package.to_string(),
            },
            EntrypointsConfig::Mode(EntrypointMode::All) => ocx_python::EntrypointSelection::All,
            EntrypointsConfig::Explicit(bounds) => {
                // An unparseable app_version keeps every bound — the same
                // fail-open fallback `filter.rs`/`ExcludeEntry` use for a
                // version string that doesn't parse.
                let version = Version::parse(app_version);
                let names = bounds
                    .iter()
                    .filter(|bound| match &version {
                        Some(v) => bound.applies(v),
                        None => true,
                    })
                    .map(|bound| bound.name.clone())
                    .collect();
                ocx_python::EntrypointSelection::Explicit(names)
            }
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
            entrypoints: default_entrypoints(),
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
            entrypoints: default_entrypoints(),
        };
        let mut errors = Vec::new();
        config.validate(&mut errors);
        assert!(errors.iter().any(|e| e.contains("not a valid CPython ABI tag")));
    }

    #[test]
    fn validate_rejects_empty_explicit_entrypoint_name() {
        let mut config = PythonConfig {
            version: "3.13.1".to_string(),
            abi: "cp313".to_string(),
            interpreter_package: "ocx.sh/python/cpython:3.13.1".to_string(),
            lock: None,
            entrypoints: default_entrypoints(),
        };
        config.entrypoints = EntrypointsConfig::Explicit(vec![EntrypointBound {
            name: "  ".to_string(),
            min_version: None,
            max_version: None,
        }]);
        let mut errors = Vec::new();
        config.validate(&mut errors);
        assert!(
            errors
                .iter()
                .any(|e| e.contains("entrypoints[].name must not be empty"))
        );
    }

    // ── entrypoints: deserialization ────────────────────────────────────────

    #[test]
    fn entrypoints_defaults_to_auto_when_omitted() {
        let yaml = "version: \"3.13.1\"\nabi: cp313\ninterpreter_package: \"ocx.sh/python/cpython:3.13.1\"\n";
        let config: PythonConfig = serde_yaml_ng::from_str(yaml).expect("config parses");
        assert!(matches!(
            config.entrypoints,
            EntrypointsConfig::Mode(EntrypointMode::Auto)
        ));
    }

    #[test]
    fn entrypoints_parses_bare_auto_and_all_strings() {
        for (raw, expected) in [("auto", EntrypointMode::Auto), ("all", EntrypointMode::All)] {
            let yaml = format!(
                "version: \"3.13.1\"\nabi: cp313\ninterpreter_package: \"ocx.sh/python/cpython:3.13.1\"\nentrypoints: {raw}\n"
            );
            let config: PythonConfig = serde_yaml_ng::from_str(&yaml).expect("config parses");
            assert!(
                matches!(config.entrypoints, EntrypointsConfig::Mode(mode) if mode == expected),
                "entrypoints: {raw} must parse as Mode({expected:?})"
            );
        }
    }

    #[test]
    fn entrypoints_parses_explicit_list_with_bounds() {
        let yaml = "version: \"3.13.1\"\nabi: cp313\ninterpreter_package: \"ocx.sh/python/cpython:3.13.1\"\nentrypoints:\n  - name: foo\n  - name: bar\n    min_version: \"1.0.0\"\n    max_version: \"2.0.0\"\n";
        let config: PythonConfig = serde_yaml_ng::from_str(yaml).expect("config parses");
        match config.entrypoints {
            EntrypointsConfig::Explicit(bounds) => {
                assert_eq!(bounds.len(), 2);
                assert_eq!(bounds[0].name, "foo");
                assert!(bounds[0].min_version.is_none());
                assert_eq!(bounds[1].name, "bar");
                assert_eq!(bounds[1].min_version.as_deref(), Some("1.0.0"));
                assert_eq!(bounds[1].max_version.as_deref(), Some("2.0.0"));
            }
            other => panic!("expected Explicit, got {other:?}"),
        }
    }

    // ── resolve_entrypoint_selection ─────────────────────────────────────────

    #[test]
    fn resolve_auto_yields_root_only_with_the_given_root_package() {
        let config = PythonConfig {
            version: "3.13.1".to_string(),
            abi: "cp313".to_string(),
            interpreter_package: "ocx.sh/python/cpython:3.13.1".to_string(),
            lock: None,
            entrypoints: EntrypointsConfig::Mode(EntrypointMode::Auto),
        };
        let selection = config.resolve_entrypoint_selection("1.0.0", "my-app");
        assert!(matches!(
            selection,
            ocx_python::EntrypointSelection::RootOnly { root_package } if root_package == "my-app"
        ));
    }

    #[test]
    fn resolve_all_yields_all() {
        let config = PythonConfig {
            version: "3.13.1".to_string(),
            abi: "cp313".to_string(),
            interpreter_package: "ocx.sh/python/cpython:3.13.1".to_string(),
            lock: None,
            entrypoints: EntrypointsConfig::Mode(EntrypointMode::All),
        };
        let selection = config.resolve_entrypoint_selection("1.0.0", "my-app");
        assert!(matches!(selection, ocx_python::EntrypointSelection::All));
    }

    #[test]
    fn resolve_explicit_keeps_only_bounds_whose_window_contains_the_app_version() {
        let config = PythonConfig {
            version: "3.13.1".to_string(),
            abi: "cp313".to_string(),
            interpreter_package: "ocx.sh/python/cpython:3.13.1".to_string(),
            lock: None,
            entrypoints: EntrypointsConfig::Explicit(vec![
                EntrypointBound {
                    name: "unbounded".to_string(),
                    min_version: None,
                    max_version: None,
                },
                EntrypointBound {
                    name: "too-new".to_string(),
                    min_version: Some("5.0.0".to_string()),
                    max_version: None,
                },
                EntrypointBound {
                    name: "windowed".to_string(),
                    min_version: Some("1.0.0".to_string()),
                    max_version: Some("2.0.0".to_string()),
                },
                EntrypointBound {
                    name: "past-window".to_string(),
                    min_version: Some("1.0.0".to_string()),
                    max_version: Some("1.5.0".to_string()),
                },
            ]),
        };
        let selection = config.resolve_entrypoint_selection("1.5.0", "my-app");
        match selection {
            ocx_python::EntrypointSelection::Explicit(names) => {
                assert_eq!(names, vec!["unbounded".to_string(), "windowed".to_string()]);
            }
            other => panic!("expected Explicit, got {other:?}"),
        }
    }

    #[test]
    fn resolve_explicit_keeps_every_bound_when_app_version_is_unparseable() {
        let config = PythonConfig {
            version: "3.13.1".to_string(),
            abi: "cp313".to_string(),
            interpreter_package: "ocx.sh/python/cpython:3.13.1".to_string(),
            lock: None,
            entrypoints: EntrypointsConfig::Explicit(vec![EntrypointBound {
                name: "foo".to_string(),
                min_version: Some("5.0.0".to_string()),
                max_version: None,
            }]),
        };
        // Fail-open fallback (filter.rs/ExcludeEntry convention): an
        // unparseable app_version keeps every bound rather than dropping all
        // of them.
        let selection = config.resolve_entrypoint_selection("not-a-version", "my-app");
        assert!(
            matches!(selection, ocx_python::EntrypointSelection::Explicit(names) if names == vec!["foo".to_string()])
        );
    }
}
