// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Native HTTP PUT of the emitted tree.
//!
//! One implementation, no trait: Artifactory generic repositories, Nexus raw
//! repositories, GitLab generic packages and WebDAV stores are the *same* PUT,
//! and Azure Blob differs by one header — which is what `upload.headers`
//! exists for. A store that genuinely cannot be reached this way (S3 and GCS
//! need request signing) is served by the emitted tree and the operator's own
//! `aws s3 sync`, so it needs no code here either.
//!
//! Shelling out to `curl` was the alternative and is worse on three counts:
//! `curl` is absent from distroless images, credentials in `argv` are readable
//! from `/proc` for the process lifetime, and a template script per artifact
//! buys a process spawn and quoting bugs in exchange for no control over
//! retries.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, RETRY_AFTER};
use reqwest::{Response, StatusCode};
use url::Url;

use crate::spec::{Identity, Upload};

/// Ceiling applied to a server-supplied `Retry-After`.
///
/// The header is honoured because ignoring it makes throttling worse, but a
/// store answering `Retry-After: 3600` under load would otherwise turn a
/// five-step backoff into a five-hour hang in CI. Clamping keeps the
/// throttling contract and bounds the worst case.
const RETRY_AFTER_CEILING: Duration = Duration::from_secs(300);

/// What happened to one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadOutcome {
    /// Already present at the destination — the HEAD answered 2xx.
    Skipped,
    /// Transferred by this run.
    Uploaded,
}

/// Credentials, already resolved from the environment.
///
/// Resolution happens once at construction so a missing variable fails before
/// the first byte moves rather than half way through a tree.
#[derive(Clone)]
enum ResolvedIdentity {
    Bearer { token: String },
    Basic { username: String, password: String },
}

/// Redacted on purpose: this type is reachable from the uploader's own
/// `Debug`, and a token in a `tracing` line outlives the run in every CI log.
impl fmt::Debug for ResolvedIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bearer { .. } => f.write_str("Bearer(<redacted>)"),
            Self::Basic { .. } => f.write_str("Basic(<redacted>)"),
        }
    }
}

/// A configured destination store.
#[derive(Debug)]
pub struct Uploader {
    client: reqwest::Client,
    base: Url,
    identity: Option<ResolvedIdentity>,
    headers: HeaderMap,
    retry_delays: Vec<Duration>,
}

impl Uploader {
    /// Resolve credentials and header names up front.
    ///
    /// # Errors
    ///
    /// A named environment variable that is unset or empty, or a header name
    /// or value the HTTP grammar rejects. Both are operator configuration
    /// errors, and both are cheaper to report before the run than during it.
    pub fn new(client: reqwest::Client, base: &Url, upload: &Upload) -> Result<Uploader> {
        let identity = match &upload.identity {
            None => None,
            Some(Identity::Bearer { token_env }) => Some(ResolvedIdentity::Bearer {
                token: read_env(token_env)?,
            }),
            Some(Identity::Basic {
                username_env,
                password_env,
            }) => Some(ResolvedIdentity::Basic {
                username: read_env(username_env)?,
                password: read_env(password_env)?,
            }),
        };

        Ok(Uploader {
            client,
            base: base.clone(),
            identity,
            headers: build_headers(&upload.headers)?,
            retry_delays: upload.retry_delays.iter().copied().map(Duration::from_secs).collect(),
        })
    }

    /// HEAD, then PUT when absent.
    ///
    /// HEAD-before-PUT rather than a cached record of what a previous run
    /// wrote: the destination is the authority, so a file deleted from the
    /// store is re-uploaded by the next run instead of being skipped forever
    /// by stale local state.
    ///
    /// `sha256` is sent as `X-Checksum-Sha256` when present — Artifactory
    /// verifies it server-side and links an already-stored blob instead of
    /// re-reading the body.
    ///
    /// # Errors
    ///
    /// Any non-retryable response, or the last failure after the retry
    /// schedule is exhausted.
    pub async fn put_file(&self, relative: &str, file: &Path, sha256: Option<&str>) -> Result<UploadOutcome> {
        // The same composition the manifest row was stamped with, so the PUT
        // target and the URL consumers resolve are the same object by
        // construction rather than by two functions agreeing.
        let target = super::mirrored_url(&self.base, relative).map_err(|error| anyhow::anyhow!(error))?;

        if self.exists(&target).await? {
            return Ok(UploadOutcome::Skipped);
        }

        let body = tokio::fs::read(file)
            .await
            .with_context(|| format!("failed to read {} for upload", file.display()))?;

        self.with_retries(&target, || {
            let mut request = self.client.put(target.clone()).headers(self.headers.clone());
            if let Some(sha256) = sha256 {
                request = request.header("X-Checksum-Sha256", sha256);
            }
            self.authenticate(request).body(body.clone())
        })
        .await?;

        Ok(UploadOutcome::Uploaded)
    }

    /// Whether the destination already holds this path.
    ///
    /// A `404` or `410` is the answer "no", not a failure — every other
    /// non-success status is propagated, so a `401` on the probe fails the run
    /// rather than being read as "absent" and answered with an upload that
    /// fails again.
    ///
    /// # Errors
    ///
    /// Any non-success status other than `404`/`410`, or a transport failure
    /// that outlives the retry schedule.
    async fn exists(&self, target: &Url) -> Result<bool> {
        let response = self
            .with_retries(target, || {
                self.authenticate(self.client.head(target.clone()).headers(self.headers.clone()))
            })
            .await;

        match response {
            Ok(_) => Ok(true),
            Err(error) => match error.downcast_ref::<StatusError>() {
                // A store that has never seen the path answers 404; some
                // answer 410 for a tombstone. Both mean "upload it".
                Some(StatusError { status, .. }) if *status == StatusCode::NOT_FOUND || *status == StatusCode::GONE => {
                    Ok(false)
                }
                _ => Err(error),
            },
        }
    }

