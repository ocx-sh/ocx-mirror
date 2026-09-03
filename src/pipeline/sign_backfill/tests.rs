// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Contract tests for the signature backfill — C-067, C-068, C-069, C-070.
//!
//! Everything here is pure: the filter, the two-pass plan, the counters, the
//! exit derivation and the wire envelope. The registry read they sit either
//! side of is `test/tests/test_signing_backfill.py`'s job, against a live
//! registry and a live Sigstore stack — a fake would only prove that this file
//! agrees with itself.

use super::*;

// ── Fixtures ────────────────────────────────────────────────────────────────

/// A digest that parses, distinct per `seed`.
fn digest(seed: u8) -> Digest {
    ocx_lib::oci::Algorithm::Sha256.hash([seed])
}

fn platform(text: &str) -> Platform {
    text.parse().expect("test platform literal parses")
}

fn candidate(discovery: DiscoveryMethod) -> SignatureCandidate {
    SignatureCandidate::found(discovery)
}

/// A candidate carrying a keyless identity, built the way production builds
/// one — through `From<SignerCandidate>`, so the narrowing tests below
/// exercise the real mapping rather than a mirror-local literal that could
/// agree with a broken one.
fn signed_by(discovery: DiscoveryMethod, identity: Option<&str>, issuer: Option<&str>) -> SignatureCandidate {
    let mut upstream = SignerCandidate::new(discovery, digest(9));
    if let Some(identity) = identity {
        upstream = upstream.with_certificate_identity(identity);
    }
    if let Some(issuer) = issuer {
        upstream = upstream.with_certificate_issuer(issuer);
    }
    SignatureCandidate::from(upstream)
}

/// The filter the operator gets from `--identity` / `--issuer`.
fn narrowed(identity: Option<&str>, issuer: Option<&str>) -> SignFilter {
    SignFilter {
        force: false,
        identity: identity.map(str::to_string),
        issuer: issuer.map(str::to_string),
    }
}

fn item(tag: &str, platform: Option<&str>, status: ItemStatus) -> ItemReport {
    ItemReport {
        tag: tag.to_string(),
        platform: platform.map(str::to_string),
        status,
        subject: digest(1).to_string(),
        discovery: None,
        reason: None,
        error: None,
    }
}

fn failed_item(tag: &str, exit: i32) -> ItemReport {
    let mut row = item(tag, None, ItemStatus::Failed);
    row.error = Some(ItemError {
        code: ErrorCategory::from_exit_code(crate::error::sign_exit_code(exit)),
        exit,
    });
    row
}

// ── C-067 · already_signed ──────────────────────────────────────────────────

/// The default verdict: any candidate at all means signed.
///
/// Every candidate a producer lists is signature-class by construction
/// (`.att`/`.sbom` are filtered out upstream), so the filter reads presence
/// and nothing else.
#[test]
fn any_candidate_counts_as_signed_by_default() {
    let filter = SignFilter::default();
    for discovery in [
        DiscoveryMethod::ReferrersApi,
        DiscoveryMethod::FallbackTag,
        DiscoveryMethod::SidecarTag,
    ] {
        assert!(
            already_signed(&[candidate(discovery)], &filter),
            "a {discovery} candidate must count as a signature",
        );
    }
}

/// The case the whole command exists for: nothing attached, so sign it.
#[test]
fn an_empty_listing_is_not_signed() {
    assert!(!already_signed(&[], &SignFilter::default()));
}

/// `--force` is how a second identity joins an existing signature, so it must
/// override a *present* candidate — a force that only affected empty listings
/// would be a no-op flag.
#[test]
fn force_overrides_a_present_candidate() {
    let filter = SignFilter {
        force: true,
        ..SignFilter::default()
    };
    assert!(!already_signed(&[candidate(DiscoveryMethod::ReferrersApi)], &filter));
    assert!(!already_signed(&[], &filter));
}

/// Several candidates on one subject is the ordinary multi-signature state,
/// not a reason to sign again.
#[test]
fn several_candidates_are_still_signed() {
    let candidates = [
        candidate(DiscoveryMethod::ReferrersApi),
        candidate(DiscoveryMethod::SidecarTag),
    ];
    assert!(already_signed(&candidates, &SignFilter::default()));
}

