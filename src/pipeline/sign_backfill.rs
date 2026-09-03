// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! The signature backfill — sign what the target repository already holds and
//! nothing ever signed.
//!
//! `push --sign` and the closing sweep (`adr_mirror_signing.md` D2) cover
//! everything published *after* `sign:` reaches the spec. A mirror that has
//! been publishing for a year has none of it, and re-pushing to get signatures
//! would rewrite digests every consumer has pinned. This command signs the
//! published content in place instead.
//!
//! **Continue-and-collect** (PKG-23): one subject's failure is a `failed` row,
//! never an abort. Signing is convergent — a re-run signs only what is still
//! unsigned — so a partial pass is a valid resumable state, and the operator
//! gets the 39 signatures that did land rather than the first error.
//!
//! Three things decide correctness here, and each was paid for once:
//!
//! - **The skip keys on the subject digest, never the tag.** A cascade
//!   publishes `3.28.1`, `3.28`, `3` and `latest` at one index digest; keying
//!   on the tag asks the same subject four times, counts one signature as one
//!   candidate for four items, and re-signs three of them.
//! - **Discovery reads the referrers listing, never `ocx package verify`.**
//!   `verify --platform` exits 0 on an index whose *index* is signed even when
//!   the platform manifest carries none, so it cannot answer the per-subject
//!   question this filter asks.
//! - **`ocx package sign` appends by design.** A second identity joining an
//!   existing signature is a supported operation, so a subject that is already
//!   signed must be skipped *here* — nothing downstream will deduplicate it,
//!   and ocx's verifier caps candidates at 8.

use std::collections::{BTreeMap, BTreeSet};

use ocx_lib::cli::{ErrorCategory, ExitCode};
use ocx_lib::oci::client::OciTransport;
use ocx_lib::oci::verify::{DiscoveryMethod, SignerCandidate, list_signature_candidates};
use ocx_lib::oci::{Digest, Platform, native};
use serde::Serialize;

use crate::error::{MirrorError, sign_exit_code};
use crate::pipeline::ocx_cli::push::{push_exit_is_transient, push_retry_delay};
use crate::pipeline::ocx_cli::sign::{ResolvedSign, invoke_sign_reference};
use crate::spec::Target;

/// One signature attached to a subject, as a *listing* reports it.
///
/// Mirror-owned rather than `ocx_lib`'s [`SignerCandidate`]: that type is
/// `#[non_exhaustive]`, so a later identity field would be free to add
/// upstream and impossible to construct in a test here.
///
/// **Nothing on this type has been verified.** Every field was read out of a
/// certificate whose chain was not checked. Presence is all the default filter
/// reads, and presence is a fact about the registry, not about a signer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SignatureCandidate {
    /// Which mechanism reported this candidate.
    pub(crate) discovery: DiscoveryMethod,
    /// The referrer's `artifactType`, when it carried one.
    pub(crate) artifact_type: Option<String>,
    /// Certificate SAN, for a keyless signature. **Unvalidated.**
    pub(crate) certificate_identity: Option<String>,
    /// Certificate OIDC issuer, for a keyless signature. **Unvalidated.**
    pub(crate) certificate_issuer: Option<String>,
    /// `verificationMaterial.publicKey.hint` for a key-pair signature.
    pub(crate) public_key_hint: Option<String>,
}

/// Test-only constructors (TEST-03): production candidates come off the wire
/// through [`From<SignerCandidate>`], never from a literal.
#[cfg(test)]
impl SignatureCandidate {
    /// A candidate carrying nothing but how it was found — the whole of what
    /// the presence-only filter reads.
    pub(crate) fn found(discovery: DiscoveryMethod) -> Self {
        Self {
            discovery,
            artifact_type: None,
            certificate_identity: None,
            certificate_issuer: None,
            public_key_hint: None,
        }
    }
}

impl From<SignerCandidate> for SignatureCandidate {
    fn from(candidate: SignerCandidate) -> Self {
        // The digest is deliberately dropped: the filter reads presence, and a
        // referrer manifest digest is not something any row of the report or
        // any decision here addresses.
        Self {
            discovery: candidate.discovery,
            artifact_type: candidate.artifact_type,
            certificate_identity: candidate.certificate_identity,
            certificate_issuer: candidate.certificate_issuer,
            public_key_hint: candidate.public_key_hint,
        }
    }
}

