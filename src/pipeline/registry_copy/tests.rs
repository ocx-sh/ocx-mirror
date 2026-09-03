// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Copy-engine tests (C-021…C-026, C-046).
//!
//! The engine's I/O is two concrete `native::Client`s, which no unit test can
//! stand in for — so every decision the ladder makes is a function of its own,
//! tested here, and the wire behaviour is WP-16's acceptance suite against two
//! real registries. What that split leaves uncovered is *which* function the
//! ladder calls; the structural guards at the bottom of this file cover the
//! calls where reaching for the wrong one is silent and expensive.

use std::sync::atomic::{AtomicUsize, Ordering};

use ocx_lib::oci::native::oci_client::errors::{OciEnvelope, OciError};
use ocx_lib::oci::{Descriptor, ImageIndex, ImageIndexEntry, ImageManifest};

use super::*;

// ── Helpers ─────────────────────────────────────────────────────────────────

/// This module's own source text with every comment line removed.
///
/// Load-bearing: the doc comments below deliberately *name* the calls the
/// structural guards forbid — which is the right thing for a comment to do and
/// exactly what makes an unstripped denylist match itself.
fn module_source_without_comments() -> String {
    include_str!("../registry_copy.rs")
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The source text of one `async fn`, from its signature to the next item.
///
/// Structural guards that index occurrences by ordinal go stale the moment a
/// call site is added elsewhere in the module — and go stale *silently*, still
/// asserting something about whichever call the ordinal now lands on. Slicing
/// the function by name is what keeps a guard scoped to the code it is about.
fn function_body<'a>(module: &'a str, name: &str) -> &'a str {
    let start = module
        .find(&format!("async fn {name}("))
        .unwrap_or_else(|| panic!("`{name}` must exist under this name, or this guard tests nothing"));
    let function = &module[start..];
    // The next item's opening line ends this one. Both spellings are searched
    // because a `pub` item does not begin with `async`.
    let end = ["\nasync fn ", "\npub async fn ", "\nfn ", "\npub fn "]
        .iter()
        .filter_map(|needle| function[1..].find(needle).map(|offset| offset + 1))
        .min()
        .unwrap_or(function.len());
    &function[..end]
}

fn sha256_of(bytes: &[u8]) -> Digest {
    ocx_lib::oci::Algorithm::Sha256.hash(bytes)
}

fn digest_of(seed: u8) -> Digest {
    sha256_of(&[seed])
}

fn descriptor(digest: &Digest, size: i64) -> Descriptor {
    Descriptor {
        media_type: "application/vnd.oci.image.layer.v1.tar".to_string(),
        digest: digest.to_string(),
        size,
        urls: None,
        annotations: None,
        artifact_type: None,
    }
}

fn index_entry(digest: &Digest, size: i64, platform: Option<native::Platform>) -> ImageIndexEntry {
    ImageIndexEntry {
        media_type: "application/vnd.oci.image.manifest.v1+json".to_string(),
        digest: digest.to_string(),
        size,
        platform,
        annotations: None,
        artifact_type: None,
    }
}

fn platform(os: &str, architecture: &str) -> native::Platform {
    native::Platform {
        architecture: native::Arch::Other(architecture.to_string()),
        os: native::Os::Other(os.to_string()),
        os_version: None,
        os_features: None,
        variant: None,
        features: None,
    }
}

fn image_index(entries: Vec<ImageIndexEntry>) -> Manifest {
    Manifest::ImageIndex(ImageIndex {
        schema_version: 2,
        media_type: Some("application/vnd.oci.image.index.v1+json".to_string()),
        manifests: entries,
        artifact_type: None,
        annotations: None,
    })
}

fn image_manifest(config: Descriptor, layers: Vec<Descriptor>) -> Manifest {
    Manifest::Image(ImageManifest {
        schema_version: 2,
        media_type: Some("application/vnd.oci.image.manifest.v1+json".to_string()),
        config,
        layers,
        subject: None,
        artifact_type: None,
        annotations: None,
    })
}

fn root_tag(content: Digest) -> RootTag {
    // `RootTag` is deserialize-only upstream, so it is built through its wire
    // form rather than by struct literal.
    serde_json::from_value(serde_json::json!({ "content": content.to_string() })).expect("a well-formed root tag")
}

fn yanked_root_tag(content: Digest) -> RootTag {
    serde_json::from_value(serde_json::json!({
        "content": content.to_string(),
        "yanked": { "reason": "withdrawn", "at": "2026-08-14T00:00:00Z" },
    }))
    .expect("a well-formed yanked root tag")
}

fn server_error(code: u16) -> OciDistributionError {
    OciDistributionError::ServerError {
        code,
        url: "https://registry.example.com/v2/ns/pkg/blobs/sha256:0".to_string(),
        message: String::new(),
    }
}

fn registry_error(code: OciErrorCode) -> OciDistributionError {
    OciDistributionError::RegistryError {
        envelope: OciEnvelope {
            errors: vec![OciError {
                code,
                message: String::new(),
                detail: serde_json::Value::Null,
            }],
        },
        url: "https://registry.example.com/v2/ns/pkg/blobs/uploads/".to_string(),
    }
}

/// A `RequestError` carrying `status`, the exact shape the destination presence
/// probe emits: the fork's `head_blob_response` turns a non-404 HEAD into
/// `Err(error_for_status_ref().into())`. `reqwest` builds an `Error` carrying a
/// status only from a `Response`, so synthesize a minimal one.
fn request_error(status: u16) -> OciDistributionError {
    let response = http::Response::builder()
        .status(status)
        .body(Vec::new())
        .expect("a well-formed HTTP response");
    let source = reqwest::Response::from(response)
        .error_for_status()
        .expect_err("a >= 400 status is an error");
    OciDistributionError::RequestError(source)
}

// ── C-021 — the tag copy plan classifies nothing ────────────────────────────

/// The whole contract in one test: every key in, every key out, with its own
/// digest, whatever the text looks like.
#[test]
fn every_tag_is_copied_verbatim_whatever_its_text() {
    let keys = [
        "1.2.3",
        "1.2",
        "1",
        "latest",
        "nightly",
        "edge",
        "2026-08-14",
        "20260814",
        "a1b2c3d",
        "debug",
    ];
    let tags: BTreeMap<String, RootTag> = keys
        .iter()
        .enumerate()
        .map(|(seed, key)| ((*key).to_string(), root_tag(digest_of(seed as u8))))
        .collect();

    let plan = tag_copy_plan(&tags);

    assert_eq!(plan.len(), keys.len(), "no tag may be dropped: {plan:?}");
    for (seed, key) in keys.iter().enumerate() {
        let entry = plan
            .iter()
            .find(|entry| entry.tag == *key)
            .unwrap_or_else(|| panic!("`{key}` must appear in the plan"));
        assert_eq!(entry.content, digest_of(seed as u8), "`{key}` must keep its own digest");
    }
}