// ── C-067 · the narrowing arms (--identity / --issuer) ──────────────────────

/// The rotation case the flags exist for: a signature by somebody else does
/// not satisfy `--identity`, so the subject is signed again.
///
/// This is the deliberate reading of "already signed" under a narrowing
/// filter — *signed by the identity this run cares about*, not *signed by
/// anyone*. An operator who has rotated identities is asking for their own
/// signature to be present, and the fail-safe direction for a backfill is a
/// redundant re-sign rather than a subject skipped on a stranger's evidence.
#[test]
fn a_foreign_identity_does_not_satisfy_an_identity_filter() {
    let candidates = [signed_by(DiscoveryMethod::ReferrersApi, Some("ci@old.example"), None)];

    assert!(
        already_signed(&candidates, &narrowed(Some("ci@old.example"), None)),
        "the identity that signed it must satisfy the filter",
    );
    assert!(
        !already_signed(&candidates, &narrowed(Some("ci@new.example"), None)),
        "a signature by a different identity must not skip the subject",
    );
    // Unnarrowed, presence alone still decides — the flags narrow, they never
    // widen, so the default must be unchanged by their existence.
    assert!(already_signed(&candidates, &SignFilter::default()));
}

/// `--issuer` narrows on its own, on the same rule as `--identity`.
#[test]
fn a_foreign_issuer_does_not_satisfy_an_issuer_filter() {
    let candidates = [signed_by(
        DiscoveryMethod::ReferrersApi,
        Some("ci@example.com"),
        Some("https://token.actions.githubusercontent.com"),
    )];

    assert!(already_signed(
        &candidates,
        &narrowed(None, Some("https://token.actions.githubusercontent.com")),
    ));
    assert!(!already_signed(
        &candidates,
        &narrowed(None, Some("https://gitlab.example"))
    ));
}

/// Both flags are **AND**, and both must hold on **one** candidate.
///
/// Two candidates each satisfying one half is not a match: nothing signed
/// this subject with the pair the operator named, and reading the two halves
/// off different signatures would skip a subject that carries neither.
#[test]
fn both_flags_must_match_the_same_candidate() {
    let candidates = [
        signed_by(
            DiscoveryMethod::ReferrersApi,
            Some("ci@a.example"),
            Some("https://x.example"),
        ),
        signed_by(
            DiscoveryMethod::ReferrersApi,
            Some("ci@b.example"),
            Some("https://y.example"),
        ),
    ];

    assert!(
        already_signed(&candidates, &narrowed(Some("ci@a.example"), Some("https://x.example")),),
        "one candidate carries both halves, so the subject is signed",
    );
    assert!(
        !already_signed(&candidates, &narrowed(Some("ci@a.example"), Some("https://y.example")),),
        "the halves come off two different candidates, which is not a match",
    );
}

/// A candidate with no identity at all must never satisfy a narrowing filter.
///
/// The `.sig` sidecar is the concrete case — its certificate lives in a
/// per-layer annotation and upstream deliberately leaves all three fields
/// `None` — and an unparseable bundle produces the same shape. Matching it
/// would skip a subject on evidence that says nothing about who signed it.
#[test]
fn a_candidate_with_no_identity_never_matches_a_narrowing_filter() {
    let candidates = [signed_by(DiscoveryMethod::SidecarTag, None, None)];

    assert!(!already_signed(&candidates, &narrowed(Some("ci@example.com"), None)));
    assert!(!already_signed(&candidates, &narrowed(None, Some("https://x.example"))));
    // Still a signature for the default filter: presence is all that reads.
    assert!(already_signed(&candidates, &SignFilter::default()));
}

/// `--force` outranks a narrowing filter that would otherwise skip (C-067).
#[test]
fn force_outranks_a_matching_narrowing_filter() {
    let candidates = [signed_by(
        DiscoveryMethod::ReferrersApi,
        Some("ci@example.com"),
        Some("https://x.example"),
    )];
    let filter = SignFilter {
        force: true,
        ..narrowed(Some("ci@example.com"), Some("https://x.example"))
    };

    assert!(!already_signed(&candidates, &filter));
}