/// What the operator asked the filter to consider (C-067).
///
/// [`identity`](Self::identity) and [`issuer`](Self::issuer) **narrow** what
/// counts as already signed; they never widen it. An unset field admits every
/// candidate, which is why the default filter reads presence alone.
#[derive(Debug, Clone, Default)]
pub(crate) struct SignFilter {
    /// Sign every subject regardless of what it already carries.
    pub(crate) force: bool,
    /// Only a candidate whose certificate SAN is exactly this counts.
    pub(crate) identity: Option<String>,
    /// Only a candidate whose certificate OIDC issuer is exactly this counts.
    pub(crate) issuer: Option<String>,
}

impl SignFilter {
    /// Whether one candidate satisfies every constraint the operator set.
    ///
    /// **AND, on one candidate.** Both flags must hold on the *same*
    /// signature: reading the identity off one candidate and the issuer off
    /// another would skip a subject that carries neither of the thing asked
    /// for. An unset constraint holds vacuously, so a filter with neither flag
    /// matches anything and the default verdict stays presence-only.
    fn matches(&self, candidate: &SignatureCandidate) -> bool {
        constraint_holds(self.identity.as_deref(), candidate.certificate_identity.as_deref())
            && constraint_holds(self.issuer.as_deref(), candidate.certificate_issuer.as_deref())
    }
}

/// Whether a candidate's field satisfies a constraint.
///
/// Exact, byte-equal comparison — no glob, no regex. `ocx package verify`'s
/// `--certificate-identity` is exact too, and the two must agree: a filter
/// that skipped subjects `verify` would then reject is the one outcome a
/// backfill must not produce.
///
/// `None` on the candidate never matches a *present* constraint. A `.sig`
/// sidecar leaves all three identity fields `None` by design, and so does a
/// bundle that would not parse; matching either would skip a subject on
/// evidence that says nothing about who signed it.
fn constraint_holds(wanted: Option<&str>, found: Option<&str>) -> bool {
    wanted.is_none_or(|wanted| found == Some(wanted))
}

/// Whether `subject` already carries a signature this run should respect
/// (C-067).
///
/// Pure: the registry read happens in [`discover_signature_candidates`], and
/// the decision is a function of its result so the table below can be a unit
/// test rather than a live Sigstore stack.
///
/// With no narrowing flag, presence alone decides. Every candidate a producer
/// lists is signature-class by construction — `.att` and `.sbom` are filtered
/// out upstream, because an attestation routinely sits on a subject nothing
/// ever signed and counting one would skip exactly the artifact this command
/// exists to sign.
///
/// With `--identity` and/or `--issuer`, "already signed" narrows to *signed by
/// the signer this run cares about* ([`SignFilter::matches`]). A subject
/// carrying only a **foreign** signature therefore reads as unsigned and is
/// signed again — deliberately: an operator who has rotated identities is
/// asking for their own signature to be present, and `ocx package sign`
/// appends, so the foreign one survives beside it. The direction is also the
/// fail-safe one for a backfill: a redundant re-sign costs a candidate slot,
/// a wrongly skipped subject stays unsigned forever.
///
/// That re-sign converges when the narrowing value is the one this run signs
/// *as* — the signature it writes then satisfies the filter on the next pass —
/// **provided the subject stays under ocx's eight-candidate listing cap**.
/// `list_signature_candidates` truncates to the first eight referrers in the
/// order the registry returned them, and the OCI listing order is
/// unspecified, so on a subject already at the cap a correctly-written
/// signature can fall outside the window the next pass reads and be signed
/// again, growing the count and narrowing the window further. Not detectable
/// here: the cap is `pub(super)` in ocx and the truncation is not reported to
/// the caller (ocx-sh/ocx#403). Against another signer's identity, or any `--identity` under
/// key-pair signing (which carries a `publicKey.hint` and no certificate
/// identity at all), nothing this run can add will ever match and every pass
/// re-signs, towards ocx's eight-candidate verifier cap. Not checkable here:
/// a keyless SAN is minted by the OIDC exchange inside the signing child, so
/// it is not known before that child runs. `docs/reference/cli.md` states it
/// as the operator-facing rule.
pub(crate) fn already_signed(candidates: &[SignatureCandidate], filter: &SignFilter) -> bool {
    // `--force` overrides a *present* candidate, which is the whole point: a
    // force that only affected empty listings would be a no-op flag. It
    // outranks the narrowing flags too — nothing is skipped, so there is
    // nothing for them to narrow.
    !filter.force && matching_candidate(candidates, filter).is_some()
}

