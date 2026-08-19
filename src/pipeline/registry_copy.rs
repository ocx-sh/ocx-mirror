// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! The copy engine — by-digest transfer of manifests and blobs from a source
//! registry into the destination registry.
//!
//! A sibling of [`registry_sync`](super::registry_sync) rather than a child of
//! it: this is shared machinery below the command layer, and flat siblings
//! under `pipeline/` with per-module child directories is the crate's own
//! shape (`orchestrator.rs`, `push.rs` + `push/alias.rs`).
//!
//! **A mirror is a transport and filters nothing.** No tag is parsed, no
//! cascade is computed, no platform predicate is applied — every `tags{}` key
//! and every `manifests[]` descriptor travels by digest, whatever its text or
//! shape. Contracts C-021…C-026 and C-046 of `plan_registry_mirror_sync.md`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use futures::{StreamExt, TryStreamExt};
use ocx_lib::oci::index::RootTag;
use ocx_lib::oci::native::oci_client::client::BlobMountResponse;
use ocx_lib::oci::native::oci_client::errors::{OciDistributionError, OciErrorCode};
use ocx_lib::oci::{Digest, Identifier, Index, Manifest, Reference, native};
use tokio::sync::{Mutex, Semaphore};

use crate::error::MirrorError;
use crate::filter::pep440_sort_key;
use crate::spec::{RegistrySource, Target};

/// Bound on a **manifest** body this mirror republishes or recurses into.
///
/// Applied post-hoc in both walks: `Index::fetch_manifest_raw_bytes` has
/// already allocated by the time it returns, so what this bounds is what
/// travels on — parsed, recursed into, republished — never what was read.
/// `ocx_lib`'s own `fetch_manifest_raw_bytes_capped` is post-hoc for the same
/// reason and says so (its FU-3 note): the fork's `pull_manifest_raw` does a
/// bare `res.bytes()`, so bounding the allocation needs a capped read on the
/// transport itself and neither layer has one.
///
/// It reaches a referrers response even less far, whatever an older revision
/// of this line claimed: `Client::pull_referrers` does the same bare
/// `res.bytes()` and exposes no capped variant, so [`detect_referrers`] has
/// nothing to apply a bound *with* — not post-hoc, not at all. Said here
/// rather than left as a promise, because a comment claiming a cap that does
/// not exist is worse than no cap: it is what stops the next reader looking.
pub const MANIFEST_FETCH_CEILING: usize = 32 * 1024 * 1024;

/// Bound on a single blob's **declared** size, and — through it — on the body
/// this mirror will read for one (C-023).
///
/// A descriptor's `size` is foreign data, and it sizes the buffer [`upload`]
/// allocates before the first byte arrives. `i64::MAX` clears the sibling
/// non-negative guard and clears `Vec::with_capacity`'s own capacity-overflow
/// check too, so it reaches `handle_alloc_error` — which **aborts the process**
/// rather than unwinding into a failed package.
///
/// Bounding the declaration is only half of it, and on its own it bounds
/// nothing that matters: a declaration is a claim, and [`BoundedSink`] is
/// what holds the *body* to it. Without that second half a source could
/// declare 4 KiB and stream forever, and `max_blobs × largest_blob` would be a
/// ceiling only against a source that told the truth.
///
/// 4 GiB, and it is a policy ceiling rather than a memory budget.
/// [`MANIFEST_FETCH_CEILING`] bounds manifest bodies at 32 MiB, but a layer is
/// the opposite kind of object: the largest real one this module's own ceiling
/// note cites is 221 MB, and 4 GiB clears that by ~19×, so no plausible layer —
/// up to and including a CUDA-sized wheel — is refused. What it buys is that
/// `max_blobs × largest_blob` becomes a number an operator can compute (16 GiB
/// at the default `max_blobs: 4`) instead of `max_blobs × i64::MAX`. The knob
/// for the actual budget is `concurrency.max_blobs`, never this.
pub const BLOB_SIZE_CEILING: u64 = 4 * 1024 * 1024 * 1024;

/// Blobs of one manifest whose destination probe may be in flight at once.
///
/// The run-scoped permit pool bounds the pull → verify → push window, but the
/// probe deliberately sits outside it so an all-present re-run is not
/// serialised to `max_blobs`. `layers[]` is foreign data with no bound of its
/// own — one manifest at [`MANIFEST_FETCH_CEILING`] holds on the order of 2×10⁵
/// descriptors — so without a ceiling here that is 2×10⁵ simultaneous `HEAD`s
/// against the destination.
///
/// 64: the same bound and the same reasoning as `pipeline plan`'s
/// `DRIFT_SCAN_CONCURRENCY`, itself copied from `LocalIndex::refresh_tags` in
/// `ocx_lib` — enough that a several-hundred-layer env package resolves in a
/// handful of rounds, low enough that the burst does not read as an attack to a
/// registry that answers bursts with `429`. It must stay **at or above** any
/// sane `concurrency.max_blobs` (default 4): below it, this would silently cap
/// the operator's own knob.
const BLOB_FANOUT_CEILING: usize = 64;

/// Levels of nested image index either manifest walk descends before refusing
/// (C-022).
///
/// An entry of `manifests[]` may itself be an image index — the OCI image spec
/// admits it, and a descriptor says nothing about how deep the chain below it
/// goes — so the walk's depth is chosen by the source, not by this mirror. A
/// chain of indexes each naming one more is a few hundred bytes to author and
/// ends in stack exhaustion. Digest cycles are not the risk: a child's digest
/// covers its parent's bytes, so a cycle would need a sha256 preimage. Depth
/// alone is. [`detect_referrers`] was bounded to one level for this same
/// reason; this walk was not.
///
/// 8, counting the dispatch object the tag names as level 0. Every
/// multi-platform artifact published anywhere is **depth 1** — one image index
/// whose `manifests[]` are image manifests. That is what `ocx` itself
/// publishes, and what `docker buildx` emits including its attestation
/// entries. The deepest shape anyone actually ships is an index grouping other
/// indexes, at depth 2. 8 clears that by 4×, so no plausible artifact is
/// refused, while a hostile chain is cut two orders of magnitude before the
/// recursion is a problem.
const MANIFEST_DEPTH_CEILING: usize = 8;

/// Referrer digests named in a detection failure before the message is
/// truncated to a total count (C-024).
pub const REFERRER_REPORT_LIMIT: usize = 10;

/// The reserved tag carrying a package's rendered description (C-025).
///
/// `ocx` filters reserved tags at render, so it never appears in `root.tags{}`
/// and no `tags{}` walk reaches it — it is copied explicitly or not at all.
///
/// `pub` because [`copy_description`] takes its two references as **location**
/// only and the orchestrator has to build them: naming them with this tag is
/// what keeps a log line or an error message from claiming the description was
/// copied from some unrelated tag.
pub const DESCRIPTION_TAG: &str = "__ocx.desc";