/// A tag the OCI grammar cannot express is dropped, and its legal siblings are
/// still copied.
///
/// The traversal key is the one that matters. A tag reaches the destination as
/// `.../manifests/{tag}` interpolated raw, and re-parsing that URL collapses
/// `..` — so `../../../../../prod/critical-app/manifests/latest` resolves to
/// `https://dest/v2/prod/critical-app/manifests/latest`, PUTting upstream bytes
/// into a repository outside the prefix `physical_repository` enforces. Nothing
/// else on the path judges a tag: `Reference::with_tag` is a plain struct
/// literal, and only `FromStr` applies the reference regexp.
///
/// The legal half is asserted too, because dropping the whole package on one
/// bad key would hand a hostile upstream a denial of service against the other
/// tags — the fix has to be drop-and-continue, not fail-the-package.
#[test]
fn a_tag_the_oci_grammar_cannot_express_is_dropped_and_its_siblings_survive() {
    let illegal = [
        "../../../../../prod/critical-app/manifests/latest",
        "has/slash",
        ".leading-dot",
        "-leading-dash",
        "with space",
        "",
    ];
    let legal = ["1.2.3", "latest", "3.31.0_20260731", "20260814", "_underscore-start"];

    let tags: BTreeMap<String, RootTag> = illegal
        .iter()
        .chain(legal.iter())
        .enumerate()
        .map(|(seed, key)| ((*key).to_string(), root_tag(digest_of(seed as u8))))
        .collect();

    let plan = tag_copy_plan(&tags);
    let copied: Vec<&str> = plan.iter().map(|entry| entry.tag.as_str()).collect();

    for key in illegal {
        assert!(
            !copied.contains(&key),
            "`{key}` is not expressible as an OCI tag and must never reach a destination URL: {copied:?}"
        );
    }
    for key in legal {
        assert!(
            copied.contains(&key),
            "`{key}` is a legal tag and dropping it would let one hostile key deny the rest: {copied:?}"
        );
    }
    assert_eq!(copied.len(), legal.len(), "{copied:?}");
}

/// Grammar, not classification: the build-stamped and bare-date forms this
/// project deliberately refuses to *parse* must still pass the grammar check.
/// Roughly half of real upstream tags look like these, so a validator that
/// reached for a version parser would silently stop mirroring most of a catalog.
#[test]
fn the_grammar_check_never_asks_what_a_tag_means() {
    for tag in [
        "3.31.0_20260731",
        "20260814",
        "nightly",
        "edge",
        "a1b2c3d",
        "2026-08-14",
    ] {
        assert!(super::is_legal_oci_tag(tag), "`{tag}` must pass the grammar check");
    }
}

/// An unparseable tag sorts **first**, so a date stamp or a git sha can never
/// be mistaken for the newest release in a log or an interrupted run.
#[test]
fn an_unparseable_tag_copies_before_every_version() {
    let tags: BTreeMap<String, RootTag> = ["1.2.3", "nightly", "1.30.0", "a1b2c3d"]
        .iter()
        .map(|key| ((*key).to_string(), root_tag(digest_of(0))))
        .collect();

    let plan = tag_copy_plan(&tags);
    let ordered: Vec<&str> = plan.iter().map(|entry| entry.tag.as_str()).collect();

    let first_version = ordered
        .iter()
        .position(|tag| *tag == "1.2.3")
        .expect("the version tags are in the plan");
    for unparseable in ["nightly", "a1b2c3d"] {
        let position = ordered
            .iter()
            .position(|tag| *tag == unparseable)
            .unwrap_or_else(|| panic!("`{unparseable}` must be in the plan"));
        assert!(
            position < first_version,
            "`{unparseable}` must sort before every parseable version: {ordered:?}"
        );
    }
    assert!(
        ordered.iter().position(|tag| *tag == "1.2.3") < ordered.iter().position(|tag| *tag == "1.30.0"),
        "parseable versions stay in release order: {ordered:?}"
    );
}

/// A yanked tag is still copied. The marker travels with the root document, so
/// dropping the tag here would make the mirror disagree with the source about
/// which digests exist — and a consumer pinned to it would stop resolving.
#[test]
fn a_yanked_tag_is_copied_like_any_other() {
    let mut tags = BTreeMap::new();
    tags.insert("1.2.3".to_string(), yanked_root_tag(digest_of(1)));
    tags.insert("1.2.4".to_string(), root_tag(digest_of(2)));

    let plan = tag_copy_plan(&tags);

    assert_eq!(plan.len(), 2, "a yank is a signal, never a delete: {plan:?}");
    assert!(
        plan.iter()
            .any(|entry| entry.tag == "1.2.3" && entry.content == digest_of(1))
    );
}

#[test]
fn an_empty_tag_map_yields_an_empty_plan() {
    assert!(tag_copy_plan(&BTreeMap::new()).is_empty());
}

// ── C-022 — the completeness walk ───────────────────────────────────────────

/// S-024, the scenario every platform-shaped assertion passes and only this one
/// catches: an image index carrying a descriptor with **no `platform` key**
/// alongside normal entries. ocx's own candidate walk drops it as a
/// non-candidate; a mirror must carry it, because the index is republished
/// verbatim and a dropped child leaves the mirrored index pointing at a
/// manifest that does not exist at the destination.
#[test]
fn every_image_index_descriptor_travels_including_the_platformless_ones() {
    let manifest = image_index(vec![
        index_entry(&digest_of(1), 100, Some(platform("linux", "amd64"))),
        index_entry(&digest_of(2), 200, None),
        index_entry(&digest_of(3), 300, Some(platform("unknown", "unknown"))),
        index_entry(&digest_of(4), 400, Some(platform("darwin", "arm64"))),
    ]);

    let ChildReferences::Manifests(children) = child_references(&manifest).expect("a well-formed index") else {
        panic!("an image index decomposes into child manifests");
    };

    assert_eq!(
        children,
        vec![
            (digest_of(1), 100),
            (digest_of(2), 200),
            (digest_of(3), 300),
            (digest_of(4), 400),
        ],
        "every descriptor must travel, in wire order — a mirror filters nothing"
    );
}

#[test]
fn an_image_manifest_yields_its_config_first_then_every_layer() {
    let manifest = image_manifest(
        descriptor(&digest_of(9), 191),
        vec![descriptor(&digest_of(10), 53_000_000), descriptor(&digest_of(11), 12)],
    );

    let ChildReferences::Blobs(blobs) = child_references(&manifest).expect("a well-formed manifest") else {
        panic!("an image manifest decomposes into blobs");
    };

    assert_eq!(
        blobs,
        vec![(digest_of(9), 191), (digest_of(10), 53_000_000), (digest_of(11), 12)]
    );
}

#[test]
fn an_empty_image_index_is_not_an_error() {
    let ChildReferences::Manifests(children) = child_references(&image_index(vec![])).expect("empty is legal") else {
        panic!("an image index decomposes into child manifests");
    };
    assert!(children.is_empty());
}

/// A descriptor is foreign data. Both failures below would otherwise be
/// silent: an unparseable digest would have to be skipped, and a negative size
/// wraps into an enormous `u64` that sizes an allocation.
#[test]
fn a_descriptor_the_mirror_cannot_transport_fails_closed() {
    let mut bad_digest = descriptor(&digest_of(1), 10);
    bad_digest.digest = "not-a-digest".to_string();
    let negative_size = descriptor(&digest_of(1), -1);

    for manifest in [
        image_manifest(descriptor(&digest_of(0), 1), vec![bad_digest.clone()]),
        image_manifest(descriptor(&digest_of(0), 1), vec![negative_size.clone()]),
        image_manifest(bad_digest, vec![]),
        image_index(vec![index_entry(&digest_of(1), -5, None)]),
    ] {
        let error = child_references(&manifest).expect_err("a malformed descriptor must be refused");
        assert!(
            matches!(error, CopyError::MalformedManifest(_)),
            "a malformed descriptor is permanent, not a transient fetch failure: {error}"
        );
        assert!(!error.is_whole_run_abort(), "it fails the package, never the run");
    }
}

