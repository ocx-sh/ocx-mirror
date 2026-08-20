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

/// What the destination said about a path, from a HEAD alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// Nothing there — 404 or 410.
    Absent,
    /// The store holds the path.
    ///
    /// `sha256` is the digest the store reports for it (bare lowercase hex),
    /// when it reports one at all. Artifactory, Nexus and GitLab answer a HEAD
    /// with `X-Checksum-Sha256`; a plain WebDAV or S3-alike does not, which is
    /// why this is an `Option` and never an assumption. `None` means "present,
    /// contents unverifiable from here" — a caller that needs certainty must
    /// fetch the bytes.
    Present { sha256: Option<String> },
}

/// The sha256 a store reports for an object, if it reports one.
///
/// Normalised to lowercase because the comparison is against a manifest digest
/// that is itself lowercase hex, and a store answering in upper case would
/// otherwise read as a mismatch and re-transfer the whole archive forever.
/// Anything that is not 64 hex characters is discarded rather than trusted —
/// a truncated or prefixed value must degrade to "unverifiable", never to a
/// false match.
fn reported_sha256(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get("x-checksum-sha256")?.to_str().ok()?.trim();
    let normalised = raw.to_ascii_lowercase();
    (normalised.len() == 64 && normalised.chars().all(|c| c.is_ascii_hexdigit())).then_some(normalised)
}

/// Whether [`Uploader::put_file`] asks the destination before it writes.
///
/// Not a bare bool at the call site: the two answers are reached for two
/// different reasons and getting either wrong is silent, so the reason travels
/// with the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Precheck {
    /// HEAD first and leave an existing object alone.
    ///
    /// Correct only where the path pins the bytes — the content-addressed
    /// `dist/<sha256>.json` snapshot. "Already there" then means "already
    /// correct".
    HeadFirst,
    /// PUT unconditionally.
    ///
    /// Two callers need this and neither is a mistake. `dist.json` is rolling:
    /// the path outlives its contents, so "already there" says nothing about
    /// *which* version is there, and skipping it freezes the store's manifest
    /// at whatever the first run wrote while fresh archives land beside it.
    /// An archive reaches here having *already* been probed by the caller —
    /// see [`Uploader::probe`] — so a second HEAD would only re-ask a question
    /// just answered.
    Unconditional,
}

/// The four checksums announced on every PUT.
///
/// Artifactory records a "Client Checksum" per algorithm and shows *"Client did
/// not publish a checksum value"* for each header that was absent, so sending
/// only one still reads as unannounced on the other rows. It consumes MD5,
/// SHA-1 and SHA-256; SHA-512 is sent for stores that take it and is ignored
/// where it is not.
///
/// Computed from the body this uploader already read rather than passed in by
/// the caller: the manifest's declared sha256 covers only the archives, and
/// every file needs all four. There is no I/O to save either way — the read has
/// already happened by the time this runs.
struct Checksums {
    md5: String,
    sha1: String,
    sha256: String,
    sha512: String,
}

impl Checksums {
    /// Hash `body` four ways.
    ///
    // ponytail: four sequential passes over an in-memory buffer. The algorithms
    // share no compression work, so "one pass" could only interleave the
    // updates — same CPU, better cache locality. Worth doing only if archives
    // reach GB scale; at ocx dist sizes this is sub-second and the network PUT
    // that follows dominates.
    fn of(body: &[u8]) -> Checksums {
        use md5::Md5;
        use sha1::Sha1;
        use sha2::{Digest, Sha256, Sha512};

        Checksums {
            md5: hex::encode(Md5::digest(body)),
            sha1: hex::encode(Sha1::digest(body)),
            sha256: hex::encode(Sha256::digest(body)),
            sha512: hex::encode(Sha512::digest(body)),
        }
    }
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

