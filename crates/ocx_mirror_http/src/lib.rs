// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! The mirror's HTTP plumbing:
//!
//! - the `reqwest` client factory ([`builder`], [`client`]) and the trust roots
//!   it seeds ([`install_extra_roots`], [`extra_roots`]);
//! - [`auth`] — host-keyed credentials (`OCX_AUTH_*`, netrc) for its own legs;
//! - [`retry::jitter`] — the ±10% spread the retry ladders put on each backoff;
//! - [`read_capped`] — a response body read that refuses to buffer past a cap.
//!
//! The rest of this page is about the first: the one place a mirror-owned
//! `reqwest::Client` is constructed.
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
//! [`ocx_util::tls::seed_embedded_roots`], putting every client on the
//! second branch: the operator's corporate CA arrives from the platform store
//! (which is what makes `SSL_CERT_FILE` / `SSL_CERT_DIR` work), and the bundled
//! roots keep a store-less host serving public hosts anyway.
//!
//! On top of both come the operator's **extra CA roots** — `OCX_EXTRA_CA_CERTS`
//! (a PEM path, or the PEM text itself), the same variable and the same
//! [`ocx_config::tls`] ladder `ocx` reads, resolved once at startup by
//! [`install_extra_roots`] and appended by every factory in the mirror:
//! [`builder`] here, and the three OCI transports (`registry_client`,
//! `registry_sync::source_read_seam`, `registry_copy::client_config`) through
//! [`extra_roots`]. A corporate CA that is not in the platform store — a
//! stripped runner, a container — reaches every leg this way, not just the
//! `ocx` children.
//!
//! The seeding routines are `ocx_util`'s, not copies — the submodule is a path
//! dependency on the same reqwest major, so its `ClientBuilder` is this
//! crate's `ClientBuilder` and `forge::github`, the ocx index transport and
//! every mirror leg run the same code. Keeping a second implementation here is
//! how the two drift.
//!
//! The OCI transport is deliberately **not** routed through here: it belongs to
//! `ocx_oci`, which configures its own roots, timeouts and auth ladder.

use std::sync::{LazyLock, OnceLock};
use std::time::Duration;

use ocx_config::tls::TlsError;
use ocx_util::tls::ExtraRoots;

pub mod auth;
pub mod retry;

/// What [`client`] and [`read_capped`] raise.
///
/// Each variant carries the message the `MirrorError` it replaced carried and
/// renders byte-identically to it (adr_bazel_crate_split.md § C3, E3): these
/// errors also travel through `anyhow` chains, where `{err:#}` prints them.
/// `From<HttpError> for MirrorError` maps each back to that variant.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HttpError {
    /// `MirrorError::ExecutionFailed` with this one line (exit 1); the
    /// trailing newline is the one that variant's `writeln!` emits.
    #[error("mirror execution failed:\n  - {0}\n")]
    ClientBuild(String),
    /// `MirrorError::SourceError` (exit 69).
    #[error("source error: {0}")]
    Source(String),
}

/// The operator's extra CA roots, installed once by [`install_extra_roots`].
static EXTRA_ROOTS: OnceLock<ExtraRoots> = OnceLock::new();

/// Resolve `OCX_EXTRA_CA_CERTS` once, before any client is built.
///
/// `main` calls this before dispatch, so a bundle that cannot be used fails
/// the process there — with `ocx`'s own exit code for it (74 unreadable file,
/// 65 bad file content, 78 bad inline value) — rather than surfacing as a TLS
/// handshake error deep inside a leg. The mirror parses no `config.toml`, so
/// the environment arm is the whole ladder and an empty `Config` is the
/// honest input; an unset or empty variable resolves to no roots at all.
///
/// The same set is installed for `ocx_sign`'s Sigstore clients, so a leg that
/// reaches a trust service through the library trusts what the registry legs
/// trust.
///
/// **Blocking** (a bounded file read and a probe build) — startup only.
///
/// # Errors
///
/// The [`TlsError`] `ocx_config` raised, verbatim; the caller classifies it.
pub fn install_extra_roots() -> Result<(), TlsError> {
    let env = ocx_util::env::var(ocx_config::env::keys::OCX_EXTRA_CA_CERTS);
    let roots = ocx_config::tls::resolve_extra_roots(&ocx_config::Config::default(), env.as_deref(), None)?;
    ocx_util::tls::install_sigstore_roots(roots.clone());
    // A second install is a no-op: the roots validated at startup are what
    // every later client uses for the process's lifetime. Unreachable while
    // `main` installs once; a trace the day that stops being true, since the
    // second set would otherwise vanish silently.
    if let Err(later) = EXTRA_ROOTS.set(roots)
        && EXTRA_ROOTS.get() != Some(&later)
    {
        log::debug!(
            "a second install_extra_roots with a different set ({} certificates) is ignored",
            later.len()
        );
    }
    Ok(())
}

