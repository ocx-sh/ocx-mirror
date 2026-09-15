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
//! On top of both come the operator's **extra CA roots** — `OCX_EXTRA_CA_CERTS`
//! (a PEM path, or the PEM text itself), the same variable and the same
//! [`ocx_lib::tls`] ladder `ocx` reads, resolved once at startup by
//! [`install_extra_roots`] and appended by every factory in this crate:
//! [`builder`] here, and the three OCI transports (`registry_client`,
//! `registry_sync::source_read_seam`, `registry_copy::client_config`) through
//! [`extra_roots`]. A corporate CA that is not in the platform store — a
//! stripped runner, a container — reaches every leg this way, not just the
//! `ocx` children.
//!
//! The seeding routines are `ocx_lib`'s, not copies — the submodule is a path
//! dependency on the same reqwest major, so its `ClientBuilder` is this
//! crate's `ClientBuilder` and `forge::github`, the ocx index transport and
//! every mirror leg run the same code. Keeping a second implementation here is
//! how the two drift.
//!
//! The OCI transport is deliberately **not** routed through here: it belongs to
//! `ocx_lib`, which configures its own roots, timeouts and auth ladder.

use std::sync::{LazyLock, OnceLock};
use std::time::Duration;

use ocx_lib::tls::{ExtraRoots, TlsError};