/// The candidate whose presence makes [`already_signed`] true.
///
/// Shared with the skipped row's `discovery` field so the report names the
/// signature the decision was actually made on. Under `--identity`/`--issuer`
/// a subject can be skipped on its *second* candidate, and `candidates.first()`
/// would name a discovery method belonging to a signature the filter rejected.
///
/// Ignores `--force` on purpose: force is `already_signed`'s short-circuit, and
/// a forced run never reaches a skipped row for this reason.
fn matching_candidate<'a>(candidates: &'a [SignatureCandidate], filter: &SignFilter) -> Option<&'a SignatureCandidate> {
    candidates.iter().find(|candidate| filter.matches(candidate))
}

/// Where the process exit comes from when the batch is over (C-069, PKG-24).
///
/// The **worst** classified failure among the `failed` rows, never a count and
/// never a bucket: the child's own code travels through
/// [`MirrorError::SignFailed`] and out.
///
/// "Worst" is ordered by what the operator does next, most-actionable first,
/// because that is the only ordering a CI job can act on. A run that exits 83
/// says "the transparency log was down, run me again"; one that exits 65 says
/// "a human must read the report". When both happened, 83 is the answer that
/// gets the remaining work done — and the 65 rows are still in `items`. This
/// is what C-069's "83 + 65 → 83" pins.
fn worst_exit(codes: &[i32]) -> ExitCode {
    codes
        .iter()
        .map(|code| sign_exit_code(*code))
        .min_by_key(|code| severity_rank(*code))
        .unwrap_or(ExitCode::Success)
}

/// Rank for [`worst_exit`] — lower sorts worse (is reported).
fn severity_rank(code: ExitCode) -> u8 {
    match code {
        // Re-run and it may well pass.
        ExitCode::TempFail | ExitCode::TransparencyLogUnavailable => 0,
        // A credential to refresh.
        ExitCode::AuthError | ExitCode::PermissionDenied => 1,
        // A spec or a key backend to change.
        ExitCode::ConfigError | ExitCode::UnsupportedKeyBackend => 2,
        // A registry that cannot carry signatures at all.
        ExitCode::ReferrersUnsupported => 3,
        // The registry answered, and the answer was no.
        ExitCode::Unavailable | ExitCode::NotFound => 4,
        // A human must read the report.
        ExitCode::UsageError | ExitCode::DataError => 5,
        // Everything a failed child can otherwise produce.
        ExitCode::Success
        | ExitCode::Failure
        | ExitCode::IoError
        | ExitCode::PolicyBlocked
        | ExitCode::DirtyRcBlock => 6,
        // `ExitCode` is `#[non_exhaustive]`: a code a newer `ocx_lib` adds
        // ranks last rather than failing this build, the same direction
        // `sign_exit_code` already degrades in.
        _ => 6,
    }
}

/// What happened to one `(tag, platform)` pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ItemStatus {
    Succeeded,
    Failed,
    Skipped,
}

/// Why an item was never attempted (PKG-21 — `skipped` is not `failed`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SkipReason {
    /// The subject already carries a signature candidate.
    AlreadySigned,
    /// `--dry-run`: the verdict was reported and no child ran (C-070).
    DryRun,
    /// A `SIGINT` arrived before this subject was attempted (PKG-27).
    Cancelled,
}

/// The run's verdict (C-068).
///
/// [`Cancelled`](Self::Cancelled) outranks the other three rather than
/// blending into `partial_failure` (PKG-27): the two call for different
/// operator responses. A partial failure says some subjects are wrong and
/// naming them is the next step; a cancelled run says nothing is wrong and
/// re-running it is, because the command is convergent and the unattempted
/// subjects are simply still unattempted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SummaryStatus {
    Success,
    PartialFailure,
    Failure,
    Cancelled,
}