/// The skipped row names the candidate the filter matched, not the first found.
///
/// Under `--identity`/`--issuer` the deciding candidate is rarely the first
/// one the registry returned, and `discovery` is what an operator reads to
/// find the signature that made the run skip. `candidates.first()` names a
/// signature the filter *rejected*, sending them to the wrong artifact.
#[test]
fn the_skipped_rows_discovery_is_the_candidate_the_filter_matched() {
    let candidates = [
        signed_by(DiscoveryMethod::SidecarTag, Some("someone@else.example"), None),
        signed_by(DiscoveryMethod::ReferrersApi, Some("ci@example.com"), None),
    ];
    let filter = narrowed(Some("ci@example.com"), None);

    assert!(already_signed(&candidates, &filter), "the second candidate matches");
    assert_eq!(
        matching_candidate(&candidates, &filter).map(|candidate| candidate.discovery),
        Some(DiscoveryMethod::ReferrersApi),
    );
    // And the unnarrowed case still reports the first, which is the only
    // candidate the default filter can be said to have decided on.
    assert_eq!(
        matching_candidate(&candidates, &SignFilter::default()).map(|candidate| candidate.discovery),
        Some(DiscoveryMethod::SidecarTag),
    );
    assert!(matching_candidate(&[], &filter).is_none());
}

/// The upstream mapping keeps every field the filter and the report can read.
///
/// `public_key_hint` in particular: no flag narrows on it today, so nothing
/// else would notice the mapping dropping it, and it is the field a key-mode
/// operator's future filter would read.
#[test]
fn the_upstream_mapping_preserves_every_field() {
    let upstream = SignerCandidate::new(DiscoveryMethod::FallbackTag, digest(3))
        .with_artifact_type("application/vnd.dev.sigstore.bundle.v0.3+json")
        .with_certificate_identity("ci@example.com")
        .with_certificate_issuer("https://x.example")
        .with_public_key_hint("c0ffee");

    let mapped = SignatureCandidate::from(upstream);

    assert_eq!(mapped.discovery, DiscoveryMethod::FallbackTag);
    assert_eq!(
        mapped.artifact_type.as_deref(),
        Some("application/vnd.dev.sigstore.bundle.v0.3+json"),
    );
    assert_eq!(mapped.certificate_identity.as_deref(), Some("ci@example.com"));
    assert_eq!(mapped.certificate_issuer.as_deref(), Some("https://x.example"));
    assert_eq!(mapped.public_key_hint.as_deref(), Some("c0ffee"));
}

// ── C-074 / the handoff constraint · plan_targets ───────────────────────────

/// **The cascade collapse.** Four tags at one index digest are one index
/// target, not four.
///
/// Keying on the tag would file four referrers against one subject and spend
/// half of ocx's eight-candidate verifier budget in a single run — the defect
/// the WP 2 finding named.
#[test]
fn one_index_digest_yields_one_index_target() {
    let index = digest(1);
    let published: Vec<PublishedTag> = ["3.28.1", "3.28", "3", "latest"]
        .iter()
        .map(|tag| PublishedTag {
            tag: (*tag).to_string(),
            digest: index.clone(),
            children: vec![(platform("linux/amd64"), digest(2))],
        })
        .collect();

    let (indexes, platforms) = plan_targets(&published);

    assert_eq!(indexes.len(), 1, "four aliases at one digest are one subject");
    assert_eq!(indexes[0].subject, index);
    assert_eq!(
        platforms.len(),
        1,
        "the shared child is one subject too, got {platforms:?}"
    );
}

/// Two genuinely different versions stay two subjects.
#[test]
fn distinct_index_digests_stay_distinct_targets() {
    let published = vec![
        PublishedTag {
            tag: "3.28.1".to_string(),
            digest: digest(1),
            children: vec![(platform("linux/amd64"), digest(2))],
        },
        PublishedTag {
            tag: "3.29.0".to_string(),
            digest: digest(3),
            children: vec![(platform("linux/amd64"), digest(4))],
        },
    ];

    let (indexes, platforms) = plan_targets(&published);

    assert_eq!(indexes.len(), 2);
    assert_eq!(platforms.len(), 2);
}