/// A descriptor's `size` is foreign data and it sizes the buffer `upload`
/// allocates **before the first byte arrives**. `i64::MAX` clears the sibling
/// non-negative guard, and `Vec::with_capacity` at that value clears its own
/// capacity-overflow check too — so it reaches `handle_alloc_error`, which
/// aborts the process rather than failing the package. Reachable whenever a
/// blob is absent at the destination and the mount declines, which is every
/// first sync.
#[test]
fn a_descriptor_declaring_an_absurd_size_is_refused_before_anything_allocates() {
    let huge = descriptor(&digest_of(7), i64::MAX);

    for manifest in [
        image_manifest(descriptor(&digest_of(0), 1), vec![huge.clone()]),
        image_manifest(huge.clone(), vec![]),
        image_index(vec![index_entry(&digest_of(7), i64::MAX, None)]),
    ] {
        let error = child_references(&manifest).expect_err("an unbounded declared size must be refused");
        let CopyError::MalformedManifest(message) = &error else {
            panic!("an out-of-policy size is permanent, not a transient fetch failure: {error}");
        };
        assert!(
            message.contains(&digest_of(7).to_string()),
            "the refusal must name the offending descriptor: {message}"
        );
        assert!(!error.is_whole_run_abort(), "it fails the package, never the run");
    }
}

/// The boundary itself, so the ceiling cannot drift into refusing the largest
/// blob it is meant to admit.
#[test]
fn a_blob_at_the_ceiling_is_admitted_and_one_byte_past_it_is_not() {
    let digest = digest_of(1).to_string();
    let at = i64::try_from(BLOB_SIZE_CEILING).expect("the ceiling fits an i64");

    assert_eq!(
        descriptor_target(&digest, at).expect("the ceiling itself is legal"),
        (digest_of(1), BLOB_SIZE_CEILING)
    );
    assert!(descriptor_target(&digest, at + 1).is_err(), "one byte past it is not");
}

/// Defence in depth at the allocation itself. `ensure_blob` is `pub`, so a
/// caller reaching it with a size that never passed `descriptor_target` must
/// still not be able to reach `handle_alloc_error`. The hint is only a hint —
/// the `Vec` grows on demand — so clamping can never truncate a body.
#[test]
fn the_upload_buffer_hint_is_clamped_however_large_the_declared_size() {
    let ceiling = usize::try_from(BLOB_SIZE_CEILING).expect("a 64-bit target");

    assert_eq!(capacity_hint(0), 0);
    assert_eq!(capacity_hint(4096), 4096, "an ordinary size is passed through");
    for absurd in [BLOB_SIZE_CEILING + 1, u64::MAX] {
        assert!(capacity_hint(absurd) <= ceiling, "{absurd} must not size an allocation");
    }
}

// ── C-022 — digest verification ─────────────────────────────────────────────

#[test]
fn bytes_matching_their_digest_verify() {
    let bytes = b"{\"schemaVersion\":2}";
    assert!(verify_digest(&sha256_of(bytes), bytes).is_ok());
}

#[test]
fn bytes_that_do_not_hash_to_their_digest_are_refused_and_both_digests_are_named() {
    let claimed = sha256_of(b"the manifest the source promised");
    let served = b"the manifest the source actually sent";

    let error = verify_digest(&claimed, served).expect_err("substituted content must be refused");

    let CopyError::DigestMismatch { expected, actual } = &error else {
        panic!("a substitution must be reported as a digest mismatch, got {error}");
    };
    assert_eq!(*expected, claimed);
    assert_eq!(*actual, sha256_of(served));
    let message = error.to_string();
    assert!(
        message.contains(&claimed.to_string()),
        "the message names what was expected"
    );
    assert!(
        message.contains(&sha256_of(served).to_string()),
        "and what actually arrived"
    );
}

/// The algorithm comes from the digest, not from a hardcoded `sha256`: a source
/// publishing `sha512` content must be verified under `sha512` rather than
/// silently passed.
#[test]
fn verification_uses_the_algorithm_the_digest_names() {
    let bytes = b"content addressed under sha512";
    let expected = ocx_lib::oci::Algorithm::Sha512.hash(bytes);

    assert!(verify_digest(&expected, bytes).is_ok());
    assert!(verify_digest(&expected, b"different bytes").is_err());
}

// ── C-023 — the destination probe ───────────────────────────────────────────

/// The fork's `fetch_blob_size` answers `Ok(None)` **iff** 404. Everything else
/// is an error, and reading one as "absent" is how a root gets written over
/// content that was never uploaded.
#[test]
fn only_an_authoritative_answer_decides_whether_a_blob_is_present() {
    assert_eq!(
        probe_verdict(Ok(Some(1234))).expect("a hit is authoritative"),
        Some(1234)
    );
    assert_eq!(probe_verdict(Ok(None)).expect("a 404 is authoritative"), None);

    for code in [401, 403, 429, 500, 503] {
        let error = probe_verdict(Err(server_error(code))).expect_err("a non-404 answer decides nothing");
        assert!(
            error.is_whole_run_abort(),
            "HTTP {code} on the probe must abort the run, not skip the blob: {error}"
        );
        assert!(
            matches!(error, CopyError::Abort(MirrorError::TargetError(_))),
            "the abort must carry TargetError (69): {error}"
        );
    }
}

// ── C-026 — reactive retry ──────────────────────────────────────────────────

#[test]
fn only_a_rate_limit_is_retryable() {
    assert!(is_rate_limited(&server_error(429)));
    assert!(is_rate_limited(&registry_error(OciErrorCode::Toomanyrequests)));
    // The HEAD presence probe emits its 429 as a `reqwest`-wrapped `RequestError`,
    // not one of the two shapes above; without its arm the probe's retry ladder
    // is inert and one throttle aborts the run.
    assert!(is_rate_limited(&request_error(429)));

    for other in [
        server_error(500),
        server_error(503),
        server_error(404),
        request_error(503),
        request_error(500),
        registry_error(OciErrorCode::BlobUnknown),
        registry_error(OciErrorCode::Denied),
        OciDistributionError::RegistryNoLocationError,
    ] {
        assert!(!is_rate_limited(&other), "must not be retried: {other}");
    }
}

#[tokio::test]
async fn a_registry_that_keeps_rate_limiting_costs_exactly_max_retries_extra_attempts() {
    for max_retries in [0, 1, 3, 5] {
        let attempts = AtomicUsize::new(0);

        let result: Result<(), OciDistributionError> = retry_while(max_retries, is_rate_limited, || {
            attempts.fetch_add(1, Ordering::SeqCst);
            async { Err(server_error(429)) }
        })
        .await;

        assert!(result.is_err(), "the ladder gives up rather than looping forever");
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            max_retries as usize + 1,
            "max_retries={max_retries} must yield exactly that many extra attempts"
        );
    }
}

