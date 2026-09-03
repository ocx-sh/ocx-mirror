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
use ocx_lib::oci::client::OciTransport;
use ocx_lib::oci::client::error::ClientError;
use ocx_lib::oci::index::RootTag;
use ocx_lib::oci::native::oci_client::client::BlobMountResponse;
use ocx_lib::oci::native::oci_client::errors::{OciDistributionError, OciErrorCode};
use ocx_lib::oci::{Descriptor, Digest, Identifier, ImageIndexEntry, Index, Manifest, Reference, native};
use tokio::sync::{Mutex, OnceCell, Semaphore};

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
/// [`source_referrers`]'s two legs are bounded differently, and neither by
/// this constant alone. The native leg is bounded *before* the read: ocx
/// v0.6.0's `Client::pull_referrers_native` streams through a 4 MiB counted
/// read and additionally refuses past 4096 descriptors, which is the pre-read
/// cap `Client::pull_referrers` never had. The tag-schema fallback leg is
/// bounded post-hoc by this constant, because it reads through
/// `Index::fetch_manifest_raw_bytes` like every other manifest here. Said
/// exactly rather than rounded off, because a comment claiming a cap that does
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

/// Child manifests of one image index copied at once, at the top level only.
///
/// 8: an index carries one manifest per platform, and every published OCX
/// package is inside that — so this is "all of them, concurrently" for real
/// input, with a bound in place for input that is not. Deliberately far below
/// [`BLOB_FANOUT_CEILING`]: each child opens a fan-out of its own, and the
/// product is what the destination sees.
const CHILD_FANOUT_CEILING: usize = 8;

/// Levels of nested image index either manifest walk descends before refusing
/// (C-022).
///
/// An entry of `manifests[]` may itself be an image index — the OCI image spec
/// admits it, and a descriptor says nothing about how deep the chain below it
/// goes — so the walk's depth is chosen by the source, not by this mirror. A
/// chain of indexes each naming one more is a few hundred bytes to author and
/// ends in stack exhaustion. Digest cycles are not the risk: a child's digest
/// covers its parent's bytes, so a cycle would need a sha256 preimage. Depth
/// alone is. The referrer sweep is bounded separately, by
/// [`REFERRER_DEPTH_CEILING`], because its hops are a different graph.
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

/// Referrer hops the sweep carries below a copied manifest (C-061).
///
/// A referrer is itself a manifest and can carry referrers of its own — a
/// signature over a signature, an attestation over an attestation — so the
/// sweep is a walk over a second graph, not a flat list, and it needs its own
/// ceiling for the same reason [`MANIFEST_DEPTH_CEILING`] exists: the depth is
/// chosen by the source.
///
/// 2, counting the copied manifest itself as hop 0. Hop 1 is what cosign,
/// notation and every attestation tool writes; hop 2 covers an attestation
/// *about* a signature, which is the deepest shape anyone ships. A referrer
/// discovered at hop 3 is refused rather than silently dropped — a mirror that
/// quietly stops carrying signatures past some depth is worse than one that
/// says so.
const REFERRER_DEPTH_CEILING: usize = 2;

/// Referrers of one subject the sweep carries before refusing the package
/// (C-061).
///
/// The referrers listing is foreign data with no bound of its own below the
/// 4096 descriptors ocx's own read already refuses, and every entry costs a
/// manifest fetch plus a blob fan-out at the destination. 64 clears every real
/// subject by orders of magnitude — a signed release carries a signature, an
/// attestation and an SBOM, so three — while keeping a hostile subject from
/// turning one tag into 4096 nested copies.
const REFERRER_COUNT_CEILING: usize = 64;

/// The cosign sidecar tag suffixes, appended to [`referrer_fallback_tag`]'s
/// spelling of a subject digest (C-062).
///
/// [`referrer_fallback_tag`]: ocx_lib::package::tag::referrer_fallback_tag
///
/// Cosign's pre-OCI-1.1 scheme parks a signature at `sha256-<hex>.sig` beside
/// its subject, and the OCI referrers fallback tag for a sha256 subject is
/// `sha256-<hex>` exactly — 64 hex characters, so the spec's "truncated to 64"
/// truncates nothing. The two schemes therefore share a prefix, which is why
/// one helper spells both.
///
/// These tags are **not** referrers: nothing links them to their subject but
/// the tag name, so no referrers listing at either end mentions them and a
/// mirror that copies only the referrers graph silently drops every cosign
/// signature written before OCI 1.1.
///
/// **Duplicates ocx's `package::tag::SIDECAR_SUFFIXES`** (`tag.rs:205-207`),
/// identical today and asymmetrically consumed: the backfill's tag filter
/// reaches *ocx's* list through `Tag::is_reserved_str`, while this sweep uses
/// this one. A fourth suffix added upstream would therefore be **reserved by
/// the backfill and not carried by `registry sync`** — one-sided and silent.
///
/// Not deduplicated here because ocx's list and its `sidecar_tag` are both
/// `pub(crate)`, and `is_reserved_str` cannot substitute: it is a predicate
/// ("is this tag reserved") where the sweep needs an enumerator ("which tags
/// do I go fetch"). Closing it needs an ocx export — tracked upstream at
/// ocx-sh/ocx#404. The mitigating fact is that both the list and
/// `is_referrer_fallback_tag` sit within six lines of each other in `tag.rs`,
/// so whoever adds a fourth is already editing the place that must change.
const SIDECAR_SUFFIXES: [&str; 3] = [".sig", ".att", ".sbom"];

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