// ── Client construction (C-046) ─────────────────────────────────────────────
//
// The three constants below are copied from `ClientBuilder::new`
// (`external/ocx/crates/ocx_lib/src/oci/client/builder.rs:98-114`), which is
// their source of truth — they are `pub(crate)` there and so cannot be
// imported. `ClientConfig::default()` sets none of the timeouts, so a
// hand-built config inherits `None` for both: on a multi-hour transfer that
// means one hung socket stalls the whole mirror forever.

/// Body size of one chunked-push `PATCH`, held below GHCR's 4 MiB request cap.
const PUSH_CHUNK_SIZE: usize = 3 * 1024 * 1024;

/// Per-frame idle bound on a response body, and a hard deadline on everything
/// before the first body frame.
const REGISTRY_READ_TIMEOUT: Duration = Duration::from_secs(120);

/// Bound on the connect phase, so a black-holing host cannot park a socket in
/// `SYN_SENT` for minutes.
const REGISTRY_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// First retry delay after a rate limit; each further attempt doubles it.
const RETRY_BACKOFF_BASE: Duration = Duration::from_secs(1);

/// Ceiling on the doubling, mirroring `ocx_cli::push`'s ladder.
const RETRY_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// The shared `ClientConfig` both constructors start from.
///
/// Single construction site on purpose: it is what keeps the three fields
/// above from drifting apart between the source and destination clients, and
/// it is the seam their unit test asserts on — a `native::Client` exposes no
/// way to read its own config back.
fn client_config() -> native::ClientConfig {
    let mut config = native::ClientConfig {
        push_chunk_size: PUSH_CHUNK_SIZE,
        read_timeout: Some(REGISTRY_READ_TIMEOUT),
        connect_timeout: Some(REGISTRY_CONNECT_TIMEOUT),
        ..Default::default()
    };
    // Same source `ClientBuilder::plain_http_registries` reads, so the
    // acceptance harness's plain-HTTP loopback registries are reachable
    // through this client too. Left at the `Https` default when unset.
    let insecure = ocx_lib::env::insecure_registries();
    if !insecure.is_empty() {
        config.protocol = native::ClientProtocol::HttpsExcept(insecure);
    }
    config
}

/// Build the **SSRF-guarded** client used for every source-side registry call
/// (C-046).
///
/// The guard is not optional and not a flag: an upstream root's `repository`
/// pointer is foreign data, and a `GuardedResolver` on
/// `ClientConfig::dns_resolver` installs exactly what
/// `ClientBuilder::ssrf_guard` installs — resolve → validate → pin, which is
/// what closes the DNS-rebinding window a pre-flight check alone leaves open.
///
/// Built here rather than through `ocx_lib::oci::Client` because that wrapper
/// exposes no raw push and its `native_transport` is `pub(crate)`.
pub async fn build_source_client(source: &RegistrySource) -> native::Client {
    let mut config = client_config();
    config.dns_resolver = Some(Arc::new(ocx_lib::oci::ssrf::GuardedResolver::new(Arc::new(
        source.trusted_hosts.clone(),
    ))));
    authenticated(config, &source.registry).await
}

/// Build the destination client — **deliberately unguarded** (C-046).
///
/// SSRF exists to stop *foreign data* from making this tool dial an internal
/// host. The destination registry is operator-authored config, inside the
/// threat model's trusted boundary, and the ADR mandates the guard source-side
/// only. Guarding it would refuse the deployment the feature exists for: the
/// SSRF floor rejects loopback, RFC1918, link-local and CGNAT unless a
/// `trusted` entry exempts them, so a guarded destination refuses both the
/// motivating RFC1918 Artifactory and the acceptance harness's own
/// `localhost:5002`. There is also nowhere for a `trusted` list to come from —
/// `trusted_hosts` is per-source, and `Target` carries only `registry` and
/// `repository`.
///
// ponytail: no `dns_resolver` here on purpose. Do not "restore parity" with
// `build_source_client` — see the paragraph above; it refuses the deployment.
///
/// Bypassing `ocx_lib::oci::Client` also bypasses its `[mirrors]` base-URL
/// rewriting, which is **correct for the destination**: a corporate mirror
/// must write to the literal configured registry, never one a client-side
/// mirror map redirected.
pub async fn build_destination_client(target: &Target) -> native::Client {
    authenticated(client_config(), &target.registry).await
}

/// Build the client and seed its credential for `registry`.
///
/// Seeding is all that is needed: the fork's `get_auth_token` reads the stored
/// `RegistryAuth` and performs the token challenge itself, per reference and
/// per operation, so no explicit `auth()` call has to be threaded through the
/// ladder. `Auth::get_or_fallback` *is* the `OCX_AUTH_<slug>_*` →
/// docker-credential-store → anonymous chain, which is how ADR decision 10
/// (zero credentials in the spec) is satisfied.
async fn authenticated(config: native::ClientConfig, registry: &str) -> native::Client {
    let client = native::Client::new(config);
    let auth = ocx_lib::auth::Auth::new().get_or_fallback(registry).await;
    client.store_auth_if_needed(registry, &auth).await;
    client
}

/// Everything one package's copy needs, threaded through the ladder below.
///
/// WP-09 owns these fields. Two of them are load-bearing beyond their type:
/// `blob_semaphore` must be **run-scoped** (one `Arc`, never one constructed
/// per call — per-call permits multiply the memory ceiling by the in-flight
/// tag count), and `mounted_from` is what makes a cross-repository
/// `mount_blob` possible at all.
pub struct CopyContext {
    /// The read seam (C-022): `Index::fetch_manifest_raw_bytes` returns
    /// verbatim bytes, digest and parsed manifest in one call.
    pub source_index: Index,
    /// Source-side fork client — blob pulls and referrer queries (C-046).
    pub source_client: native::Client,
    /// Destination-side fork client — every probe and push (C-046).
    pub destination_client: native::Client,
    /// Run-scoped blob permit pool sized by `concurrency.max_blobs`.
    pub blob_semaphore: Arc<Semaphore>,
    /// Reactive-retry budget from `concurrency.max_retries`.
    pub max_retries: u32,
    /// Digest → the destination repository already holding it, for cross-repo
    /// `mount_blob`. Populated as blobs land.
    pub mounted_from: Arc<Mutex<HashMap<Digest, String>>>,
    /// `(source repository, destination repository)` pairs already warned
    /// about. `mount_blob` authenticates Push-only and the token cache keys on
    /// `{registry, repository, operation}`, so cross-repo mounts lack the
    /// source pull scope on GHCR and Artifactory — the upload fallback is
    /// correct, but an unsuppressed warning fires once per blob, ~9,510 times
    /// on a full catalog.
    pub mount_warnings: Arc<Mutex<HashSet<(String, String)>>>,
}

/// One tag to copy, and the manifest digest it names (C-021).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagCopyPlan {
    /// The tag key, verbatim from the source root — never parsed.
    pub tag: String,
    /// The dispatch-object digest the source root recorded for it.
    pub content: Digest,
}