/// One `(tag, platform)` row of the report.
///
/// `platform` is `null` rather than absent on an index row: a script reading
/// `items[].platform` must be able to tell "the index itself" from a field
/// that happened not to be emitted.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ItemReport {
    pub(crate) tag: String,
    pub(crate) platform: Option<String>,
    pub(crate) status: ItemStatus,
    /// The digest that was signed, skipped, or failed.
    pub(crate) subject: String,
    /// How the existing signature was found — absent when none was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) discovery: Option<DiscoveryMethod>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<SkipReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<ItemError>,
}

impl ItemReport {
    /// The row every outcome starts from — the target, and nothing decided yet.
    fn for_target(target: &SignTarget, status: ItemStatus) -> Self {
        Self {
            tag: target.tag.clone(),
            platform: target.platform.as_ref().map(ToString::to_string),
            status,
            subject: target.subject.to_string(),
            discovery: None,
            reason: None,
            error: None,
        }
    }
}

/// A failed row's error, in the pinned slug envelope (C-068, CLI-04).
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ItemError {
    /// `ocx`'s own frozen `error.kind` vocabulary, so the slug a backfill row
    /// carries is the slug every other OCX tool carries for that code.
    pub(crate) code: ErrorCategory,
    pub(crate) exit: i32,
}

impl ItemError {
    /// The envelope for a child exit, slug and integer derived from one code.
    fn from_exit(exit: i32) -> Self {
        Self {
            code: ErrorCategory::from_exit_code(sign_exit_code(exit)),
            exit,
        }
    }
}

/// The counters and verdict `--format json` puts under `summary` (C-068).
#[derive(Debug, Clone, Copy, Serialize)]
pub(crate) struct BatchSummary {
    pub(crate) status: SummaryStatus,
    pub(crate) total: usize,
    pub(crate) succeeded: usize,
    pub(crate) failed: usize,
    pub(crate) skipped: usize,
    /// Mirrors the process exit code, so a script never has to derive it.
    pub(crate) exit_code: u8,
}

/// Every item the run considered (PKG-21).
///
/// The counters are derived, never incremented alongside the rows: a summary
/// kept in step by hand is a summary that eventually disagrees with `items`,
/// and the two are the same fact.
#[derive(Debug, Default)]
pub(crate) struct BatchReport {
    pub(crate) items: Vec<ItemReport>,
}

/// The wire shape of [`BatchReport`] — `{ "summary": {...}, "items": [...] }`,
/// **never a bare array** (C-068, PKG-25).
///
/// A borrowed view rather than a field on the report, so `summary` cannot be
/// serialized stale.
#[derive(Debug, Serialize)]
pub(crate) struct BatchEnvelope<'a> {
    pub(crate) summary: BatchSummary,
    pub(crate) items: &'a [ItemReport],
}

impl BatchReport {
    /// The counters and verdict for what has been collected so far.
    pub(crate) fn summary(&self) -> BatchSummary {
        let count = |wanted: ItemStatus| self.items.iter().filter(|item| item.status == wanted).count();
        let succeeded = count(ItemStatus::Succeeded);
        let failed = count(ItemStatus::Failed);
        let skipped = count(ItemStatus::Skipped);

        // An all-skipped run is the steady state of a convergent command, so
        // it is `success` — the alternative reds every scheduled pass over a
        // repository that is already fully signed.
        //
        // A cancelled run outranks both, including one that also failed
        // subjects: the operator stopped this pass, which is the fact that
        // decides what they do next (PKG-27). The failures are still in
        // `items` and still in `exit_code`.
        let cancelled = self.items.iter().any(|item| item.reason == Some(SkipReason::Cancelled));
        let status = if cancelled {
            SummaryStatus::Cancelled
        } else {
            match (succeeded, failed) {
                (_, 0) => SummaryStatus::Success,
                (0, _) => SummaryStatus::Failure,
                (_, _) => SummaryStatus::PartialFailure,
            }
        };

        BatchSummary {
            status,
            total: self.items.len(),
            succeeded,
            failed,
            skipped,
            exit_code: self.exit_code() as u8,
        }
    }

    /// The process exit code (C-069).
    pub(crate) fn exit_code(&self) -> ExitCode {
        let codes: Vec<i32> = self
            .items
            .iter()
            .filter_map(|item| item.error.as_ref().map(|error| error.exit))
            .collect();
        worst_exit(&codes)
    }

