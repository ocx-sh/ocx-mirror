// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! The source side: the guarded HTTP client and the four index-tree fetches
//! (`config.json`, `c/index.json`, `p/<ns>/<pkg>.json`, and the README/logo
//! objects a root's `desc` names under `p/<ns>/<pkg>/o/`), plus the SSRF check
//! on a root's physical host.
//!
//! Every byte read here is foreign data. Each fetch returns the **raw bytes**
//! alongside the parsed shape, because the mirror re-serves the bytes verbatim
//! and hashes them — a parse → re-serialize round trip would drop fields this
//! ocx does not model. `config.json` is the exception and the exception is
//! deliberate: its bytes are read to gate `format_version` and **never
//! written**, because the mirror's own `config.json` declares *our* tree's name
//! grammar and a hostile `name_segments` copied from upstream would change how
//! every client in the fleet resolves names against us (C-031).
//! Contracts C-016…C-020 of `plan_registry_mirror_sync.md`.
//!
//! **Foreign strings are `{:?}`-formatted wherever they reach a message**
//! (CWE-117): a catalog key carrying a newline forges log lines in the CI
//! output an operator reads, and `\u{202e}` or an ANSI escape reaches their
//! terminal. The pre-validation sites are the exploitable ones — those keys
//! have passed no charset guard yet.
//!
//! **Two network-trust mechanisms, because there are two trust situations
//! (C-016).** The index base URL is operator-authored spec config: it gets a
//! `guard_destination` pre-flight plus `redirect::Policy::none()`, both
//! folded into [`build_source_index_client`] so no client can exist that was
//! not validated first. A root's `repository` pointer is **foreign data** — an
//! upstream index naming the host to dial — so it gets the full treatment at
//! [`validate_root_host`], before any registry request.
//!
//! **The validated answer is the answer that gets dialled.** The pre-flight
//! returns the addresses it judged and [`index_client`] pins them with
//! `ClientBuilder::resolve_to_addrs`; the operator authored the *hostname*, not
//! the source's nameserver, so discarding the answer and letting reqwest
//! resolve a second time at connect leaves the whole SSRF window open — a
//! source authoritative for its own index hostname serves a short-TTL record
//! and flips it to `169.254.169.254` between validation and connect (CWE-918
//! via CWE-367). `redirect::Policy::none()` does not touch that path.
//! Beneath the pin sits `GuardedResolver` — the registry half's resolve →
//! validate → pin hook (C-046) — as the client's `dns_resolver`: reqwest
//! consults the override map first, so a pinned name never reaches it, and
//! every name that does (a proxied route has no pin; the resolver then admits
//! the proxy host and nothing else) meets the floor again at connect. Two
//! layers, so the proxied route no longer rests on the pre-flight's matcher
//! agreeing with reqwest's alone.
//!
//! **Both pre-flights are route-aware** (`guard_destination`, ocx 0.6.1) and
//! take the [`ProxyRules`] to judge under, so a test can drive either route
//! without the ambient proxy environment. Behind a configured HTTP proxy the
//! process resolves and dials only the proxy — the destination is literal text
//! in a `CONNECT` line — so there is no address to judge or pin, and a runner
//! whose resolver cannot answer for external names must not fail a fetch the
//! proxy would have carried. A forbidden IP *literal* is still refused
//! textually on that route.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use ocx_index::{CatalogDocument, IndexFormatConfig, IndexRoot, SUPPORTED_FORMAT_VERSION, parse_physical_repository};
use ocx_oci::Digest;
use ocx_oci::ssrf::{
    DialRoute, DialScheme, GuardedResolver, ProxyRules, guard_destination, proxy_rules, split_host_port,
};
use serde::Deserialize;
use url::{Host, Url};

use crate::error::MirrorError;

/// Bound on any index-document response body.
pub const INDEX_FETCH_CEILING: usize = 32 * 1024 * 1024;

