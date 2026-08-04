// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! PEP 751 `pylock.toml` parser (wheels-only, hash-required subset).
//!
//! Parses the fields `ocx_python` needs to select and mirror wheels and
//! rejects locks it cannot faithfully translate: an entry with no wheels
//! (sdist-only) or a wheel missing its `sha256` hash is a hard
//! [`LockError`], because both would force either a build step (out of scope —
//! wheels only) or an unverifiable mirror.

/// A parsed PEP 751 lock, reduced to the wheels-only subset.
///
/// Only the fields relevant to wheel selection and mirroring are retained;
/// unknown keys in the source TOML are ignored so a newer `pylock.toml` still
/// parses as long as its required subset is intact.
#[derive(Debug, Clone)]
pub struct Pylock {
    /// The `lock-version` field (PEP 751), e.g. `"1.0"`.
    pub lock_version: String,
    /// The `requires-python` specifier, when present (e.g. `">=3.9"`).
    pub requires_python: Option<String>,
    /// The lock's top-level `extras` key: the set of extras the lock was
    /// resolved with. Drives extras-gated entrypoint synthesis in `compose`;
    /// `EnvSpec::requested_extras` is validated against this set.
    pub extras: Vec<String>,
    /// The locked packages, in lock order.
    pub packages: Vec<LockedPackage>,
}

/// A single locked package with its candidate wheels.
#[derive(Debug, Clone)]
pub struct LockedPackage {
    /// Normalized distribution name (e.g. `"charset-normalizer"`).
    pub name: String,
    /// The pinned project version (e.g. `"3.4.0"`).
    pub version: String,
    /// The package's PEP 508 environment marker, when present
    /// (e.g. `sys_platform == "win32"`). Evaluated during selection.
    pub marker: Option<String>,
    /// The candidate wheels for this package (never empty — a package with
    /// only sdists is rejected at parse time as [`LockError::SdistOnly`]).
    pub wheels: Vec<LockedWheel>,
}

/// A single wheel candidate for a [`LockedPackage`].
#[derive(Debug, Clone)]
pub struct LockedWheel {
    /// The wheel filename (e.g. `numpy-2.1.3-cp313-cp313-manylinux_2_28_x86_64.whl`).
    ///
    /// Honors the explicit `name` field of the wheel table when present,
    /// falling back to the URL-derived basename otherwise — PEP 751 permits
    /// both, and the explicit field wins.
    pub filename: String,
    /// The wheel's download URL, when the lock provides one. Absent for
    /// path-based locks; the consumer owns fetching.
    pub url: Option<String>,
    /// The wheel's `sha256` hash (hex, no `sha256:` prefix). Required — a
    /// wheel table without it is rejected as [`LockError::MissingHash`].
    pub sha256: String,
}

/// Parses a PEP 751 `pylock.toml` document into the wheels-only [`Pylock`]
/// subset.
///
/// # Errors
///
/// Returns [`LockError::Parse`] on malformed TOML or a missing required field,
/// [`LockError::SdistOnly`] for a package that ships no wheels, and
/// [`LockError::MissingHash`] for a wheel without a `sha256` hash.
pub fn parse_pylock(input: &str) -> Result<Pylock, LockError> {
    let raw: RawPylock = toml::from_str(input).map_err(LockError::Parse)?;

    let packages = raw
        .packages
        .into_iter()
        .map(convert_package)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(Pylock {
        lock_version: raw.lock_version,
        requires_python: raw.requires_python,
        extras: raw.extras,
        packages,
    })
}

/// Converts a raw package table, rejecting a package that ships no wheels
/// (sdist-only) — whether or not the source TOML also carries an `[sdist]`
/// table is irrelevant to this wheels-only subset.
fn convert_package(raw: RawPackage) -> Result<LockedPackage, LockError> {
    let RawPackage {
        name,
        version,
        marker,
        wheels,
    } = raw;
    if wheels.is_empty() {
        return Err(LockError::SdistOnly { package: name });
    }

    let wheels = wheels
        .into_iter()
        .map(|wheel| convert_wheel(&name, wheel))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(LockedPackage {
        name,
        version,
        marker,
        wheels,
    })
}

/// Converts a raw wheel table, resolving its filename (explicit `name` field
/// takes precedence over the URL-derived basename — PEP 751 errata, March
/// 2026) before that filename is used anywhere, including in a
/// [`LockError::MissingHash`] message.
fn convert_wheel(package: &str, wheel: RawWheel) -> Result<LockedWheel, LockError> {
    let filename = wheel
        .name
        .or_else(|| wheel.url.as_deref().and_then(url_basename))
        .ok_or_else(|| {
            LockError::Parse(<toml::de::Error as serde::de::Error>::custom(format!(
                "wheel for package '{package}' has neither a name nor a url"
            )))
        })?;

    let sha256 = wheel
        .hashes
        .get("sha256")
        .cloned()
        .ok_or_else(|| LockError::MissingHash {
            package: package.to_string(),
            filename: filename.clone(),
        })?;

    Ok(LockedWheel {
        filename,
        url: wheel.url,
        sha256,
    })
}

/// The last `/`-separated segment of a wheel URL, or `None` for an empty
/// segment (e.g. a URL ending in `/`).
fn url_basename(url: &str) -> Option<String> {
    url.rsplit('/')
        .next()
        .map(str::to_string)
        .filter(|segment| !segment.is_empty())
}