/// Two distinct indexes sharing one child manifest sign that child once.
///
/// The digest dedupe on the *index* side cannot reach this: these are two
/// different index digests, so the collapse in
/// `one_index_digest_yields_one_index_target` runs the children loop twice.
/// A shared child is what a re-tag or a `pipeline patch` re-emission produces,
/// and signing it twice files a second referrer against one subject.
#[test]
fn a_child_shared_by_two_indexes_is_one_platform_target() {
    let shared_child = digest(9);
    let published = vec![
        PublishedTag {
            tag: "3.28.1".to_string(),
            digest: digest(1),
            children: vec![(platform("linux/amd64"), shared_child.clone())],
        },
        PublishedTag {
            tag: "3.29.0".to_string(),
            digest: digest(2),
            children: vec![(platform("linux/amd64"), shared_child.clone())],
        },
    ];

    let (indexes, platforms) = plan_targets(&published);

    assert_eq!(indexes.len(), 2, "two index digests are two subjects");
    assert_eq!(platforms.len(), 1, "one child digest is one subject, got {platforms:?}");
    assert_eq!(platforms[0].subject, shared_child);
}

/// A tag pointing straight at a manifest that is also a child of an index
/// yields one target, not two.
///
/// The two passes dedupe against one set, so the same subject digest reached
/// as an index and as a child cannot be signed twice in one run. Reached in
/// production by any bare-manifest tag over a published child — which is what
/// a reserved keep tag is, before `signing_tags` drops it.
#[test]
fn a_digest_reachable_both_ways_is_one_target() {
    let child = digest(9);
    let published = vec![
        PublishedTag {
            tag: "3.28.1".to_string(),
            digest: digest(1),
            children: vec![(platform("linux/amd64"), child.clone())],
        },
        PublishedTag {
            tag: "pinned".to_string(),
            digest: child.clone(),
            children: Vec::new(),
        },
    ];

    let (indexes, platforms) = plan_targets(&published);

    let subjects: Vec<&Digest> = indexes.iter().chain(&platforms).map(|target| &target.subject).collect();
    assert_eq!(subjects.len(), 2, "{indexes:?} {platforms:?}");
    assert_eq!(subjects.iter().filter(|subject| **subject == &child).count(), 1);
}

/// The representative tag is deterministic, so a run's report and its child
/// invocations do not reshuffle between runs over identical registry state.
#[test]
fn the_representative_tag_is_the_lowest_sorting_one() {
    let index = digest(1);
    let published: Vec<PublishedTag> = ["latest", "3", "3.28.1"]
        .iter()
        .map(|tag| PublishedTag {
            tag: (*tag).to_string(),
            digest: index.clone(),
            children: Vec::new(),
        })
        .collect();

    let (indexes, _) = plan_targets(&published);

    assert_eq!(indexes.len(), 1);
    assert_eq!(indexes[0].tag, "3");
}

/// A single-platform publish is a bare manifest: no children, and the manifest
/// is itself the only subject. It must still be signed — with no `-p`, which
/// is what `platform: None` on an index target means.
#[test]
fn a_bare_manifest_tag_is_an_index_target_with_no_children() {
    let published = vec![PublishedTag {
        tag: "3.28.1".to_string(),
        digest: digest(1),
        children: Vec::new(),
    }];

    let (indexes, platforms) = plan_targets(&published);

    assert_eq!(indexes.len(), 1);
    assert_eq!(indexes[0].platform, None, "a bare manifest is signed without -p");
    assert!(platforms.is_empty());
}

/// A platform target carries the tag it will be narrowed from and the child
/// digest the signature attaches to — the two are different values and a row
/// that confused them would skip on the wrong listing.
#[test]
fn a_platform_target_names_its_parent_tag_and_its_own_subject() {
    let published = vec![PublishedTag {
        tag: "3.28.1".to_string(),
        digest: digest(1),
        children: vec![(platform("linux/arm64"), digest(9))],
    }];

    let (_, platforms) = plan_targets(&published);

    assert_eq!(platforms.len(), 1);
    assert_eq!(platforms[0].tag, "3.28.1");
    assert_eq!(platforms[0].platform, Some(platform("linux/arm64")));
    assert_eq!(platforms[0].subject, digest(9));
}

