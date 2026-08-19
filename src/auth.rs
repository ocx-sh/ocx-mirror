// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Credentials for the mirror's own HTTP legs, resolved from the **URL being
//! requested** — never from anything a spec author wrote.
//!
//! # Why host-keyed, and why nothing in the spec
//!
//! A `mirror.yml` is community-contributed. A spec that could name the
//! environment variable to read (`token_env: GITHUB_TOKEN`) could also name
//! the host to send it to, which is credential exfiltration wearing ordinary
//! config: npm and pnpm removed environment expansion from project-level
//! `.npmrc` for exactly this. Deriving the variable name from the *host* binds
//! a credential to the destination it is sent to, so a hostile spec can only
//! ever receive credentials an operator explicitly configured for the hostile
//! spec's own host.
//!
//! That is also why an operator-authored file may still name variables:
//! `dist.yml`'s `upload.identity` (see `spec::Identity`) is one store, written
//! by the operator who owns the runner.
//!
//! # The ladder
//!
//! 1. `OCX_AUTH_<slug>_{TYPE,USER,TOKEN}` — the same variables, the same slug
//!    rule and the same precedence `ocx` itself uses for registries, so a
//!    machine already able to pull from a host can already fetch from it.
//! 2. `netrc` — `$NETRC`, else `~/.netrc`, on an exact `machine <host>` line.
//!    This is the file `uv` reads for the lock-derivation leg, so our own
//!    downloads and `uv` agree on the hosts an operator named, rather than by
//!    two mechanisms happening to be configured alike. They diverge on one
//!    entry deliberately: a `default` line answers for **every** host, and
//!    every URL this ladder sees came from a lock or an index — foreign data.
//!    `uv` honours `default`; we refuse it. The cost is a 401 an operator can
//!    read and fix with a `machine` line; the alternative is their credential
//!    leaving for whatever host a hostile index named.
//! 3. Anonymous — the common case, and the only one a public index needs.
//!
//! The OCI legs are **not** served from here: `ocx_lib::auth` owns that ladder
//! (env → Docker credential store → anonymous), and netrc is deliberately not
//! part of it. One Artifactory host commonly serves a PyPI repo and an OCI
//! repo under different tokens; a netrc line written for one must not silently
//! become the credential for the other.

use std::path::PathBuf;

use ocx_lib::auth::AuthType;
use ocx_lib::utility::string_ext::StringExt as _;
use url::Url;

use crate::error::MirrorError;

/// A resolved credential for one host.
///
/// Carries the secret itself, so it is never logged, never formatted into an
/// error, and never reaches a subprocess `argv` — `/proc/<pid>/cmdline` is
/// world-readable for the lifetime of the process.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum Credential {
    Basic { user: String, secret: String },
    Bearer(String),
}

/// Redacted on purpose: this type is reachable from the `Debug` of anything
/// holding it, and a token in a `tracing` line outlives the run in every CI
/// log. Same rationale as `pipeline::dist_sync::upload::ResolvedIdentity`.
impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Basic { .. } => f.write_str("Basic(<redacted>)"),
            Self::Bearer(_) => f.write_str("Bearer(<redacted>)"),
        }
    }
}

impl Credential {
    /// Attaches this credential to one request.
    pub(crate) fn apply(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self {
            Self::Basic { user, secret } => request.basic_auth(user, Some(secret)),
            Self::Bearer(token) => request.bearer_auth(token),
        }
    }

    /// The `(username, password)` pair to hand a resolver that speaks only
    /// HTTP Basic — `uv`'s `UV_INDEX_<NAME>_USERNAME`/`_PASSWORD`, for one.
    ///
    /// A bearer token is expressed the way every Python index that issues one
    /// documents it: the token as the password, under a sentinel user name.
    /// PyPI uses `__token__`; Artifactory, Nexus and Azure Artifacts accept
    /// any user name alongside a token password.
    pub(crate) fn as_basic_pair(&self) -> (String, String) {
        match self {
            Self::Basic { user, secret } => (user.clone(), secret.clone()),
            Self::Bearer(token) => ("__token__".to_string(), token.clone()),
        }
    }
}

/// Resolves the credential for `url`, or `None` for anonymous access.
///
/// # Errors
///
/// [`MirrorError::SpecUsageError`] (exit 64) when `OCX_AUTH_<slug>_TYPE` names
/// a scheme whose variables are missing, or is not one of
/// `anonymous`/`basic`/`token`. An operator who set the variables meant them to
/// be used, so a half-configured identity fails loudly here rather than
/// silently degrading to anonymous and surfacing as a 401 much later.
///
/// A malformed `netrc` never fails the run: it is a file the mirror does not
/// own, shared with every other tool on the machine.
pub(crate) fn resolve(url: &Url) -> Result<Option<Credential>, MirrorError> {
    let Some(host) = url.host_str() else {
        return Ok(None);
    };

    if let Some(credential) = from_env(host)? {
        return Ok(Some(credential));
    }
    Ok(from_netrc(host))
}