/// Raw `pylock.toml` document shape for `toml`/`serde` deserialization.
/// Unknown keys are ignored (no `deny_unknown_fields`), so newer lock fields
/// this crate doesn't need still parse.
#[derive(serde::Deserialize)]
struct RawPylock {
    #[serde(rename = "lock-version")]
    lock_version: String,
    #[serde(rename = "requires-python")]
    requires_python: Option<String>,
    #[serde(default)]
    extras: Vec<String>,
    #[serde(default)]
    packages: Vec<RawPackage>,
}

#[derive(serde::Deserialize)]
struct RawPackage {
    name: String,
    version: String,
    marker: Option<String>,
    #[serde(default)]
    wheels: Vec<RawWheel>,
}

#[derive(serde::Deserialize)]
struct RawWheel {
    name: Option<String>,
    url: Option<String>,
    #[serde(default)]
    hashes: std::collections::HashMap<String, String>,
}

/// Errors from parsing a `pylock.toml`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LockError {
    /// The TOML is malformed or a required field is missing/ill-typed.
    #[error("invalid pylock.toml")]
    Parse(#[source] toml::de::Error),
    /// A locked package ships no wheels (sdist-only). Building from source is
    /// out of scope — the lock must resolve to wheels.
    #[error("package '{package}' has no wheels (sdist-only)")]
    SdistOnly {
        /// The offending package name.
        package: String,
    },
    /// A wheel entry is missing its required `sha256` hash.
    #[error("wheel '{filename}' for package '{package}' is missing a sha256 hash")]
    MissingHash {
        /// The package the wheel belongs to.
        package: String,
        /// The wheel filename.
        filename: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multi_wheel_lock() {
        let toml = r#"
lock-version = "1.0"
requires-python = ">=3.9"

[[packages]]
name = "example"
version = "1.0.0"

[[packages.wheels]]
url = "https://files.pythonhosted.org/example-1.0.0-py3-none-any.whl"
hashes = { sha256 = "aaaa" }

[[packages.wheels]]
url = "https://files.pythonhosted.org/example-1.0.0-cp313-cp313-manylinux_2_28_x86_64.whl"
hashes = { sha256 = "bbbb" }
"#;

        let lock = parse_pylock(toml).expect("valid lock parses");
        assert_eq!(lock.lock_version, "1.0");
        assert_eq!(lock.requires_python.as_deref(), Some(">=3.9"));
        assert_eq!(lock.packages.len(), 1);

        let package = &lock.packages[0];
        assert_eq!(package.name, "example");
        assert_eq!(package.wheels.len(), 2);
        assert_eq!(package.wheels[0].filename, "example-1.0.0-py3-none-any.whl");
        assert_eq!(package.wheels[0].sha256, "aaaa");
        assert_eq!(package.wheels[1].sha256, "bbbb");
    }

    #[test]
    fn rejects_sdist_only_package() {
        let toml = r#"
lock-version = "1.0"

[[packages]]
name = "no-wheels-here"
version = "1.0.0"
"#;

        let error = parse_pylock(toml).expect_err("sdist-only package must be rejected");
        match error {
            LockError::SdistOnly { package } => assert_eq!(package, "no-wheels-here"),
            other => panic!("expected SdistOnly, got {other:?}"),
        }
    }

    #[test]
    fn rejects_wheel_missing_hash() {
        let toml = r#"
lock-version = "1.0"

[[packages]]
name = "unverifiable"
version = "1.0.0"

[[packages.wheels]]
url = "https://files.pythonhosted.org/unverifiable-1.0.0-py3-none-any.whl"
"#;

        let error = parse_pylock(toml).expect_err("wheel without a sha256 hash must be rejected");
        match error {
            LockError::MissingHash { package, filename } => {
                assert_eq!(package, "unverifiable");
                assert_eq!(filename, "unverifiable-1.0.0-py3-none-any.whl");
            }
            other => panic!("expected MissingHash, got {other:?}"),
        }
    }

    #[test]
    fn explicit_name_field_takes_precedence_over_url_basename() {
        let toml = r#"
lock-version = "1.0"

[[packages]]
name = "renamed"
version = "1.0.0"

[[packages.wheels]]
name = "renamed-1.0.0-py3-none-any.whl"
url = "https://files.pythonhosted.org/redirect/download?id=1234"
hashes = { sha256 = "cccc" }
"#;

        let lock = parse_pylock(toml).expect("valid lock parses");
        assert_eq!(lock.packages[0].wheels[0].filename, "renamed-1.0.0-py3-none-any.whl");
    }

    #[test]
    fn parses_top_level_extras() {
        let toml = r#"
lock-version = "1.0"
extras = ["dev", "test"]

[[packages]]
name = "example"
version = "1.0.0"

[[packages.wheels]]
url = "https://files.pythonhosted.org/example-1.0.0-py3-none-any.whl"
hashes = { sha256 = "aaaa" }
"#;

        let lock = parse_pylock(toml).expect("valid lock parses");
        assert_eq!(lock.extras, vec!["dev".to_string(), "test".to_string()]);
    }

    #[test]
    fn preserves_marker_string() {
        let toml = r#"
lock-version = "1.0"

[[packages]]
name = "colorama"
version = "0.4.6"
marker = "sys_platform == 'win32'"

[[packages.wheels]]
url = "https://files.pythonhosted.org/colorama-0.4.6-py3-none-any.whl"
hashes = { sha256 = "aaaa" }
"#;

        let lock = parse_pylock(toml).expect("valid lock parses");
        assert_eq!(lock.packages[0].marker.as_deref(), Some("sys_platform == 'win32'"));
    }

    #[test]
    fn rejects_malformed_toml() {
        let error = parse_pylock("this is not valid toml [[[").expect_err("malformed toml must be rejected");
        assert!(matches!(error, LockError::Parse(_)));
    }
}
