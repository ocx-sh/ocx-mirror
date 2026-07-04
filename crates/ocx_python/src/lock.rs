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
    let _ = input;
    unimplemented!("W1.1")
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
