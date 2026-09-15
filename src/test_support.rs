// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Crate-wide test scaffolding.
//!
//! Lives at the crate root rather than inside whichever module happened to
//! need it first: the process environment these guards serialise is global, so
//! a lock owned by one command module would have to be reached upward by every
//! other test that touches the same variables.

/// Serialises every test in this crate that reads or writes the process-global
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
pub(crate) static OCX_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Blocking accessor for [`OCX_ENV_LOCK`] — sync `#[test]` contexts only
/// (`blocking_lock` panics inside a runtime; async tests lock the static
/// directly with `.lock().await`).
pub(crate) fn ocx_env_lock() -> tokio::sync::MutexGuard<'static, ()> {
    OCX_ENV_LOCK.blocking_lock()
}

/// Puts the named variables back the way they were when dropped.
///
/// The env tests run in one process under [`OCX_ENV_LOCK`], and an assertion
/// that fails between a `set_var("GITLAB_CI", "true")` and its cleanup would
/// otherwise hand the next lock holder a GitLab job it never asked for.
/// Declare the guard *after* the lock guard so it drops before it.
pub(crate) struct EnvRestore(Vec<(&'static str, Option<String>)>);

impl EnvRestore {
    /// Snapshot every name in `vars`, then apply it (`None` removes).
    pub(crate) fn set(vars: &[(&'static str, Option<&str>)]) -> Self {
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