/// What happened to one blob at the destination (C-023).
///
/// A mirror-side report enum feeding the run counters — a different thing from
/// the wire-level `BlobMountResponse`, and it must not absorb it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobOutcome {
    /// Already present at the destination; nothing transferred.
    Skipped,
    /// Mounted from another repository on the same destination registry.
    Mounted,
    /// Pulled from the source, verified, and uploaded.
    Uploaded,
}

/// Per-package transfer counters, summed into the run report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CopyStats {
    pub manifests: usize,
    pub blobs_skipped: usize,
    pub blobs_mounted: usize,
    pub blobs_uploaded: usize,
    pub bytes_uploaded: u64,
}

impl CopyStats {
    /// Fold a child manifest's counters into this one.
    fn merge(&mut self, other: CopyStats) {
        self.manifests += other.manifests;
        self.blobs_skipped += other.blobs_skipped;
        self.blobs_mounted += other.blobs_mounted;
        self.blobs_uploaded += other.blobs_uploaded;
        self.bytes_uploaded += other.bytes_uploaded;
    }

    /// Record one blob outcome and the bytes it moved.
    fn record(&mut self, outcome: BlobOutcome, size: u64) {
        match outcome {
            BlobOutcome::Skipped => self.blobs_skipped += 1,
            BlobOutcome::Mounted => self.blobs_mounted += 1,
            BlobOutcome::Uploaded => {
                self.blobs_uploaded += 1;
                self.bytes_uploaded += size;
            }
        }
    }
}

/// A failure raised anywhere on the copy ladder, carrying **which of C-040's
/// two classes it belongs to** in its own shape.
///
/// The dividing line is a class, not a list: only a *read* whose answer is not
/// authoritative aborts the run; every *write* failure, whatever its status,
/// fails its package. A failed write leaves content at the destination with no
/// root naming it, which is the ordinary interrupted state and is repaired by
/// the next ordinary run. Every variant but [`Self::Abort`] is aggregating —
/// counted, reported, and subject to `on_error`.
#[derive(Debug)]
#[non_exhaustive]
pub enum CopyError {
    /// Pulled bytes did not hash to the digest the source claimed. Carries
    /// both digests — the expected one and what the bytes actually hash to.
    DigestMismatch { expected: Digest, actual: Digest },
    /// A manifest or blob could not be pulled from the source.
    SourceUnavailable(String),
    /// The source served a manifest this mirror cannot transport faithfully —
    /// an unparseable descriptor digest, a negative size, a declared size past
    /// [`BLOB_SIZE_CEILING`], a body past [`MANIFEST_FETCH_CEILING`], a blob
    /// body past the size its own descriptor declared, a chain of nested image
    /// indexes past [`MANIFEST_DEPTH_CEILING`]. Distinct from
    /// [`Self::SourceUnavailable`] because retrying cannot fix it: the bytes
    /// are what they are.
    MalformedManifest(String),
    /// The destination rejected a push.
    PushRejected(String),
    /// The manifest carries referrers, which v1 detects and refuses to copy.
    /// Bounded to [`REFERRER_REPORT_LIMIT`] digests plus the total.
    ReferrersPresent { shown: Vec<Digest>, total: usize },
    /// A manifest the source no longer serves — a tag's `content` digest, or
    /// a descriptor nested under one, garbage-collected upstream between the
    /// root fetch and the copy.
    ///
    /// Carries the digest alone and **not** the tag: the recursion reaches
    /// manifests no tag names directly, and the orchestrator prefixes every
    /// per-tag failure with its tag anyway, so a `tag` field here would be a
    /// second, sometimes-wrong copy of the same context.
    ContentMissing { content: Digest },
    /// A destination read whose answer was **not authoritative** — a 401, a
    /// 503, a timeout, a reset. Aborts the whole run under *both* `on_error`
    /// values and is never counted in the per-package aggregate: a run that
    /// does not know what the destination holds cannot correctly skip or copy
    /// anything.
    Abort(MirrorError),
}

impl CopyError {
    /// Whether this failure aborts the run rather than failing one package
    /// (C-040). The orchestrator branches on this, never on a variant list.
    pub fn is_whole_run_abort(&self) -> bool {
        matches!(self, Self::Abort(_))
    }
}

impl std::fmt::Display for CopyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DigestMismatch { expected, actual } => {
                write!(f, "digest mismatch: source claimed {expected}, bytes hash to {actual}")
            }
            Self::SourceUnavailable(message) => write!(f, "source unavailable: {message}"),
            Self::MalformedManifest(message) => write!(f, "malformed source manifest: {message}"),
            Self::PushRejected(message) => write!(f, "destination rejected the push: {message}"),
            Self::ReferrersPresent { shown, total } => {
                write!(
                    f,
                    "manifest carries {total} referrer(s), which v1 detects but does not copy: "
                )?;
                for (position, digest) in shown.iter().enumerate() {
                    if position > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{digest}")?;
                }
                if *total > shown.len() {
                    write!(f, ", and {} more", total - shown.len())?;
                }
                Ok(())
            }
            Self::ContentMissing { content } => {
                write!(f, "content {content} no longer resolves at the source")
            }
            Self::Abort(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for CopyError {
    /// [`Self::Abort`] is the one variant that *wraps* an error rather than
    /// describing one, and the derived default would drop that link — leaving a
    /// `{err:#}` render, a downcast, or an exit-code classifier unable to reach
    /// the [`MirrorError`] the variant exists to carry.
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Abort(error) => Some(error),
            _ => None,
        }
    }
}

/// Every key of the source root's `tags{}`, in copy order (C-021).
///
/// Every key in, every key out, unchanged. The mirror **parses no tag and
/// computes no cascade**: the source root already carries the cascade result,
/// and copying each entry by digest reproduces it exactly.
///
/// Reusing `AliasTag::parse`, `variant_names`, `resolve_cascade_tags` or
/// `decompose` here is a regression, not an optimisation — a mirror holding a
/// filtered subset would compute a *different* cascade than the source
/// published, and `Version::parse` accepts a bare date stamp as a major-only
/// version that outsorts every dotted release.
///
/// A **yanked** tag is copied like any other: the marker travels with the root
/// document, so refusing the tag here would make the mirror disagree with the
/// source about which digests exist. That is a *semantic* refusal and it still
/// stands. [`is_legal_oci_tag`] below is a different question — a tag the OCI
/// grammar cannot express cannot be published to the destination at all, so
/// dropping it is the only faithful option rather than a judgement about
/// meaning.
///
/// Ordering is `pep440_sort_key` and is **legibility only, never a gate** — it
/// makes the log read chronologically and an interrupt deterministic. Its
/// `None` arm sorts first, so an unparseable tag copies first and can never be
/// mistaken for the newest.
pub fn tag_copy_plan(tags: &BTreeMap<String, RootTag>) -> Vec<TagCopyPlan> {
    let mut plan: Vec<TagCopyPlan> = tags
        .iter()
        .filter(|(tag, _)| {
            // A tag key is foreign data and reaches the destination URL as
            // `.../manifests/{tag}`, interpolated raw. `Reference::with_tag` is
            // a plain struct literal — only `FromStr` applies the reference
            // regexp, and it is not on this path — so nothing else judges it.
            // The URL is then re-parsed, and the WHATWG parser collapses `..`:
            // a key of `../../../../../prod/app/manifests/latest` resolves to
            // `https://dest/v2/prod/app/manifests/latest`, escaping the prefix
            // `destination::physical_repository` exists to enforce and PUTting
            // upstream bytes into a repository the operator never named.
            let legal = is_legal_oci_tag(tag);
            if !legal {
                // `{tag:?}` for the same reason every other foreign-string site
                // in this crate uses it: the key is unfiltered upstream text and
                // a raw `{}` would let it forge log lines (CWE-117).
                tracing::warn!("dropping tag {tag:?}: not a legal OCI tag, so it cannot be published");
            }
            legal
        })
        .map(|(tag, entry)| TagCopyPlan {
            tag: tag.clone(),
            content: entry.content.clone(),
        })
        .collect();
    plan.sort_by_cached_key(|entry| pep440_sort_key(&entry.tag));
    plan
}