/// Connect-phase timeout for one index-document fetch. A dead or
/// slow-to-accept endpoint must not stall a whole-registry run indefinitely.
const INDEX_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Total request timeout (connect + response + capped body read). The byte cap
/// alone does not bound a slowloris-style stall — a server dribbling one byte
/// per minute never exceeds it.
const INDEX_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Build the HTTP client used for this source's index-tree fetches (C-016).
///
/// The `index_base` pre-flight is **part of construction, not a step beside
/// it**: `guard_destination` runs before the client is built, so there is no
/// way to hold a client for a base URL that was never judged — and its answer
/// is pinned into the client rather than discarded, so the connect phase cannot
/// reach an address the floor never saw. A bare `reqwest::get` anywhere on the
/// source path is a Block-tier defect: `src/source/url_index.rs` is the right
/// JSON shape and the **wrong** trust model for this path. `trusted_hosts`
/// comes from the source's own `trusted_hosts:` — without it the SSRF floor
/// refuses a corporate index on an RFC1918 address, which is the motivating
/// deployment.
///
/// # Errors
///
/// [`MirrorError::SourceError`] when `index_base` is unparseable or hostless,
/// when its host fails the SSRF floor, or when the client cannot be built.
pub async fn build_source_index_client(
    index_base: &str,
    trusted_hosts: &[String],
) -> Result<reqwest::Client, MirrorError> {
    let rules = proxy_rules();
    let pin = validate_index_base_host(index_base, trusted_hosts, &rules).await?;
    index_client(pin, trusted_hosts, rules)
}

/// The index base URL's host as `guard_destination` and `trusted_hosts:`
/// spell it — **unbracketed** for an IPv6 literal.
///
/// [`Url::host_str`] hands back a bracketed `[::1]` for an IPv6 authority,
/// which `guard_destination` parses as neither an IP literal nor a DNS name:
/// it would fail closed, but a `trusted_hosts: ["::1"]` entry could never open
/// it again either. Shared with [`RegistrySpec::validate`]'s transport rule
/// (C-006) so the host the spec judges is the host the pre-flight judges — two
/// spellings would mean a `trusted_hosts` entry that exempts one check and not
/// the other.
///
/// [`RegistrySpec::validate`]: crate::spec::RegistrySpec::validate
pub fn index_host(url: &Url) -> Option<String> {
    match url.host()? {
        Host::Domain(domain) => Some(domain.to_string()),
        Host::Ipv4(address) => Some(address.to_string()),
        Host::Ipv6(address) => Some(address.to_string()),
    }
}

/// Resolve and validate the index base URL's host, returning the pin its answer
/// earns.
///
/// `Some((host, addresses))` for a DNS name dialled directly: those are the
/// addresses that passed the floor, and [`index_client`] makes them the only
/// ones the client will dial. `None` for an IP literal — reqwest resolves
/// nothing for one, so the address in the URL is already the address that was
/// judged — and `None` on a proxied route, where the process resolves nothing
/// at all. `rules` decides the route; production passes the process-wide
/// [`proxy_rules`].
async fn validate_index_base_host(
    index_base: &str,
    trusted_hosts: &[String],
    rules: &ProxyRules,
) -> Result<Option<(String, Vec<SocketAddr>)>, MirrorError> {
    let url = Url::parse(index_base)
        .map_err(|error| MirrorError::SourceError(format!("source index base URL is unparseable: {error}")))?;

    // The scheme, not the URL: a hostless base has no origin to name, and its
    // path is the one part worth not echoing (see [`document_name`]).
    let Some(host) = index_host(&url) else {
        return Err(MirrorError::SourceError(format!(
            "source index base URL has no host: the '{}' scheme names no authority",
            url.scheme()
        )));
    };

    let scheme = if url.scheme() == "http" {
        DialScheme::Http
    } else {
        DialScheme::Https
    };
    let route = guard_destination(
        scheme,
        &host,
        url.port_or_known_default().unwrap_or(443),
        trusted_hosts,
        rules,
    )
    .await
    .map_err(|error| MirrorError::SourceError(format!("source index base URL refused: {error}")))?;

    Ok(match route {
        DialRoute::Direct(addresses) => matches!(url.host(), Some(Host::Domain(_))).then_some((host, addresses)),
        DialRoute::Proxied => None,
    })
}