#[tokio::test]
async fn a_rate_limit_that_clears_is_retried_and_then_succeeds() {
    let attempts = AtomicUsize::new(0);

    let result: Result<&str, OciDistributionError> = retry_while(3, is_rate_limited, || {
        let attempt = attempts.fetch_add(1, Ordering::SeqCst);
        async move {
            if attempt == 0 {
                Err(server_error(429))
            } else {
                Ok("pushed")
            }
        }
    })
    .await;

    assert_eq!(result.expect("the second attempt succeeds"), "pushed");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn an_error_that_is_not_a_rate_limit_is_never_retried() {
    let attempts = AtomicUsize::new(0);

    let result: Result<(), OciDistributionError> = retry_while(5, is_rate_limited, || {
        attempts.fetch_add(1, Ordering::SeqCst);
        async { Err(server_error(403)) }
    })
    .await;

    assert!(result.is_err());
    assert_eq!(attempts.load(Ordering::SeqCst), 1, "403 will not clear by trying again");
}

/// A digest mismatch is never retried even though it rides the same ladder as
/// the two transport steps: the same bytes will not start hashing differently.
#[test]
fn an_upload_failure_is_retryable_only_when_the_transport_was_rate_limited() {
    assert!(UploadFailure::Pull(server_error(429)).retryable());
    assert!(UploadFailure::Push(server_error(429)).retryable());
    assert!(!UploadFailure::Pull(server_error(500)).retryable());
    assert!(!UploadFailure::Push(server_error(403)).retryable());
    assert!(
        !UploadFailure::Verify(CopyError::DigestMismatch {
            expected: digest_of(1),
            actual: digest_of(2),
        })
        .retryable(),
        "re-pulling cannot make substituted bytes hash correctly"
    );
}

#[test]
fn the_retry_ladder_doubles_and_then_stops_climbing() {
    let delays: Vec<Duration> = (1..=8).map(retry_delay).collect();
    for pair in delays.windows(2) {
        assert!(pair[1] >= pair[0], "the ladder never goes backwards: {delays:?}");
    }
    assert_eq!(
        delays.last().copied().expect("eight rungs"),
        retry_delay(64),
        "and it is capped rather than climbing forever"
    );
}

// ── C-026 — the memory ceiling ──────────────────────────────────────────────

/// The ceiling is `max_blobs × largest_blob`, and this is the half of it this
/// module owns: no more than `max_blobs` bodies are ever in flight. The other
/// half — that the semaphore is run-scoped rather than per call — is
/// structural, and guarded at the bottom of this file.
#[tokio::test]
async fn no_more_than_max_blobs_bodies_are_in_flight_at_once() {
    const PERMITS: usize = 2;
    const TASKS: usize = 8;

    let semaphore = Semaphore::new(PERMITS);
    let in_flight = AtomicUsize::new(0);
    let peak = AtomicUsize::new(0);

    let work = (0..TASKS).map(|_| {
        with_blob_permit(&semaphore, async {
            let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(5)).await;
            in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        })
    });
    futures::future::try_join_all(work)
        .await
        .expect("the permit pool stays open");

    let observed = peak.load(Ordering::SeqCst);
    assert!(
        observed <= PERMITS,
        "{observed} bodies were in flight, ceiling is {PERMITS}"
    );
    // Not `<=` alone: a serial execution would satisfy that trivially, so the
    // test would pass with the pool doing nothing.
    assert_eq!(observed, PERMITS, "and the pool must actually be saturated");
}

/// The probe half of the ceiling. The permit pool above bounds the bodies, but
/// the destination probe deliberately sits outside it — and `layers[]` is
/// foreign data with no bound of its own, so one manifest at the 32 MiB ceiling
/// holds on the order of 2×10⁵ descriptors and would put that many
/// simultaneous `HEAD`s against the destination.
#[tokio::test]
async fn no_more_than_the_fanout_ceiling_of_blobs_is_probed_at_once() {
    const TASKS: usize = BLOB_FANOUT_CEILING * 3;

    let in_flight = AtomicUsize::new(0);
    let peak = AtomicUsize::new(0);

    let work = (0..TASKS).map(|_| async {
        let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        peak.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(5)).await;
        in_flight.fetch_sub(1, Ordering::SeqCst);
        Ok::<(), CopyError>(())
    });
    let outcomes = bounded_fanout(work).await.expect("nothing in this fixture fails");

    assert_eq!(outcomes.len(), TASKS, "every blob is still visited");
    let observed = peak.load(Ordering::SeqCst);
    assert!(
        observed <= BLOB_FANOUT_CEILING,
        "{observed} probes were in flight, ceiling is {BLOB_FANOUT_CEILING}"
    );
    // Not `<=` alone: a serial drive satisfies that trivially, and the whole
    // point of leaving the probe outside the blob permit is that it stays
    // concurrent enough to keep an all-present re-run fast.
    assert_eq!(
        observed, BLOB_FANOUT_CEILING,
        "and the fan-out must actually saturate the ceiling"
    );
}

// ── C-061 — the bounded referrer sweep ──────────────────────────────────────

/// The hop ceiling is a refusal, not a silent stop.
///
/// A mirror that quietly stopped carrying signatures past some undocumented
/// depth would be indistinguishable, downstream, from one whose upstream never
/// signed anything — which is the failure mode signing exists to make
/// impossible.
#[test]
fn a_referrer_past_the_hop_ceiling_is_refused() {
    let subject = digest_of(1);

    for hop in 0..=REFERRER_DEPTH_CEILING {
        assert!(
            within_referrer_depth(&subject, hop).is_ok(),
            "hop {hop} is inside the {REFERRER_DEPTH_CEILING}-hop ceiling"
        );
    }

    let error = within_referrer_depth(&subject, REFERRER_DEPTH_CEILING + 1)
        .expect_err("a referrer one hop past the ceiling must be refused");
    let CopyError::MalformedManifest(message) = &error else {
        panic!("a depth refusal is a malformed-source refusal, got {error}");
    };
    assert!(
        message.contains(&REFERRER_DEPTH_CEILING.to_string()),
        "the refusal names the ceiling it applied: {message}"
    );
    assert!(
        !error.is_whole_run_abort(),
        "a hostile referrer chain fails its package, never the run"
    );
}

/// The 65th referrer of one subject is where the sweep stops.
#[test]
fn a_subject_past_the_referrer_budget_is_refused() {
    let subject = digest_of(2);

    assert!(
        within_referrer_budget(&subject, REFERRER_COUNT_CEILING).is_ok(),
        "exactly {REFERRER_COUNT_CEILING} referrers is inside the budget"
    );

    let error = within_referrer_budget(&subject, REFERRER_COUNT_CEILING + 1)
        .expect_err("one referrer past the budget must be refused");
    let CopyError::ReferrerBudgetExceeded { subject: named, limit } = &error else {
        panic!("a budget refusal is its own variant, got {error}");
    };
    assert_eq!(named, &subject, "the refusal names the subject that overran");
    assert_eq!(*limit, REFERRER_COUNT_CEILING, "and the limit it applied");
    assert!(
        !error.is_whole_run_abort(),
        "a subject with too many referrers fails its package, never the run"
    );
}

/// Every field a fallback-index reader filters on survives the conversion.
///
/// `artifactType` and the annotations are exactly what
/// [sigstore/cosign#4641](https://github.com/sigstore/cosign/issues/4641)
/// reports cosign's own fallback write dropping: a descriptor without them
/// lists the referrer but makes it unfindable by kind.
#[test]
fn a_carried_referrer_descriptor_keeps_what_a_reader_filters_on() {
    let entry = ImageIndexEntry {
        media_type: "application/vnd.oci.image.manifest.v1+json".to_string(),
        digest: digest_of(3).to_string(),
        size: 1234,
        platform: None,
        annotations: Some(BTreeMap::from([(
            "org.opencontainers.foo".to_string(),
            "bar".to_string(),
        )])),
        artifact_type: Some("application/vnd.dev.cosign.simplesigning+json".to_string()),
    };

    let descriptor = referrer_descriptor(&entry);

    assert_eq!(descriptor.digest, entry.digest);
    assert_eq!(descriptor.size, entry.size);
    assert_eq!(descriptor.media_type, entry.media_type);
    assert_eq!(
        descriptor.artifact_type, entry.artifact_type,
        "artifactType is what a reader selects a signature by"
    );
    assert_eq!(
        descriptor.annotations, entry.annotations,
        "and the annotations are the rest of that selection"
    );
}

