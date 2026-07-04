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

use std::fmt::Display;
use std::str::FromStr;

use uv_distribution_filename::WheelFilename;

use crate::select::WheelRef;

/// Fallback index-host segment for a `WheelRef` with no URL.
///
/// Per the frozen contract (design spec, `naming` module), `select` (W1.3)
/// rejects URL-less wheels before they reach this crate, so this value is a
/// documented safety net, not an expected path — it keeps `wheel_reference`
/// infallible instead of returning `Result` for a case that should not occur.
const NO_URL_INDEX_HOST: &str = "unknown-index-host";

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
    let index_host = wheel.url.as_deref().and_then(extract_host).unwrap_or(NO_URL_INDEX_HOST);
    let package = normalize_package_name(&wheel.name);
    let slug = wheel_slug(&wheel.filename);
    WheelReference {
        repository: format!("{}/{index_host}/{package}/{slug}", scope.as_str()),
        tag: wheel.sha256.clone(),
    }
}

/// Extracts the host from a URL, stripping scheme, userinfo, port, and path.
///
/// Minimal hand-rolled parser: this crate carries no `url` dependency (that
/// crate is mirror-only per CLAUDE.md's dependency model), and wheel index
/// URLs (e.g. `https://files.pythonhosted.org/...`) are plain
/// `scheme://host/path` with no userinfo or IPv6 literal in practice.
fn extract_host(url: &str) -> Option<&str> {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority_end = after_scheme.find(['/', '?', '#']).unwrap_or(after_scheme.len());
    let authority = &after_scheme[..authority_end];
    let host = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    let host = host.split(':').next().unwrap_or(host);
    (!host.is_empty()).then_some(host)
}

/// PEP 503 normalization: lowercase, runs of `-`/`_`/`.` collapsed to a
/// single `-`. Equivalent to `re.sub(r"[-_.]+", "-", name).lower()`.
fn normalize_package_name(name: &str) -> String {
    let mut normalized = String::with_capacity(name.len());
    let mut last_was_separator = false;
    for ch in name.chars() {
        if matches!(ch, '-' | '_' | '.') {
            if !last_was_separator {
                normalized.push('-');
                last_was_separator = true;
            }
        } else {
            normalized.push(ch.to_ascii_lowercase());
            last_was_separator = false;
        }
    }
    normalized
}

/// Filesystem-safe build/variant disambiguator: the wheel's build tag (if
/// any) plus its ABI and platform tags — deliberately NOT the Python tag
/// (design spec, naming convention #1: the slug disambiguates
/// build/variant, not interpreter). `AbiTag`/`PlatformTag` `Display` output
/// is already lowercase alphanumeric plus `_`/`.` (`uv-platform-tags`), so no
/// extra sanitization is needed once parsed.
fn wheel_slug(filename: &str) -> String {
    match WheelFilename::from_str(filename) {
        Ok(parsed) => {
            let abi = join_tags(parsed.abi_tags());
            let platform = join_tags(parsed.platform_tags());
            match parsed.build_tag() {
                Some(build) => format!("{build}-{abi}-{platform}"),
                None => format!("{abi}-{platform}"),
            }
        }
        // Defensive fallback only: `select` (W1.3) already parses every
        // candidate filename via the same crate before producing a
        // `WheelRef`, so this arm should be unreachable in practice — it
        // exists so this function stays infallible.
        Err(_) => sanitize_for_slug(filename.trim_end_matches(".whl")),
    }
}

fn join_tags<T: Display>(tags: &[T]) -> String {
    tags.iter().map(ToString::to_string).collect::<Vec<_>>().join(".")
}

fn sanitize_for_slug(raw: &str) -> String {
    raw.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wheel_ref(name: &str, filename: &str, url: Option<&str>, sha256: &str) -> WheelRef {
        WheelRef {
            name: name.to_string(),
            version: "1.0.0".to_string(),
            filename: filename.to_string(),
            url: url.map(str::to_string),
            sha256: sha256.to_string(),
        }
    }

    #[test]
    fn normalizes_package_names_per_pep_503() {
        assert_eq!(normalize_package_name("Flask_Cors"), "flask-cors");
        assert_eq!(normalize_package_name("foo..bar"), "foo-bar");
        assert_eq!(normalize_package_name("foo---bar"), "foo-bar");
        assert_eq!(normalize_package_name("A.B_C-D"), "a-b-c-d");
    }

    #[test]
    fn renders_full_reference_for_pythonhosted_wheel() {
        let wheel = wheel_ref(
            "Flask-Cors",
            "flask_cors-4.0.0-py2.py3-none-any.whl",
            Some("https://files.pythonhosted.org/packages/aa/bb/flask_cors-4.0.0-py2.py3-none-any.whl"),
            "deadbeef",
        );

        let reference = wheel_reference(&WheelScope::default(), &wheel);

        assert_eq!(
            reference.repository,
            "pip-packages/files.pythonhosted.org/flask-cors/none-any"
        );
        assert_eq!(reference.tag, "deadbeef");
        assert_eq!(
            reference.to_string(),
            "pip-packages/files.pythonhosted.org/flask-cors/none-any:deadbeef"
        );
    }

    #[test]
    fn slug_includes_build_tag_when_present_and_is_deterministic() {
        let with_build = wheel_slug("foo-1.2.3-202206090410-py3-none-any.whl");
        assert_eq!(with_build, "202206090410-none-any");
        // Determinism: re-deriving from the same filename yields the same slug.
        assert_eq!(wheel_slug("foo-1.2.3-202206090410-py3-none-any.whl"), with_build);

        let without_build = wheel_slug("foo-1.2.3-py3-none-any.whl");
        assert_eq!(without_build, "none-any");

        // Compound (multi-tag) platform tags join with `.`, mirroring the
        // wheel filename's own tag-set syntax.
        let compound = wheel_slug("numpy-1.26.2-cp311-cp311-manylinux_2_17_x86_64.manylinux2014_x86_64.whl");
        assert_eq!(compound, "cp311-manylinux_2_17_x86_64.manylinux2014_x86_64");
    }

    #[test]
    fn scope_defaults_to_pip_packages_and_can_be_overridden() {
        let wheel = wheel_ref(
            "foo",
            "foo-1.2.3-py3-none-any.whl",
            Some("https://example.com/foo.whl"),
            "cafebabe",
        );

        let default_reference = wheel_reference(&WheelScope::default(), &wheel);
        assert!(default_reference.repository.starts_with("pip-packages/"));

        let custom_reference = wheel_reference(&WheelScope::new("acme-wheels"), &wheel);
        assert!(custom_reference.repository.starts_with("acme-wheels/"));
    }

    #[test]
    fn missing_url_falls_back_to_documented_host() {
        let wheel = wheel_ref("foo", "foo-1.2.3-py3-none-any.whl", None, "cafebabe");

        let reference = wheel_reference(&WheelScope::default(), &wheel);

        assert_eq!(
            reference.repository,
            format!("pip-packages/{NO_URL_INDEX_HOST}/foo/none-any")
        );
    }
}