/// Seed the source client's credential for the **physical** host a root points
/// at, before any request addressed to it.
///
/// [`build_source_client`] can only seed the source's *logical* `registry:`
/// name — `ocx.sh`, the index's identity — because the physical host is not
/// known until a root document has been fetched, and one source may name
/// several. Every request the source client makes goes to that physical host
/// instead, and the fork's `get_auth_token` keys its auth store by
/// `Reference::resolve_registry()`. A miss is silent: the request goes out
/// with **no `Authorization` header at all**, which a registry demanding a
/// token challenge answers 401 — including for a public package, whose
/// anonymous token still has to be fetched. The public `ocx.sh` index points
/// every package at `ghcr.io`, so without this the first referrer query of the
/// first tag fails and no real source is copyable.
///
/// Seeding is enough: `_auth` runs the `WWW-Authenticate` challenge itself
/// from the stored `RegistryAuth`, anonymous included.
///
/// **A real credential is resolved only for a host the operator named.**
/// `registry` is the *physical* host, and it arrives in the upstream root's
/// pointer — remote-controlled data. The env-credential ladder keys on
/// `registry.to_slug()`, and `to_slug` replaces every non-alphanumeric byte
/// with `_`, so `registry.corp.example.com` and an attacker-registrable
/// `registry-corp-example.com` produce **one** slug: a hostile source pointing
/// a package at the lookalike host would otherwise have the mirror resolve the
/// operator's real `OCX_AUTH_registry_corp_example_com_*` and send it there.
/// So a physical host is credentialed only when it is the source's own
/// `registry:` or one of its `trusted_hosts` — the same list that already
/// vouches for a host against the SSRF floor. Everything else is seeded
/// `Anonymous`, which is all the public case ever needed: the 401 this fixes
/// is a token-store **miss** (`get_auth_token` returns `None` and no
/// `Authorization` header goes out at all), and an `Anonymous` entry turns the
/// miss into a hit whose `WWW-Authenticate` challenge fetches the anonymous
/// bearer token. The public `ocx.sh` index points every package at `ghcr.io`,
/// which no operator names, so it takes the anonymous path and copies.
///
/// A private *indirected* mirror — index on `ocx.sh`, content on a private
/// `ghcr.io` repo — lists `ghcr.io` in that source's `trusted_hosts` to opt it
/// back into credential resolution.
///
/// The resolver is `context.source_auth`, held once per run so its cache is
/// shared across every package; an unnamed host never touches it, so the
/// common path makes no env read and no credential-helper subprocess call.
/// Idempotent — `store_auth_if_needed` skips a host already stored, so the
/// per-package call costs one read lock after the first.
pub async fn ensure_source_auth(context: &CopyContext, registry: &str, allowed: &[&str]) {
    let auth = if host_is_credentialed(registry, allowed) {
        context.source_auth.get_or_fallback(registry).await
    } else {
        native::Auth::Anonymous
    };
    context.source_client.store_auth_if_needed(registry, &auth).await;
}

/// Whether a physical host may receive a *resolved* credential — the security
/// boundary of [`ensure_source_auth`], as a pure function so the allow-list
/// decision is unit-testable without a live registry.
///
/// `allowed` is the source's own `registry:` plus its `trusted_hosts`. Exact
/// string match, never a slug or a suffix: the whole point is that a lookalike
/// physical host from a hostile root — which `to_slug` would collapse onto the
/// same env-var key as a real one — is **not** in this list and so takes the
/// `Anonymous` path.
fn host_is_credentialed(registry: &str, allowed: &[&str]) -> bool {
    allowed.contains(&registry)
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
    /// Credential resolver for source physical hosts, held once per run so its
    /// cache is shared across packages. Consulted by [`ensure_source_auth`]
    /// only for a host the operator named; an unnamed host is seeded
    /// `Anonymous` without touching it.
    pub source_auth: ocx_lib::auth::Auth,
    /// Destination-side fork client — every probe and push (C-046).
    pub destination_client: native::Client,
    /// Run-scoped blob permit pool sized by `concurrency.max_blobs`.
    pub blob_semaphore: Arc<Semaphore>,
    /// Reactive-retry budget from `concurrency.max_retries`.
    pub max_retries: u32,
    /// Whether each copied manifest also gets its `sha256.<hex>` canonical
    /// tag at the destination — `canonical_tags:`, resolved once.
    pub canonical_tags: bool,
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
    /// Destination-side transport — the **only** route to the referrers
    /// tag-schema merge (C-063).
    ///
    /// `native::Client` can push a manifest and read a tag, but it has no
    /// spelling for the read-modify-write append an OCI fallback index needs,
    /// and re-implementing that here would fork the retry budget and the
    /// already-present check away from `ocx_lib`'s. Everything else on the
    /// destination keeps going through `destination_client`, so this field is
    /// not a second client so much as one missing verb.
    pub destination_transport: Box<dyn OciTransport>,
    /// Whether the destination answers the Referrers API, probed **once per
    /// run** (C-063).
    ///
    /// One destination registry per run, so the verdict cannot differ between
    /// packages; a per-package probe would be one extra round trip per tag for
    /// an answer already known. Fallible init, so `get_or_try_init` — a probe
    /// that could not be answered must not cache a guess.
    pub referrer_destination: OnceCell<ReferrerDestination>,
    /// `(destination repository, referrer digest)` pairs this run already
    /// carried, so a diamond copies its shared subject once (C-061).
    ///
    /// Keyed by repository for the same reason [`confirmed_here`] is: a
    /// referrer present in one destination repository says nothing about
    /// another, and each package resolves to its own `physical_repository`.
    /// That makes this **at least** the per-package set C-061 asks for, plus
    /// the dedup across the tags of one package that a per-package set would
    /// also give — 21 tags over 9 indexes is an ordinary package, and every
    /// one of them re-declares the same signature.
    pub carried_referrers: Arc<Mutex<HashSet<(String, Digest)>>>,
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

/// How a carried referrer becomes discoverable at the destination (C-063).
///
/// Not a capability report about the registry so much as a routing decision:
/// pushing a subject-bearing manifest is the same request either way, and this
/// says only whether a fallback index has to be written beside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferrerDestination {
    /// The destination answers `/v2/<name>/referrers/<digest>` and computes
    /// the listing itself — nothing more to write.
    Supported,
    /// The destination has no Referrers API, so the carried referrer is
    /// discoverable only through the OCI tag-schema fallback index, which this
    /// mirror appends to.
    Fallback,
}

/// Whether the manifest about to be pushed carries an OCI `subject`
/// descriptor.
///
/// An enum rather than a `bool` because it decides how one status code is
/// read: a `405` on a subject-bearing PUT is [`CopyError::SubjectRejected`]
/// and a `405` on any other PUT is an ordinary rejection, and a bare `true` at
/// the call site says neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubjectCarriage {
    /// An ordinary manifest — a tag's content, a platform manifest, a sidecar.
    Absent,
    /// A referrer manifest, whose `subject` is the whole reason it exists.
    Present,
}

/// What a manifest walk carries besides its own bytes (C-060…C-063).
///
/// Threaded beside `depth` rather than folded into it: the manifest-nesting
/// depth and the referrer hop count are two different graphs with two
/// different ceilings, and collapsing them would let a deep index budget away
/// a signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Carriage {
    /// The referrer hop this manifest sits at, or `None` where the sweep does
    /// not run below it at all — a sidecar's own subtree, which is a leaf by
    /// construction.
    hop: Option<usize>,
    /// How a `405` from the destination is read for this manifest's push.
    subject: SubjectCarriage,
}

impl Carriage {
    /// The carriage a tag's own content starts with: hop 0, no subject.
    fn root() -> Self {
        Self {
            hop: Some(0),
            subject: SubjectCarriage::Absent,
        }
    }

    /// The carriage a child manifest of an image index inherits.
    ///
    /// The hop is **not** advanced: an index's platform manifests sit at the
    /// same referrer depth as the index, and cosign signs the platform
    /// manifests, so a child that did not sweep would drop every per-platform
    /// signature.
    fn child(self) -> Self {
        Self {
            hop: self.hop,
            subject: SubjectCarriage::Absent,
        }
    }