/// A row carries its target's identity — including the platform that decides
/// whether the child invocation gets a `-p`.
///
/// The two `SignTarget` shapes map to the two row shapes, and nothing else in
/// this file exercises that constructor: every other test builds rows by hand.
#[test]
fn a_row_carries_its_targets_tag_platform_and_subject() {
    let index = SignTarget {
        tag: "3.28.1".to_string(),
        platform: None,
        subject: digest(1),
    };
    let row = ItemReport::for_target(&index, ItemStatus::Succeeded);
    assert_eq!(row.tag, "3.28.1");
    assert_eq!(row.platform, None);
    assert_eq!(row.subject, digest(1).to_string());
    assert_eq!(row.status, ItemStatus::Succeeded);

    let child = SignTarget {
        tag: "3.28.1".to_string(),
        platform: Some(platform("linux/arm64")),
        subject: digest(2),
    };
    let row = ItemReport::for_target(&child, ItemStatus::Skipped);
    assert_eq!(row.platform.as_deref(), Some("linux/arm64"));
    assert_eq!(row.subject, digest(2).to_string());
}

// ── C-069 · the worst classified failure ────────────────────────────────────

/// The pinned case: a run that hit both a transparency-log outage and a data
/// error exits **83**.
///
/// 83 tells CI "run me again"; 65 tells CI "a human must read the report".
/// When both happened, 83 is the answer that gets the remaining work done —
/// and the 65 rows are still in `items`.
#[test]
fn a_transparency_log_outage_outranks_a_data_error() {
    assert_eq!(worst_exit(&[65, 83]), ExitCode::TransparencyLogUnavailable);
    assert_eq!(worst_exit(&[83, 65]), ExitCode::TransparencyLogUnavailable);
}

/// PKG-28: retries-exhausted-on-transient and hard not-found never collapse
/// onto one generic code.
#[test]
fn temp_fail_and_not_found_stay_distinct() {
    assert_ne!(worst_exit(&[75]) as u8, worst_exit(&[79]) as u8);
    assert_eq!(worst_exit(&[75]), ExitCode::TempFail);
    assert_eq!(worst_exit(&[79]), ExitCode::NotFound);
}

/// A code this binary predates degrades to `Failure`, never to a wrong
/// meaning — the same fall-through `sign_exit_code` already takes.
#[test]
fn an_unknown_child_code_falls_through_to_failure() {
    assert_eq!(worst_exit(&[99]), ExitCode::Failure);
}

/// No failures is exit 0. Asserted rather than assumed: a batch that exits
/// non-zero on an all-skipped run would red every scheduled convergent pass.
#[test]
fn no_failures_is_success() {
    assert_eq!(worst_exit(&[]), ExitCode::Success);
}

/// The exit code is never derived from how many items there were — PKG-24's
/// counting trap. Ten identical failures exit the same as one.
///
/// Asserted through [`BatchReport::exit_code`] rather than [`worst_exit`]:
/// the count is available at the report, not inside the classifier, so a
/// length test there is the one that can go wrong.
#[test]
fn the_exit_code_does_not_track_the_failure_count() {
    let one = BatchReport {
        items: vec![failed_item("3.28.1", 65)],
    };
    let many = BatchReport {
        items: (0..10).map(|n| failed_item(&format!("3.28.{n}"), 65)).collect(),
    };

    assert_eq!(one.exit_code(), ExitCode::DataError);
    assert_eq!(many.exit_code(), one.exit_code());
    assert_eq!(many.summary().exit_code, one.summary().exit_code);
    assert_eq!(many.summary().failed, 10);
}

// ── C-068 · the envelope ────────────────────────────────────────────────────

/// A mixed run is `partial_failure`, and `summary.exit_code` mirrors the
/// process exit — a script must never have to derive it.
#[test]
fn a_mixed_run_is_partial_failure() {
    // Deliberately asymmetric — one of each would let a report that swapped
    // two counters agree with every assertion below.
    let report = BatchReport {
        items: vec![
            item("3.28.1", None, ItemStatus::Succeeded),
            item("3.28.0", None, ItemStatus::Succeeded),
            failed_item("3.27.9", 83),
            item("3.26.0", None, ItemStatus::Skipped),
            item("3.25.0", None, ItemStatus::Skipped),
            item("3.24.0", None, ItemStatus::Skipped),
        ],
    };

    let summary = report.summary();
    assert_eq!(summary.status, SummaryStatus::PartialFailure);
    assert_eq!(summary.total, 6);
    assert_eq!(summary.succeeded, 2);
    assert_eq!(summary.failed, 1);
    assert_eq!(summary.skipped, 3);
    assert_eq!(summary.exit_code, ExitCode::TransparencyLogUnavailable as u8);
    assert_eq!(report.exit_code(), ExitCode::TransparencyLogUnavailable);
}

