# Discussion: Mirror signing — carry, produce, and backfill signatures

State: handed-off → plan · Updated: 2026-09-02
Ratified: 2026-09-02 → plan
Confidence: ratified by the owner (explicit `/hex-plan` invocation after the restate, 2026-09-02); research vintages 2026-09-01 — five artifacts under `.claude/artifacts/research_*.md` (prior-art, SOTA, archaeology, codebase recon, relocation fact check)

## Intent

Owner's words (2026-09-01): "recently we updated to the new OCX 0.6.0 release but
we explicitly did not support signing — how much work would it be now for our
mirrors to put signing as well. The registry mirror: if we copy over, we should
also copy the signatures, including sidecar signatures and the referrer
signatures. If we copy into a registry that has no referrers from a repository
that has referrers — merge something there, or just do not support it? Also the
mirror application itself needs some kind of signing, and specific commands to
resign everything that has not been signed yet, or maybe even resign even if
already signed. SBOM: no, only the signatures."

Why now: ocx 0.6.0 (submodule pin `v0.6.0`) ships `ocx package push --sign`,
`ocx package sign --tags-file` sweeps, `ocx package verify`, and referrer attach
with the OCI tag-schema fallback; the mirror adopted 0.6.0 without any of it.

Out of scope: SBOM generation by the mirror; index-freshness / rollback
protection (a separate problem — see `research_mirror_supply_chain.md`).

## Requirements

Provisional prose (IDs originate downstream).

- Keyless (ambient OIDC → Fulcio/Rekor) is the default signing mode for
  mirror-produced packages; rendered GitHub workflows carry
  `id-token: write` whenever `sign:` is set.
- Key-pair mode is supported through ocx's `--key` grammar unchanged:
  a PEM file path, `env://NAME` holding the PEM, or a KMS scheme
  (`awskms`, `gcpkms`, `azurekms`, `hashivault`, `k8s`), with
  `OCX_KEY_PASSWORD` for encrypted keys — secrets reach the subprocess as
  env or files, never as argv. Acknowledged as the weaker mode; offered for
  CI without OIDC (Jenkins, cron boxes).
- Self-hosted Sigstore works end to end: Fulcio/Rekor URLs and the trusted
  root come from ocx's `[trust.sigstore]` config (the fleet `config.toml`
  the ocx tutorial ships) or the `--fulcio-url` / `--rekor-url` /
  `--sigstore-trusted-root` overrides; the mirror forwards, never models
  them. GHES and GitLab self-managed issuers per the tutorial's issuer
  matrix.
- ocx gains acceptance coverage for keyless signing and verification
  against its own self-hosted stack (dex → Fulcio → Rekor, already in
  `external/ocx/test/docker-compose.yml` behind profiles) — a satellite WP.
- The mirror's acceptance harness exercises keyless signing offline by
  reusing that stack, so the fallback-index merge, sidecar copy, backfill
  pre-filter and fail-closed paths are all testable without public
  Sigstore.
- A dedicated end-to-end repository under the owner's personal GitHub
  profile (working name `e2e-ocxmirror-signing`, created with the owner's
  `gh`) runs a rendered mirror workflow that signs keyless and pushes to
  `dev.ocx.sh`; `ocx package verify` against the result is the tier-2 gate
  (`/e2e-test`). Creating that repository is an outward-facing act — the
  executing run confirms it with the owner first.
- Registry copy carries the whole referrer graph and verbatim sidecars;
  fallback-index merge on registries without the Referrers API; subject
  before referrers; fail closed on a destination that rejects `subject`.
- Backfill signs only subjects lacking a signature by the configured
  identity, scoped to mirror-produced repositories; `--force` re-signs.

## Decisions

