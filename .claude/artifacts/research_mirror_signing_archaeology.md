# Research: referrers and signing — repo archaeology

<!-- hex-discuss research lane: repo archaeology. Discussion: .agents/discussions/mirror-signing.md -->

## Metadata

**Date:** 2026-09-01
**Domain:** packaging | security
**Triggered by:** discussion `mirror-signing` — what both repos already decided about referrer carriage and signing, and why.
**Expires:** 2027-03-01 (ocx#391/#392 may change the copy-side position)

## Direct Answer

**1. ocx-mirror: referrer copy → "detect, refuse to copy".** Owner ruling 2026-08-14 in `.claude/artifacts/adr_registry_mirror_sync.md` Open question 3 (commit `3fa477e`): *"integrity first — this mirror carries integrity, not trust."* Signing was "in active development in ocx and not ready" (cites ocx#195, ocx#196). v1 detects via `Client::pull_referrers_native` + tag-schema fallback and fails the package with a counted error (`CopyError::ReferrersPresent`, `REFERRER_REPORT_LIMIT=10`) — guarding against the Harbor [goharbor#23210](https://github.com/goharbor/harbor/issues/23210) silent drop. Tracked as [ocx-mirror#7](https://github.com/ocx-sh/ocx-mirror/issues/7) (open, untouched since 2026-06-13; describes the signing engine as nonexistent). No earlier walk-then-removed attempt: detect-only from `937824f` onward, hardened by `1117f8d`/`26a9d1f`/`177b70a`/`25adc22`.

**2. ocx-mirror `OCX_VARS`.** Introduced whole in `fb1e70c`, 13 vars, predates upstream signing. `research_ocx_060_semantics.md` §9e (in `4731939`) flags the four signing vars as "UNCERTAIN — owner call". `src/pipeline/ocx_cli.rs:42-56` at HEAD has neither the vars nor the deliberate-omission comment — **still open**.

**3. ocx signing ADR evolution.** Original ADR + milestone `72a69781`/`1243a8a4` (2026-08-18, "feat(sign)!: keyless Sigstore signing … over OCI referrers"; closes #24, #194, #195, #196, #98, #99, #106, #87, #197, #204, #205-210). Cosign-compat amendment `c96b23dd` (2026-08-30): `pull_referrers_native` (404 → `None` = unsupported) split from the fork's dead `pull_referrers`, later deleted per closed [ocx#368](https://github.com/ocx-sh/ocx/issues/368) (untruncated fallback tag for sha384/512; fail-open on `DENIED`/`UNAUTHORIZED`). Amendment ~line 1123 ratifies **D4 — optimistic read-back with bounded retry** (`MAX_FALLBACK_ATTEMPTS=5`) for the *signing* pipeline's fallback-tag writes. Multi-signature verify: `MAX_SIGNATURE_CANDIDATES=8`, ANY-of. Re-signing: no distinct decision; closest is *"a re-append never repairs an entry another tool wrote badly … deferred, not settled."*

**4. `ocx package copy`.** Added `1278d01f` with `adr_package_copy.md`: referrers copied **recursively by default** (`--referrers`/`--no-referrers`), fails closed exit 84 (`ReferrersUnsupported`) when the target lacks the Referrers API, **no fallback-tag scheme** — "OCX is referrers-only by design (#106)." Cosign sidecar tags added separately by `bf24416a` (2026-08-30, closes [ocx#376](https://github.com/ocx-sh/ocx/issues/376)): swept verbatim byte-for-byte (reconstruction corrupts signatures, [cosign#4207](https://github.com/sigstore/cosign/issues/4207)), runs *before* the referrers-API gate so it lands on referrers-less registries, refuses a same-tag PUT that would clobber a destination-only signature. Recorded as decision "D4" in ocx's `plan_issue_sweep_2026-08-30.md`: *"sidecar copy is verbatim … OCX still never writes the OCI fallback index tag"* — citing the [go-containerregistry#2205](https://github.com/google/go-containerregistry/issues/2205) lost-update race, scoped to a merged fallback *index* tag only.

**5. Issue map (corrected).** ocx#195 (closed) = referrers-capable acceptance registry; ocx#196 (closed) = offline/air-gapped verify trust-root cache. **Open and directly relevant:** [ocx#392](https://github.com/ocx-sh/ocx/issues/392) "promoting a cosign-signed package to a referrers-less registry can never exit 0" (lane 3 = write the fallback index tag = reverses ratified D4, needs an ADR); [ocx#391](https://github.com/ocx-sh/ocx/issues/391) "referrer count reports PUTs issued, not referrers discoverable at destination" (no read-back after `push_referrer_manifest`).

**6. Reverted/abandoned attempts.** None in either repo.

## Key Findings

1. **ocx-mirror#7 and the ADR's "not ready" framing are dated**: the engine landed 2026-08-18 and hardened 2026-08-30; the mirror's deferral rationale no longer holds.
2. **Two upstream referrer-copy mechanisms with different maturity**: native Referrers API copy (mature, default-on in `ocx package copy`) vs cosign sidecar sweep (newer, two open bugs #391/#392). A mirror v2 built against only the first misses the second.
3. **The lost-update race is the one constraint repeated across all three artifacts** (mirror ADR, ocx signing ADR, ocx issue-sweep plan): it blocks writing a *merged fallback index tag* — not reading referrers, not copying whole-manifest sidecar tags, not the native API. ocx's *signing* pipeline nevertheless writes it under D4's bounded optimistic retry; ocx's *copy* pipeline refuses by ratified decision.

## negative

- No detect-then-remove history in ocx-mirror; refusal was designed in from the first draft.
- The label "D4" names two unrelated decisions in two sibling ocx artifacts (signing-pipeline fallback retry vs copy-pipeline verbatim sidecars); cite with the artifact name.

## leads

- ocx#109 (open) threat model + incident references — cross-check against the mirror's own threat model.
- ocx#316 (open) auto-verify trust-service fan-out — relevant if the mirror ever runs verify itself.
- ocx#392 lane 2 (relax the referrers-API gate when every source referrer is representable as a cosign sidecar) — a middle path.
- ocx `research_cross_registry_sidecar_promotion.md` — fullest writeup of sidecar-vs-fallback-index.

## Sources

| Source | Type | Date | Relevance |
|--------|------|------|-----------|
| `.claude/artifacts/adr_registry_mirror_sync.md` (`3fa477e`) | ADR | 2026-08-14 | open question 3 ruling |
| https://github.com/ocx-sh/ocx-mirror/issues/7 | Issue | 2026-06-13, open | signing follow-up, stale |
| `external/ocx` `1243a8a4`, `c96b23dd`, `bf24416a`, `1278d01f` | Commits | 2026-08 | signing engine, cosign compat, sidecar sweep, package copy |
| https://github.com/ocx-sh/ocx/issues/368 | Issue | closed | dead fallback function removed |
| https://github.com/ocx-sh/ocx/issues/376 | Issue | closed | sidecar sweep |
| https://github.com/ocx-sh/ocx/issues/391 | Issue | open | referrer count without read-back |
| https://github.com/ocx-sh/ocx/issues/392 | Issue | open | referrers-less target never exits 0 |
| https://github.com/google/go-containerregistry/issues/2205 | Issue | flagged, >18mo | fallback-index lost-update race |
| https://github.com/sigstore/cosign/issues/4207 | Issue | current | reconstruction corrupts signatures |
