// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! The one place a mirror-owned `reqwest::Client` is constructed.
//!
//! # Why a factory rather than `Client::new()` at each site
//!
//! Trust roots, and a build that cannot fail open.
//!
//! reqwest's rustls path resolves roots one of two ways, chosen by whether the
//! builder carries any explicit root: with none it calls
//! `rustls_platform_verifier::Verifier::new`, which fails on a host whose trust
//! store is empty (distroless image, stripped CI runner) — and `Client::new`
//! turns that failure into a panic. With at least one it calls
//! `Verifier::new_with_extra_roots`, which keeps the platform store *and* adds
//! what the builder carries.
//!
//! [`builder`] therefore seeds the bundled Mozilla set through
//! [`ocx_lib::utility::tls::seed_embedded_roots`], putting every client on the
//! second branch: the operator's corporate CA arrives from the platform store
//! (which is what makes `SSL_CERT_FILE` / `SSL_CERT_DIR` work), and the bundled
//! roots keep a store-less host serving public hosts anyway.
//!
//! The seeding routine is `ocx_lib`'s, not a copy — the submodule is a path
//! dependency on the same reqwest major, so its `ClientBuilder` is this
//! crate's `ClientBuilder` and `forge::github`, the ocx index transport and
//! every mirror leg run the same code. Keeping a second implementation here is
//! how the two drift.
//!
//! The OCI transport is deliberately **not** routed through here: it belongs to
//! `ocx_lib`, which configures its own roots, timeouts and auth ladder.

use std::time::Duration;

use crate::error::MirrorError;

/// Bound on the connect phase of every mirror-owned request.
///
/// Matches `registry_sync::catalog`'s `INDEX_CONNECT_TIMEOUT` and ocx's
/// `REGISTRY_CONNECT_TIMEOUT`: without it a black-holing firewall leaves the
/// socket in `SYN_SENT` until the OS gives up, which on Linux is over two
/// minutes.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// A [`reqwest::ClientBuilder`] with the mirror's trust roots and connect
/// bound already applied.
///
/// Callers that need a different redirect policy or request timeout layer it
/// on top rather than starting from `reqwest::Client::builder()` — starting
/// over is what silently drops the platform roots again.
pub(crate) fn builder() -> reqwest::ClientBuilder {
    ocx_lib::utility::tls::seed_embedded_roots(reqwest::Client::builder().connect_timeout(CONNECT_TIMEOUT))
}

/// The default client: [`builder`] with no further configuration.
///
/// # Errors
///
/// [`MirrorError::ExecutionFailed`] when the TLS backend cannot be built.
pub(crate) fn client() -> Result<reqwest::Client, MirrorError> {
    builder()
        .build()
        .map_err(|error| MirrorError::ExecutionFailed(vec![format!("cannot build an HTTP client: {error}")]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The client builds, which is what the seeding buys.
    ///
    /// reqwest exposes no getter for the resolved root set, so the assertion is
    /// the observable one: construction succeeds. It is not vacuous — an
    /// unseeded builder takes the `Verifier::new` branch, and that branch is
    /// exactly the one that fails on a host with an empty trust store, which
    /// some CI images are.
    #[test]
    fn client_builds_with_both_root_sets() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        client().expect("the mirror HTTP client must build");
    }

    /// No production leg constructs its own `reqwest::Client`.
    ///
    /// The factory is only worth having if every caller goes through it, and
    /// that is precisely what does not hold on its own: the commit introducing
    /// this module converted the legs it was written for and left `package
    /// sync`, `pipeline describe` and the Discord webhook on
    /// `reqwest::Client::new()` — three paths that kept failing behind a
    /// corporate proxy after the bug was declared fixed. Nothing about a bare
    /// constructor looks wrong at the call site, so the guard is a scan rather
    /// than a review note.
    ///
    /// Test corpora are exempt: a test client talks to a loopback listener and
    /// wants no roots at all.
    #[test]
    fn every_production_client_is_built_through_the_factory() {
        fn scan(dir: &std::path::Path, offenders: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).expect("src/ must be readable") {
                let path = entry.expect("a readable directory entry").path();
                if path.is_dir() {
                    if path.file_name().is_some_and(|name| name == "tests") {
                        continue;
                    }
                    scan(&path, offenders);
                    continue;
                }
                let name = path.file_name().unwrap_or_default().to_string_lossy().to_string();
                if path.extension().is_none_or(|ext| ext != "rs")
                    || name == "http.rs"
                    || name == "tests.rs"
                    || name == "test_support.rs"
                {
                    continue;
                }
                let source = std::fs::read_to_string(&path).expect("a readable Rust source file");
                // Inline `#[cfg(test)]` modules sit at the end of a file, so
                // everything from the first one on is test code.
                let production = source.split("#[cfg(test)]").next().unwrap_or_default();
                for (offset, line) in production.lines().enumerate() {
                    if line.contains("reqwest::Client::new()") || line.contains("reqwest::Client::builder()") {
                        offenders.push(format!("{}:{}", path.display(), offset + 1));
                    }
                }
            }
        }

        let mut offenders = Vec::new();
        scan(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src").as_path(),
            &mut offenders,
        );
        assert!(
            offenders.is_empty(),
            "these legs bypass `crate::http` and so drop the platform trust roots a corporate CA needs: {offenders:#?}"
        );
    }
}