    /// The report as `--format json` emits it.
    pub(crate) fn envelope(&self) -> BatchEnvelope<'_> {
        BatchEnvelope {
            summary: self.summary(),
            items: &self.items,
        }
    }
}

/// List the signature candidates attached to `subject` (C-074).
///
/// **C-074's mirror-local presence-only producer is superseded and must not be
/// restored.** It was specified because the upstream seam did not exist when
/// the plan was written; `ocx_lib::oci::verify::list_signature_candidates`
/// exists at the pin this builds against and does the same job. Re-adding a
/// local producer would fork referrer discovery in two places that diverge on
/// the first bug fix.
///
/// A thin adapter over `ocx_lib`'s own listing rather than a second
/// implementation of it (IDIOM-11). That function already does exactly what
/// C-074 specifies — one referrers page with the `sha256-<hex>` fallback, both
/// filtered to the two signature artifact types, plus the `.sig` sidecar tag —
/// and it additionally drops a DSSE **attestation** wearing the signature
/// artifact type, which no artifact-type filter can do and which is the exact
/// mistake that would skip an unsigned subject.
///
/// # Errors
///
/// [`MirrorError::TargetError`] when the registry could not answer. An absent
/// signature is an **empty vector**, never an error: "nothing is attached" and
/// "I could not look" are different answers and a backfill must not confuse
/// them.
pub(crate) async fn discover_signature_candidates(
    transport: &dyn OciTransport,
    image: &native::Reference,
    subject: &Digest,
) -> Result<Vec<SignatureCandidate>, MirrorError> {
    let candidates = list_signature_candidates(transport, image, subject)
        .await
        .map_err(|error| MirrorError::TargetError(format!("cannot list signatures for {subject}: {error}")))?;
    Ok(candidates.into_iter().map(SignatureCandidate::from).collect())
}

/// One thing the backfill can sign.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SignTarget {
    /// The tag the reference is built from — one representative per distinct
    /// subject digest.
    pub(crate) tag: String,
    /// `None` for the index itself; `Some` narrows the child manifest.
    pub(crate) platform: Option<Platform>,
    /// What a signature would be attached to.
    pub(crate) subject: Digest,
}

/// Collapse the published `(tag → index digest → children)` shape into the
/// two passes, deduplicated by subject digest.
///
/// This is where the cascade collapses: `3.28.1`, `3.28`, `3` and `latest` are
/// four tags at one index digest, and they produce **one** index target, not
/// four. Signing per tag would file four referrers against one subject and
/// spend half of ocx's eight-candidate verifier budget in a single run.
///
/// Returns `(indexes, platforms)` — the two passes in order, indexes first,
/// each sorted so a run's report and its child invocations are deterministic.
pub(crate) fn plan_targets(published: &[PublishedTag]) -> (Vec<SignTarget>, Vec<SignTarget>) {
    // The representative tag is the lowest-sorting one at each digest, so two
    // runs over identical registry state issue identical invocations. The
    // children are whichever entry was seen first, which is not a choice: two
    // tags at one index digest resolve to one manifest, so their child lists
    // are the same list.
    let mut by_digest: BTreeMap<&Digest, IndexEntry<'_>> = BTreeMap::new();
    for entry in published {
        by_digest
            .entry(&entry.digest)
            .and_modify(|slot| {
                if entry.tag.as_str() < slot.tag {
                    slot.tag = entry.tag.as_str();
                }
            })
            .or_insert(IndexEntry {
                tag: entry.tag.as_str(),
                children: &entry.children,
            });
    }

    let mut indexes = Vec::new();
    let mut platforms = Vec::new();
    // One set across **both** passes, not one per pass: a tag can point
    // straight at a manifest that is also a child of an index, and the same
    // subject reached two ways is still one subject. Two targets there would
    // file two referrers against it under `--force`, which is the count
    // `already_signed` must not be relied on to hold down.
    let mut seen: BTreeSet<&Digest> = BTreeSet::new();
    for (digest, entry) in &by_digest {
        if !seen.insert(digest) {
            continue;
        }
        indexes.push(SignTarget {
            tag: entry.tag.to_string(),
            // A bare manifest lands here too, with no children: it *is* the
            // only subject, and a no-`-p` sign is the correct call for it.
            platform: None,
            subject: (*digest).clone(),
        });
        for (platform, child) in entry.children {
            if seen.insert(child) {
                platforms.push(SignTarget {
                    tag: entry.tag.to_string(),
                    platform: Some(platform.clone()),
                    subject: child.clone(),
                });
            }
        }
    }

    indexes.sort_by(|left, right| left.tag.cmp(&right.tag));
    // `sort_by`, not `sort_by_key`: the tag decides almost every pair and
    // compares borrowed. Only the tiebreak allocates, and it has to —
    // `Platform` derives no `Ord`, so its display form is the ordering.
    platforms.sort_by(|left, right| {
        left.tag.cmp(&right.tag).then_with(|| {
            left.platform
                .as_ref()
                .map(ToString::to_string)
                .cmp(&right.platform.as_ref().map(ToString::to_string))
        })
    });
    (indexes, platforms)
}