// ── C-062 — cosign sidecar tags ─────────────────────────────────────────────

/// The sidecar tag is the referrers fallback tag plus a suffix.
///
/// Not a coincidence worth leaving implicit: cosign's `sha256-<hex>.sig` and
/// the OCI fallback tag `sha256-<hex>` share a prefix exactly because a sha256
/// hex string is already 64 characters, so the spec's "truncate to 64"
/// truncates nothing. A future algorithm with a longer digest would move both.
#[test]
fn a_sidecar_tag_extends_the_referrers_fallback_tag() {
    let subject = digest_of(4);
    let fallback = ocx_lib::package::tag::referrer_fallback_tag(&subject);

    for suffix in SIDECAR_SUFFIXES {
        let tag = sidecar_tag(&subject, suffix);
        assert_eq!(tag, format!("{fallback}{suffix}"));
        assert!(
            is_legal_oci_tag(&tag),
            "a sidecar tag must be publishable at the destination: {tag}"
        );
    }

    assert!(
        SIDECAR_SUFFIXES.contains(&".sig"),
        "the signature suffix is the one every cosign user has"
    );
}

// ── C-063 — the destination's referrer route ────────────────────────────────

/// **404 and 405 both** route to the fallback tag.
///
/// The spec names 404, and a registry routing `/v2/<name>/referrers/…` to a
/// handler it does not implement answers 405 instead. Reading the second as a
/// transport fault would abort a run against a registry the fallback route
/// serves perfectly well — which is the whole reason this is two arms and not
/// one.
#[test]
fn a_destination_without_a_referrers_api_routes_to_the_fallback_tag() {
    let empty_listing = ocx_lib::oci::ImageIndex {
        schema_version: 2,
        media_type: Some("application/vnd.oci.image.index.v1+json".to_string()),
        manifests: Vec::new(),
        artifact_type: None,
        annotations: None,
    };
    assert_eq!(
        referrer_verdict(Ok(Some(empty_listing))).expect("a listing is an answer"),
        ReferrerDestination::Supported,
        "an answered listing — empty or not — means the API is there"
    );
    assert_eq!(
        referrer_verdict(Ok(None)).expect("the fork's own 404 verdict is an answer"),
        ReferrerDestination::Fallback,
        "`Ok(None)` is the fork reading a 404 off the status alone"
    );

    for status in [404, 405] {
        assert_eq!(
            referrer_verdict(Err(server_error(status)))
                .unwrap_or_else(|error| panic!("{status} is a verdict: {error}")),
            ReferrerDestination::Fallback,
            "{status} on the referrers route means the endpoint is not implemented"
        );
    }
}

/// Anything else is not a verdict, and guessing either way is worse.
#[test]
fn an_unanswered_referrers_probe_aborts_the_run() {
    for probe in [
        server_error(503),
        request_error(500),
        registry_error(OciErrorCode::Denied),
    ] {
        let error = referrer_verdict(Err(probe)).expect_err("a non-verdict must not be read as one");
        assert!(
            error.is_whole_run_abort(),
            "a run that cannot tell which referrer route the destination wants \
             would publish signatures nothing can find: {error}"
        );
    }
}

/// A `405` on a **subject-bearing** PUT is the ECR refusal, and only then.
///
/// The same status on an ordinary manifest is an ordinary rejection: it is the
/// `subject` field the destination is refusing, not these bytes, and
/// collapsing the two would report a package failure for content that copied.
#[test]
fn only_a_subject_bearing_push_reads_a_405_as_a_subject_refusal() {
    let destination = Reference::with_tag("dest.example".to_string(), "ns/pkg".to_string(), "1.0".to_string());

    let refused = push_failure(&destination, SubjectCarriage::Present, &server_error(405));
    let CopyError::SubjectRejected { registry, status } = &refused else {
        panic!("a 405 on a subject-bearing push is its own class, got {refused}");
    };
    assert_eq!(registry, "dest.example", "the refusal names the registry that refused");
    assert_eq!(*status, 405);
    assert!(
        !refused.is_whole_run_abort(),
        "a registry that refuses subjects refuses them for every package; \
         ending the run would mirror nothing at all"
    );

    assert!(
        matches!(
            push_failure(&destination, SubjectCarriage::Absent, &server_error(405)),
            CopyError::PushRejected(_)
        ),
        "a 405 on an ordinary manifest is an ordinary rejection"
    );
    assert!(
        matches!(
            push_failure(&destination, SubjectCarriage::Present, &server_error(500)),
            CopyError::PushRejected(_)
        ),
        "and a subject-bearing push can still fail for ordinary reasons"
    );
}

/// The transport seam gets a retry predicate of its own, read structurally.
///
/// `ClientError` collapses 429/502/503/504/timeout into one variant, so the
/// only honest predicate matches that variant — and it must **not** match an
/// authoritative not-found, which is the answer `destination_tag_digest` reads
/// as "no sidecar here yet" and would otherwise retry `max_retries` times
/// before reaching the same conclusion.
#[test]
fn only_a_transient_transport_failure_is_retried() {
    assert!(is_transient(&ClientError::RegistryTransient("503".into())));

    for settled in [
        ClientError::ManifestNotFound("sha256-…".to_string()),
        ClientError::RepositoryNotFound("ns/pkg".to_string()),
        ClientError::InvalidManifest("not an index".to_string()),
    ] {
        assert!(
            !is_transient(&settled),
            "an answered request must not be retried: {settled}"
        );
    }
}

/// The status is read off the error's **structure**, never its rendering.
///
/// ERR-13: the wording of an `OciDistributionError` is not a contract, and a
/// `contains("405")` against it would silently stop classifying on the next
/// fork bump — while still passing, because the number appears in the message.
#[test]
fn a_registry_status_is_read_structurally() {
    assert_eq!(registry_status(&server_error(405)), Some(405));
    assert_eq!(registry_status(&request_error(429)), Some(429));
    assert_eq!(
        registry_status(&registry_error(OciErrorCode::Denied)),
        None,
        "an OCI error envelope carries no status of its own"
    );
}

// ── C-040 — the two error classes ───────────────────────────────────────────

#[test]
fn only_a_non_authoritative_destination_read_aborts_the_run() {
    let aggregating: Vec<CopyError> = vec![
        CopyError::DigestMismatch {
            expected: digest_of(1),
            actual: digest_of(2),
        },
        CopyError::SourceUnavailable("connection reset".to_string()),
        CopyError::MalformedManifest("descriptor digest 'x' is not a digest".to_string()),
        CopyError::PushRejected("507 insufficient storage".to_string()),
        CopyError::SubjectRejected {
            registry: "dest.example".to_string(),
            status: 405,
        },
        CopyError::ReferrerBudgetExceeded {
            subject: digest_of(3),
            limit: REFERRER_COUNT_CEILING,
        },
        CopyError::ContentMissing { content: digest_of(4) },
    ];
    for error in aggregating {
        assert!(
            !error.is_whole_run_abort(),
            "a write failure fails its package, never the run: {error}"
        );
    }

    assert!(CopyError::Abort(MirrorError::TargetError("503".to_string())).is_whole_run_abort());
}