/// Nothing succeeded: `failure`, not `partial_failure`. The two call for
/// different recovery and a consumer branches on exactly this field.
#[test]
fn an_all_failed_run_is_failure() {
    let report = BatchReport {
        items: vec![failed_item("3.28.1", 65), failed_item("3.27.9", 65)],
    };
    assert_eq!(report.summary().status, SummaryStatus::Failure);
}

/// The steady state of a convergent command: everything already signed, so
/// nothing ran and the verdict is still `success`.
#[test]
fn an_all_skipped_run_is_success_with_exit_zero() {
    let report = BatchReport {
        items: vec![
            item("3.28.1", None, ItemStatus::Skipped),
            item("3.28.0", None, ItemStatus::Skipped),
        ],
    };
    let summary = report.summary();
    assert_eq!(summary.status, SummaryStatus::Success);
    assert_eq!(summary.exit_code, 0);
    assert_eq!(summary.skipped, 2);
    assert_eq!(summary.succeeded, 0);
}

/// A run over a repository with no published tags is not a failure.
#[test]
fn an_empty_run_is_success() {
    let report = BatchReport::default();
    let summary = report.summary();
    assert_eq!(summary.status, SummaryStatus::Success);
    assert_eq!(summary.total, 0);
    assert_eq!(summary.exit_code, 0);
}

/// **Never a bare array** (PKG-25). The two top-level keys and the frozen
/// value vocabularies are what a script pattern-matches on; changing any
/// string here is a wire break.
#[test]
fn the_envelope_carries_summary_and_items() {
    let mut skipped = item("3.26.0", Some("linux/amd64"), ItemStatus::Skipped);
    skipped.reason = Some(SkipReason::AlreadySigned);
    skipped.discovery = Some(DiscoveryMethod::ReferrersApi);

    let report = BatchReport {
        items: vec![
            item("3.28.1", None, ItemStatus::Succeeded),
            skipped,
            failed_item("3.27.9", 83),
        ],
    };

    let value: serde_json::Value = serde_json::to_value(report.envelope()).expect("the envelope serializes");

    assert!(value.is_object(), "the envelope is an object, never a bare array");
    assert_eq!(value["summary"]["status"], "partial_failure");
    assert_eq!(value["summary"]["exit_code"], 83);
    assert_eq!(value["items"][0]["status"], "succeeded");
    // `null`, not absent: a script must tell "the index itself" from a field
    // that happened not to be emitted.
    assert_eq!(value["items"][0]["platform"], serde_json::Value::Null);
    assert_eq!(value["items"][1]["status"], "skipped");
    assert_eq!(value["items"][1]["reason"], "already_signed");
    assert_eq!(value["items"][1]["discovery"], "referrers_api");
    assert_eq!(value["items"][1]["platform"], "linux/amd64");
    assert_eq!(value["items"][2]["status"], "failed");
    assert_eq!(value["items"][2]["error"]["code"], "transparency_log_unavailable");
    assert_eq!(value["items"][2]["error"]["exit"], 83);
}

/// The slug is `ocx`'s own frozen `error.kind` vocabulary, so a backfill row
/// carries the same string every other OCX tool carries for that code.
#[test]
fn the_item_error_slug_comes_from_the_shared_vocabulary() {
    let report = BatchReport {
        items: vec![failed_item("3.28.1", 65)],
    };
    let value: serde_json::Value = serde_json::to_value(report.envelope()).expect("serializes");
    assert_eq!(value["items"][0]["error"]["code"], "data_error");
}

// ── C-070 · dry run ─────────────────────────────────────────────────────────