use crate::error::MirrorError;

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
/// The same set is installed for `ocx_lib`'s Sigstore clients, so a leg that
/// reaches a trust service through the library trusts what the registry legs
/// trust.
///
/// **Blocking** (a bounded file read and a probe build) — startup only.
///
/// # Errors
///
/// The [`TlsError`] `ocx_lib` raised, verbatim; the caller classifies it.
pub fn install_extra_roots() -> Result<(), TlsError> {
    let env = ocx_lib::env::var(ocx_lib::env::keys::OCX_EXTRA_CA_CERTS);
    let roots = ocx_lib::tls::resolve_extra_roots(&ocx_lib::Config::default(), env.as_deref(), None)?;
    ocx_lib::tls::install_sigstore_roots(roots.clone());
    // A second install is a no-op: the roots validated at startup are what
    // every later client uses for the process's lifetime. Unreachable while
    // `main` installs once; a trace the day that stops being true, since the
    // second set would otherwise vanish silently.
    if let Err(later) = EXTRA_ROOTS.set(roots)
        && EXTRA_ROOTS.get() != Some(&later)
    {
        ocx_lib::log::debug!(
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
/// runs — the same shape as `ocx_lib::tls::sigstore_roots`.
pub(crate) fn extra_roots() -> &'static ExtraRoots {
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
pub(crate) fn builder() -> reqwest::ClientBuilder {
    extra_roots().seed(ocx_lib::utility::tls::seed_embedded_roots(
        reqwest::Client::builder().connect_timeout(CONNECT_TIMEOUT),
    ))
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
    ///
    /// The scan reads production code only — everything from the first
    /// `#[cfg(test)]` on is test code, and `//` comment lines are dropped, so
    /// a factory that is merely *mentioned* in a doc comment neither counts as
    /// a factory nor as an offender. Each needle proves itself on a synthetic
    /// positive control first: a needle emptied by a careless edit would
    /// otherwise turn the whole scan green.
    #[test]
    fn every_production_client_is_built_through_the_factory() {
        /// The bare constructors a leg must not call: each is a client with
        /// no extra roots, `reqwest::get` included (it builds a fresh
        /// `Client::new()` per call).
        const BARE_CONSTRUCTORS: [&str; 3] = ["reqwest::Client::new()", "reqwest::Client::builder()", "reqwest::get("];
        /// The OCI transports are built outside this module by design
        /// (`ocx_lib` owns their roots, timeouts and auth), so the extra-CA
        /// seam is one call each factory has to make itself:
        /// `ClientBuilder::extra_roots(..)` or a `ClientConfig`
        /// `extra_root_certificates` append, both fed from `extra_roots()`.
        /// A factory without it is a leg a corporate CA never reaches — the
        /// same defect as a bare `reqwest` constructor, one layer down.
        /// `= native::ClientConfig {` rather than the bare type name so a
        /// `-> native::ClientConfig {` signature is not counted as a second
        /// factory (the spelling `registry_copy`'s own tests use).
        const OCI_FACTORIES: [&str; 2] = ["ClientBuilder::new()", "= native::ClientConfig {"];
        const OCI_SEAM: &str = "crate::http::extra_roots()";

        #[derive(Default)]
        struct Scan {
            offenders: Vec<String>,
            /// Production files read.
            files: usize,
            /// Files in which an OCI factory was seen.
            oci_factory_files: usize,
        }

        /// One file's production lines, comment lines dropped.
        ///
        /// The cut is the `#[cfg(test)]` that gates the test *module* — the
        /// first one whose next non-blank, non-attribute line is a `mod` —
        /// not the first `#[cfg(test)]` in the file: a single-item
        /// `#[cfg(test)] use …` above the module would otherwise end the
        /// scan there and leave everything below it unread.
        fn production_lines(source: &str) -> Vec<(usize, &str)> {
            let lines: Vec<&str> = source.lines().collect();
            let opens_test_module = |index: usize| {
                lines[index].trim() == "#[cfg(test)]"
                    && lines[index + 1..]
                        .iter()
                        .map(|line| line.trim())
                        .find(|line| !line.is_empty() && !line.starts_with("#["))
                        .is_some_and(|line| line.starts_with("mod ") || line.contains(" mod "))
            };
            let end = (0..lines.len())
                .find(|&index| opens_test_module(index))
                .unwrap_or(lines.len());
            lines[..end]
                .iter()
                .copied()
                .enumerate()
                .filter(|(_, line)| !line.trim_start().starts_with("//"))
                .collect()
        }

        fn check(label: &str, source: &str, scan: &mut Scan) {
            scan.files += 1;
            let lines = production_lines(source);
            for (offset, line) in &lines {
                if BARE_CONSTRUCTORS.iter().any(|needle| line.contains(needle)) {
                    scan.offenders.push(format!("{label}:{}", offset + 1));
                }
            }
            let factories: usize = lines
                .iter()
                .map(|(_, line)| {
                    OCI_FACTORIES
                        .iter()
                        .map(|needle| line.matches(needle).count())
                        .sum::<usize>()
                })
                .sum();
            let seams: usize = lines.iter().map(|(_, line)| line.matches(OCI_SEAM).count()).sum();
            if factories > 0 {
                scan.oci_factory_files += 1;
            }
            if seams < factories {
                scan.offenders.push(format!(
                    "{label} ({factories} OCI client factories, {seams} extra_roots() seams)"
                ));
            }
        }

        fn scan(dir: &std::path::Path, scan_state: &mut Scan) {
            for entry in std::fs::read_dir(dir).expect("src/ must be readable") {
                let path = entry.expect("a readable directory entry").path();
                if path.is_dir() {
                    if path.file_name().is_some_and(|name| name == "tests") {
                        continue;
                    }
                    scan(&path, scan_state);
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
                check(&path.display().to_string(), &source, scan_state);
            }
        }

        // The cut is the test *module*, not the first `#[cfg(test)]`: a
        // single-item `#[cfg(test)] use …` above it (push.rs has one) must
        // not end the scan early, and a `#[path]` attribute between the
        // `cfg` and its `mod` must not hide the module.
        let mut after_import = Scan::default();
        check(
            "control",
            "#[cfg(test)]\nuse x;\nlet c = reqwest::Client::new();\n#[cfg(test)]\nmod tests {\nreqwest::get(\n}\n",
            &mut after_import,
        );
        assert_eq!(
            after_import.offenders,
            vec!["control:3"],
            "a needle after a `#[cfg(test)] use` import is production code; one inside the test module is not"
        );
        let mut pathed = Scan::default();
        check(
            "control",
            "#[cfg(test)]\n#[path = \"x/tests.rs\"]\nmod tests;\nreqwest::get(\n",
            &mut pathed,
        );
        assert!(
            pathed.offenders.is_empty(),
            "a `#[path]` between `#[cfg(test)]` and `mod tests;` must not hide the test module"
        );
        let push_rs = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/command/package/pipeline/push.rs"),
        )
        .expect("push.rs is readable");
        let first_cfg_test = push_rs
            .lines()
            .position(|line| line.trim() == "#[cfg(test)]")
            .expect("push.rs carries a `#[cfg(test)]` import above its test module");
        let last_scanned = production_lines(&push_rs).last().map(|(offset, _)| *offset);
        assert!(
            last_scanned.is_some_and(|last| last > first_cfg_test),
            "push.rs must be scanned past its `#[cfg(test)] use` import at line {}, scanned through {last_scanned:?}",
            first_cfg_test + 1
        );

        // Positive controls: every needle flags on a synthetic source, a
        // comment line flags nothing, and the OCI clause counts rather than
        // merely finds the seam.
        for needle in BARE_CONSTRUCTORS {
            let mut control = Scan::default();
            check("control", &format!("let c = {needle}url);\n"), &mut control);
            assert_eq!(
                control.offenders,
                vec!["control:1"],
                "needle {needle:?} must flag a bare constructor"
            );
            let mut commented = Scan::default();
            check("control", &format!("    // {needle}\n"), &mut commented);
            assert!(
                commented.offenders.is_empty(),
                "a comment line must not flag {needle:?}"
            );
        }
        for needle in OCI_FACTORIES {
            let mut control = Scan::default();
            check(
                "control",
                &format!("let a {needle}\nlet b {needle}\n{OCI_SEAM}\n// {OCI_SEAM}\n"),
                &mut control,
            );
            assert_eq!(control.oci_factory_files, 1);
            assert_eq!(
                control.offenders,
                vec!["control (2 OCI client factories, 1 extra_roots() seams)"],
                "needle {needle:?} must count every factory against the seams"
            );
        }

        let mut state = Scan::default();
        scan(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src").as_path(),
            &mut state,
        );
        assert!(state.files > 0, "the scan must have read production files");
        assert!(
            state.oci_factory_files > 0,
            "the scan must have met the OCI factories it exists to check"
        );
        assert!(
            state.offenders.is_empty(),
            "these legs bypass `crate::http` and so drop the trust roots a corporate CA needs: {:#?}",
            state.offenders
        );
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
        let _guard = crate::test_support::ocx_env_lock();
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        assert!(extra_roots().is_empty(), "nothing is installed yet");

        let restore =
            crate::test_support::EnvRestore::set(&[(ocx_lib::env::keys::OCX_EXTRA_CA_CERTS, Some(TEST_ROOT_PEM))]);
        let installed = install_extra_roots();
        drop(restore);
        installed.expect("a well-formed PEM bundle installs");
        assert_eq!(extra_roots().len(), 1, "the install must be what a later read sees");

        // The variable is unset again, so this resolves to the empty set — and
        // must neither fail nor replace what startup validated.
        let _restore = crate::test_support::EnvRestore::set(&[(ocx_lib::env::keys::OCX_EXTRA_CA_CERTS, None)]);
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
        let empty = ocx_lib::tls::resolve_extra_roots(&ocx_lib::Config::default(), None, None)
            .expect("no variable is no roots");
        assert!(empty.is_empty());
        let refused = ocx_lib::tls::resolve_extra_roots(&ocx_lib::Config::default(), Some("not a certificate"), None);
        assert!(
            refused.is_err(),
            "a value that is neither PEM nor a readable path is refused"
        );
    }
}