/// `Abort` is the one variant that wraps an error rather than describing one,
/// and the default `source()` drops that link — so anything walking the chain
/// (a `{err:#}` render, a downcast, an exit-code classifier) never reaches the
/// `MirrorError` the variant exists to carry.
#[test]
fn only_the_aborting_variant_hands_back_its_inner_error() {
    use std::error::Error as _;

    let abort = CopyError::Abort(MirrorError::TargetError("503".to_string()));
    let source = abort.source().expect("the wrapped error stays reachable");
    assert!(
        matches!(source.downcast_ref::<MirrorError>(), Some(MirrorError::TargetError(_))),
        "and it is the MirrorError itself, not a re-stringified copy"
    );

    for terminal in [
        CopyError::SourceUnavailable("connection reset".to_string()),
        CopyError::MalformedManifest("descriptor 'x' declares a negative size".to_string()),
        CopyError::ContentMissing { content: digest_of(1) },
    ] {
        assert!(
            terminal.source().is_none(),
            "{terminal} carries its context as text, so the chain ends here"
        );
    }
}

// ── Counters ────────────────────────────────────────────────────────────────

#[test]
fn only_an_upload_moves_bytes() {
    let mut stats = CopyStats::default();
    stats.record(BlobOutcome::Skipped, 500);
    stats.record(BlobOutcome::Mounted, 700);
    stats.record(BlobOutcome::Uploaded, 900);

    assert_eq!(
        stats,
        CopyStats {
            manifests: 0,
            blobs_skipped: 1,
            blobs_mounted: 1,
            blobs_uploaded: 1,
            bytes_uploaded: 900,
            referrers_copied: 0,
            sidecars_copied: 0,
            sidecar_conflicts: 0,
        },
        "a skipped or mounted blob transfers nothing"
    );
}

#[test]
fn child_counters_fold_into_the_parent() {
    let mut parent = CopyStats {
        manifests: 1,
        ..CopyStats::default()
    };
    parent.merge(CopyStats {
        manifests: 6,
        blobs_skipped: 2,
        blobs_mounted: 1,
        blobs_uploaded: 3,
        bytes_uploaded: 42,
        referrers_copied: 2,
        sidecars_copied: 1,
        sidecar_conflicts: 3,
    });

    assert_eq!(parent.manifests, 7);
    assert_eq!(parent.bytes_uploaded, 42);
    // The signature counters fold the same way: a referrer copied inside a
    // child walk is a referrer this package carried (C-064).
    assert_eq!(parent.referrers_copied, 2);
    assert_eq!(parent.sidecars_copied, 1);
    assert_eq!(parent.sidecar_conflicts, 3);
}

// ── C-046 — client construction ─────────────────────────────────────────────

/// `ClientConfig::default()` leaves both timeouts `None` and a different chunk
/// size; `ClientBuilder::new` sets all three, and a hand-built config inherits
/// none of it. On a multi-hour transfer a missing read timeout means one hung
/// socket stalls the whole mirror forever, which no test above would ever
/// notice.
#[test]
fn the_three_fields_client_config_default_leaves_unset_are_set() {
    let config = client_config();

    assert_eq!(config.push_chunk_size, 3 * 1024 * 1024);
    assert_eq!(config.read_timeout, Some(Duration::from_secs(120)));
    assert_eq!(config.connect_timeout, Some(Duration::from_secs(30)));

    let bare = native::ClientConfig::default();
    assert!(
        bare.read_timeout.is_none() && bare.connect_timeout.is_none(),
        "precondition: the fork's default is what leaves these unset"
    );
}

#[test]
fn the_source_client_is_guarded_and_the_destination_client_is_not() {
    let body = module_source_without_comments();

    assert_eq!(
        body.matches("dns_resolver").count(),
        1,
        "exactly one resolver assignment, and it is the source client's"
    );
    let source_start = body
        .find("pub async fn build_source_client")
        .expect("the source constructor");
    let destination_start = body
        .find("pub async fn build_destination_client")
        .expect("the destination constructor");
    let resolver_at = body.find("dns_resolver").expect("the resolver assignment");
    assert!(
        resolver_at > source_start && resolver_at < destination_start,
        "the guard belongs to the source constructor only — guarding the destination refuses \
         both the RFC1918 Artifactory and the harness's loopback registry"
    );
}

/// Both constructors must start from the one config, or the three fields above
/// drift apart between them and only one is covered by its test.
#[test]
fn every_client_is_built_from_the_one_config() {
    let body = module_source_without_comments();

    // `= native::ClientConfig {`, not `native::ClientConfig {`: the latter also
    // matches the return type of `client_config` followed by its own opening
    // brace, which made this guard read 2 for a module that has one literal.
    assert_eq!(
        body.matches("= native::ClientConfig {").count(),
        1,
        "there is exactly one `ClientConfig` literal in this module"
    );
    assert_eq!(
        body.matches("native::Client::new(").count(),
        1,
        "and exactly one construction site, which takes that config"
    );
}

// ── Structural guards ───────────────────────────────────────────────────────
//
// Each of these forbids a call that is silent when wrong and expensive when
// silent. They are tripwires for the likely accident, not the contract itself
// — the contract is the doc comment beside each call site.

/// The destination probe must never route through `ocx_lib::oci::Client`.
///
/// `Client::head_blob` opens with `transport_reference`, the `[mirrors]` seam.
/// With `OCX_MIRRORS` set, HEAD answers "present" from mirror host M, the
/// upload is skipped, the blob never reaches destination D, and the root is
/// written because content was "confirmed" — a tree that resolves nowhere
/// (CWE-345/367).
#[test]
fn the_destination_probe_never_reaches_for_the_mirror_rewriting_client() {
    let body = module_source_without_comments();

    assert!(
        !body.contains("head_blob"),
        "the probe must use the fork's `fetch_blob_size`, never ocx's mirror-aware `head_blob`"
    );
    assert!(
        body.contains("fetch_blob_size"),
        "and the guard is worthless if the call it protects has been renamed away"
    );
}

/// A platform filter is correct for resolution and catastrophic for a mirror:
/// the index travels verbatim, so a skipped child publishes an index
/// referencing a manifest that was never copied.
#[test]
fn the_copy_walk_applies_no_platform_predicate() {
    let body = module_source_without_comments();

    for forbidden in ["candidate_from_descriptor", "fetch_candidates", "from_image_index"] {
        assert!(
            !body.contains(forbidden),
            "`{forbidden}` drops platform-less descriptors — a mirror filters nothing"
        );
    }
}

/// Re-deriving the cascade computes a *different* one from the mirror's own
/// filtered subset, and `Version::parse` reads a bare date stamp as a
/// major-only version that outranks every dotted release and wins `latest`.
#[test]
fn the_copy_path_classifies_no_tag() {
    let body = module_source_without_comments();

    for forbidden in ["AliasTag", "resolve_cascade_tags", "variant_names", "decompose"] {
        assert!(
            !body.contains(forbidden),
            "`{forbidden}` classifies; a mirror transports"
        );
    }
}