/// Whether `tag` is expressible as an OCI tag: `[a-zA-Z0-9_][a-zA-Z0-9._-]{0,127}`.
///
/// **Grammar, never classification.** The doc on [`tag_copy_plan`] forbids
/// `AliasTag::parse`, `variant_names`, `resolve_cascade_tags` and `decompose`,
/// and it is right to — a mirror holding a filtered subset would compute a
/// different cascade than the source published. This function asks only whether
/// a string *can be written down* as a tag, never what it means, so a
/// build-stamped `3.31.0_20260731` and a bare `20260814` both pass unexamined.
/// Conflating "the mirror parses no tag" with "the mirror validates no tag" is
/// how a path traversal reached the destination registry; they are orthogonal.
///
/// Hand-rolled rather than a `Regex`: the grammar is ASCII-only and four
/// predicates long, so a `LazyLock<Regex>` would cost a dependency edge and a
/// first-call compile to express less clearly. Length is checked in bytes, which
/// is exact here because every accepted character is ASCII.
fn is_legal_oci_tag(tag: &str) -> bool {
    let mut characters = tag.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    (first.is_ascii_alphanumeric() || first == '_')
        && tag.len() <= 128
        && characters.all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-'))
}

// ── Pure decisions ──────────────────────────────────────────────────────────

/// Refuse bytes that do not hash to the digest that named them.
///
/// The algorithm comes from the expected digest, so a source serving `sha512`
/// content is verified under `sha512` — the check is never silently narrowed to
/// one algorithm.
fn verify_digest(expected: &Digest, bytes: &[u8]) -> Result<(), CopyError> {
    let actual = expected.algorithm().hash(bytes);
    if actual == *expected {
        return Ok(());
    }
    Err(CopyError::DigestMismatch {
        expected: expected.clone(),
        actual,
    })
}

/// Refuse a manifest nested past [`MANIFEST_DEPTH_CEILING`].
///
/// Called at the entry of both walks, before the fetch rather than after it, so
/// a chain authored to exhaust the stack costs the mirror one refusal and not
/// one more round trip per level.
fn within_depth(digest: &Digest, depth: usize) -> Result<(), CopyError> {
    if depth <= MANIFEST_DEPTH_CEILING {
        return Ok(());
    }
    Err(CopyError::MalformedManifest(format!(
        "manifest {digest} sits {depth} image indexes deep, past the {MANIFEST_DEPTH_CEILING}-level ceiling"
    )))
}

/// What one manifest references, one level down.
#[derive(Debug, PartialEq, Eq)]
enum ChildReferences {
    /// Every entry of an image index's `manifests[]`, in wire order.
    Manifests(Vec<(Digest, u64)>),
    /// An image manifest's config blob followed by every layer, in wire order.
    Blobs(Vec<(Digest, u64)>),
}

/// Decompose one manifest into what has to travel with it (C-022).
///
/// **Every `manifests[]` entry, unconditionally.** `Platform::candidate_from_descriptor`,
/// `Index::fetch_candidates` and `Platform::from_image_index` all return `None`
/// for a descriptor that omits `platform` or declares `unknown/unknown` —
/// correct for *resolution*, where a client picks one artifact, and wrong for a
/// *mirror*. The image index is copied verbatim, so every descriptor travels
/// into the mirrored bytes regardless; a platform-filtered copy therefore
/// publishes an index referencing manifests that do not exist at the
/// destination.
fn child_references(manifest: &Manifest) -> Result<ChildReferences, CopyError> {
    match manifest {
        Manifest::ImageIndex(index) => {
            let mut children = Vec::with_capacity(index.manifests.len());
            for entry in &index.manifests {
                children.push(descriptor_target(&entry.digest, entry.size)?);
            }
            Ok(ChildReferences::Manifests(children))
        }
        Manifest::Image(image) => {
            let mut blobs = Vec::with_capacity(image.layers.len() + 1);
            blobs.push(descriptor_target(&image.config.digest, image.config.size)?);
            for layer in &image.layers {
                blobs.push(descriptor_target(&layer.digest, layer.size)?);
            }
            Ok(ChildReferences::Blobs(blobs))
        }
    }
}

/// Parse one descriptor's `(digest, size)` — fail closed on any of the three.
///
/// A descriptor is foreign data: an unparseable digest would otherwise be
/// skipped, a negative size would wrap into an enormous `u64`, and a size past
/// [`BLOB_SIZE_CEILING`] sizes an allocation the process does not survive. The
/// refusal happens here, beside the sibling guards, so it names the offending
/// descriptor instead of arriving as a silently clamped buffer.
fn descriptor_target(digest: &str, size: i64) -> Result<(Digest, u64), CopyError> {
    let parsed = Digest::try_from(digest)
        .map_err(|_| CopyError::MalformedManifest(format!("descriptor digest '{digest}' is not a digest")))?;
    let size = u64::try_from(size)
        .map_err(|_| CopyError::MalformedManifest(format!("descriptor '{digest}' declares a negative size {size}")))?;
    if size > BLOB_SIZE_CEILING {
        return Err(CopyError::MalformedManifest(format!(
            "descriptor '{digest}' declares {size} bytes, past the {BLOB_SIZE_CEILING}-byte ceiling"
        )));
    }
    Ok((parsed, size))
}

/// Read the destination blob probe (C-023).
///
/// The fork's `fetch_blob_size` answers `Ok(None)` **iff** the registry
/// returned 404 and `Err` on every other non-success status, so the
/// discrimination this function owns is exactly: present, authoritatively
/// absent, or *no authoritative answer at all*. The third case aborts the whole
/// run — a run that does not know what the destination holds cannot correctly
/// skip or copy anything, and treating a 401 or a 503 as "absent" is how a root
/// gets written over content that was never uploaded.
fn probe_verdict(probe: Result<Option<u64>, OciDistributionError>) -> Result<Option<u64>, CopyError> {
    probe.map_err(|error| {
        CopyError::Abort(MirrorError::TargetError(format!(
            "destination blob probe did not answer authoritatively: {error}"
        )))
    })
}