- Drain target: plan (`/hex-plan`).
- Trust claim: the mirror signs the packages it *produces* (github / url / pypi
  push legs) with its own identity; registry→registry copies carry upstream
  signatures verbatim and the mirror adds no signature of its own to copied
  content. Rationale: two distinct claims ("upstream key K signed D" vs "the
  mirror vouches for D"); consumers pick which identity their trust policy
  names.
- Destination without the OCI 1.1 Referrers API: merge into the
  `sha256-<hex>` fallback index per distribution-spec 1.1 (fetch → append →
  put), never refuse. Rationale: the acceptance harness runs `registry:2`
  (no Referrers API), so the fallback is the only path tests can exercise;
  ocx already performs this merge for its own pushes
  (`external/ocx/crates/ocx_lib/src/oci/sign/referrers.rs`, `pub(crate)`).
  Amended 2026-09-01 on archaeology: ocx's *copy* pipeline refuses to write
  the fallback index by ratified decision (lost-update race,
  go-containerregistry#2205) while its *signing* pipeline writes it under
  "D4 — optimistic read-back with bounded retry" (`MAX_FALLBACK_ATTEMPTS=5`).
  The mirror adopts the signing-pipeline shape: read-back after PUT, bounded
  retry, `If-Match` where the registry returns an ETag. Position held, not
  changed: refusing leaves no exit-0 path to `registry:2`, GHCR, or GitLab
  (ocx#392 is that defect upstream).

- Referrer copy scope: the whole referrer graph of a copied subject —
  every artifactType (signatures, attestations, SBOMs), depth-bounded, no
  type filter. "No SBOM" means the mirror never *generates* one; it still
  copies what upstream attached. Rationale: a type filter re-creates the
  silent-drop defect for whatever it skips.
- Signing failure on a mirror-produced push: fail the package (exit code, no
  tag advanced) whenever `sign:` is configured. Rationale: a silently
  unsigned package is the same defect class as a silently dropped signature.
- Verification at copy time: none. The registry copy is a faithful
  relocation; trust policy lives on the consumer side (ocx auto-verify).
- Defaulted, not asked: sign configuration is a pure passthrough of ocx's
  own flags (`--key`, `--signature-format`, identity-token flags) via a
  `sign:` spec block plus env; rendered GitHub workflows default to keyless
  with `id-token: write`. Backfill treats "unsigned" as "no signature by the
  mirror's configured identity", scoped to mirror-produced repositories;
  `--force` adds another signature.

- Cross-repo scope: **federated plan** — the mirror is the lead, ocx a
  satellite carrying its own work packages; the mirror consumes them by
  submodule bump. Owner's call against the mirror-only recommendation.
  Given the two decisions below, the ocx WP set is small: a public
  attach / fallback-index-merge seam (`attach_referrer` and
  `append_referrer_fallback_index` are `pub(crate)` today) so the mirror
  does not re-implement the D4 merge, and optionally ocx#391's read-back.
  Strongest remaining counter, said once: the plan's critical path now
  includes an ocx release and a submodule bump; a mirror-local merge could
  have shipped first and been swapped later.
- Copy engine: **extend `src/pipeline/registry_copy.rs`** — mirror-owned,
  depth-bounded walk over the native API and the fallback index, verbatim
  sidecar sweep (`.sig`/`.att`/`.sbom`, byte-for-byte, never clobber a
  destination-only same-tag signature), subject pushed before its
  referrers, fallback-index merge through the ocx seam above. Not
  `ocx package copy`: exit 84 on referrers-less targets by ratified ocx
  decision, which includes the acceptance harness.
- Backfill: **mirror pre-filters, ocx signs** — the mirror walks its own
  tag tree, discovers existing signatures per subject (ocx verify
  discovery: referrers API, fallback tag, sidecars), drops subjects already
  signed by the configured identity, hands the remainder to
  `ocx package sign --tags-file`; `--force` bypasses the filter. Scoped to
  mirror-produced repositories (trust-claim decision).

- Signing modes (2026-09-01, owner): keyless is the default; key-pair via
  ocx's `--key` schemes (file, `env://`, KMS) is supported as the explicit
  fallback for OIDC-less CI. Self-hosted Sigstore is a passthrough of ocx
  config (`[trust.sigstore]`) and the three override flags — the mirror
  adds no Sigstore configuration of its own. Defaulted: the `sign:` spec
  block names only `key`, `format`, and identity-token source; endpoints
  live in ocx config so the fleet tutorial applies unchanged.
- Verification infrastructure (2026-09-01, owner): a satellite WP adds ocx
  acceptance tests for keyless sign/verify against ocx's self-hosted
  stack; the mirror harness reuses that stack; a personal-profile e2e
  repository exercises the dev channel.

## Threads

- resolved — backfill: `ocx package sign --tags-file` is **not idempotent**
  across runs (`ocx_lib/src/package_manager/tasks/sign.rs:193` skips only
  bare manifests; no `--force`, no signed-by-identity skip). Verify caps
  candidates at 8 (`verify/pipeline.rs:91`), so unconditional re-runs
  accumulate referrers past what a verifier inspects. A backfill needs a
  pre-filter (discover signatures, skip those by the configured identity).
- resolved — registry copy: cosign `.sig`/`.att`/`.sbom` sidecar tags are
  **copied through as ordinary tags** today (not reserved in
  `Tag::is_reserved`, not probed by `detect_referrers`), untested. Earlier
  reading "dropped silently" was wrong. v2 must keep copying them verbatim
  (byte-for-byte; reconstruction corrupts signatures, cosign#4207) and never
  clobber a destination-only same-tag signature (ocx `bf24416a`).
- open — two push legs need two insertion points: in-process
  `Publisher` (`src/pipeline/push.rs`, github/url via `orchestrator.rs:850`)
  and subprocess argv (`src/pipeline/ocx_cli/push.rs`, pypi + `pipeline push`
  + `patch`). `--sign` on push signs platform manifests only; the index is
  signed afterwards by a `sign --tags-file` sweep. Rendered workflows lack
  `id-token: write` (`generate/ci/permissions.rs`).
- resolved — copy engine: extend `registry_copy.rs` (see Decisions);
  the fallback-index merge is reached through a new public ocx seam
  (federated WP), not re-implemented.
- open — env forwarding: `src/pipeline/ocx_cli.rs:42` `OCX_VARS` omits
  `OCX_SIGNING_KEY`, `OCX_KEY_PASSWORD`, `OCX_IDENTITY_TOKEN`, `OCX_NO_VERIFY`
  (owner call left open in `research_ocx_060_semantics.md` §9e).

- open — destination rejects referrer manifests outright (ECR 405 on
  `subject`, [containers-roadmap#2783](https://github.com/aws/containers-roadmap/issues/2783)):
  not "no Referrers API" but "cannot store a referrer at all". Defaulted
  under the fail-closed decision: the package fails with a counted error
  naming the rejection; owner may object.
- constraint — the fallback-index merge is non-atomic by spec text; ETag
  conditional PUT is the only mitigation. Mirror serialises per package,
  but an upstream signer writing the same index concurrently can still lose
  data. Plan should use `If-Match` where the registry returns an ETag.
- constraint — cosign 2.x re-signing is not idempotent (Rekor conflict,
  [cosign#3356](https://github.com/sigstore/cosign/issues/3356)); ocx has
  its own signer, so the backfill thread needs recon's answer on
  `sign_tags` idempotency before assuming a clean no-op.

- constraint — push order: GitLab rejects a referrer whose subject does not
  yet exist, and distribution-spec #459 may make that normative. v1 runs
  `detect_referrers` *before* the subject push (`registry_copy.rs:861`); v2
  must push the subject first, then its referrers and sidecars.
- resolved — relocation: cosign (`SimpleClaimVerifier`) and ocx
  (`oci/verify/simplesigning_read.rs:881-895`) compare the manifest digest
  only and never read `docker-reference`, so a digest-preserving copy to a
  different repository verifies by default. cosign#2790 was a
  re-serialising `docker load`/`push` flow, not a reference mismatch. The
  exception is skopeo/podman policy (`matchRepoDigestOrExact` default),
  which rejects relocated identity — a docs note, not a design change.

## Research

- `.claude/artifacts/research_mirror_signature_carriage.md` — prior-art web
  scan (2026-09-01): how oras/regsync/skopeo/Harbor/zot carry referrers and
  sidecars, spec fallback-merge race, ECR `subject` rejection, cosign
  idempotency, no first-party backfill tool exists.
- `.claude/artifacts/research_oci_signing_sota_2026.md` — SOTA lane
  (2026-09-01): cosign v3 bundle-as-referrer default, sidecar still read,
  registry Referrers-API map (GitLab/GHCR gaps), GitLab push-order rule,
  relocation-verify claim (disputed), distribution-spec 1.2 milestones.
- `.claude/artifacts/research_mirror_signing_archaeology.md` — repo
  archaeology (2026-09-01): the 2026-08-14 "integrity, not trust" ruling and
  why it is dated; ocx `package copy` referrers + sidecar sweep; ocx#391/#392;
  the lost-update race as the one recurring constraint.
- `.claude/artifacts/research_mirror_signing_recon.md` — codebase recon
  (2026-09-01): push legs and insertion points, sidecar pass-through, sweep
  non-idempotency, `pub(crate)` attach seam, missing `id-token: write`.
- `.claude/artifacts/research_relocation_verify.md` — disputed-fact check
  (2026-09-01): digest-only claim check in cosign and ocx; skopeo policy is
  the relocation-rejecting exception.

## Related

- `.claude/artifacts/adr_registry_mirror_sync.md` — v1 "detect referrers,
  refuse to copy" (open question 3; ~586-600, ~796-835, ~1215-1240).
- `.claude/artifacts/research_ocx_060_semantics.md` §9e — signing env vars not
  forwarded, owner call.
- `.claude/artifacts/research_mirror_supply_chain.md` §3 — signatures under a
  copy; referrers-aware copy as a design requirement.
- `docs/reference/registry-yml.md:468` — documented "does not copy signatures".
- `external/ocx/.claude/artifacts/adr_oci_referrers_signing_v1.md` — ocx's
  signing design record (amendment ~1123: fallback-tag optimistic retry).
- `external/ocx/.claude/artifacts/adr_package_copy.md` — `ocx package copy`:
  referrers default-on, exit 84 without the API, no fallback index.
- [ocx-mirror#7](https://github.com/ocx-sh/ocx-mirror/issues/7) — open
  tracking issue for signing, stale (filed before the engine existed).
- [ocx#392](https://github.com/ocx-sh/ocx/issues/392),
  [ocx#391](https://github.com/ocx-sh/ocx/issues/391) — open upstream copy
  defects (referrers-less target never exits 0; no read-back after PUT).

## Open questions

- [NEEDS CLARIFICATION: a destination that rejects any `subject`-bearing
  manifest outright (ECR, 405, containers-roadmap#2783) — fail the package
  or drop the referrers with a counted warning?] Recommended: fail the
  package with a counted error naming the rejection — the fail-closed
  decision, applied to the copy leg.
- [NEEDS CLARIFICATION: does `registry:2` in `test/docker-compose.yml`
  serve the OCI 1.1 Referrers API?] Recommended: assume not; keep
  `registry:2` for the fallback-index path and add a zot service for the
  native-API path so both legs of the walk and the merge are exercised.
- [NEEDS CLARIFICATION: signing the in-process `Publisher` leg
  (`src/pipeline/push.rs`) — sign each platform manifest inline via
  `ocx_lib::oci::sign::pipeline`, or sign only after the tag write through
  the same tag sweep the backfill uses?] Recommended: one mechanism — the
  sweep over the tags just written — so fresh pushes and backfill share a
  code path; per-platform inline signing is an optimisation, not a
  requirement.
- [NEEDS CLARIFICATION: signature format default for mirror-produced
  packages — ocx's default (Sigstore bundle as referrer) or `Both` for
  cosign v2 consumers?] Recommended: ocx's default, passthrough of
  `--signature-format` for operators who need sidecars.
- Docs, not design: "copies preserved" verifies under cosign and ocx;
  skopeo/podman default policy (`signedIdentity: matchRepoDigestOrExact`)
  rejects a relocated repository and needs `matchRepository`/remapping —
  state this in `registry-yml.md` where the old "does not copy
  signatures" paragraph lived.
- Prerequisite for the federated plan: the lead's `.agents/memory/hex.md`
  › Pointers needs the row
  `- Federation: \`ocx\` → \`../ocx\` (\`https://github.com/ocx-sh/ocx.git\`); verification documented in its \`CLAUDE.md\` › "Build & Development"`
  — `/hex-init` owns that write (this skill never writes it). The sibling
  clone `../ocx` exists and carries its own `hex.md`; the plan's ocx WPs
  run there, never in the read-only `external/ocx` submodule.

- [NEEDS CLARIFICATION: scope of the ocx acceptance WP — cover only
  keyless sign/verify on the self-hosted stack, or also key-pair and
  sidecar format?] Recommended: keyless plus key-pair, both formats — the
  mirror harness depends on all four paths existing upstream.
- [NEEDS CLARIFICATION: e2e repository name and lifecycle — keep
  `e2e-ocxmirror-signing` permanently as the dev-channel canary, or create
  per run?] Recommended: permanent, one repo, re-pinned per Deploy Dev
  build; deletion is the owner's act.

## Verification

- Acceptance (`test/tests/test_registry_sync.py`): S-011 flips from
  "carrying a referrer fails with a counted error" to "referrers are copied
  and discoverable at the destination" (read back via the API on a
  referrers-capable registry and via the `sha256-<hex>` index on
  `registry:2`); a sidecar-tag fixture (`.sig`/`.att`/`.sbom`) copies
  byte-for-byte and never overwrites a destination-only same-tag
  signature; subject lands before its referrers; a seeded fallback index
  with a foreign entry survives the merge (no lost update).
- Rust unit tests (`src/pipeline/registry_copy.rs`): walk depth cap,
  visited-digest set, referrer count cap, counted error on a `subject`
  rejection.
- Backfill: a repository seeded with one subject already signed by the
  configured identity and one unsigned — the sweep signs exactly the
  second; `--force` signs both; verify shows ≤ 2 candidates per subject
  after repeated runs.
- Push legs: `OCX_VARS` forwards the four signing vars; a rendered
  workflow carries `id-token: write` when `sign:` is set; with `sign:` set
  and no identity available the package fails (exit code asserted
  separately from stderr).
- Docs: `docs/reference/registry-yml.md:468` rewritten; `mirror-yml.md`
  gains the `sign:` block; `cli.md` gains the backfill command.
- Self-hosted Sigstore: the mirror harness brings up ocx's `dex`,
  `fulcio`, `rekor` compose profile (`external/ocx/test/docker-compose.yml`,
  `test/sigstore/`) and signs keyless with a dex-issued token; verify
  against `test/sigstore/trusted_root.json` passes; the same run with
  `--key env://…` and `OCX_KEY_PASSWORD` passes; `sign:` set with neither
  token nor key fails the package.
- ocx satellite: acceptance scenarios for `package push --sign`,
  `package sign --tags-file`, and `package verify` against the self-hosted
  stack, both formats.
- Tier 2: `e2e-ocxmirror-signing` (owner's profile) renders, signs keyless
  in GitHub Actions, pushes to `dev.ocx.sh`; `ocx package verify` on the
  pushed tag passes with the mirror's workflow identity in the policy.
- Gates: `task verify`; `/e2e-test` tiers 0–2 as above.
