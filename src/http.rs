// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! The one place a mirror-owned `reqwest::Client` is constructed.
//!
//! # Why a factory rather than `Client::new()` at each site
//!
//! Trust roots. reqwest's default rustls path here is
//! `rustls-tls-webpki-roots-no-provider`: the bundled Mozilla set and nothing
//! else, so a host's own trust store — and therefore `SSL_CERT_FILE` /
//! `SSL_CERT_DIR` — is never consulted. Behind a TLS-intercepting corporate
//! proxy every fetch then fails certificate verification with no way to fix it
//! short of rebuilding the binary.
//!
//! [`builder`] enables **both** root sets, so the bundled roots keep the binary
//! self-contained on a host with no trust store (distroless image, stripped CI
//! runner) while the platform roots add whatever the operator has installed.
//! `rustls-native-certs` reads `SSL_CERT_FILE` and `SSL_CERT_DIR` on the way,
//! which is what makes a corporate root usable without a new config key.
//!
//! ocx solved the same problem one layer up — see `seed_embedded_roots` in
//! `ocx_lib::utility::tls` and its `environment.md` documentation — but that
//! fix lives in ocx's own reqwest (0.13, a different major), so it never
//! reached the clients this crate builds for itself.
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
    reqwest::Client::builder()
        // Both, not either: bundled roots keep an empty-trust-store host
        // working, platform roots carry the operator's corporate CA.
        .tls_built_in_webpki_certs(true)
        .tls_built_in_native_certs(true)
        .connect_timeout(CONNECT_TIMEOUT)
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

    /// Both root sets enabled, and the resulting client actually builds.
    ///
    /// reqwest 0.12 exposes no getter for the resolved root set, so this
    /// asserts what is observable: construction succeeds with both toggles on.
    /// A feature-flag regression (dropping `rustls-tls-native-roots-no-provider`
    /// from `Cargo.toml`) fails at compile time on `tls_built_in_native_certs`
    /// rather than here, which is the stronger guard.
    #[test]
    fn client_builds_with_both_root_sets() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        client().expect("the mirror HTTP client must build");
    }
}