/// Whether a registry error is a rate limit worth retrying.
///
/// Two shapes, because the fork maps 429 two ways: `extract_location_header`
/// (every push path) produces `ServerError { code: 429 }`, while
/// `validate_registry_response` turns a 4xx carrying an OCI error envelope into
/// `RegistryError`, which has no status code of its own.
///
/// `Retry-After` is deliberately not honoured: the fork consumes the response
/// before building the error, so the header is not reachable from here. The
/// doubling ladder below stands in for it.
fn is_rate_limited(error: &OciDistributionError) -> bool {
    match error {
        OciDistributionError::ServerError { code, .. } => *code == 429,
        OciDistributionError::RegistryError { envelope, .. } => envelope
            .errors
            .iter()
            .any(|entry| matches!(entry.code, OciErrorCode::Toomanyrequests)),
        _ => false,
    }
}

/// Delay before retry `attempt`, doubling from [`RETRY_BACKOFF_BASE`] and
/// capped at [`RETRY_BACKOFF_MAX`].
///
/// Scaled down under `cfg(test)` exactly as `ocx_cli::push::push_retry_delay`
/// is, so the retry-count test does not spend real seconds asleep.
fn retry_delay(attempt: u32) -> Duration {
    let delay = RETRY_BACKOFF_BASE
        .saturating_mul(2u32.saturating_pow(attempt.saturating_sub(1)))
        .min(RETRY_BACKOFF_MAX);
    #[cfg(test)]
    let delay = delay / 1000;
    delay
}