/// The client for a validated base URL, with the pre-flight's own answer pinned
/// into it and the SSRF floor installed as its resolver.
///
/// Split out of [`build_source_index_client`] so the pin can be exercised with
/// a name the resolver cannot answer at all — the only way to observe that the
/// connect phase never re-resolves, since both the pre-flight and reqwest would
/// otherwise consult the same resolver and agree.
fn index_client(
    pin: Option<(String, Vec<SocketAddr>)>,
    trusted_hosts: &[String],
    rules: Arc<ProxyRules>,
) -> Result<reqwest::Client, MirrorError> {
    let mut builder = crate::http::builder()
        // Not decoration: without it a validated host answers `302` and
        // relocates the fetch to a host the pre-flight never saw, which is the
        // whole SSRF window reopened (CWE-918) — and a redirect to `http://`
        // would strip the transport too (CWE-319). A static index tree needs
        // no redirects.
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(INDEX_CONNECT_TIMEOUT)
        .timeout(INDEX_REQUEST_TIMEOUT)
        // The floor at dial time, beneath the pin: any name that reaches DNS
        // (none is pinned on a proxied route) is resolved, validated and
        // pinned here, and the proxy's own host is the one name admitted with
        // no range judgement (C-046, the same hook `registry_copy` installs).
        .dns_resolver(Arc::new(GuardedResolver::new(Arc::new(trusted_hosts.to_vec()), rules)));

    // The pre-flight's answer, pinned — consulted before the resolver above,
    // so a pinned name never reaches DNS. Without it reqwest performs a *second*
    // DNS lookup when the first socket opens, and nothing carries the verdict
    // from one to the other. The port rides along unused: reqwest documents
    // that the URL's own port always wins over the override's.
    if let Some((host, addresses)) = &pin {
        builder = builder.resolve_to_addrs(host, addresses);
    }

    builder
        .build()
        .map_err(|error| MirrorError::SourceError(format!("index HTTP client build failed: {error}")))
}

/// Refuse a source root whose physical host fails the SSRF floor (C-017).
///
/// `split_host_port` the root's physical authority, then `guard_destination`
/// **before any registry request** — the same "SSRF-before-any-registry-request"
/// ordering `announce::pipeline`'s `guarded_physical()` states, and there must
/// not be a second guard written for it.
///
/// A refusal is a **whole-run abort**, never a per-package failure: a source
/// index steering the mirror at link-local addresses is not a package problem.
/// The exit code diverges from `announce`'s deliberately — announce maps
/// `SsrfError` to `ConfigError` (78) because its forbidden host came from the
/// operator's own config; here it came off a foreign index over the network,
/// so the *source* is what is misbehaving.
///
/// # Errors
///
/// [`MirrorError::SourceError`] (exit 69) for an unparseable `repository`
/// pointer, a forbidden host, or a host that does not resolve. The
/// [`SsrfError`](ocx_oci::ssrf::SsrfError) supplies the message text.
pub async fn validate_root_host(root: &IndexRoot, trusted: &[String], rules: &ProxyRules) -> Result<(), MirrorError> {
    let (registry, _repository) = parse_physical_repository(&root.repository).map_err(|error| {
        MirrorError::SourceError(format!("source root has an unusable repository pointer: {error}"))
    })?;

    let (host, port) = split_host_port(&registry);
    // `split_host_port` does not strip brackets, and its own doc says any
    // future stripping must land there so both of *its* call sites get it
    // together — but that file is a read-only submodule here. Without this,
    // `oci://[fd00::1]:5000/x` yields the host `"[fd00::1]"`, which parses as
    // neither an IP literal nor a DNS name: it fails closed, and no
    // `trusted_hosts: ["fd00::1"]` entry can ever open it again. Same
    // normalisation [`index_host`] applies on the index half.
    let host = host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(host);

    // The verdict only: the registry client's own `GuardedResolver` re-judges
    // and pins at dial time (C-046), so nothing here is discarded.
    guard_destination(
        DialScheme::for_registry(&ocx_config::env::insecure_registries(), &registry),
        host,
        port,
        trusted,
        rules,
    )
    .await
    .map_err(|error| MirrorError::SourceError(format!("source root repository pointer refused: {error}")))?;
    Ok(())
}

