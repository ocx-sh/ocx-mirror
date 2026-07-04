// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Conventional wheel repo naming — the one-way-door naming convention.
//!
//! Renders the repo-relative reference for a mirrored wheel:
//! `<scope>/<index-host>/<package>/<slug>:<sha256>`. The scope is
//! maintainer-configured (default `pip-packages`); `<index-host>` groups
//! wheels by their source index; `<slug>` disambiguates build/variant; the tag
//! is the wheel's `sha256`. The reference is **repo-relative** — it carries no
//! registry host; the consumer prepends the registry when building the final
//! [`ocx_lib::oci::Identifier`].

use crate::select::WheelRef;

/// The default wheel scope when the maintainer configures none.
pub const DEFAULT_WHEEL_SCOPE: &str = "pip-packages";

/// The maintainer-configured scope prefix for mirrored wheel repos.
///
/// Defaults to [`DEFAULT_WHEEL_SCOPE`] via [`WheelScope::default`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WheelScope(String);

impl WheelScope {
    /// Wraps a maintainer-configured scope string.
    pub fn new(scope: impl Into<String>) -> Self {
        Self(scope.into())
    }

    /// The scope as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for WheelScope {
    fn default() -> Self {
        Self(DEFAULT_WHEEL_SCOPE.to_string())
    }
}

/// A rendered, repo-relative wheel reference.
///
/// Splits the conventional string into its repository path and tag so the
/// consumer can attach a registry host and digest without re-parsing.
/// [`Display`](std::fmt::Display) renders `<repository>:<tag>`.
///
/// Assumes the wheel has a URL — `<index-host>` derives from the URL host.
/// URL-less / path-only wheels are not mirrorable and must be rejected upstream
/// in `select` (W1.4), so `wheel_reference` can stay infallible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WheelReference {
    /// The repo-relative repository path
    /// (`<scope>/<index-host>/<package>/<slug>`) — no registry host.
    pub repository: String,
    /// The tag: the wheel `sha256` (hex, no `sha256:` prefix).
    pub tag: String,
}

impl std::fmt::Display for WheelReference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.repository, self.tag)
    }
}

/// Renders the conventional repo-relative [`WheelReference`] for a wheel.
///
/// Pure function: `<scope>/<index-host>/<package>/<slug>:<sha256>`, with the
/// index host derived from [`WheelRef::url`] and the slug disambiguating
/// build/variant. Never emits a registry host (target-agnostic).
pub fn wheel_reference(scope: &WheelScope, wheel: &WheelRef) -> WheelReference {
    let _ = (scope, wheel);
    unimplemented!("W1.4")
}