/// The memory ceiling's other half: the permit pool is handed in per run, never
/// constructed here. A per-call pool with 44 concurrent tags is 44 × 4 × 221 MB
/// — and nothing about the code would look different.
#[test]
fn the_blob_permit_pool_is_never_constructed_inside_the_copy_engine() {
    let body = module_source_without_comments();

    assert!(
        !body.contains("Semaphore::new"),
        "the pool must come from `CopyContext::blob_semaphore`, which is run-scoped"
    );
    assert!(
        body.contains("blob_semaphore"),
        "and the guard is worthless if the field it protects has been renamed away"
    );
}

// ── C-022 — the recursion depth ceiling ─────────────────────────────────────

/// A chain of nested image indexes is refused before the stack is exhausted.
///
/// `manifests[]` entries may themselves be image indexes, and a descriptor says
/// nothing about how deep the chain below it goes — so the walk's depth is
/// chosen by the *source*. A few hundred bytes of authored JSON is enough to
/// recurse until the stack dies. Digest cycles are not the risk (a child's
/// digest covers its parent's bytes, so a cycle needs a sha256 preimage);
/// depth alone is.
#[test]
fn a_manifest_nested_past_the_ceiling_is_refused() {
    let digest = digest_of(0);

    for depth in 0..=MANIFEST_DEPTH_CEILING {
        assert!(
            within_depth(&digest, depth).is_ok(),
            "depth {depth} is within the ceiling and must be accepted — every real \
             multi-platform artifact is depth 1, and an index of indexes is depth 2"
        );
    }

    let error = within_depth(&digest, MANIFEST_DEPTH_CEILING + 1).expect_err("one past the ceiling is refused");
    let rendered = error.to_string();
    assert!(
        rendered.contains(&digest.to_string()) && rendered.contains("deep"),
        "the refusal must name the offending manifest and say why: {rendered}"
    );
}

// ── C-023 — the blob body is held to its declared size ──────────────────────

/// A body longer than its descriptor declared is refused mid-stream.
///
/// `BLOB_SIZE_CEILING` bounds the *declaration*, which bounds the
/// pre-allocation. It bounds nothing about the **stream**: `pull_blob` writes
/// the whole response into the sink and the digest is only checked afterwards,
/// so a source declaring 1 KiB and then streaming forever exhausts memory
/// before any verification runs. The declared size is a claim by the same party
/// sending the bytes — this is what makes it load-bearing.
#[tokio::test]
async fn a_blob_body_longer_than_its_declared_size_is_refused() {
    use tokio::io::AsyncWriteExt;

    // Under the declaration: accepted, and the bytes land verbatim.
    let mut buffer = Vec::new();
    let mut sink = BoundedSink {
        buffer: &mut buffer,
        remaining: 8,
    };
    sink.write_all(b"12345").await.expect("a body within its declaration");
    assert_eq!(buffer, b"12345", "accepted bytes must reach the buffer unchanged");

    // The running budget, not just the per-chunk check. reqwest yields many
    // small frames, so a sink that checks each chunk against `remaining` but
    // never *spends* it accepts unlimited total bytes while passing every
    // individual check — which is the unbounded-memory defect this exists to
    // stop. One oversized write would not catch that; an accumulation past the
    // budget in chunks that each fit is the only shape that does.
    let mut buffer = Vec::new();
    let mut sink = BoundedSink {
        buffer: &mut buffer,
        remaining: 10,
    };
    for chunk in 0..3 {
        sink.write_all(b"abc")
            .await
            .unwrap_or_else(|error| panic!("chunk {chunk} is within the running budget: {error}"));
    }
    let error = sink
        .write_all(b"abcd")
        .await
        .expect_err("the fourth chunk crosses the budget even though it fits on its own");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData, "{error}");
    assert_eq!(
        buffer.len(),
        9,
        "a refused chunk must not be partially written: {buffer:?}"
    );

    // Past it in one write: refused rather than truncated. Truncating would
    // hand the digest check a body the source never sent and blame the wrong
    // party.
    let mut buffer = Vec::new();
    let mut sink = BoundedSink {
        buffer: &mut buffer,
        remaining: 4,
    };
    let error = sink
        .write_all(b"123456789")
        .await
        .expect_err("a body past its declared size must be refused");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData, "{error}");
    assert!(
        error.to_string().contains("longer than its descriptor declared"),
        "the refusal must say which party lied: {error}"
    );
}

/// The `--dry-run` measurement walks the signature surface too (C-065).
///
/// The estimate exists so an operator can size a first sync, and a run that
/// measured only the manifests while the real run also moved every signature
/// would under-report by the whole signature surface. Both halves are asserted
/// because they fail independently: the referrer sweep and the sidecar sweep
/// are separate loops, and dropping either leaves the other still measuring.
///
/// Structural, and the reason is what makes `--dry-run` cheap in the first
/// place: `missing_descriptors` reaches the registry on every path, so there
/// is no value to assert on without two live clients. This guard therefore
/// asserts that the measurement *walks* the signature surface, never what it
/// computes — a total wrong by a factor of two passes it unchanged. The
/// arithmetic itself is the acceptance suite's
/// (`test_a_dry_run_over_a_signed_package_measures_and_fails_nothing`), which
/// seeds a referrer carrying blobs its subject does not via `put_blob` and
/// asserts the estimate grows by exactly their length.
#[test]
fn the_dry_run_measurement_walks_referrers_and_sidecars() {
    let body = module_source_without_comments();

    let walk = function_body(&body, "missing_descriptors_at");
    assert!(
        walk.contains("missing_signature_descriptors("),
        "the measuring walk must reach the signature surface, or `--dry-run` under-reports \
         a signed mirror by every byte its signatures move"
    );

    let measured = function_body(&body, "missing_signature_descriptors");
    assert!(
        measured.contains("source_referrers("),
        "the measurement must enumerate referrers"
    );
    assert!(
        measured.contains("SIDECAR_SUFFIXES"),
        "the measurement must enumerate the cosign sidecar tags"
    );
    // The same ceilings as the copy: a hostile referrer graph must not be
    // walkable through the cheap path just because it moves no bytes.
    for bound in ["within_referrer_budget(", "within_referrer_depth("] {
        assert!(
            measured.contains(bound),
            "`{bound}` must bound the measurement too — an unbounded walk is unbounded \
             whether or not it transfers anything"
        );
    }
}

/// Both sweeps run only **after** the subject's own manifest has been pushed.
///
/// C-060, and it is the ordering the whole feature rests on: a referrer names
/// its subject, so publishing one before the subject landed leaves the
/// destination holding a signature over content it does not have — precisely
/// the state a registry with referrer garbage collection is entitled to
/// delete. The sidecar sweep is second only because its tag is derived from
/// the same digest.
///
/// Structural, and for the same reason as the guard below: the seam is
/// `OciTransport`, ocx_lib's own double for it is `pub(crate)` there, and this
/// crate has no `async-trait` dependency to write one against — so there is no
/// call ordering to observe. Anchored on the two sweep calls by name rather
/// than an ordinal, so adding a third call site cannot silently re-point it.
#[test]
fn the_subject_is_pushed_before_either_sweep_runs() {
    let body = module_source_without_comments();
    let walk = function_body(&body, "copy_manifest_tree_at");

    let push = walk
        .find("push_manifest(")
        .expect("the walk must push the subject's own manifest, or this guard tests nothing");
    for sweep in ["copy_referrers(", "copy_sidecars("] {
        let call = walk
            .find(sweep)
            .unwrap_or_else(|| panic!("`{sweep}` must be reached from the copy walk"));
        assert!(
            push < call,
            "`{sweep}` must sit after `push_manifest`; sweeping first advertises a \
             signature over a subject the destination does not hold yet"
        );
    }

    // The guard is only worth anything while the sweeps are reachable at all:
    // the `hop` gate is what turns them off for a sidecar's own subtree, and a
    // gate that stopped matching would leave the ordering above vacuously true.
    assert!(
        walk.contains("if let Some(hop) = carriage.hop"),
        "the sweeps must stay gated on the carriage's hop, or a sidecar walks its own sidecars"
    );
}

