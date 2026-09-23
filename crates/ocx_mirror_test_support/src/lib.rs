// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Workspace-wide test scaffolding — a dev-only crate every member may take as
//! a dev-dependency.
//!
//! Lives in its own crate rather than inside whichever module happened to
//! need it first: the process environment these guards serialise is global, so
//! a lock owned by one command module would have to be reached upward by every
//! other test that touches the same variables. One lock per test process
//! suffices — Cargo links a single copy of this crate into each test binary,
//! and the environment is per process.

/// Serialises every test in one test binary that reads or writes the process-global
/// `OCX_*` environment — `OCX_BINARY_PIN` above all.
///
/// One lock, not one per test module: the hazard is a *neighbouring* module's
/// stub. A `pipeline plan` pypi test pinning `OCX_BINARY_PIN` at its `uv`
/// stand-in while a `pipeline push` test assumes "no `ocx` is reachable" makes
/// the push resolve that stand-in and publish into another test's fixture — a
/// failure that reproduces roughly one run in twelve and never in isolation.
///
/// `tokio::sync::Mutex` rather than `std::sync::Mutex`: `lock_derive`'s
/// `#[tokio::test]`s must hold the guard across their subprocess `.await`s
/// (async-aware guard, no `await_holding_lock`), while `push`'s and `plan`'s
/// sync `#[test]`s take it via [`ocx_env_lock`]'s `blocking_lock` *before*
/// entering their `Runtime::block_on`. It is not reentrant, so it is taken by
/// the test, never by a helper.
pub static OCX_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Blocking accessor for [`OCX_ENV_LOCK`] — sync `#[test]` contexts only
/// (`blocking_lock` panics inside a runtime; async tests lock the static
/// directly with `.lock().await`).
pub fn ocx_env_lock() -> tokio::sync::MutexGuard<'static, ()> {
    OCX_ENV_LOCK.blocking_lock()
}

/// Puts the named variables back the way they were when dropped.
///
/// The env tests run in one process under [`OCX_ENV_LOCK`], and an assertion
/// that fails between a `set_var("GITLAB_CI", "true")` and its cleanup would
/// otherwise hand the next lock holder a GitLab job it never asked for.
/// Declare the guard *after* the lock guard so it drops before it.
pub struct EnvRestore(Vec<(&'static str, Option<String>)>);

impl EnvRestore {
    /// Snapshot every name in `vars`, then apply it (`None` removes).
    pub fn set(vars: &[(&'static str, Option<&str>)]) -> Self {
        let saved = vars.iter().map(|(name, _)| (*name, std::env::var(name).ok())).collect();
        // SAFETY: test-only process env, serialised by the caller's lock.
        unsafe {
            for (name, value) in vars {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
        Self(saved)
    }
}

impl Drop for EnvRestore {
    fn drop(&mut self) {
        // SAFETY: test-only process env, still under the caller's lock — the
        // guard is declared after the lock, so it drops before it.
        unsafe {
            for (name, value) in &self.0 {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }
}

/// Neutralises the CI markers `ocx_mirror_spec::annotations::push_args` autodetects,
/// so an argv assertion reads the same on a developer box and on a runner.
///
/// Without it every `build_push_args` / `patch_push_args` / `build_env_push_args`
/// assertion is green locally and red in GitHub Actions, where `GITHUB_ACTIONS=true`
/// appends a `--ci-annotations=github` the expectation does not list.
///
/// Holds [`OCX_ENV_LOCK`] for the same reason the tests that *set* these
/// markers do: the hazard is a neighbouring module's writer. `_restore` is
/// declared before `_lock` so the runner's own markers go back while the lock
/// is still held.
pub struct NoCiEnv {
    _restore: EnvRestore,
    _lock: tokio::sync::MutexGuard<'static, ()>,
}

/// Guard for [`NoCiEnv`] — sync `#[test]` contexts only, as [`ocx_env_lock`].
pub fn no_ci_env() -> NoCiEnv {
    let lock = ocx_env_lock();
    NoCiEnv {
        _restore: EnvRestore::set(&[("GITHUB_ACTIONS", None), ("GITLAB_CI", None)]),
        _lock: lock,
    }
}

/// A stand-in `ocx` whose push fails its first `failures` invocations with
/// `exit_code` and succeeds afterwards, reporting `tag` as the one cascade tag
/// written. The attempt count lands in `{dir}/push-attempts` — read it back
/// with [`push_attempts`]; it is the only way to tell one attempt from four.
#[cfg(unix)]
pub fn fake_ocx_flaky_push(dir: &std::path::Path, failures: u32, exit_code: u8, tag: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = dir.join("fake-ocx");
    let body = format!(
        r#"#!/bin/sh
attempts=$(cat '{counter}' 2>/dev/null || echo 0)
attempts=$((attempts + 1))
echo "$attempts" > '{counter}'
if [ "$attempts" -le {failures} ]; then
  echo 'operation timed out' >&2
  exit {exit_code}
fi
echo '{{"cascade_tags_written":["{tag}"],"status":"pushed"}}'
"#,
        counter = dir.join("push-attempts").display(),
    );
    std::fs::write(&script, body).expect("write the fake ocx script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("make the fake ocx executable");
    script
}

/// How many times [`fake_ocx_flaky_push`] was invoked in `dir`.
#[cfg(unix)]
pub fn push_attempts(dir: &std::path::Path) -> u32 {
    std::fs::read_to_string(dir.join("push-attempts"))
        .map(|body| body.trim().parse().unwrap_or(0))
        .unwrap_or(0)
}

/// Install the rustls crypto provider the mirror's reqwest clients need.
///
/// Tests run without `main()`, and reqwest builds its TLS stack lazily on
/// first use — panicking with "no provider set" if none is registered, even
/// for `http://` URLs. `install_default` errs once a provider is set, so every
/// call after the first is a no-op.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}