/// The installed extra CA roots — empty when [`install_extra_roots`] never
/// ran, which is every unit test and nothing else.
///
/// Reads the cell without `get_or_init`: initialising it with a default would
/// permanently pin the empty set for a caller that reads before the install
/// runs — the same shape as `ocx_util::tls::sigstore_roots`.
pub fn extra_roots() -> &'static ExtraRoots {
    static EMPTY: LazyLock<ExtraRoots> = LazyLock::new(ExtraRoots::default);
    EXTRA_ROOTS.get().unwrap_or(&EMPTY)
}

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
pub fn builder() -> reqwest::ClientBuilder {
    extra_roots().seed(ocx_util::tls::seed_embedded_roots(
        reqwest::Client::builder().connect_timeout(CONNECT_TIMEOUT),
    ))
}

/// The default client: [`builder`] with no further configuration.
///
/// # Errors
///
/// [`HttpError::ClientBuild`] when the TLS backend cannot be built.
pub fn client() -> Result<reqwest::Client, HttpError> {
    builder()
        .build()
        .map_err(|error| HttpError::ClientBuild(format!("cannot build an HTTP client: {error}")))
}

/// Read a response body into memory, refusing more than `cap` bytes.
///
/// `label` names the thing being read in every message: a URL, or a
/// capability-stripped document name where echoing the URL would re-emit a
/// path the caller deliberately withheld.
///
/// Lifted from `dist_sync::fetch_manifest` when a second in-memory foreign
/// body — a resolved `versions:` bound — needed the same two ceilings. The
/// auth, status and 404 policy stay at the call sites: those differ by leg
/// by design, only the read is identical.
///
/// # Errors
///
/// [`HttpError::Source`] when the body is declared or streamed over
/// `cap`, or when a chunk cannot be read.
// ponytail: `registry_sync/catalog.rs` keeps its own copy. Its read error
// goes through `fetch_failed`, so converting it would change a message in a
// subsystem this seam does not own.
pub async fn read_capped(response: &mut reqwest::Response, label: &str, cap: usize) -> Result<Vec<u8>, HttpError> {
    // Refuse a declared oversize body before reading a byte of it.
    if let Some(declared) = response.content_length()
        && declared > cap as u64
    {
        return Err(HttpError::Source(format!(
            "{label} declares {declared} bytes, over the {cap}-byte cap"
        )));
    }

    // An endpoint that omits or lies about `Content-Length` — chunked transfer,
    // or a hostile host — still cannot stream more than the cap into memory,
    // because the running total is checked before each chunk is appended.
    let mut bytes: Vec<u8> = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| HttpError::Source(format!("cannot read {label}: {error}")))?
    {
        if bytes.len() + chunk.len() > cap {
            return Err(HttpError::Source(format!("{label} exceeds the {cap}-byte cap")));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
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
        ocx_mirror_test_support::install_crypto_provider();
        client().expect("the mirror HTTP client must build");
    }

    /// A read before install does not pin the empty set, and a second install
    /// is a harmless no-op — the roots validated at startup are what every
    /// later client uses.
    ///
    /// Every unit test reads [`extra_roots`] through [`client`] before any
    /// install; with `get_or_init(ExtraRoots::default)` that read pinned the
    /// empty set, and the real install at startup vanished silently.
    #[test]
    fn a_read_before_install_does_not_pin_the_empty_set() {
        let _guard = ocx_mirror_test_support::ocx_env_lock();
        ocx_mirror_test_support::install_crypto_provider();
        assert!(extra_roots().is_empty(), "nothing is installed yet");

        let restore = ocx_mirror_test_support::EnvRestore::set(&[(
            ocx_config::env::keys::OCX_EXTRA_CA_CERTS,
            Some(TEST_ROOT_PEM),
        )]);
        let installed = install_extra_roots();
        drop(restore);
        installed.expect("a well-formed PEM bundle installs");
        assert_eq!(extra_roots().len(), 1, "the install must be what a later read sees");

        // The variable is unset again, so this resolves to the empty set — and
        // must neither fail nor replace what startup validated.
        let _restore = ocx_mirror_test_support::EnvRestore::set(&[(ocx_config::env::keys::OCX_EXTRA_CA_CERTS, None)]);
        install_extra_roots().expect("a second install is a no-op, not an error");
        assert_eq!(
            extra_roots().len(),
            1,
            "the installed set is stable across a second install"
        );
    }

    /// A minted P-256 root (`openssl req -x509 -newkey ec`), valid for a
    /// century; any parseable X.509 certificate would do.
    const TEST_ROOT_PEM: &str = "-----BEGIN CERTIFICATE-----
MIIBljCCATugAwIBAgIUY0s7qkDJa7k7d6j/HdWYKmtWuIswCgYIKoZIzj0EAwIw
HzEdMBsGA1UEAwwUb2N4LW1pcnJvciB0ZXN0IHJvb3QwIBcNMjYwOTE1MTkwNzE4
WhgPMjEyNjA4MjIxOTA3MThaMB8xHTAbBgNVBAMMFG9jeC1taXJyb3IgdGVzdCBy
b290MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEAlknaalJMadUUWC1szduhDWA
HJ+nBKJ4J+xiSoiXEZhhB2d8CRd8XTbWw7Xu9xX60NIW0O+oEgqNcCNvRRWrUaNT
MFEwHQYDVR0OBBYEFOMzbqRN0PVqVAM9ZEhvoeo67gAqMB8GA1UdIwQYMBaAFOMz
bqRN0PVqVAM9ZEhvoeo67gAqMA8GA1UdEwEB/wQFMAMBAf8wCgYIKoZIzj0EAwID
SQAwRgIhAIimfmMHX7/vmMP25byiTv2805OMtA09CIveorIXnd22AiEA3cW26Dqb
0JecKf47ZoqlCRaM12ic/Vx0AqZJ4XdPB2I=
-----END CERTIFICATE-----
";

    /// An unset variable is no roots — and `install_extra_roots` reports a
    /// value that cannot be used rather than seeding nothing quietly.
    #[test]
    fn extra_roots_resolve_from_the_environment_arm_alone() {
        let empty = ocx_config::tls::resolve_extra_roots(&ocx_config::Config::default(), None, None)
            .expect("no variable is no roots");
        assert!(empty.is_empty());
        let refused =
            ocx_config::tls::resolve_extra_roots(&ocx_config::Config::default(), Some("not a certificate"), None);
        assert!(
            refused.is_err(),
            "a value that is neither PEM nor a readable path is refused"
        );
    }

    // ── E4 characterization: `read_capped` messages ─────────────────────────
    //
    // Pinned byte for byte before the crate split moved this module, so the
    // move is provably message-preserving. The exit code each variant maps to
    // is pinned once, by the `From<HttpError> for MirrorError` identity table
    // in `ocx_mirror_error`'s tests. `client()`'s own failure arm is not
    // pinned: `builder()` takes no input, and the only way `build()` fails is a
    // TLS backend that cannot initialise, which a test cannot provoke.

    /// Serve `raw` verbatim to the first connection, then hang up.
    async fn serve_raw(raw: &'static str) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let address = listener.local_addr().expect("a bound address");
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("one connection");
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request).await;
            let _ = stream.write_all(raw.as_bytes()).await;
            let _ = stream.shutdown().await;
        });
        format!("http://{address}/versions.json")
    }

    /// `read_capped`'s error for the response `raw` under `cap`.
    async fn read_capped_error(raw: &'static str, cap: usize) -> HttpError {
        ocx_mirror_test_support::install_crypto_provider();
        let url = serve_raw(raw).await;
        let mut response = client()
            .expect("the mirror HTTP client must build")
            .get(url)
            .send()
            .await
            .expect("the fixture answers with headers");
        read_capped(&mut response, "versions document", cap)
            .await
            .expect_err("the fixture body must be refused")
    }

    #[tokio::test]
    async fn a_declared_oversize_body_is_refused_with_its_declared_length() {
        let error = read_capped_error(
            "HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\n0123456789",
            4,
        )
        .await;
        assert_eq!(
            error.to_string(),
            "source error: versions document declares 10 bytes, over the 4-byte cap"
        );
    }

    #[tokio::test]
    async fn a_streamed_oversize_body_is_refused_at_the_cap() {
        let error = read_capped_error(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nhello\r\n5\r\nworld\r\n0\r\n\r\n",
            8,
        )
        .await;
        assert_eq!(
            error.to_string(),
            "source error: versions document exceeds the 8-byte cap"
        );
    }

    #[tokio::test]
    async fn a_body_cut_short_is_a_read_failure() {
        let error = read_capped_error(
            "HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nabc",
            100,
        )
        .await;
        assert_eq!(
            error.to_string(),
            "source error: cannot read versions document: error decoding response body"
        );
    }
}