/// `GET <index>/config.json` — the format gate, and the bytes to re-serve
/// (C-018).
///
/// Parsed only to gate `format_version` against `SUPPORTED_FORMAT_VERSION`.
/// The raw bytes come back beside the parsed shape for symmetry with the other
/// two fetches, but **nothing writes them**: the mirror always synthesizes its
/// own `config.json` (C-031), so republishing a source's declaration is not a
/// thing this path can do.
///
/// A 404 is **not** fatal: the mirror synthesizes `{"format_version": 1}`
/// either way, so this returns `Ok(None)` for an absent document and the gate
/// simply does not run.
///
/// # Errors
///
/// [`MirrorError::IndexFormatUnsupported`] (exit 65) for a version this ocx
/// does not implement — the run writes nothing. [`MirrorError::SourceError`]
/// (exit 69) for a transport failure or an unparseable document.
pub async fn fetch_source_config(
    client: &reqwest::Client,
    index_base: &str,
) -> Result<Option<(Vec<u8>, IndexFormatConfig)>, MirrorError> {
    let Some(bytes) = fetch_index_document(client, index_base, "config.json").await? else {
        return Ok(None);
    };

    let config: IndexFormatConfig = serde_json::from_slice(&bytes)
        .map_err(|error| MirrorError::SourceError(format!("source config.json is unparseable: {error}")))?;
    gate_format_version(config.format_version)?;
    Ok(Some((bytes, config)))
}

/// `GET <index>/c/index.json` — the package listing (C-019).
///
/// Returns the raw bytes (whose `sha256` is the short-circuit's cache key) and
/// the parsed document.
///
/// # Errors
///
/// [`MirrorError::SourceError`] (exit 69) for any transport or parse failure —
/// a whole-run abort. Unlike `config.json`, an absent catalog is fatal: the
/// mirror has no other way to enumerate the source.
/// [`MirrorError::IndexFormatUnsupported`] (exit 65) when the envelope's own
/// version pin is one this ocx does not implement.
pub async fn fetch_source_catalog(
    client: &reqwest::Client,
    index_base: &str,
) -> Result<(Vec<u8>, CatalogDocument), MirrorError> {
    let bytes = fetch_index_document(client, index_base, "c/index.json")
        .await?
        .ok_or_else(|| {
            MirrorError::SourceError(format!(
                "source catalog is absent: {}",
                document_name(index_base, "c/index.json")
            ))
        })?;

    let catalog: CatalogDocument = serde_json::from_slice(&bytes)
        .map_err(|error| MirrorError::SourceError(format!("source catalog is unparseable: {error}")))?;
    // The catalog envelope carries the same version pin `config.json` does, and
    // an absent `config.json` is resolved to v1 rather than refused (C-018) —
    // so gating only there would let a source serve no config and a v2 catalog
    // with nothing checking. `CatalogDocument::into_packages` gates it upstream
    // for exactly this reason; the mirror keeps `packages` and so reaches the
    // gate here instead.
    gate_format_version(catalog.format_version)?;
    Ok((bytes, catalog))
}

/// `GET <index>/p/<ns>/<pkg>.json` — one package's root document (C-020).
///
/// Returns the raw bytes (the rewrite operates on these, so unknown fields ride
/// through) plus the typed root, for `tags{}`, `repository` and the skip
/// predicate. The body is capped at [`INDEX_FETCH_CEILING`].
///
/// # Errors
///
/// [`MirrorError::SourceError`] (exit 69) for a name that cannot address a
/// document, a transport failure, an absent root, an oversized body, or an
/// unparseable root.
pub async fn fetch_source_root(
    client: &reqwest::Client,
    index_base: &str,
    name: &str,
) -> Result<(Vec<u8>, IndexRoot), MirrorError> {
    let relative = package_document_path(name)?;
    let bytes = fetch_index_document(client, index_base, &relative)
        .await?
        .ok_or_else(|| MirrorError::SourceError(format!("source root is absent for package {name:?}")))?;

    let root: IndexRoot = serde_json::from_slice(&bytes)
        .map_err(|error| MirrorError::SourceError(format!("source root for {name:?} is unparseable: {error}")))?;
    Ok((bytes, root))
}