    /// The carriage a referrer sitting `hop` hops out is copied under.
    ///
    /// Takes the hop rather than deriving it from `self`, because the caller
    /// has already had to compute and bound it ([`within_referrer_depth`]) —
    /// deriving it twice is where the two would drift.
    fn referrer(hop: usize) -> Self {
        Self {
            hop: Some(hop),
            subject: SubjectCarriage::Present,
        }
    }

    /// The carriage a cosign sidecar is copied under.
    ///
    /// `hop: None` — a sidecar is discovered by tag name alone, so its own
    /// referrers and sidecars are not part of any subject's graph, and
    /// sweeping them would walk `.sig.sig` tags forever.
    fn sidecar() -> Self {
        Self {
            hop: None,
            subject: SubjectCarriage::Absent,
        }
    }
}

/// Per-package transfer counters, summed into the run report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CopyStats {
    pub manifests: usize,
    pub blobs_skipped: usize,
    pub blobs_mounted: usize,
    pub blobs_uploaded: usize,
    pub bytes_uploaded: u64,
    /// Referrer manifests carried across, counted once per (repository,
    /// digest) pair the sweep actually copied (C-064).
    pub referrers_copied: usize,
    /// Cosign sidecar tags carried across (C-064).
    pub sidecars_copied: usize,
    /// Sidecar tags **not** carried because the destination already holds that
    /// tag at a different digest (C-062, C-064).
    pub sidecar_conflicts: usize,
}