/// Run `operation`, retrying **only** while `retryable` says so, at most
/// `max_retries` extra times (C-026).
///
/// Reactive only: no proactive pacing, and the only predicate any caller passes
/// is a rate limit. A registry that keeps answering 429 costs exactly
/// `max_retries + 1` attempts and then gives up.
async fn retry_while<T, E, F, Fut>(max_retries: u32, retryable: impl Fn(&E) -> bool, mut operation: F) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let mut attempt = 0;
    loop {
        match operation().await {
            Ok(value) => return Ok(value),
            Err(error) if retryable(&error) && attempt < max_retries => {
                attempt += 1;
                tracing::debug!("destination rate-limited; retry {attempt} of {max_retries}");
                tokio::time::sleep(retry_delay(attempt)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

/// Run `work` holding one permit from the **run-scoped** blob pool.
///
/// The memory ceiling is `max_blobs × largest_blob`, and it holds only because
/// this permit is taken from `CopyContext::blob_semaphore` — one `Arc` for the
/// whole run — rather than from a semaphore constructed per call. Per-call
/// permits with 44 concurrent tags would be 44 × 4 × 221 MB.
async fn with_blob_permit<T>(
    semaphore: &Semaphore,
    work: impl Future<Output = Result<T, CopyError>>,
) -> Result<T, CopyError> {
    let _permit = semaphore.acquire().await.map_err(|_| {
        CopyError::Abort(MirrorError::ExecutionFailed(vec![
            "blob concurrency semaphore was closed mid-run".to_string(),
        ]))
    })?;
    work.await
}

/// Drive `work` with at most [`BLOB_FANOUT_CEILING`] futures polled at once.
///
/// The sibling of [`with_blob_permit`] one level up: that pool bounds how many
/// blob *bodies* are resident, this bounds how many destination *probes* are in
/// flight, and only the second of those is a function of how many descriptors
/// the source chose to declare.
///
/// `stream::iter` is also lazy where `try_join_all` is not — the futures are
/// built as slots free up, so a manifest declaring 2×10⁵ layers no longer
/// materialises 2×10⁵ pending state machines before the first probe is sent.
async fn bounded_fanout<T, F>(work: impl Iterator<Item = F>) -> Result<Vec<T>, CopyError>
where
    F: Future<Output = Result<T, CopyError>>,
{
    futures::stream::iter(work)
        .buffer_unordered(BLOB_FANOUT_CEILING)
        .try_collect()
        .await
}

/// The logical identifier for a reference, for the ocx-side read seam.
///
/// `new_registry` adopts both halves verbatim — no parsing, so a repository
/// this crate already validated is not re-decomposed here.
fn source_identifier(reference: &Reference) -> Identifier {
    Identifier::new_registry(reference.repository(), reference.registry())
}

/// The same repository as `reference`, addressed by `digest`.
fn addressed(reference: &Reference, digest: &Digest) -> Reference {
    Reference::with_digest(
        reference.registry().to_string(),
        reference.repository().to_string(),
        digest.to_string(),
    )
}

// ── The ladder ──────────────────────────────────────────────────────────────

/// Copy one manifest and everything it references, by digest (C-022).
///
/// Pulls the manifest with `Index::fetch_manifest_raw_bytes`, asserts
/// `sha256(bytes) == digest` **before anything else**, then recurses into
/// every entry of an image index's `manifests[]` — or ensures the config and
/// every layer blob of an image manifest — and finally pushes the manifest
/// bytes **verbatim**, so the destination digest equals the source digest.
///
/// `source_reference` and `destination_reference` carry **location** only
/// (registry + repository); `digest` carries the content. The one thing the
/// destination reference's own specifier decides is whether a tag is created:
/// the fork addresses a manifest `PUT` by digest whenever the reference has
/// one, so a caller wanting `<repo>:<tag>` at the destination must pass a
/// tag-only reference. Children are always addressed by digest.
///
/// Referrer detection runs per copied manifest and **before** the push, so a
/// manifest this version cannot carry faithfully is never published.
///
/// Returns the counters **and the verified manifest bytes**, because the
/// top-level call's bytes are the package's dispatch object: C-033 writes them
/// under `o/` and one fetch has to serve both the registry push and the index
/// write. The recursion discards its children's — only a tag's own `content`
/// digest is a dispatch object.
///
/// # Errors
///
/// [`CopyError`] — fails the package, never the run. A digest mismatch names
/// both digests.
pub async fn copy_manifest_tree(
    source_reference: &Reference,
    destination_reference: &Reference,
    digest: &Digest,
    context: &CopyContext,
) -> Result<(CopyStats, Vec<u8>), CopyError> {
    copy_manifest_tree_at(source_reference, destination_reference, digest, context, 0).await
}

/// [`copy_manifest_tree`] with the recursion depth threaded through.
///
/// Split rather than adding a parameter to the public entry point: `depth` is
/// an implementation detail of the walk, and a caller passing anything but 0
/// would silently move the ceiling it exists to enforce.
async fn copy_manifest_tree_at(
    source_reference: &Reference,
    destination_reference: &Reference,
    digest: &Digest,
    context: &CopyContext,
    depth: usize,
) -> Result<(CopyStats, Vec<u8>), CopyError> {
    within_depth(digest, depth)?;
    let identifier = source_identifier(source_reference).clone_with_digest(digest.clone());
    let fetched = context
        .source_index
        .fetch_manifest_raw_bytes(&identifier)
        .await
        .map_err(|error| CopyError::SourceUnavailable(format!("{identifier}: {error}")))?;
    // An authoritative not-found is its own class (C-040's "a tag whose
    // `content` is gone upstream"), never `SourceUnavailable`: the source
    // answered, and retrying cannot bring a garbage-collected manifest back.
    let Some((bytes, _reported, manifest)) = fetched else {
        return Err(CopyError::ContentMissing {
            content: digest.clone(),
        });
    };

    // Before anything else, and against the digest the caller asked for — never
    // against the one the response reported about itself.
    verify_digest(digest, &bytes)?;

    // Post-hoc, because the fork exposes no pre-read cap on a manifest fetch:
    // the allocation has already happened, so this bounds what is republished
    // and recursed into, not what was read.
    if bytes.len() > MANIFEST_FETCH_CEILING {
        return Err(CopyError::MalformedManifest(format!(
            "manifest {digest} is {} bytes, past the {MANIFEST_FETCH_CEILING}-byte ceiling",
            bytes.len()
        )));
    }

    detect_referrers(source_reference, digest, context).await?;

    let mut stats = CopyStats {
        manifests: 1,
        ..CopyStats::default()
    };

    match child_references(&manifest)? {
        ChildReferences::Manifests(children) => {
            // Sequential: concurrency lives inside a package at the blob level,
            // and nesting it here would multiply the memory ceiling.
            for (child_digest, _size) in children {
                let child_source = addressed(source_reference, &child_digest);
                let child_destination = addressed(destination_reference, &child_digest);
                let (child_stats, _child_bytes) = Box::pin(copy_manifest_tree_at(
                    &child_source,
                    &child_destination,
                    &child_digest,
                    context,
                    depth + 1,
                ))
                .await?;
                stats.merge(child_stats);
            }
        }
        ChildReferences::Blobs(blobs) => {
            // Each future carries its own blob's size back rather than being
            // zipped against the input afterwards, so the counters no longer
            // depend on the order results arrive in — `buffer_unordered` yields
            // in completion order, and a zip against it would pair an
            // `Uploaded` outcome with a different blob's byte count. The summed
            // `CopyStats` is identical whichever order they land in, which is
            // the determinism that actually matters here; the per-blob log
            // lines are emitted inside `ensure_blob` and were already
            // concurrent.
            let outcomes = bounded_fanout(blobs.iter().map(|(blob_digest, size)| async move {
                let outcome = ensure_blob(source_reference, destination_reference, blob_digest, *size, context).await?;
                Ok((outcome, *size))
            }))
            .await?;
            for (outcome, size) in outcomes {
                stats.record(outcome, size);
            }
        }
    }

    push_manifest(destination_reference, &bytes, manifest.content_type(), context).await?;
    Ok((stats, bytes))
}

/// Push one manifest's bytes verbatim, retrying only a rate limit.
async fn push_manifest(
    destination_reference: &Reference,
    bytes: &[u8],
    content_type: &str,
    context: &CopyContext,
) -> Result<(), CopyError> {
    // `HeaderValue` is `http` 1.x's type, which reqwest 0.12 (this crate) and
    // reqwest 0.13 (the fork) both re-export from the single `http` in the
    // lockfile. A second `http` major would break this line, which is the right
    // place for that to surface.
    let header = reqwest::header::HeaderValue::from_str(content_type).map_err(|_| {
        CopyError::MalformedManifest(format!("manifest declares an unusable media type '{content_type}'"))
    })?;

    retry_while(context.max_retries, is_rate_limited, || {
        // Cloned per attempt: a manifest is bounded by MANIFEST_FETCH_CEILING,
        // so the copy is cheap and the alternative (holding a `bytes::Bytes`)
        // would name a crate this package does not depend on directly.
        let body = bytes.to_vec();
        let header = header.clone();
        async move {
            context
                .destination_client
                .push_manifest_raw(destination_reference, body, header)
                .await
        }
    })
    .await
    .map_err(|error| CopyError::PushRejected(format!("manifest at {destination_reference}: {error}")))?;

    Ok(())
}

/// Ensure one blob is present at the destination (C-023).
///
/// **The destination probe never touches `ocx_lib::oci::Client`.** Its
/// `head_blob` opens with `transport_reference` — the `[mirrors]` seam — and
/// every `_addressed` sibling that would bypass it is `pub(crate)`. A
/// mirror-rewritten HEAD would confirm content at mirror host M while the push
/// went to destination D, and the root would then be written over a tree that
/// resolves nowhere.
///
/// Order: destination `fetch_blob_size` → hit ⇒ [`BlobOutcome::Skipped`]; an
/// authoritative 404 ⇒ continue; **any other outcome (401, 503, timeout,
/// connection reset) ⇒ [`MirrorError::TargetError`] aborting the whole run**.
/// On a miss: consult the in-run digest → repository map and attempt
/// `mount_blob`; `BlobMountResponse::UploadSessionOpened(_)` — or any `Err` —
/// falls through to pull → verify → push. A mount that fails although the
/// source blob demonstrably exists warns **once per source→destination
/// repository pair**, never per blob.
///
/// # Errors
///
/// [`CopyError`] — an aggregating variant fails the package;
/// [`CopyError::Abort`] carrying [`MirrorError::TargetError`] aborts the whole
/// run.
pub async fn ensure_blob(
    source_reference: &Reference,
    destination_reference: &Reference,
    digest: &Digest,
    size: u64,
    context: &CopyContext,
) -> Result<BlobOutcome, CopyError> {
    let digest_string = digest.to_string();
    let destination_repository = destination_reference.repository().to_string();

    let probe = context
        .destination_client
        .fetch_blob_size(destination_reference, &digest_string)
        .await;
    if probe_verdict(probe)?.is_some() {
        record_location(context, digest, &destination_repository).await;
        // `info!`, not `debug!`: an all-present re-run takes this branch for
        // every blob, and at `debug!` the whole run printed nothing at all.
        tracing::info!("blob {digest} already present in {destination_repository}");
        return Ok(BlobOutcome::Skipped);
    }

    if mount(
        destination_reference,
        digest,
        &digest_string,
        &destination_repository,
        context,
    )
    .await
    {
        record_location(context, digest, &destination_repository).await;
        tracing::info!("blob {digest} mounted into {destination_repository}");
        return Ok(BlobOutcome::Mounted);
    }

    // The permit covers pull → verify → push, which is the only window holding
    // a blob in memory. The probe and the mount attempt stay outside it so an
    // all-present re-run is not serialised down to `max_blobs`; how many of
    // those run at once is the caller's `BLOB_FANOUT_CEILING`, not this pool.
    with_blob_permit(&context.blob_semaphore, async {
        upload(
            source_reference,
            destination_reference,
            digest,
            &digest_string,
            size,
            context,
        )
        .await
    })
    .await?;

    record_location(context, digest, &destination_repository).await;
    tracing::info!("blob {digest} uploaded to {destination_repository} ({size} bytes)");
    Ok(BlobOutcome::Uploaded)
}

/// Try a cross-repository mount; `true` iff the registry mounted the blob.
///
/// Every miss — a declined mount, an auth failure, any transport error — is a
/// fall-through to the upload path, so this function never fails a package.
async fn mount(
    destination_reference: &Reference,
    digest: &Digest,
    digest_string: &str,
    destination_repository: &str,
    context: &CopyContext,
) -> bool {
    // Scoped so the guard is dropped before the await below.
    let from = {
        let located = context.mounted_from.lock().await;
        located.get(digest).cloned()
    };
    let Some(from) = from else {
        return false;
    };
    if from == destination_repository {
        return false;
    }

    // `mount_blob` reads only `repository()` off the source reference — the
    // registry and tag are never sent — so the tag here is an inert placeholder.
    let source = Reference::with_tag(
        destination_reference.registry().to_string(),
        from.clone(),
        "latest".to_string(),
    );
    match context
        .destination_client
        .mount_blob(destination_reference, &source, digest_string)
        .await
    {
        Ok(BlobMountResponse::Mounted) => true,
        outcome => {
            warn_mount_once(context, &from, destination_repository, &outcome).await;
            false
        }
    }
}

/// Warn once per `(source repository, destination repository)` pair.
async fn warn_mount_once(
    context: &CopyContext,
    from: &str,
    destination_repository: &str,
    outcome: &Result<BlobMountResponse, OciDistributionError>,
) {
    let pair = (from.to_string(), destination_repository.to_string());
    let first = context.mount_warnings.lock().await.insert(pair);
    if first {
        let reason = match outcome {
            Ok(_) => "the registry declined and opened an upload session".to_string(),
            Err(error) => error.to_string(),
        };
        tracing::warn!(
            "cross-repository mount from {from} into {destination_repository} unavailable, uploading instead: {reason}"
        );
    }
}

/// Which step of one upload attempt failed, and whether trying again could
/// change the outcome.
#[derive(Debug)]
enum UploadFailure {
    Pull(OciDistributionError),
    Verify(CopyError),
    Push(OciDistributionError),
}

impl UploadFailure {
    /// Only a rate limit is retried, and a digest mismatch never is: the same
    /// bytes will not start hashing differently.
    fn retryable(&self) -> bool {
        match self {
            Self::Pull(error) | Self::Push(error) => is_rate_limited(error),
            Self::Verify(_) => false,
        }
    }

    fn into_copy_error(self, digest: &Digest) -> CopyError {
        match self {
            Self::Pull(error) => CopyError::SourceUnavailable(format!("blob {digest}: {error}")),
            Self::Verify(error) => error,
            Self::Push(error) => CopyError::PushRejected(format!("blob {digest}: {error}")),
        }
    }
}

/// A `Vec` sink that refuses to accept more than `limit` bytes.
///
/// [`BLOB_SIZE_CEILING`] bounds the *declared* size and so bounds the
/// pre-allocation, but nothing bounded the **stream**: `pull_blob` writes the
/// whole response into the sink and the digest is only checked afterwards, so a
/// source that declares 1 KiB and then streams forever exhausts memory before
/// any verification runs. The declared size is a claim by the same party
/// sending the bytes; this makes it load-bearing.
///
/// Refusing is correct rather than truncating: a body longer than its
/// descriptor claims cannot hash to the expected digest, so the copy was going
/// to fail regardless — failing at the first over-long chunk turns an
/// unbounded read into a bounded one and says why.
struct BoundedSink<'buffer> {
    buffer: &'buffer mut Vec<u8>,
    remaining: u64,
}

impl tokio::io::AsyncWrite for BoundedSink<'_> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        _context: &mut std::task::Context<'_>,
        chunk: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let accepted = chunk.len() as u64;
        if accepted > self.remaining {
            return std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "blob body is longer than its descriptor declared",
            )));
        }
        self.remaining -= accepted;
        self.buffer.extend_from_slice(chunk);
        std::task::Poll::Ready(Ok(chunk.len()))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