/// One README or logo object the source tree serves beside a root — the bytes
/// a `desc.readme` / `desc.logo` digest points at, and the extension the tree
/// serves them under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescriptionObject {
    /// The digest the root names, and `sha256(bytes)` — verified on fetch.
    pub digest: Digest,
    /// `md`, `svg` or `png` — the extension the object was found under, which
    /// is the one the mirrored tree must serve it under too.
    pub extension: &'static str,
    /// The object, verbatim.
    pub bytes: Vec<u8>,
}

/// Extensions a `desc.readme` object is served under.
const README_EXTENSIONS: &[&str] = &["md"];
/// Extensions a `desc.logo` object is served under.
const LOGO_EXTENSIONS: &[&str] = &["svg", "png"];

/// The two `desc` fields that point into the package's own CAS subtree —
/// the only part of `desc` the mirror reads. Every other `desc` field rides
/// through the root rewrite verbatim and is never modelled here.
#[derive(Deserialize)]
struct DescriptionPointers {
    #[serde(default)]
    readme: Option<String>,
    #[serde(default)]
    logo: Option<String>,
}

#[derive(Deserialize)]
struct DescribedRoot {
    #[serde(default)]
    desc: Option<DescriptionPointers>,
}

/// `GET <index>/p/<ns>/<pkg>/o/<algo>/<hex>.<ext>` for every object the root's
/// `desc.readme` / `desc.logo` name — the README and logo the index serves
/// beside the root, which a catalog renderer resolves against the tree and
/// nothing else.
///
/// Fetched from the source **tree**, not rebuilt from the registry's
/// `__ocx.desc` layers, so the mirrored root and the objects it references
/// always come from one source snapshot: a registry whose description moved
/// ahead of the index it is published in would otherwise leave the root
/// naming digests the tree does not hold — the exact dangling reference the
/// catalog refuses (ocx-mirror#68).
///
/// A digest alone carries no extension, and an HTTP tree cannot be listed, so
/// each pointer is tried under the extensions the index writes for that field
/// — `.md` for the README, `.svg` then `.png` for the logo — and the first hit
/// wins. Every body is verified against the digest the root named before it
/// is returned.
///
/// # Errors
///
/// Two classes, split the way C-040 splits them and the way
/// [`super::index_write`] already does with `refused`: **foreign data that
/// will not validate** — a `desc` that is not the documented shape, a pointer
/// that is not a digest, an object absent under every extension (a dangling
/// reference upstream), a body that does not hash to its digest — is
/// [`MirrorError::ExecutionFailed`], one package's failure, because a hostile
/// or merely broken upstream must not deny the whole mirror through one root.
/// **A read that did not answer** — a transport failure, a non-404 status, a
/// body over the cap — stays [`MirrorError::SourceError`] and aborts the run,
/// exactly as the root fetch above does.
pub async fn fetch_description_objects(
    client: &reqwest::Client,
    index_base: &str,
    name: &str,
    root_bytes: &[u8],
) -> Result<Vec<DescriptionObject>, MirrorError> {
    let root: DescribedRoot = serde_json::from_slice(root_bytes)
        .map_err(|error| refused(format!("source root for {name:?} has an unusable desc: {error}")))?;
    let Some(desc) = root.desc else {
        return Ok(Vec::new());
    };

    let mut objects = Vec::new();
    for (field, pointer, extensions) in [
        ("desc.readme", desc.readme, README_EXTENSIONS),
        ("desc.logo", desc.logo, LOGO_EXTENSIONS),
    ] {
        let Some(pointer) = pointer else { continue };
        let digest = Digest::try_from(pointer.as_str()).map_err(|error| {
            refused(format!(
                "source root for {name:?} has an unusable {field} pointer {pointer:?}: {error}"
            ))
        })?;
        objects.push(fetch_description_object(client, index_base, name, field, digest, extensions).await?);
    }
    Ok(objects)
}