/// One distinct index digest, as [`plan_targets`] groups the published tags.
struct IndexEntry<'a> {
    /// The lowest-sorting tag pointing at this digest.
    tag: &'a str,
    /// The index's platform entries — identical for every tag at one digest.
    children: &'a [(Platform, Digest)],
}

/// One tag as the target registry holds it — the input to [`plan_targets`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PublishedTag {
    pub(crate) tag: String,
    /// The digest the tag resolves to. An image index for a normal publish; a
    /// bare manifest for a single-platform one, which has no children and is
    /// itself the only subject.
    pub(crate) digest: Digest,
    /// `(platform, child manifest digest)` for each entry of the index; empty
    /// for a bare manifest.
    pub(crate) children: Vec<(Platform, Digest)>,
}

/// The run: everything a pass needs that does not vary per target.
///
/// A struct rather than eight parameters — the values travel together through
/// the driver and both per-target steps, which is the shape ARCH-01 names.
pub(crate) struct Backfill<'a> {
    pub(crate) target: &'a Target,
    pub(crate) sign: &'a ResolvedSign,
    pub(crate) transport: &'a dyn OciTransport,
    pub(crate) filter: SignFilter,
    /// Report the filter verdict per subject and issue no child (C-070).
    pub(crate) dry_run: bool,
    pub(crate) max_retries: u32,
}

/// The row for a subject the run never attempted (PKG-27).
///
/// `skipped`, never `failed`: no child ran, so there is nothing to diagnose
/// and nothing about the subject is known to be wrong.
fn cancelled_row(target: &SignTarget) -> ItemReport {
    let mut row = ItemReport::for_target(target, ItemStatus::Skipped);
    row.reason = Some(SkipReason::Cancelled);
    row
}