    /// PUT the file, optionally asking the destination first — see [`Precheck`].
    ///
    /// Where a HEAD does happen it is asked of the destination rather than of a
    /// cached record of what a previous run wrote: the store is the authority,
    /// so a file deleted from it is re-uploaded by the next run instead of
    /// being skipped forever by stale local state.
    ///
    /// Every PUT announces all four checksums; see [`Checksums`] for why they
    /// are computed here rather than passed in.
    ///
    /// # Errors
    ///
    /// Any non-retryable response, or the last failure after the retry
    /// schedule is exhausted.
    pub async fn put_file(&self, relative: &str, file: &Path, precheck: Precheck) -> Result<UploadOutcome> {
        // The same composition the manifest row was stamped with, so the PUT
        // target and the URL consumers resolve are the same object by
        // construction rather than by two functions agreeing.
        let target = super::mirrored_url(&self.base, relative).map_err(|error| anyhow::anyhow!(error))?;

        if precheck == Precheck::HeadFirst && matches!(self.probe_url(&target).await?, Probe::Present { .. }) {
            return Ok(UploadOutcome::Skipped);
        }

        let body = tokio::fs::read(file)
            .await
            .with_context(|| format!("failed to read {} for upload", file.display()))?;

        // Off the reactor: four synchronous passes over a body that can be tens
        // of megabytes would otherwise park a worker thread, and
        // `concurrency.max_uploads` puts several of these in flight at once.
        // The body comes back as `Bytes` so the per-attempt `.body(body.clone())`
        // below is an `Arc` refcount bump, not a fresh copy of the whole archive
        // on every retry — which keeps the upload term of the memory ceiling at
        // one body per in-flight PUT, as `DistConcurrency` documents.
        let (body, sums) = tokio::task::spawn_blocking(move || {
            let sums = Checksums::of(&body);
            (bytes::Bytes::from(body), sums)
        })
        .await
        .with_context(|| format!("checksum task panicked for {}", file.display()))?;

        self.with_retries(&target, || {
            let request = self
                .client
                .put(target.clone())
                .headers(self.headers.clone())
                .header("X-Checksum-Md5", &sums.md5)
                .header("X-Checksum-Sha1", &sums.sha1)
                .header("X-Checksum-Sha256", &sums.sha256)
                .header("X-Checksum-Sha512", &sums.sha512);
            self.authenticate(request).body(body.clone())
        })
        .await?;

        Ok(UploadOutcome::Uploaded)
    }

    /// Ask the destination about one path, without transferring its body.
    ///
    /// Public because the caller probes **before deciding to download**, not
    /// only before deciding to upload: an archive the store already holds
    /// should cost neither transfer, and on a cold runner with a warm
    /// destination that is the difference between pulling the whole mirror and
    /// pulling nothing.
    ///
    /// # Errors
    ///
    /// Any non-success status other than `404`/`410`, or a transport failure
    /// that outlives the retry schedule.
    pub async fn probe(&self, relative: &str) -> Result<Probe> {
        let target = super::mirrored_url(&self.base, relative).map_err(|error| anyhow::anyhow!(error))?;
        self.probe_url(&target).await
    }

    /// [`Self::probe`] against an already-composed URL.
    ///
    /// A `404` or `410` is the answer "no", not a failure — every other
    /// non-success status is propagated, so a `401` on the probe fails the run
    /// rather than being read as "absent" and answered with an upload that
    /// fails again.
    async fn probe_url(&self, target: &Url) -> Result<Probe> {
        let response = self
            .with_retries(target, || {
                self.authenticate(self.client.head(target.clone()).headers(self.headers.clone()))
            })
            .await;

        match response {
            Ok(response) => Ok(Probe::Present {
                sha256: reported_sha256(response.headers()),
            }),
            Err(error) => match error.downcast_ref::<StatusError>() {
                // A store that has never seen the path answers 404; some
                // answer 410 for a tombstone. Both mean "upload it".
                Some(StatusError { status, .. }) if *status == StatusCode::NOT_FOUND || *status == StatusCode::GONE => {
                    Ok(Probe::Absent)
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