/// The buffer [`upload`] pre-sizes for a blob declaring `size` bytes.
///
/// A **hint**, never a contract: the `Vec` grows on demand, so a clamp costs
/// one reallocation on content that should not exist and can never truncate a
/// body. It is clamped anyway because this is the exact allocation
/// [`BLOB_SIZE_CEILING`] exists to stop and [`ensure_blob`] is `pub` — a caller
/// reaching it with a size that never passed [`descriptor_target`] must not be
/// able to reintroduce the process abort.
fn capacity_hint(size: u64) -> usize {
    // `try_from` rather than a cast: on a 32-bit target the ceiling exceeds
    // `usize::MAX`, and a hint of zero is the right answer there — the `Vec`
    // still grows to whatever the pull delivers.
    usize::try_from(size.min(BLOB_SIZE_CEILING)).unwrap_or(0)
}

/// Pull the blob, verify it against its digest, then push it.
///
/// The whole sequence is one retry unit. A rate limit on the push therefore
/// re-pulls rather than holding a second copy of the body alive for a retry —
/// the memory ceiling is `max_blobs × largest_blob` and a clone-to-retry would
/// double it on exactly the largest blobs.
///
/// **Verification happens before the first byte is pushed.** The fork does
/// expose `push_blob_stream`/`pull_blob_stream`, so the ADR's original "no
/// streaming push exists" rationale for buffering is stale; the live reason is
/// this ordering, which a naive tee cannot satisfy. Deleting the buffer would
/// delete the verification with it.
async fn upload(
    source_reference: &Reference,
    destination_reference: &Reference,
    digest: &Digest,
    digest_string: &str,
    size: u64,
    context: &CopyContext,
) -> Result<(), CopyError> {
    let source = addressed(source_reference, digest);
    // Before the pull, because everything after it reports a blob that already
    // finished — and a 221 MB layer spends its entire transfer in this call
    // with nothing on stderr to show for it.
    tracing::info!("pulling blob {digest} ({size} bytes)");
    retry_while(context.max_retries, UploadFailure::retryable, || async {
        let mut body = Vec::with_capacity(capacity_hint(size));
        // Bounded by the descriptor's own claim, not just pre-sized by it —
        // see `BoundedSink`. `size` has already passed `BLOB_SIZE_CEILING`.
        let sink = BoundedSink {
            buffer: &mut body,
            remaining: size,
        };
        context
            .source_client
            .pull_blob(&source, digest_string, sink)
            .await
            .map_err(UploadFailure::Pull)?;

        verify_digest(digest, &body).map_err(UploadFailure::Verify)?;

        context
            .destination_client
            .push_blob(destination_reference, body, digest_string)
            .await
            .map_err(UploadFailure::Push)?;
        Ok(())
    })
    .await
    .map_err(|failure| failure.into_copy_error(digest))
}