impl CopyStats {
    /// Fold a child manifest's counters into this one.
    pub fn merge(&mut self, other: CopyStats) {
        self.manifests += other.manifests;
        self.blobs_skipped += other.blobs_skipped;
        self.blobs_mounted += other.blobs_mounted;
        self.blobs_uploaded += other.blobs_uploaded;
        self.bytes_uploaded += other.bytes_uploaded;
        self.referrers_copied += other.referrers_copied;
        self.sidecars_copied += other.sidecars_copied;
        self.sidecar_conflicts += other.sidecar_conflicts;
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
    /// The destination refused a manifest carrying an OCI `subject`
    /// descriptor — Amazon ECR answers `405 Method Not Allowed` (C-063).
    ///
    /// Its own variant rather than a [`Self::PushRejected`] because it is not
    /// a fault of these bytes: the *package* copied, and only its signatures
    /// did not. Aggregating, per package, never a run abort — a registry that
    /// refuses subjects refuses them for every package, and ending the run
    /// would mean a mirror pointed at ECR copies nothing at all.
    SubjectRejected { registry: String, status: u16 },
    /// One subject named more referrers than [`REFERRER_COUNT_CEILING`]
    /// (C-061).
    ReferrerBudgetExceeded { subject: Digest, limit: usize },
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
            Self::SubjectRejected { registry, status } => write!(
                f,
                "{registry} answered {status} to a manifest carrying a subject descriptor, \
                 so this package's signatures were not carried"
            ),
            Self::ReferrerBudgetExceeded { subject, limit } => write!(
                f,
                "subject {subject} names more than {limit} referrers, past the sweep's ceiling"
            ),
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

/// Refuse a referrer discovered past [`REFERRER_DEPTH_CEILING`] (C-061).
///
/// Called on the *discovered* referrer's hop, before it is fetched, so a chain
/// authored to exhaust the sweep costs one refusal rather than one round trip
/// per level. A refusal rather than a silent stop: a mirror that quietly drops
/// signatures past a depth nobody documented is indistinguishable from one
/// that never had them.
fn within_referrer_depth(subject: &Digest, hop: usize) -> Result<(), CopyError> {
    if hop <= REFERRER_DEPTH_CEILING {
        return Ok(());
    }
    Err(CopyError::MalformedManifest(format!(
        "a referrer of {subject} sits {hop} referrer hops out, past the \
         {REFERRER_DEPTH_CEILING}-hop ceiling"
    )))
}

/// Refuse a subject naming more referrers than [`REFERRER_COUNT_CEILING`]
/// (C-061).
fn within_referrer_budget(subject: &Digest, count: usize) -> Result<(), CopyError> {
    if count <= REFERRER_COUNT_CEILING {
        return Ok(());
    }
    Err(CopyError::ReferrerBudgetExceeded {
        subject: subject.clone(),
        limit: REFERRER_COUNT_CEILING,
    })
}

/// The HTTP status behind a registry error, where the fork kept one.
///
/// Structural, never a string match on the rendered message (ERR-13): the
/// wording of an `OciDistributionError` is not a contract and a `contains`
/// against it would silently stop classifying on the next fork bump. Two of
/// the three shapes carry a status — `ServerError` its own `code`, a
/// `reqwest`-wrapped `RequestError` its response's — and `RegistryError`
/// carries an OCI error envelope with no status at all, which is why this
/// answers `Option`.
fn registry_status(error: &OciDistributionError) -> Option<u16> {
    match error {
        OciDistributionError::ServerError { code, .. } => Some(*code),
        OciDistributionError::RequestError(source) => source.status().map(|status| status.as_u16()),
        _ => None,
    }
}

/// Read a destination referrers probe into a routing decision (C-063).
///
/// `Ok(None)` is the fork's own capability verdict — it checks the status
/// before reading the body, which is what makes it usable against `registry:2`
/// at all: that registry answers the referrers route with a plain-text
/// `404 page not found`, so anything asserting on a parsed body would report a
/// malformed index instead of a missing endpoint.
///
/// **404 and 405 both mean "no Referrers API"** (C-063). The spec names 404,
/// but a registry that routes `/v2/<name>/referrers/…` to a handler it does
/// not implement answers `405 Method Not Allowed`, and reading that as a
/// transport fault would abort a run against a registry the fallback route
/// serves perfectly well.
///
/// Any other failure aborts the run rather than guessing: routing a carried
/// referrer down the wrong branch either publishes a fallback index a
/// Referrers-API registry will contradict, or publishes nothing discoverable
/// at all.
fn referrer_verdict(
    probe: Result<Option<ocx_lib::oci::ImageIndex>, OciDistributionError>,
) -> Result<ReferrerDestination, CopyError> {
    match probe {
        Ok(Some(_)) => Ok(ReferrerDestination::Supported),
        Ok(None) => Ok(ReferrerDestination::Fallback),
        Err(error) if matches!(registry_status(&error), Some(404 | 405)) => Ok(ReferrerDestination::Fallback),
        Err(error) => Err(CopyError::Abort(MirrorError::TargetError(format!(
            "destination referrers probe did not answer authoritatively: {error}"
        )))),
    }
}

/// Classify a failed manifest PUT (C-063).
///
/// `subject` is the whole discriminator. ECR answers a subject-bearing
/// manifest PUT with `405 Method Not Allowed`, and that refusal is about the
/// *shape* — it applies to every package in the run, so it fails the package
/// and lets the rest of the mirror proceed. The same status on an ordinary
/// manifest is an ordinary rejection of those bytes.
///
/// The status is read off the error's structure by [`registry_status`], never
/// off its rendering: an `OciDistributionError`'s wording is not a contract,
/// and a `contains("405")` would keep passing while it stopped classifying.
fn push_failure(
    destination_reference: &Reference,
    subject: SubjectCarriage,
    error: &OciDistributionError,
) -> CopyError {
    match (subject, registry_status(error)) {
        (SubjectCarriage::Present, Some(status @ 405)) => CopyError::SubjectRejected {
            registry: destination_reference.registry().to_string(),
            status,
        },
        _ => CopyError::PushRejected(format!("manifest at {destination_reference}: {error}")),
    }
}

/// The cosign sidecar tag for `subject` and one suffix (C-062).
fn sidecar_tag(subject: &Digest, suffix: &str) -> String {
    format!("{}{suffix}", ocx_lib::package::tag::referrer_fallback_tag(subject))
}

/// Convert a source referrers-listing entry into the descriptor the
/// destination's fallback index has to carry.
///
/// Taken from the source's own listing rather than re-derived from the pushed
/// bytes: `artifactType` and the annotations are what a reader filters on, and
/// a fallback entry that drops them is the defect sigstore/cosign#4641 reports
/// in cosign's own fallback write. `platform` is deliberately not carried —
/// `Descriptor` has no such field, and a referrer is not platform-specific.
fn referrer_descriptor(entry: &ImageIndexEntry) -> Descriptor {
    Descriptor {
        media_type: entry.media_type.clone(),
        digest: entry.digest.clone(),
        size: entry.size,
        urls: None,
        artifact_type: entry.artifact_type.clone(),
        annotations: entry.annotations.clone(),
    }
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

/// Presence-probe a destination blob, **retrying a rate limit** rather than
/// aborting the whole run on a transient 429.
///
/// A copy fans presence probes out `CHILD_FANOUT_CEILING × BLOB_FANOUT_CEILING`
/// wide per package, and `max_packages` of those run at once, so a busy
/// destination is likelier to throttle a HEAD than the transfers beside it —
/// while [`probe_verdict`] turns any non-404 answer, 429 included, into a
/// whole-run [`MirrorError::TargetError`]. Routing the probe through the same
/// [`retry_while`]/[`is_rate_limited`] ladder the pull and push paths already
/// use is what keeps one throttled probe from ending the run; a 429 that
/// survives the budget still aborts, which is correct — the destination is
/// saturated.
async fn probe_blob_present(
    context: &CopyContext,
    reference: &Reference,
    digest_string: &str,
) -> Result<Option<u64>, CopyError> {
    let probe = retry_while(context.max_retries, is_rate_limited, || {
        context.destination_client.fetch_blob_size(reference, digest_string)
    })
    .await;
    probe_verdict(probe)
}

/// Whether a registry error is a rate limit worth retrying.
///
/// Three shapes, because the fork maps 429 three ways: `extract_location_header`
/// (every push path) produces `ServerError { code: 429 }`;
/// `validate_registry_response` turns a 4xx carrying an OCI error envelope into
/// `RegistryError`, which has no status code of its own; and `head_blob_response`
/// — the destination presence probe ([`probe_blob_present`]) — turns a non-404
/// HEAD into `Err(error_for_status_ref().into())`, i.e. a `reqwest`-wrapped
/// `RequestError` carrying the status. Omitting the third arm left the probe's
/// retry ladder inert, so a single throttled HEAD aborted the whole run.
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
        OciDistributionError::RequestError(source) => source.status() == Some(reqwest::StatusCode::TOO_MANY_REQUESTS),
        _ => false,
    }
}

/// Whether a transport-level destination error is worth retrying.
///
/// The [`OciTransport`] surface is the one destination seam that does **not**
/// hand back an `OciDistributionError`, so [`is_rate_limited`] cannot read it:
/// `ClientError` collapses "429 / 502 / 503 / 504 / timed out" into one
/// [`ClientError::RegistryTransient`] variant whose own documentation says
/// "run the same command again". Matching that variant is therefore the only
/// structural spelling available — a status match would need a `contains` on a
/// boxed source error, which is exactly what [`registry_status`] exists to
/// avoid.
///
/// Wider than [`is_rate_limited`] by the three 5xx codes, and deliberately so:
/// without it a destination hiccup on a sidecar conflict check aborts the whole
/// run, while the identical hiccup on the blob probe beside it costs
/// `max_retries` and continues.
fn is_transient(error: &ClientError) -> bool {
    matches!(error, ClientError::RegistryTransient(_))
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
/// The referrer sweep and the cosign sidecar sweep run per copied manifest and
/// **after** the push (C-060): a referrer's `subject` names this manifest, so
/// publishing the referrer first would leave the destination holding a
/// signature over content it does not have — the state a registry with
/// referrer garbage collection is entitled to delete.
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
    copy_manifest_tree_at(
        source_reference,
        destination_reference,
        digest,
        context,
        0,
        Carriage::root(),
    )
    .await
}

/// [`copy_manifest_tree`] with the recursion depth and the signature carriage
/// threaded through.
///
/// Split rather than adding parameters to the public entry point: `depth` is
/// an implementation detail of the walk, and a caller passing anything but 0
/// would silently move the ceiling it exists to enforce — the same argument
/// applies to [`Carriage`], where a caller passing `hop: None` would silently
/// disable the sweep for a whole package.
async fn copy_manifest_tree_at(
    source_reference: &Reference,
    destination_reference: &Reference,
    digest: &Digest,
    context: &CopyContext,
    depth: usize,
    carriage: Carriage,
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

    let mut stats = CopyStats {
        manifests: 1,
        ..CopyStats::default()
    };

    match child_references(&manifest)? {
        ChildReferences::Manifests(children) => {
            // An image index's children are the platform manifests, and copying
            // them one after another made a package's cost the *sum* of nine
            // round-trip-bound walks. They share nothing that has to be
            // serialised: each fetches its own bytes, and the one resource with
            // a ceiling — a blob body held in memory — is bounded by the
            // run-scoped `blob_semaphore` whatever the width here.
            //
            // `buffered`, not `buffer_unordered`: the widths are small and
            // in-order yielding makes *which* error `try_collect` returns a
            // property of the index, not of the network.
            //
            // Width collapses to 1 below the top level, which is what keeps the
            // bound flat: a hostile chain of nested indexes would otherwise
            // multiply it by itself once per level, `MANIFEST_DEPTH_CEILING`
            // times. Real indexes are one level deep.
            let width = if depth == 0 { CHILD_FANOUT_CEILING } else { 1 };
            let copied: Vec<CopyStats> =
                futures::stream::iter(children.into_iter().map(|(child_digest, _size)| async move {
                    let child_source = addressed(source_reference, &child_digest);
                    let child_destination = addressed(destination_reference, &child_digest);
                    let (child_stats, _child_bytes) = Box::pin(copy_manifest_tree_at(
                        &child_source,
                        &child_destination,
                        &child_digest,
                        context,
                        depth + 1,
                        carriage.child(),
                    ))
                    .await?;
                    Ok::<_, CopyError>(child_stats)
                }))
                .buffered(width)
                .try_collect()
                .await?;
            for child_stats in copied {
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

    push_manifest(
        destination_reference,
        &bytes,
        manifest.content_type(),
        context,
        carriage.subject,
    )
    .await?;
    if context.canonical_tags {
        push_canonical_tag(
            destination_reference,
            digest,
            &bytes,
            manifest.content_type(),
            context,
            carriage.subject,
        )
        .await?;
    }

    // C-060: after the push, never before. Both sweeps are keyed on this
    // manifest's digest, which the destination now holds.
    if let Some(hop) = carriage.hop {
        stats.merge(copy_referrers(source_reference, destination_reference, digest, context, depth, hop).await?);
        stats.merge(copy_sidecars(source_reference, destination_reference, digest, context, depth).await?);
    }

    Ok((stats, bytes))
}

/// Tag one copied manifest with its own digest — `sha256.<hex>`, the **frozen
/// legacy keep-tag form**.
///
/// ocx 0.6.0 renamed the concept to "keep tag" and moved its own writes to
/// `__ocx.keep.<alg>-<hex>`; it no longer writes this spelling. The mirror
/// keeps writing it deliberately. `ocx_lib`'s `Tag::LegacyKeep` is a permanent
/// read arm — "the arm stays because already-published repositories carry these
/// tags" — so `Tag::is_reserved` classifies both spellings as reserved and both
/// are filtered out of index roots and version listings identically. The tag's
/// only job is to keep a manifest referenced against a registry GC, which
/// either spelling does. Switching would add a *second* tag to every manifest
/// on every already-mirrored destination (nothing here deletes the old one) for
/// no behavioural gain, so the spelling stays put.
///
/// **The deletion safety net, mirrored.** ocx tags every manifest it publishes
/// after its own digest so that deleting a rolling or cascade tag can never
/// orphan a digest a lock pins (`adr_index_indirection.md` Decision E). Those
/// tags are *reserved*, so they are filtered out of the index root's `tags{}`
/// and no `tags{}` walk reaches them — which is exactly why a mirror that
/// copies the published tag set faithfully still ends up with none of them.
/// `ghcr.io/ocx-sh/ocx/cli` carries 54 of them against 21 version tags.
///
/// Without this the destination holds every platform manifest untagged,
/// reachable only through the index that names it, and a mirror is *less*
/// protected than the upstream it copies — while being the fleet's only source
/// under `rewrite_pointers: true`.
///
/// A `.` separator because an OCI tag cannot contain `:`. The bytes pushed are
/// the same verified bytes, so the tag and the digest reference name one
/// manifest, not two copies of it.
async fn push_canonical_tag(
    destination_reference: &Reference,
    digest: &Digest,
    bytes: &[u8],
    content_type: &str,
    context: &CopyContext,
    subject: SubjectCarriage,
) -> Result<(), CopyError> {
    let (algorithm, hex) = digest.parts();
    let canonical = Reference::with_tag(
        destination_reference.registry().to_string(),
        destination_reference.repository().to_string(),
        format!("{algorithm}.{hex}"),
    );
    // The same bytes the digest-addressed push already carried, so the subject
    // reading is the same one: a referrer's keep tag is still a subject-bearing
    // PUT, and a destination that refused the first refuses this too.
    push_manifest(&canonical, bytes, content_type, context, subject).await
}

/// Push one manifest's bytes verbatim, retrying only a rate limit.
///
/// `subject` decides how a `405` is read (C-063) and nothing else: a
/// destination that refuses `subject`-bearing manifests is not refusing *these
/// bytes*, it is refusing the whole shape, and collapsing that into
/// [`CopyError::PushRejected`] would report a package failure for content that
/// copied.
async fn push_manifest(
    destination_reference: &Reference,
    bytes: &[u8],
    content_type: &str,
    context: &CopyContext,
    subject: SubjectCarriage,
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
    .map_err(|error| push_failure(destination_reference, subject, &error))?;

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
/// Order: destination probe (via [`probe_blob_present`], which retries a 429)
/// → hit ⇒ [`BlobOutcome::Skipped`]; an authoritative 404 ⇒ continue; **any
/// other outcome (401, 503, timeout, connection reset, or a 429 that outlasts
/// the retry budget) ⇒ [`MirrorError::TargetError`] aborting the whole run**.
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

    // A digest this run already confirmed in *this* repository needs no second
    // question: the destination is append-only, so an answer of "present" does
    // not expire mid-run. Every tag of a package re-declares the same config
    // blob, so without this the query count is tags × blobs rather than
    // distinct blobs — one real package spent 249 of its 431 probes re-asking
    // about a single digest. `debug!`, not `info!`: the first probe for each
    // digest still logs, and the repeats are the noise this removes.
    if confirmed_here(context, digest, &destination_repository).await {
        tracing::debug!("blob {digest} confirmed in {destination_repository} earlier this run");
        return Ok(BlobOutcome::Skipped);
    }

    if probe_blob_present(context, destination_reference, &digest_string)
        .await?
        .is_some()
    {
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

/// List one subject's referrers **at the source** (C-061).
///
/// Goes through the fork's `Client::pull_referrers_native`, **never raw
/// HTTP** — and then owns the referrers **tag-schema** fallback itself. ocx
/// v0.6.0 dropped `Client::pull_referrers`, which bundled the two: the native
/// call now answers `None` for "this registry has no Referrers API" instead of
/// degrading it to an empty index, and the consumer applies its own bounds and
/// error policy to the fallback index. Reading `None` as "no referrers" would
/// silently drop every signature on a source registry without the API, so it
/// is [`fallback_referrers`] that answers, not this call.
///
/// **Pagination is deliberately not followed.** The distribution spec lets a
/// registry paginate this listing with a `Link` header, and the fork reads the
/// first page only. Every registry surveyed answers a subject's referrers in
/// one page — the count is single digits — and both real bounds
/// (`MAX_REFERRERS_DESCRIPTORS` upstream, [`REFERRER_COUNT_CEILING`] here) sit
/// far below any page size. A reader assuming completeness here would be
/// wrong for a subject with thousands of referrers, which is why it is said
/// out loud rather than left to the absence of a loop.
///
/// # Errors
///
/// [`CopyError::SourceUnavailable`] when neither route answers;
/// [`CopyError::MalformedManifest`] when the fallback tag holds something that
/// is not an image index.
async fn source_referrers(
    source_reference: &Reference,
    digest: &Digest,
    context: &CopyContext,
) -> Result<Vec<ImageIndexEntry>, CopyError> {
    let subject = addressed(source_reference, digest);
    let native = context
        .source_client
        .pull_referrers_native(&subject, None)
        .await
        .map_err(|error| CopyError::SourceUnavailable(format!("referrers for {digest}: {error}")))?;

    match native {
        Some(index) => Ok(index.manifests),
        // `None` is the capability verdict — the registry answered 404 on the
        // native endpoint — never "this subject has no referrers".
        None => fallback_referrers(source_reference, digest, context).await,
    }
}

/// The OCI referrers tag-schema fallback, for a source registry with no
/// Referrers API.
///
/// Reads the index parked at `<algorithm>-<encoded truncated to 64>` in the
/// subject's own repository. Spec step 3 — an absent tag *is* an empty index,
/// and that is the only path here that yields one: every other refusal is an
/// error, because "there is nothing there" and "I could not read what is
/// there" are the two answers this gate must never confuse.
///
/// Read through `Index::fetch_manifest_raw_bytes`, the same SSRF-guarded seam
/// the manifest walk uses, so the fallback cannot reach a host the pre-flight
/// did not approve — and so [`MANIFEST_FETCH_CEILING`] applies to it exactly
/// as it applies to a copied manifest.
///
/// # Errors
///
/// [`CopyError::SourceUnavailable`] when the tag cannot be read;
/// [`CopyError::MalformedManifest`] when it is past the ceiling, or holds
/// something that is not an image index (spec step 2).
async fn fallback_referrers(
    source_reference: &Reference,
    digest: &Digest,
    context: &CopyContext,
) -> Result<Vec<ImageIndexEntry>, CopyError> {
    let tag = ocx_lib::package::tag::referrer_fallback_tag(digest);
    let identifier = source_identifier(source_reference).clone_with_tag(tag.clone());
    let fetched = context
        .source_index
        .fetch_manifest_raw_bytes(&identifier)
        .await
        .map_err(|error| CopyError::SourceUnavailable(format!("referrers fallback {identifier}: {error}")))?;
    let Some((bytes, _reported, manifest)) = fetched else {
        return Ok(Vec::new());
    };

    if bytes.len() > MANIFEST_FETCH_CEILING {
        return Err(CopyError::MalformedManifest(format!(
            "referrers fallback index {tag} is {} bytes, past the {MANIFEST_FETCH_CEILING}-byte ceiling",
            bytes.len()
        )));
    }

    match manifest {
        Manifest::ImageIndex(index) => Ok(index.manifests),
        Manifest::Image(_) => Err(CopyError::MalformedManifest(format!(
            "referrers fallback tag {tag} holds an image manifest, not an image index"
        ))),
    }
}

/// Probe the destination's Referrers API once per run (C-063).
///
/// The probe is a read of the destination, so its failure class is the same as
/// [`probe_blob_present`]'s: a rate limit is retried, and an answer that is
/// neither a listing nor a capability verdict aborts the run
/// ([`referrer_verdict`] owns that decision).
///
/// Addressed at the subject the caller is copying rather than at some fixed
/// probe digest: the endpoint is per-repository, and a registry may well
/// answer 404 for a repository that does not exist yet — which is the same
/// verdict the fallback route wants anyway.
async fn referrer_destination(
    destination_reference: &Reference,
    digest: &Digest,
    context: &CopyContext,
) -> Result<ReferrerDestination, CopyError> {
    context
        .referrer_destination
        .get_or_try_init(|| async {
            let subject = addressed(destination_reference, digest);
            let probe = retry_while(context.max_retries, is_rate_limited, || {
                context.destination_client.pull_referrers_native(&subject, None)
            })
            .await;
            referrer_verdict(probe)
        })
        .await
        .copied()
}

/// Carry every referrer of one copied manifest across (C-061, C-063).
///
/// Runs after the subject's own push (C-060). Each referrer is copied by the
/// ordinary manifest walk — same digest verification, same blob ladder — under
/// a [`Carriage`] one hop further out, so a referrer's own referrers are swept
/// too, bounded by [`REFERRER_DEPTH_CEILING`].
///
/// Where the destination has no Referrers API, the carried referrer is
/// additionally appended to the destination's tag-schema fallback index. That
/// index is a **mutable tag**: anyone with push access to the repository can
/// rewrite it, and the read-modify-write append is optimistic with a bounded
/// attempt budget. Both are residual risks inherited from ocx's own fallback
/// implementation (`adr_signing.md` Amendment 10) rather than introduced here;
/// the alternative — not writing it — is a mirror whose signatures are
/// undiscoverable.
///
/// # Errors
///
/// [`CopyError::ReferrerBudgetExceeded`] past [`REFERRER_COUNT_CEILING`];
/// [`CopyError::SubjectRejected`] when the destination refuses a
/// subject-bearing manifest; every other [`CopyError`] the walk itself raises.
async fn copy_referrers(
    source_reference: &Reference,
    destination_reference: &Reference,
    digest: &Digest,
    context: &CopyContext,
    depth: usize,
    hop: usize,
) -> Result<CopyStats, CopyError> {
    let entries = source_referrers(source_reference, digest, context).await?;
    if entries.is_empty() {
        // Before the capability probe, so an unsigned mirror never asks the
        // destination a question about a feature it does not use.
        return Ok(CopyStats::default());
    }
    within_referrer_budget(digest, entries.len())?;
    let referrer_hop = hop.saturating_add(1);
    within_referrer_depth(digest, referrer_hop)?;

    let route = referrer_destination(destination_reference, digest, context).await?;
    let destination_repository = destination_reference.repository().to_string();

    let mut stats = CopyStats::default();
    // Sequential, unlike the blob and child fan-outs: a subject carries single
    // digits of referrers, and under `ReferrerDestination::Fallback` every one
    // of them appends to the *same* mutable tag — a fan-out would have this run
    // race itself for an index whose write is already optimistic.
    for entry in &entries {
        let (referrer_digest, _size) = descriptor_target(&entry.digest, entry.size)?;
        // The claim is what makes a diamond copy once (C-061): two subjects
        // naming one referrer, or one referrer reached through two tags of the
        // same package.
        if !claim_referrer(context, &destination_repository, &referrer_digest).await {
            continue;
        }
        // The claim is released on every error path, so it records only a
        // *completed* carry. A claim outliving its own failure would let a
        // later walk over the same referrer skip it and report success for a
        // referrer neither copied nor indexed.
        let attempt = async {
            let (carried, _bytes) = Box::pin(copy_manifest_tree_at(
                &addressed(source_reference, &referrer_digest),
                &addressed(destination_reference, &referrer_digest),
                &referrer_digest,
                context,
                depth,
                Carriage::referrer(referrer_hop),
            ))
            .await?;
            if route == ReferrerDestination::Fallback {
                append_fallback(destination_reference, digest, &referrer_descriptor(entry), context).await?;
            }
            Ok::<CopyStats, CopyError>(carried)
        }
        .await;

        let carried = match attempt {
            Ok(carried) => carried,
            Err(error) => {
                release_referrer(context, &destination_repository, &referrer_digest).await;
                return Err(error);
            }
        };
        stats.merge(carried);
        stats.referrers_copied += 1;
    }
    Ok(stats)
}

/// Append one carried referrer's descriptor to the destination's referrers
/// tag-schema fallback index (C-063).
///
/// The outcome is discarded because both of them are success: `Written` is the
/// append, and `AlreadyPresent` is a re-run finding its own earlier descriptor,
/// which is the ordinary idempotent case for a mirror.
///
/// # Errors
///
/// [`CopyError::PushRejected`] — the append is a write, and C-040 fails a
/// package on a failed write rather than aborting the run.
async fn append_fallback(
    destination_reference: &Reference,
    subject: &Digest,
    descriptor: &Descriptor,
    context: &CopyContext,
) -> Result<(), CopyError> {
    // Through the same retry ladder every other destination write uses
    // ([`push_manifest`]): the append's own loop retries a *concurrent writer*,
    // never a rate limit, so without this a 429 fails the package where the
    // manifest push beside it would have waited.
    retry_while(context.max_retries, is_transient, || {
        context
            .destination_transport
            .append_referrer_fallback_index(destination_reference, subject, descriptor)
    })
    .await
    .map(|_outcome| ())
    .map_err(|error| {
        CopyError::PushRejected(format!(
            "referrers fallback index for {subject} at {destination_reference}: {error}"
        ))
    })
}

/// Carry every cosign sidecar tag parked beside one copied manifest (C-062).
///
/// The pre-OCI-1.1 scheme: a signature at `sha256-<hex>.sig` in the subject's
/// own repository, linked to its subject by the tag name alone. An absent tag
/// is a no-op — most manifests have none.
///
/// A destination already holding that tag **at a different digest** is skipped
/// and counted in [`CopyStats::sidecar_conflicts`], never overwritten: the tag
/// is mutable and the local one may be a signature the operator produced,
/// while an upstream that re-signed is not evidence the local signature is
/// wrong. The package still succeeds — a conflicting sidecar is a fact to
/// report, not a copy failure.
///
/// # Errors
///
/// [`CopyError`] from the walk that copies a sidecar's own content.
async fn copy_sidecars(
    source_reference: &Reference,
    destination_reference: &Reference,
    digest: &Digest,
    context: &CopyContext,
    depth: usize,
) -> Result<CopyStats, CopyError> {
    let mut stats = CopyStats::default();
    for suffix in SIDECAR_SUFFIXES {
        let tag = sidecar_tag(digest, suffix);
        let Some(content) = source_sidecar(source_reference, &tag, context).await? else {
            continue;
        };
        let destination_tag = Reference::with_tag(
            destination_reference.registry().to_string(),
            destination_reference.repository().to_string(),
            tag.clone(),
        );
        if let Some(held) = destination_tag_digest(&destination_tag, context).await?
            && held != content
        {
            // `{tag:?}` because the tag is derived from an upstream digest and
            // reaches a log an operator reads (CWE-117).
            tracing::warn!("skipping sidecar {tag:?}: the destination already holds it at {held}, not {content}");
            stats.sidecar_conflicts += 1;
            continue;
        }
        let (carried, _bytes) = Box::pin(copy_manifest_tree_at(
            source_reference,
            &destination_tag,
            &content,
            context,
            depth,
            Carriage::sidecar(),
        ))
        .await?;
        stats.merge(carried);
        stats.sidecars_copied += 1;
    }
    Ok(stats)
}

/// The digest one cosign sidecar tag resolves to at the source, or `None` when
/// the tag is absent (C-062).
///
/// An absent tag is the overwhelmingly common case — most manifests carry no
/// sidecar at all — so it is an `Ok(None)`, never an error.
///
/// # Errors
///
/// [`CopyError::SourceUnavailable`] when the source cannot answer.
async fn source_sidecar(
    source_reference: &Reference,
    tag: &str,
    context: &CopyContext,
) -> Result<Option<Digest>, CopyError> {
    let identifier = source_identifier(source_reference).clone_with_tag(tag);
    let fetched = context
        .source_index
        .fetch_manifest_raw_bytes(&identifier)
        .await
        .map_err(|error| CopyError::SourceUnavailable(format!("sidecar {identifier}: {error}")))?;
    Ok(fetched.map(|(_bytes, content, _manifest)| content))
}

/// The digest the destination currently holds at `reference`, or `None` when
/// the tag is absent (C-062).
///
/// Through the transport rather than the raw client because the transport
/// already classifies a not-found into its own variant: reading "absent" out
/// of a raw `OciDistributionError` would mean re-deriving that from a status
/// code this crate does not own.
///
/// # Errors
///
/// [`CopyError::Abort`] — this is a destination read, and an answer that is
/// neither a digest nor an authoritative not-found leaves the run unable to
/// tell a conflict from a first publish, exactly as [`probe_verdict`] does for
/// a blob.
async fn destination_tag_digest(reference: &Reference, context: &CopyContext) -> Result<Option<Digest>, CopyError> {
    // Retried exactly as [`probe_blob_present`] is, and for the same reason:
    // this is the sibling destination read, and an unretried one turns an
    // ordinary 429 into a whole-run abort.
    let answer = retry_while(context.max_retries, is_transient, || {
        context.destination_transport.fetch_manifest_digest(reference)
    })
    .await;
    match answer {
        Ok(digest) => Digest::try_from(digest.as_str()).map(Some).map_err(|_| {
            CopyError::Abort(MirrorError::TargetError(format!(
                "destination reported an unparseable digest for {reference}"
            )))
        }),
        Err(ClientError::ManifestNotFound(_) | ClientError::RepositoryNotFound(_)) => Ok(None),
        Err(error) => Err(CopyError::Abort(MirrorError::TargetError(format!(
            "destination did not answer authoritatively for {reference}: {error}"
        )))),
    }
}

/// How a copied manifest is addressed at the destination — `publish_tags:`,
/// resolved once (C-011's shape, applied to a different key).
///
/// Reading the spec's bool at every call site would put the branch in three
/// places; this puts it in [`TagPolicy::address`] and lets everything else ask
/// for "the destination reference for this content".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TagPolicy {
    /// Create the upstream tag at the destination. The default, and what keeps
    /// the content referenced against a registry that collects untagged
    /// manifests.
    Publish,
    /// Push by digest and create no tag. The mirrored index still carries the
    /// full tag set — it resolves tag → `content` digest itself.
    DigestOnly,
}

impl TagPolicy {
    /// The destination reference for one tag's content.
    ///
    /// The fork addresses a manifest `PUT` by digest whenever the reference
    /// carries one, so this *is* the whole mechanism: a tag-carrying reference
    /// creates the tag, a digest-carrying one does not.
    pub fn address(self, registry: &str, repository: &str, tag: &str, content: &Digest) -> Reference {
        match self {
            TagPolicy::Publish => Reference::with_tag(registry.to_string(), repository.to_string(), tag.to_string()),
            TagPolicy::DigestOnly => {
                Reference::with_digest(registry.to_string(), repository.to_string(), content.to_string())
            }
        }
    }
}

/// Point one more tag at content this run already copied into the same
/// repository.
///
/// The second and later tags of a package routinely name a manifest an earlier
/// tag already brought over — 21 tags over 9 distinct indexes is an ordinary
/// ocx package, because every version carries its cascade tags. Walking the
/// tree again would re-fetch and re-push every platform manifest to learn
/// nothing; the tag itself is one `PUT` of bytes already in hand.
///
/// The media type is re-derived from those bytes rather than threaded through
/// the caller: it is a field of the document, and a manifest that parsed once
/// parses again.
///
/// # Errors
///
/// [`CopyError`] when the destination rejects the push or the cached bytes are
/// not a manifest.
pub async fn tag_manifest(
    destination_reference: &Reference,
    bytes: &[u8],
    context: &CopyContext,
) -> Result<(), CopyError> {
    let manifest: Manifest = serde_json::from_slice(bytes)
        .map_err(|error| CopyError::MalformedManifest(format!("cached manifest is unreadable: {error}")))?;
    // `Absent`: this points one more tag at a package's own dispatch object,
    // which is never a referrer. A referrer is addressed by digest and never
    // reaches this path.
    push_manifest(
        destination_reference,
        bytes,
        manifest.content_type(),
        context,
        SubjectCarriage::Absent,
    )
    .await
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
    policy: TagPolicy,
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
    let destination = policy.address(
        destination_reference.registry(),
        destination_reference.repository(),
        DESCRIPTION_TAG,
        &digest,
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
/// **Referrers and cosign sidecars are measured** (C-065), under the same
/// carriage rules the copy walk uses, because a copy moves them and an
/// estimate that omitted them would understate every signed package. Their
/// *presence* is never a failure — that refusal was v1's, and removing it is
/// the point of this work package.
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
    missing_descriptors_at(
        source_reference,
        destination_reference,
        digest,
        context,
        0,
        Carriage::root(),
    )
    .await
}

/// [`missing_descriptors`] with the recursion depth and signature carriage
/// threaded through.
///
/// The `--dry-run` walk recurses exactly as the copy walk does, so it needs the
/// same ceilings: a hostile chain would exhaust the stack before a single byte
/// was ever going to move, which is the one path an operator reaches for
/// *because* it is supposed to be harmless.
async fn missing_descriptors_at(
    source_reference: &Reference,
    destination_reference: &Reference,
    digest: &Digest,
    context: &CopyContext,
    depth: usize,
    carriage: Carriage,
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
                    carriage.child(),
                ))
                .await?;
                missing.extend(nested);
            }
        }
        ChildReferences::Blobs(blobs) => {
            for (blob_digest, size) in blobs {
                if probe_blob_present(context, destination_reference, &blob_digest.to_string())
                    .await?
                    .is_none()
                {
                    missing.push((blob_digest, size));
                }
            }
        }
    }

    if let Some(hop) = carriage.hop {
        missing.extend(
            missing_signature_descriptors(source_reference, destination_reference, digest, context, depth, hop).await?,
        );
    }
    Ok(missing)
}

/// The referrer and sidecar half of [`missing_descriptors`] (C-065).
///
/// Split out rather than inlined so the walk above reads as one shape — fetch,
/// verify, decompose, probe — with the signature surface named once beside it.
///
/// # Errors
///
/// [`CopyError`] exactly as [`missing_descriptors_at`] raises it.
async fn missing_signature_descriptors(
    source_reference: &Reference,
    destination_reference: &Reference,
    digest: &Digest,
    context: &CopyContext,
    depth: usize,
    hop: usize,
) -> Result<Vec<(Digest, u64)>, CopyError> {
    let mut missing = Vec::new();

    let entries = source_referrers(source_reference, digest, context).await?;
    if !entries.is_empty() {
        within_referrer_budget(digest, entries.len())?;
        let referrer_hop = hop.saturating_add(1);
        within_referrer_depth(digest, referrer_hop)?;
        for entry in &entries {
            let (referrer_digest, _size) = descriptor_target(&entry.digest, entry.size)?;
            missing.extend(
                Box::pin(missing_descriptors_at(
                    &addressed(source_reference, &referrer_digest),
                    &addressed(destination_reference, &referrer_digest),
                    &referrer_digest,
                    context,
                    depth,
                    Carriage::referrer(referrer_hop),
                ))
                .await?,
            );
        }
    }

    for suffix in SIDECAR_SUFFIXES {
        let tag = sidecar_tag(digest, suffix);
        let Some(content) = source_sidecar(source_reference, &tag, context).await? else {
            continue;
        };
        // The conflict check is a *copy* decision, not a measurement one: a
        // conflicting sidecar moves no bytes either way, and asking would cost
        // one destination read per suffix per manifest on a path whose whole
        // promise is that it is cheap.
        missing.extend(
            Box::pin(missing_descriptors_at(
                source_reference,
                &Reference::with_tag(
                    destination_reference.registry().to_string(),
                    destination_reference.repository().to_string(),
                    tag,
                ),
                &content,
                context,
                depth,
                Carriage::sidecar(),
            ))
            .await?,
        );
    }

    Ok(missing)
}

/// Whether this run already put `digest` in `repository` — present, mounted or
/// uploaded.
///
/// Reads the same map [`record_location`] writes, and answers `false` for a
/// digest recorded against a *different* repository: that entry exists for
/// `mount_blob`, and a blob in some other repository says nothing about this
/// one.
async fn confirmed_here(context: &CopyContext, digest: &Digest, repository: &str) -> bool {
    context
        .mounted_from
        .lock()
        .await
        .get(digest)
        .is_some_and(|recorded| recorded == repository)
}

/// Claim `digest` as this run's carry of a referrer into `repository`, and
/// answer whether the claim was new (C-061).
///
/// One lock acquisition rather than a check followed by an insert: the child
/// manifests of an index sweep concurrently, and a shared referrer would
/// otherwise be claimed twice between the two calls — copying it twice is
/// harmless (the push is idempotent) but counting it twice is a wrong report.
async fn claim_referrer(context: &CopyContext, repository: &str, digest: &Digest) -> bool {
    context
        .carried_referrers
        .lock()
        .await
        .insert((repository.to_string(), digest.clone()))
}

/// Give back a claim [`claim_referrer`] granted, because the carry it stood
/// for did not complete.
///
/// The set means "this run has carried that referrer into that repository",
/// and a failed attempt carried nothing. Leaving the claim behind would make
/// the next walk reaching the same digest `continue` past it and finish
/// green — a referrer missing from the destination and from the fallback
/// index, with nothing in the report saying so.
async fn release_referrer(context: &CopyContext, repository: &str, digest: &Digest) {
    context
        .carried_referrers
        .lock()
        .await
        .remove(&(repository.to_string(), digest.clone()));
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