/// One `desc` pointer: the first extension the tree serves it under, verified.
async fn fetch_description_object(
    client: &reqwest::Client,
    index_base: &str,
    name: &str,
    field: &str,
    digest: Digest,
    extensions: &[&'static str],
) -> Result<DescriptionObject, MirrorError> {
    for extension in extensions {
        let relative = cas_object_path(name, &digest, extension)?;
        let Some(bytes) = fetch_index_document(client, index_base, &relative).await? else {
            continue;
        };
        let observed = digest.algorithm().hash(&bytes);
        if observed != digest {
            return Err(refused(format!(
                "source object {relative} for {name:?} hashes to {observed}, not the {digest} its root names"
            )));
        }
        return Ok(DescriptionObject {
            digest,
            extension,
            bytes,
        });
    }
    Err(refused(format!(
        "source root for {name:?} names {field} {digest} but the source tree serves no such object under .{}",
        extensions.join("/.")
    )))
}

/// A refusal of **upstream bytes** on the description path — C-040's
/// aggregating class (exit 1), the same split `index_write::refused` documents.
/// Every other error this module returns is a read that did not answer and
/// aborts the run; only [`fetch_description_objects`] can tell the two apart,
/// because only it verifies content rather than transporting it.
fn refused(message: String) -> MirrorError {
    MirrorError::ExecutionFailed(vec![message])
}

/// The one comparison against [`SUPPORTED_FORMAT_VERSION`] on this path.
///
/// Fail-closed and exact: **any** version other than the supported one is
/// refused, not just a higher one. A `format_version: 0` is no more readable
/// than a `2`, and "parse it anyway because the number looks small" is how
/// foreign data becomes control flow. Mirrors `ocx_index`'s own
/// `gate_format_version`, which is `pub(crate)` upstream.
fn gate_format_version(version: u64) -> Result<(), MirrorError> {
    if version == SUPPORTED_FORMAT_VERSION {
        return Ok(());
    }
    Err(MirrorError::IndexFormatUnsupported(version))
}

/// `<index-base>/<relative>`, with the base's trailing slash normalised away.
fn document_url(index_base: &str, relative: &str) -> String {
    format!("{}/{relative}", index_base.trim_end_matches('/'))
}

/// The base URL's **origin** — `scheme://host[:port]`, no path, no userinfo.
///
/// A non-tuple origin (a base URL with no host, which [`build_source_index_client`]
/// refuses) serialises as `"null"`, and an unparseable one is named
/// generically: neither reaches a live fetch, and neither is worth leaking a
/// path over.
fn document_origin(index_base: &str) -> String {
    Url::parse(index_base).map_or_else(
        |_| "<source index>".to_string(),
        |url| url.origin().ascii_serialization(),
    )
}

/// How an index document is named in an error message: the base URL's origin
/// plus the relative document path, **never the full URL**.
///
/// `sources[].index` is refused at spec load if its authority carries userinfo
/// (C-005) and it must be `https` unless its host is trusted (C-006) — but a
/// *capability URL*, a secret in the **path** (`https://host/<token>/`), is a
/// functional index base that neither rule refuses, and echoing the URL
/// verbatim writes that token into every log the run ships. The relative half
/// is a fixed set — `config.json`, `c/index.json`, and a `p/<ns>/<pkg>.json`
/// whose key already passed [`package_document_path`] — so nothing foreign
/// rides in on it either.
fn document_name(index_base: &str, relative: &str) -> String {
    format!("{}/{relative}", document_origin(index_base))
}

/// `p/<ns>/<pkg>.json` for a catalog key, refusing a key that cannot be one.
///
/// The key is foreign data — it comes out of the source's own catalog — and
/// this is the one place it becomes a URL path. C-013 refuses a traversing key
/// at plan time for the *destination* repository; the refusal is repeated here
/// rather than delegated, because a `..` segment or a `?`/`#`/`%` would walk
/// or truncate the fetch out of the source's own `p/` subtree. The charset is
/// the OCI repository grammar's, so nothing a legal package name contains is
/// refused.
///
/// The refusal quotes the key with `{:?}`, never bare: a key is foreign data
/// and reaches this function **before** any charset guard has judged it, so a
/// key holding a newline would otherwise forge whole log lines in the CI output
/// an operator reads, and `\u{202e}` or an ANSI escape would reach their
/// terminal (CWE-117). `char::escape_debug` escapes both classes and supplies
/// the quoting the message already implies.
fn package_document_path(name: &str) -> Result<String, MirrorError> {
    check_package_name(name)?;
    Ok(format!("p/{name}.json"))
}

/// `p/<ns>/<pkg>/o/<algo>/<hex>.<ext>` for one of a root's `desc` objects —
/// the same key guard as [`package_document_path`], because the key is the
/// same foreign data on its way into the same `p/` subtree.
fn cas_object_path(name: &str, digest: &Digest, extension: &str) -> Result<String, MirrorError> {
    check_package_name(name)?;
    Ok(format!(
        "p/{name}/o/{}/{}.{extension}",
        digest.algorithm().prefix(),
        digest.hex()
    ))
}

/// The key guard behind both path builders above.
fn check_package_name(name: &str) -> Result<(), MirrorError> {
    let refuse = || MirrorError::SourceError(format!("source catalog key cannot address a root document: {name:?}"));

    if !name
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-' | '/'))
    {
        return Err(refuse());
    }
    if name
        .split('/')
        .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(refuse());
    }
    Ok(())
}