/// Detect referrers on a copied manifest and fail the package if any exist
/// (C-024).
///
/// Goes through the fork's `Client::pull_referrers`, **never raw HTTP**: that
/// call falls back to the referrers **tag schema** when a registry 404s
/// `/v2/<name>/referrers/<digest>`, and a hand-rolled endpoint call would read
/// such a registry as "no referrers" and ship the exact silent drop this
/// contract exists to prevent.
///
/// Depth **1**, no recursion — a referrer's own referrers are never queried,
/// so there is no graph and no cycle. This is the referrers **API** surface;
/// referrer and attestation descriptors nested inside an image index are a
/// different surface and are copied, not detected.
///
/// # Errors
///
/// [`CopyError::ReferrersPresent`] when the response is non-empty, naming at
/// most [`REFERRER_REPORT_LIMIT`] digests plus the total count.
pub async fn detect_referrers(
    source_reference: &Reference,
    digest: &Digest,
    context: &CopyContext,
) -> Result<(), CopyError> {
    let subject = addressed(source_reference, digest);
    let referrers = context
        .source_client
        .pull_referrers(&subject, None)
        .await
        .map_err(|error| CopyError::SourceUnavailable(format!("referrers for {digest}: {error}")))?;

    if referrers.manifests.is_empty() {
        return Ok(());
    }
    Err(referrer_failure(&referrers.manifests))
}

/// The bounded refusal for a manifest that carries referrers.
///
/// Bounded on purpose: a subject with 10,000 referrers must not produce a
/// 10,000-line error, so the message names the first
/// [`REFERRER_REPORT_LIMIT`] digests and the total.
fn referrer_failure(manifests: &[ocx_lib::oci::ImageIndexEntry]) -> CopyError {
    CopyError::ReferrersPresent {
        shown: manifests
            .iter()
            .take(REFERRER_REPORT_LIMIT)
            .filter_map(|entry| Digest::try_from(entry.digest.as_str()).ok())
            .collect(),
        total: manifests.len(),
    }
}

/// Copy a package's `<repository>:__ocx.desc` tag (C-025).
///
/// Copied explicitly, per package: ocx filters reserved tags at render, so it
/// never appears in `root.tags{}` and no `tags{}` walk reaches it. An
/// authoritative not-found is a no-op, not a failure — not every package has
/// one.
///
/// # Errors
///
/// [`CopyError`] when the description manifest exists but cannot be copied.
pub async fn copy_description(
    source_reference: &Reference,
    destination_reference: &Reference,
    context: &CopyContext,
) -> Result<(), CopyError> {
    let identifier = source_identifier(source_reference).clone_with_tag(DESCRIPTION_TAG);
    let fetched = context
        .source_index
        .fetch_manifest_raw_bytes(&identifier)
        .await
        .map_err(|error| CopyError::SourceUnavailable(format!("{identifier}: {error}")))?;
    let Some((_bytes, digest, _manifest)) = fetched else {
        return Ok(());
    };

    // Re-fetched by `copy_manifest_tree` rather than threaded through: a
    // description manifest is a few hundred bytes, and one shared ladder is
    // worth more than one saved request.
    let destination = Reference::with_tag(
        destination_reference.registry().to_string(),
        destination_reference.repository().to_string(),
        DESCRIPTION_TAG.to_string(),
    );
    copy_manifest_tree(source_reference, &destination, &digest, context).await?;
    Ok(())
}

/// Walk a manifest tree by digest and report every blob the destination is
/// **missing**, without transferring anything (C-043's input half).
///
/// The same walk [`copy_manifest_tree`] performs — same digest verification,
/// same "every `manifests[]` entry, unconditionally" decomposition, same
/// [`probe_verdict`] discrimination — with the transfer replaced by the probe
/// alone. It is a separate function rather than a `dry_run: bool` on the copy,
/// because a flag that silently disables every write is the shape a future
/// edit gets wrong.
///
/// **Blobs only.** C-043 says "every descriptor", and the descriptors that are
/// nested *manifests* are excluded here: they are a few kilobytes each against
/// layers measured in hundreds of megabytes, and probing them would need a
/// manifest `HEAD` the fork does not expose separately from a full fetch. The
/// estimate is therefore exact for the bytes that dominate it and short by the
/// manifest bodies.
///
/// Referrer detection is deliberately **not** run: `--dry-run` reports what a
/// copy would move, and refusing a package here would report a failure the
/// operator did not ask this run to discover.
///
/// # Errors
///
/// [`CopyError`] — an aggregating variant fails the package;
/// [`CopyError::Abort`] (a destination probe with no authoritative answer)
/// aborts the whole run, exactly as it does on the copy path.
pub async fn missing_descriptors(
    source_reference: &Reference,
    destination_reference: &Reference,
    digest: &Digest,
    context: &CopyContext,
) -> Result<Vec<(Digest, u64)>, CopyError> {
    missing_descriptors_at(source_reference, destination_reference, digest, context, 0).await
}

/// [`missing_descriptors`] with the recursion depth threaded through.
///
/// The `--dry-run` walk recurses exactly as the copy walk does, so it needs the
/// same ceiling: a hostile chain would exhaust the stack before a single byte
/// was ever going to move, which is the one path an operator reaches for
/// *because* it is supposed to be harmless.
async fn missing_descriptors_at(
    source_reference: &Reference,
    destination_reference: &Reference,
    digest: &Digest,
    context: &CopyContext,
    depth: usize,
) -> Result<Vec<(Digest, u64)>, CopyError> {
    within_depth(digest, depth)?;
    let identifier = source_identifier(source_reference).clone_with_digest(digest.clone());
    let fetched = context
        .source_index
        .fetch_manifest_raw_bytes(&identifier)
        .await
        .map_err(|error| CopyError::SourceUnavailable(format!("{identifier}: {error}")))?;
    let Some((bytes, _reported, manifest)) = fetched else {
        return Err(CopyError::ContentMissing {
            content: digest.clone(),
        });
    };
    verify_digest(digest, &bytes)?;

    if bytes.len() > MANIFEST_FETCH_CEILING {
        return Err(CopyError::MalformedManifest(format!(
            "manifest {digest} is {} bytes, past the {MANIFEST_FETCH_CEILING}-byte ceiling",
            bytes.len()
        )));
    }

    let mut missing = Vec::new();
    match child_references(&manifest)? {
        ChildReferences::Manifests(children) => {
            for (child_digest, _size) in children {
                let child_source = addressed(source_reference, &child_digest);
                let child_destination = addressed(destination_reference, &child_digest);
                let nested = Box::pin(missing_descriptors_at(
                    &child_source,
                    &child_destination,
                    &child_digest,
                    context,
                    depth + 1,
                ))
                .await?;
                missing.extend(nested);
            }
        }
        ChildReferences::Blobs(blobs) => {
            for (blob_digest, size) in blobs {
                let probe = context
                    .destination_client
                    .fetch_blob_size(destination_reference, &blob_digest.to_string())
                    .await;
                if probe_verdict(probe)?.is_none() {
                    missing.push((blob_digest, size));
                }
            }
        }
    }
    Ok(missing)
}

/// Record which destination repository now holds `digest`, for later mounts.
async fn record_location(context: &CopyContext, digest: &Digest, repository: &str) {
    context
        .mounted_from
        .lock()
        .await
        .entry(digest.clone())
        .or_insert_with(|| repository.to_string());
}

#[cfg(test)]
#[path = "registry_copy/tests.rs"]
mod tests;