/// Rung 1: `OCX_AUTH_<slug>_*`, with `<slug>` the host through the same
/// `to_slug` transform `ocx` applies to a registry name (every non-alphanumeric
/// byte becomes `_`), so `nexus.corp.example` reads
/// `OCX_AUTH_nexus_corp_example_TOKEN`.
fn from_env(host: &str) -> Result<Option<Credential>, MirrorError> {
    let slug = host.to_slug();
    let type_env = format!("OCX_AUTH_{slug}_TYPE");
    let user_env = format!("OCX_AUTH_{slug}_USER");
    let token_env = format!("OCX_AUTH_{slug}_TOKEN");

    let user = ocx_lib::env::var(&user_env);
    let token = ocx_lib::env::var(&token_env);

    let Some(declared) = ocx_lib::env::var(&type_env) else {
        // No declared type: the pair means Basic, a lone token means Bearer.
        // Same inference as `ocx_lib::auth::get_env_auth`.
        return Ok(match (user, token) {
            (Some(user), Some(secret)) => Some(Credential::Basic { user, secret }),
            (None, Some(token)) => Some(Credential::Bearer(token)),
            _ => None,
        });
    };

    let auth_type =
        AuthType::try_from(declared).map_err(|error| MirrorError::SpecUsageError(format!("{type_env}: {error}")))?;
    let missing = |name: &str| MirrorError::SpecUsageError(format!("{type_env} is '{auth_type}' but {name} is unset"));

    match auth_type {
        AuthType::Anonymous => Ok(None),
        AuthType::Basic => Ok(Some(Credential::Basic {
            user: user.ok_or_else(|| missing(&user_env))?,
            secret: token.ok_or_else(|| missing(&token_env))?,
        })),
        AuthType::Token => Ok(Some(Credential::Bearer(token.ok_or_else(|| missing(&token_env))?))),
    }
}

/// Rung 2: the first `machine <host>` entry whose name matches exactly, and
/// nothing else.
///
/// Read on every call rather than cached: a run is short, the file is small,
/// and a cache keyed on nothing would have to be invalidated by a `$NETRC`
/// that changed mid-process.
fn from_netrc(host: &str) -> Option<Credential> {
    let path = netrc_path()?;
    let text = std::fs::read_to_string(&path).ok()?;
    let (user, secret) = lookup_netrc(&text, host)?;
    Some(Credential::Basic { user, secret })
}

/// `$NETRC` when set, else the conventional per-user file.
fn netrc_path() -> Option<PathBuf> {
    if let Some(explicit) = ocx_lib::env::var("NETRC").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(explicit));
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|value| !value.is_empty())?;
    let home = PathBuf::from(home);
    let dot = home.join(".netrc");
    if dot.exists() {
        return Some(dot);
    }
    // Windows convention; harmless elsewhere, where it simply will not exist.
    let underscore = home.join("_netrc");
    underscore.exists().then_some(underscore)
}

/// Minimal netrc reader: enough of the format for `machine`/`login`/`password`
/// lookup, and nothing else.
///
/// The format has no RFC — it is BSD ftp's, and parsers disagree about
/// quoting, comments and `macdef`. What every parser does agree on is the
/// token stream, which is all a credential lookup needs.
///
/// Two rules here are security-relevant rather than cosmetic:
///
/// - **Exact host match.** No suffix or wildcard matching, so a `machine
///   corp.example` line cannot answer for `evil-corp.example`.
/// - **`default` never answers.** A `default` entry matches every host, and
///   the host asked about here is one a lock or an index named — so honouring
///   it would hand an operator's credential to whatever host a hostile
///   upstream chose. It is parsed only so its own `login`/`password` do not
///   bleed into the `machine` block above it.
///
/// `macdef` bodies are skipped to the next blank line — an unterminated one
/// would otherwise swallow the entries after it, which in a credential lookup
/// reads as "no credentials" rather than as a parse error.
fn lookup_netrc(text: &str, host: &str) -> Option<(String, String)> {
    let mut in_match = false;
    let (mut login, mut password) = (None, None);
    let mut tokens = Vec::new();
    let mut in_macdef = false;

    for line in text.lines() {
        if in_macdef {
            if line.trim().is_empty() {
                in_macdef = false;
            }
            continue;
        }
        for word in line.split_whitespace() {
            if word == "macdef" {
                in_macdef = true;
                break;
            }
            tokens.push(word);
        }
    }

    let mut tokens = tokens.into_iter();
    while let Some(token) = tokens.next() {
        match token {
            "machine" => {
                let Some(name) = tokens.next() else { break };
                in_match = name == host;
            }
            // Kept as an arm rather than dropped: `default` closes the
            // `machine` block before it, so its own `login`/`password` are
            // never attributed to that host. It answers for nothing itself.
            "default" => in_match = false,
            "login" | "password" | "account" => {
                let Some(value) = tokens.next() else { break };
                let value = value.trim_matches('"').to_string();
                match (in_match, token) {
                    (true, "login") => login = Some(value),
                    (true, "password") => password = Some(value),
                    _ => {}
                }
            }
            // `macdef` is stripped above; anything else is a token this
            // lookup has no use for.
            _ => {}
        }
        if let (Some(user), Some(secret)) = (&login, &password) {
            return Some((user.clone(), secret.clone()));
        }
    }

    None
}

#[cfg(test)]
#[path = "auth/tests.rs"]
mod tests;