/// One index-document `GET`, capped at [`INDEX_FETCH_CEILING`].
///
/// `Ok(None)` is a confirmed `404` and nothing else — every other non-success
/// status, a `304` answering this unconditional `GET` included, is an error.
/// Absence is a control-flow decision (`config.json` synthesis, C-018), so it
/// must not be inferred from a status the server did not send.
async fn fetch_index_document(
    client: &reqwest::Client,
    index_base: &str,
    relative: &str,
) -> Result<Option<Vec<u8>>, MirrorError> {
    fetch_index_document_capped(client, index_base, relative, INDEX_FETCH_CEILING).await
}

/// [`fetch_index_document`] with an injectable ceiling, so the streaming cap is
/// testable without fabricating a 32 MiB body.
///
/// Takes the base and the relative path rather than a composed URL, because the
/// two halves are echoed differently: the fetch needs the whole URL, every
/// message names it through [`document_name`], which drops the base's path.
async fn fetch_index_document_capped(
    client: &reqwest::Client,
    index_base: &str,
    relative: &str,
    ceiling: usize,
) -> Result<Option<Vec<u8>>, MirrorError> {
    let url = document_url(index_base, relative);
    let document = document_name(index_base, relative);

    // `without_url` is load-bearing, not tidiness: `reqwest::Error`'s `Display`
    // ends with `" for url ({url})"` — the whole URL, path included — so
    // appending it verbatim re-emits the very capability path [`document_name`]
    // withheld, one field later in the same message. The **path** case only:
    // userinfo in a `sources[].index` is refused outright at spec load
    // (`spec/prescan.rs`), and this does not stand in for that guard.
    let fetch_failed = |error: reqwest::Error| {
        // Chain-walked (`{:#}`), for the reason `dist_sync::fetch_manifest`
        // records: reqwest's `Display` names the request, never why it failed.
        MirrorError::SourceError(format!(
            "index fetch failed for {document}: {:#}",
            anyhow::Error::new(error.without_url())
        ))
    };

    let mut response = client.get(&url).send().await.map_err(fetch_failed)?;

    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(MirrorError::SourceError(format!(
            "index fetch for {document} returned unexpected status {status}"
        )));
    }

    // Refuse a declared oversize body before reading a single byte (CWE-400).
    if let Some(declared) = response.content_length()
        && declared > ceiling as u64
    {
        return Err(MirrorError::SourceError(format!(
            "index document {document} declares {declared} bytes, over the {ceiling}-byte cap"
        )));
    }

    // A server that omits or lies about `Content-Length` (chunked transfer, or
    // a hostile endpoint) still cannot stream more than the cap into memory:
    // the running total is checked before each chunk is appended.
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(fetch_failed)? {
        if body.len() + chunk.len() > ceiling {
            return Err(MirrorError::SourceError(format!(
                "index document {document} exceeds the {ceiling}-byte cap"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Some(body))
}

#[cfg(test)]
#[path = "catalog/tests.rs"]
mod tests;