/// A dry-run row reports a verdict and names no error — the flag reports, it
/// does not simulate a failure.
#[test]
fn a_dry_run_row_is_skipped_with_its_own_reason() {
    let mut row = item("3.28.1", None, ItemStatus::Skipped);
    row.reason = Some(SkipReason::DryRun);
    let report = BatchReport { items: vec![row] };

    let value: serde_json::Value = serde_json::to_value(report.envelope()).expect("serializes");
    assert_eq!(value["items"][0]["reason"], "dry_run");
    assert_eq!(value["summary"]["exit_code"], 0);
    assert!(value["items"][0].get("error").is_none());
}

// ── The frozen vocabularies ─────────────────────────────────────────────────

/// Every `summary.status` and per-item `status` value the code can produce,
/// pinned.
///
/// These strings are `docs/reference/cli.md`'s contract; a rename is a schema
/// break and this test is where it is noticed.
#[test]
fn the_status_vocabularies_are_frozen() {
    let cases = [
        (SummaryStatus::Success, "\"success\""),
        (SummaryStatus::PartialFailure, "\"partial_failure\""),
        (SummaryStatus::Failure, "\"failure\""),
        (SummaryStatus::Cancelled, "\"cancelled\""),
    ];
    for (status, expected) in cases {
        assert_eq!(serde_json::to_string(&status).expect("serializes"), expected);
    }

    let items = [
        (ItemStatus::Succeeded, "\"succeeded\""),
        (ItemStatus::Failed, "\"failed\""),
        (ItemStatus::Skipped, "\"skipped\""),
    ];
    for (status, expected) in items {
        assert_eq!(serde_json::to_string(&status).expect("serializes"), expected);
    }

    let reasons = [
        (SkipReason::AlreadySigned, "\"already_signed\""),
        (SkipReason::DryRun, "\"dry_run\""),
        (SkipReason::Cancelled, "\"cancelled\""),
    ];
    for (reason, expected) in reasons {
        assert_eq!(serde_json::to_string(&reason).expect("serializes"), expected);
    }
}

// ── PKG-27 · cancellation ───────────────────────────────────────────────────

/// An unattempted subject is `skipped`, and says why.
#[test]
fn a_cancelled_row_is_skipped_with_its_own_reason() {
    let target = SignTarget {
        tag: "3.7.0".to_string(),
        platform: Some(platform("linux/amd64")),
        subject: digest(1),
    };

    let row = cancelled_row(&target);

    assert_eq!(row.status, ItemStatus::Skipped);
    assert_eq!(row.reason, Some(SkipReason::Cancelled));
    assert!(row.error.is_none(), "no child ran, so there is nothing to diagnose");
    assert_eq!(row.tag, "3.7.0");
    assert_eq!(row.platform.as_deref(), Some("linux/amd64"));
}

/// Cancellation outranks the success/failure split (PKG-27).
///
/// The two verdicts call for different operator responses — name the broken
/// subjects, versus re-run a convergent command — so a run that was stopped
/// must not report as one that partly failed.
#[test]
fn a_cancelled_run_outranks_partial_failure() {
    let mut cancelled = item("3", None, ItemStatus::Skipped);
    cancelled.reason = Some(SkipReason::Cancelled);
    let report = BatchReport {
        items: vec![
            item("3.7.0", None, ItemStatus::Succeeded),
            failed_item("3.7", 75),
            cancelled,
        ],
    };

    let summary = report.summary();

    assert_eq!(summary.status, SummaryStatus::Cancelled);
    assert_eq!(summary.total, 3);
    assert_eq!(summary.succeeded, 1);
    assert_eq!(summary.failed, 1);
    assert_eq!(summary.skipped, 1);
    // The failure it did reach is still carried, so the exit code is still
    // the worst classified child rather than being reset by the interrupt.
    assert_eq!(summary.exit_code, ExitCode::TempFail as u8);
}

/// An `already_signed` skip is not a cancellation.
///
/// The verdict keys on the *reason*, not on the presence of skipped rows —
/// the steady state of this command is an all-skipped run, and reporting that
/// as `cancelled` would red every scheduled pass over a fully signed mirror.
#[test]
fn a_skipped_run_that_was_not_interrupted_stays_success() {
    let mut skipped = item("3.7.0", None, ItemStatus::Skipped);
    skipped.reason = Some(SkipReason::AlreadySigned);
    let report = BatchReport { items: vec![skipped] };

    assert_eq!(report.summary().status, SummaryStatus::Success);
}