/// A referrer claim that fails is given back before the error propagates.
///
/// Structural rather than behavioural because the claim lives on
/// `CopyContext`, whose `destination_transport` is a `Box<dyn OciTransport>`
/// with no double reachable from this crate — the same wall every other guard
/// in this file names. The property is an *absence*: no path may leave the
/// claim behind, and a claim outliving its own failure is invisible to any
/// single-walk assertion, because the wrong answer is only given to the
/// *second* walk over that digest, which then reports success for a referrer
/// it neither copied nor indexed.
#[test]
fn a_failed_referrer_carry_gives_its_claim_back() {
    let body = module_source_without_comments();
    let sweep = function_body(&body, "copy_referrers");

    let claim = sweep
        .find("claim_referrer(")
        .expect("the sweep must claim each referrer, or this guard tests nothing");
    let release = sweep
        .find("release_referrer(")
        .expect("a failed carry must release its claim; without this a later walk skips the referrer");
    assert!(
        claim < release,
        "the release must follow the claim it undoes, not precede it"
    );
    assert!(
        sweep[..release].contains("Err(error) =>"),
        "the release must sit on the error arm; releasing unconditionally would defeat C-061's dedup"
    );

    // Scope check: the guard reads one function, so it is only sound while that
    // function is the only claimant. A second call site elsewhere would need its
    // own release and this guard would never look at it.
    assert_eq!(
        // The trailing comma is what separates the call from the definition,
        // whose parameter list reads `claim_referrer(context: &CopyContext`.
        body.matches("claim_referrer(context,").count(),
        1,
        "`claim_referrer` must have exactly one call site, or this guard's scope is wrong"
    );
}

/// A fallback index entry is written only **after** the referrer itself landed.
///
/// The order is the whole contract: an entry appended before the push makes the
/// destination advertise a referrer whose manifest is not there, and a reader
/// resolving it gets a 404 from a mirror that told it the signature exists.
/// Because `copy_manifest_tree_at` short-circuits on `?`, "after" is also what
/// makes a failed push skip the append entirely — including the
/// [`CopyError::SubjectRejected`] case, where a destination refusing the
/// `subject` shape must not then be handed a fallback pointer to nothing.
///
/// Structural, and it has to be: the seam is `OciTransport`, ocx_lib's own
/// double for it is crate-private there, and this crate has no `async_trait`
/// dependency to write one against — so there is nothing to call.
#[test]
fn a_fallback_entry_is_appended_only_after_its_referrer_landed() {
    let body = module_source_without_comments();
    let sweep = function_body(&body, "copy_referrers");

    let push = sweep
        .find("copy_manifest_tree_at(")
        .expect("the referrer sweep must copy the referrer's own manifest tree");
    let append = sweep
        .find("append_fallback(")
        .expect("the referrer sweep must be what appends to the fallback index");
    assert!(
        push < append,
        "`append_fallback` must sit after the referrer's own push, so a rejected \
         push short-circuits before the destination is told the referrer exists"
    );
    assert!(
        sweep[push..append].contains(".await?"),
        "the push between them must propagate with `?`; an ignored result lets the \
         append advertise a manifest that never landed:\n{}",
        &sweep[push..append]
    );

    // The append has exactly one caller, so the ordering above is the whole
    // story rather than one path of several.
    let callers: Vec<&str> = ["copy_referrers", "copy_sidecars", "copy_manifest_tree_at"]
        .into_iter()
        .filter(|name| function_body(&body, name).contains("append_fallback("))
        .collect();
    assert_eq!(
        callers,
        vec!["copy_referrers"],
        "only the referrer sweep may write the fallback index"
    );
}

/// Both manifest walks must **descend** the counter, not merely carry it.
///
/// `within_depth` has its own test, but a test of the predicate says nothing
/// about its use: recursing with `depth` instead of `depth + 1` leaves every
/// call at level 0, so the ceiling is never reached and the unbounded recursion
/// is reinstated silently. No unit test can catch that — both walks fetch over
/// the network before they recurse — so it is asserted structurally.
#[test]
fn both_manifest_walks_descend_the_depth_counter() {
    let body = module_source_without_comments();

    for walk in ["copy_manifest_tree_at", "missing_descriptors_at"] {
        // Anchored on the definition and on the recursion's own argument, not
        // on an occurrence ordinal: the referrer and sidecar sweeps added call
        // sites to both walks, and an `nth(2)` guard silently walks onto the
        // wrong one every time another is added.
        let body = function_body(&body, walk);

        // The child-manifest recursion, and only it. The signature sweeps
        // deliberately re-enter at the *same* manifest depth — a signature is
        // not nested inside the index it signs — so a blanket "every call
        // passes depth + 1" would assert the opposite of the contract.
        let recursion = body
            .find("&child_source")
            .unwrap_or_else(|| panic!("`{walk}` should recurse into a `child_source` it addressed"));
        let call = body[..recursion]
            .rfind(walk)
            .unwrap_or_else(|| panic!("`{walk}`'s child recursion should call `{walk}`"));
        let tail = &body[call..];
        let call_end = tail
            .find("))")
            .unwrap_or_else(|| panic!("`{walk}`'s recursive call should be delimited"));
        assert!(
            tail[..call_end].contains("depth + 1"),
            "`{walk}` must recurse with `depth + 1`; carrying `depth` unchanged pins every \
             level at 0 and reinstates the unbounded recursion MANIFEST_DEPTH_CEILING exists \
             to stop:\n{}",
            &tail[..call_end]
        );
    }

    // Canary: if either walk is renamed, the loop above silently stops
    // asserting anything, so pin that both entry points still exist.
    assert!(
        body.contains("async fn copy_manifest_tree_at") && body.contains("async fn missing_descriptors_at"),
        "both depth-aware walks must exist under these names, or this guard tests nothing"
    );
}

/// The security boundary of `ensure_source_auth` (the slug-collision leak): a
/// physical host the operator did not name gets no resolved credential, so a
/// hostile root pointing at a lookalike of a real registry cannot exfiltrate
/// the operator's token to it.
#[test]
fn only_an_operator_named_host_is_credentialed() {
    // The source's own `registry:` plus its `trusted_hosts` — the exact
    // allow-list `sync_package` builds.
    let allowed = ["ocx.sh", "registry.corp.example.com"];

    assert!(
        host_is_credentialed("registry.corp.example.com", &allowed),
        "a host the operator named is credentialed"
    );
    assert!(
        host_is_credentialed("ocx.sh", &allowed),
        "the source's own registry is credentialed"
    );
    // `to_slug` collapses both of these onto `registry_corp_example_com`; the
    // lookalike is a different string and is not in the list, so it must not be
    // credentialed even though its slug collides with the named host's.
    assert!(
        !host_is_credentialed("registry-corp-example.com", &allowed),
        "a slug-colliding lookalike host gets the anonymous path, never the operator's credential"
    );
    assert!(
        !host_is_credentialed("ghcr.io", &allowed),
        "an unnamed host — the public ocx.sh → ghcr.io case — takes the anonymous path"
    );
}