impl Backfill<'_> {
    /// Run both passes and collect one row per target (PKG-21, PKG-22).
    ///
    /// **Continue-and-collect.** Every per-target failure becomes a `failed`
    /// row and the pass carries on; nothing here returns `Err`, because no
    /// per-subject outcome makes the remaining subjects meaningless.
    ///
    /// Sequential by design, not by omission: `ocx package sign` writes a
    /// referrer manifest per subject, and a fan-out against one repository is
    /// how a mirror earns a rate limit on a job whose whole cost is elsewhere.
    /// Indexes first, then platforms, so a reader of the report sees the same
    /// order the invocations happened in.
    pub(crate) async fn run(&self, indexes: &[SignTarget], platforms: &[SignTarget]) -> BatchReport {
        // `let _`: a failed signal registration means only that this run
        // cannot be interrupted cleanly, which must not fail the backfill.
        self.run_until(indexes, platforms, async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
    }

    /// [`run`](Self::run) with its cancellation source injected (PKG-27).
    ///
    /// Production passes `SIGINT`. The seam takes any future so a caller can
    /// pass a channel instead; **no unit test uses it today** — a `Backfill`
    /// needs a live `OciTransport`, and the accounting property is pinned
    /// behaviourally by `test_signing_backfill.py::
    /// test_sigint_mid_batch_accounts_for_every_subject`, which signals a real
    /// process mid-batch. Deleting the `remaining.extend` below turns that
    /// test red and nothing in `cargo nextest` red.
    ///
    /// **Every target gets exactly one row, cancelled or not.** That is the
    /// property, not the exit code: an interrupted pass that silently dropped
    /// the subjects it never reached would read as a complete run over a
    /// smaller repository, which is the one report an operator cannot detect.
    ///
    /// On cancellation the in-flight subject is abandoned rather than awaited.
    /// `invoke_sign_reference` sets `kill_on_drop`, so its child dies with the
    /// future, and signing writes no local state — the only durable effect is
    /// a referrer the registry either accepted whole or never saw.
    pub(crate) async fn run_until(
        &self,
        indexes: &[SignTarget],
        platforms: &[SignTarget],
        cancel: impl Future<Output = ()>,
    ) -> BatchReport {
        let mut cancel = std::pin::pin!(cancel);
        let mut report = BatchReport::default();
        let mut remaining = indexes.iter().chain(platforms);

        for target in remaining.by_ref() {
            // `biased`: a ready cancellation must win against a subject that
            // has not started. Random selection would spawn one more child
            // after the operator asked the run to stop.
            tokio::select! {
                biased;
                () = &mut cancel => {
                    report.items.push(cancelled_row(target));
                    break;
                }
                row = self.sign_one(target) => report.items.push(row),
            }
        }

        // Whatever `break` left behind. Empty on an uninterrupted run.
        report.items.extend(remaining.map(cancelled_row));
        report
    }

    /// Discover, filter, then sign one subject.
    async fn sign_one(&self, target: &SignTarget) -> ItemReport {
        let image = native::Reference::with_tag(
            self.target.registry.clone(),
            self.target.repository.clone(),
            target.tag.clone(),
        );

        let candidates = match discover_signature_candidates(self.transport, &image, &target.subject).await {
            Ok(candidates) => candidates,
            Err(error) => {
                // A registry that could not answer must never read as "not
                // signed": the row fails rather than being signed blindly.
                let mut row = ItemReport::for_target(target, ItemStatus::Failed);
                row.error = Some(ItemError::from_exit(i32::from(error.kind_exit_code() as u8)));
                return row;
            }
        };

        if already_signed(&candidates, &self.filter) {
            let mut row = ItemReport::for_target(target, ItemStatus::Skipped);
            row.reason = Some(SkipReason::AlreadySigned);
            row.discovery = matching_candidate(&candidates, &self.filter).map(|candidate| candidate.discovery);
            return row;
        }

        if self.dry_run {
            let mut row = ItemReport::for_target(target, ItemStatus::Skipped);
            row.reason = Some(SkipReason::DryRun);
            return row;
        }

        match self.invoke_with_retry(&image, target).await {
            Ok(()) => ItemReport::for_target(target, ItemStatus::Succeeded),
            Err(exit) => {
                let mut row = ItemReport::for_target(target, ItemStatus::Failed);
                row.error = Some(ItemError::from_exit(exit));
                row
            }
        }
    }

    /// One `ocx package sign`, retried on a transient child exit (C-069).
    ///
    /// The same ladder and the same transient set as the push leg, reused
    /// rather than restated: 75 is a retryable transient and 83 is a
    /// transparency-log outage, and both are answered by waiting.
    async fn invoke_with_retry(&self, image: &native::Reference, target: &SignTarget) -> Result<(), i32> {
        let platform = target.platform.as_ref().map(ToString::to_string);
        let reference = image.whole();
        for attempt in 0..=self.max_retries {
            let outcome = invoke_sign_reference(self.sign, &reference, platform.as_deref()).await;
            let exit = match outcome {
                Ok(()) => return Ok(()),
                Err(MirrorError::SignFailed { code, .. }) => code,
                Err(other) => i32::from(other.kind_exit_code() as u8),
            };
            if attempt == self.max_retries || !push_exit_is_transient(Some(exit)) {
                return Err(exit);
            }
            ocx_lib::log::warn!(
                "signing {reference} failed with exit {exit}; retrying (attempt {} of {})",
                attempt.saturating_add(2),
                self.max_retries.saturating_add(1),
            );
            // 1-based: `push_retry_backoff` computes `base * 2^(attempt - 1)`,
            // so a 0 here would spend the first two retries on the same delay.
            tokio::time::sleep(push_retry_delay(attempt.saturating_add(1))).await;
        }
        // Unreachable: the loop returns on its last attempt. Stated as a
        // failure rather than a panic, because an empty range would otherwise
        // report success for work nothing performed.
        Err(ExitCode::Failure as i32)
    }
}

#[cfg(test)]
#[path = "sign_backfill/tests.rs"]
mod tests;