    /// Attach credentials, if any were configured.
    fn authenticate(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.identity {
            None => request,
            Some(ResolvedIdentity::Bearer { token }) => request.bearer_auth(token),
            Some(ResolvedIdentity::Basic { username, password }) => request.basic_auth(username, Some(password)),
        }
    }

    /// Send `build()`'s request, retrying only what a retry can fix.
    ///
    /// Retryable: transport errors (connect resets, timeouts), 5xx, and 429.
    /// **Never 4xx** — a 401 or 403 is a credential problem that a retry
    /// cannot solve, and hammering one burns the backoff window and trips
    /// account lockout policies on exactly the stores this targets.
    async fn with_retries<F>(&self, target: &Url, build: F) -> Result<Response>
    where
        F: Fn() -> reqwest::RequestBuilder,
    {
        // One attempt, plus one per configured delay.
        let mut attempt = 0usize;
        loop {
            let (error, retryable, retry_after) = match build().send().await {
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() {
                        return Ok(response);
                    }
                    let retry_after = parse_retry_after(response.headers());
                    (
                        anyhow::Error::from(StatusError {
                            status,
                            url: redacted(target),
                        }),
                        retryable_status(status),
                        retry_after,
                    )
                }
                // Only the transport classes: a malformed request or an
                // undecodable body is deterministic, and repeating it just
                // spends the schedule.
                Err(error) => {
                    let retryable = error.is_timeout() || error.is_connect();
                    (anyhow::Error::from(error).context(redacted(target)), retryable, None)
                }
            };

            if !retryable {
                return Err(error);
            }
            let Some(delay) = self.retry_delays.get(attempt).copied() else {
                return Err(error);
            };

            let delay = effective_delay(delay, retry_after);
            if let Some(server) = retry_after
                && server > delay
            {
                tracing::warn!(
                    "{}: Retry-After {}s clamped to {}s",
                    redacted(target),
                    server.as_secs(),
                    delay.as_secs()
                );
            }

            tracing::warn!("{}: {error}; retrying in {}s", redacted(target), delay.as_secs());
            tokio::time::sleep(delay).await;
            attempt += 1;
        }
    }
}

/// A non-success HTTP status, kept as a type so [`Uploader::exists`] can ask
/// what the status was instead of matching on a message.
#[derive(Debug)]
struct StatusError {
    status: StatusCode,
    url: String,
}

impl fmt::Display for StatusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} returned {}", self.url, self.status)
    }
}

impl std::error::Error for StatusError {}

/// Whether a status is worth another attempt: 5xx, or 429.
fn retryable_status(status: StatusCode) -> bool {
    status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS
}

/// `Retry-After` in delta-seconds form. The HTTP-date form is ignored rather
/// than parsed: it needs a trusted clock on both ends, and the scheduled
/// backoff is already a correct answer.
fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    let raw = headers.get(RETRY_AFTER)?.to_str().ok()?;
    raw.trim().parse::<u64>().ok().map(Duration::from_secs)
}

/// The delay to actually wait: the scheduled one, unless the server asked for
/// longer, and never more than [`RETRY_AFTER_CEILING`].
///
/// Split out of the retry loop because it is the whole of the clamp rule and
/// the only part of it reachable without a server that throttles.
fn effective_delay(scheduled: Duration, retry_after: Option<Duration>) -> Duration {
    match retry_after {
        Some(server) if server > scheduled => server.min(RETRY_AFTER_CEILING),
        _ => scheduled,
    }
}

/// A URL with any userinfo removed, for messages and logs.
///
/// Spec validation already refuses userinfo in `publish.base_url`, so this is
/// the second guard rather than the first — but every string here reaches a CI
/// log that outlives the run.
fn redacted(url: &Url) -> String {
    let mut url = url.clone();
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.to_string()
}

/// Read a required environment variable.
///
/// # Errors
///
/// A message naming the *variable*, never its value, when it is unset or
/// empty. An empty value is treated as unset: an unset secret in CI usually
/// arrives as the empty string, and sending an empty Authorization header
/// produces a 401 that names nothing useful.
fn read_env(name: &str) -> Result<String> {
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => Ok(value),
        _ => bail!("upload.identity: environment variable {name} is unset or empty"),
    }
}

/// Parse the configured extra headers once.
///
/// # Errors
///
/// A name or value the HTTP grammar rejects, named in the message — an
/// operator configuration error, and cheaper to report before the run than
/// during it.
fn build_headers(configured: &BTreeMap<String, String>) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    for (name, value) in configured {
        let name = HeaderName::try_from(name.as_str()).with_context(|| format!("upload.headers: bad name {name:?}"))?;
        let value =
            HeaderValue::try_from(value.as_str()).with_context(|| format!("upload.headers: bad value for {name}"))?;
        headers.insert(name, value);
    }
    Ok(headers)
}

#[cfg(test)]
#[path = "upload/tests.rs"]
mod tests;
