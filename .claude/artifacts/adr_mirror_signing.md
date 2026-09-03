# ADR: Mirror signing — carry, produce, and backfill signatures

<!--
Architecture Decision Record
Owner: Architect (/architect)
Handoff to: /swarm-plan → /swarm-execute
Format: MADR. Six numbered decisions (D1…D6), each with its own option table.
-->

## Metadata

**Status:** Proposed
**Date:** 2026-09-02
**Deciders:** Owner (ratified the discussion 2026-09-02); architect
**GitHub Issue:** [ocx-mirror#7](https://github.com/ocx-sh/ocx-mirror/issues/7) (stale — filed before the ocx signing engine existed; this ADR supersedes its framing)
**Related Design Spec:** `.agents/discussions/mirror-signing.md` (ratified), `.claude/state/plans/notes_mirror_signing_discover.md`
**Stack Alignment:**

- [x] Decision fits existing stack (Rust 2024 + Tokio, see CLAUDE.md) and conventions in `.claude/rules/subsystem-mirror.md`
- [x] One deviation justified below: D5 widens one `ocx_lib` symbol, against ocx's "operations go through the CLI" doctrine. Named, scoped, and argued in D5.

**Domain Tags:** pipeline | spec | push | ci | security | docs
**Supersedes:** `adr_registry_mirror_sync.md` Open question 3 (v1 "detect referrers, refuse to copy") — the deferral rationale ("signing is in active development in ocx and not ready") expired when ocx 0.6.0 shipped the engine.
**Superseded By:** N/A

---

## Context

ocx 0.6.0 ships `ocx package push --sign`, `ocx package sign` (single reference,
`--platform` narrowing, and `--tags`/`--tags-file` sweeps), `ocx package verify`,
and referrer attach with the OCI tag-schema fallback. The mirror adopted 0.6.0
without any of it. Three gaps follow, and they are different problems wearing one
word:

1. **Carriage.** `registry sync` copies package trees between registries. Today it
   *detects* referrers and fails the whole package (`CopyError::ReferrersPresent`,
   `registry_copy.rs:440`), while cosign `.sig`/`.att`/`.sbom` sidecar tags travel
   through as ordinary tags — neither detected nor tested. So a signed upstream is
   either refused outright or partially carried, and which one it gets depends on
   which signature shape it used.
2. **Production.** The mirror publishes packages it builds from GitHub Releases,
   URL indexes and PyPI. None of those pushes are signed, and the rendered GitHub
   workflows carry no `id-token: write`, so ambient keyless signing is
   structurally unavailable to every downstream mirror repository.
3. **Backfill.** Everything already published is unsigned. Nothing in ocx or the
   wider ecosystem is a "sign every unsigned subject in repository X" tool; the
   state of the art is a hand-written loop
   (`research_mirror_signature_carriage.md` §4).

The ratified discussion settles the shape: the mirror signs what it *produces*
with its own identity, carries upstream signatures verbatim on copy, adds no
signature to copied content, and never verifies at copy time. This ADR turns that
into decisions an implementer can build against.

### Three findings from the code that change the design

These were not in the discussion or the discover notes. Each moves a decision.

**F1 — the credential deny-list does not protect `mirror.yml`.** `spec/prescan.rs`
refuses `password`/`token`/`username`/`auth`/`credentials`/`secret`/`api_key` at
any depth, but `spec/load.rs:76-78` states plainly that the pre-scan is
*deliberately not wired into* `load_spec`, because its `kind:` discriminator would
make every existing `mirror.yml` exit 64. So a `mirror.yml` can carry
`token: ghp_…` today with no refusal. The `sign:` block is the first `mirror.yml`
surface that invites a secret, so it cannot lean on a guard that is not there
(D1, C-053).

**F2 — `ocx mirror …` strips the three signing credentials.**
`ocx_cli/src/app/plugin_dispatch.rs:192-194` calls `env_remove` for every member of
`ocx_lib::env::keys::CREDENTIAL_KEYS` = `OCX_IDENTITY_TOKEN`, `OCX_KEY_PASSWORD`,
`OCX_SIGNING_KEY`. ocx's own doc for that set says why: *"a plugin is third-party
code launched with the full ambient environment, so the one thing that must not
ride along is a bearer credential."* Invoked as an ocx plugin, ocx-mirror never
sees any of the three, so key mode and explicit-token keyless silently produce
"no signing material" — while *ambient* GHA keyless keeps working, because
`ACTIONS_ID_TOKEN_REQUEST_URL`/`_TOKEN` are not in the set. The failure is
configuration-dependent and invisible in a diff. D1 answers it with operator-named
environment variables that ocx cannot know to scrub.

**F3 — `sign --tags-file` structurally cannot cover platform manifests.**
`PackageManager::sign_tags` (`tasks/sign.rs:193-215`) yields
`SweptOutcome::SkippedBareManifest` for any tag resolving to a single manifest,
and its own comment says why: *"push already signed each platform manifest
inline."* The sweep signs indexes. A sweep-only design therefore leaves every
platform manifest of a multi-platform package unsigned — and practitioner
consensus (`research_ci_signing_config_2026.md` §5) is that a puller fetching one
platform manifest fails when only the index is signed. This kills the discussion's
"one mechanism — the sweep" recommendation for D2 and is the decisive fact there.

---

## Decision Drivers

- **Fail closed on a missing signature.** A silently unsigned package and a
  silently dropped signature are the same defect class. Both fail the package.
- **Faithful relocation.** Copied signature bytes are never reconstructed
  ([cosign#4207](https://github.com/sigstore/cosign/issues/4207)); the destination
  digest equals the source digest, which is what keeps a copy verifiable
  (`research_relocation_verify.md`).
- **One pasteable command.** Every capability is reachable from a single command
  an operator can paste into GitLab, Jenkins or a cron box. Rendered GitHub
  workflows are a convenience, never the only path (CLAUDE.md).
- **The mirror models no Sigstore.** It forwards two endpoint URLs and nothing
  else; it never re-implements a trust root. The trusted root stays in ocx's
  `[trust.sigstore]`, which after the D1 amendment is the *consumer* side only.
- **Blast radius.** `pipeline generate ci` renders workflows shipped to every
  downstream mirror repository. A permissions change reaches all of them.
- **Threat model.** `.claude/rules/security-threat-model.md`: upstream, the
  network and answering registries are attackers; the execution environment and
  whoever can write to the destination registry are trusted.
- **Do not widen ocx.** ocx's `arch-principles.md` records ocx-mirror's existing
  reach into operational internals as *known drift* with a CLI migration target.
  Every seam this ADR asks for must justify not being a CLI call.

---

## Industry Context & Research

**Research artifacts (eight, 2026-09-01/02):**
[`research_mirror_signature_carriage.md`](./research_mirror_signature_carriage.md) ·
[`research_oci_signing_sota_2026.md`](./research_oci_signing_sota_2026.md) ·
[`research_mirror_signing_archaeology.md`](./research_mirror_signing_archaeology.md) ·
[`research_mirror_signing_recon.md`](./research_mirror_signing_recon.md) ·
[`research_relocation_verify.md`](./research_relocation_verify.md) ·
[`research_oci_fallback_merge_registries.md`](./research_oci_fallback_merge_registries.md) ·
[`research_ci_signing_config_2026.md`](./research_ci_signing_config_2026.md) ·
[`research_signature_backfill_identity.md`](./research_signature_backfill_identity.md)

**Trending approaches.** cosign v3 (2026) writes the Sigstore bundle as an OCI 1.1
referrer by default and still *reads* the legacy sidecar, with removal deferred to
v4 and unscheduled. Keyless-by-default in CI is settled across GitHub, GitLab,
CircleCI and Buildkite. Config blocks that name secrets by environment-variable
name (goreleaser `signs:`, apko `sign-with-temporary-key`) are the uniform
precedent.

**Key insight, and it is uncomfortable:** *no mirror-shaped tool signs mirrored
content with its own identity.* aqua and mise **verify** upstream signatures;
Harbor and Artifactory proxy caches sign nothing; skopeo's `--sign-by` is an
explicit opt-in that *adds* a signature at copy time. The design here is
consistent with that — the mirror signs only what it **produces**, and carries
what it **copies** — which is exactly the ratified trust-claim decision.

**The failure class to avoid** is Harbor's:
[harbor#23210](https://github.com/goharbor/harbor/issues/23210), where replication
silently drops OCI 1.1 referrers while a direct `oras copy --recursive` between
the same two registries works. It is a bulk-sync-path defect, not a spec gap, and
it is the exact shape `registry sync` would repeat if the walk were incomplete.

**Two constraints the ecosystem has not solved.** The fallback-index merge is
non-atomic by the distribution spec's own text, and
[go-containerregistry#2205](https://github.com/google/go-containerregistry/issues/2205)
was closed "not planned"; no tool ships the `If-Match` retry loop, so ocx's
bounded read-append-write-readback (`MAX_FALLBACK_ATTEMPTS = 5`) is already ahead
of the norm. And ECR rejects `subject`-bearing manifests with 405
([containers-roadmap#2783](https://github.com/aws/containers-roadmap/issues/2783),
open) — a destination can lack not merely the API but the ability to store a
referrer at all.

---

# D1 — The `sign:` spec block

## Context

`sign:` is a new opt-in block on `MirrorSpec`. It has to reach two consumers with
different flag vocabularies: `ocx package push --sign` (which carries `--key` and
`--rekor-upload`, and **no** endpoint or identity-token flags) and
`ocx package sign` (which carries those plus `--identity-token-file`,
`--identity-token-stdin`, `--fulcio-url`, `--rekor-url`).

Ratified and not re-litigated: keyless is the default; key mode is ocx's `--key`
grammar unchanged.

**Amended by owner ruling, 2026-09-02 — the endpoint clause is reversed.** The
ratified wording put self-hosted Sigstore endpoints in ocx's `[trust.sigstore]`
config and kept them out of `mirror.yml`. Three facts, verified against the
vendored ocx 0.6.0, overturn it:

1. `ocx_lib::trust::SigstoreTrust` (`crates/ocx_lib/src/trust.rs:134`) *is*
   `[trust.sigstore]`: **one global table per machine**, documented as "where
   verification gets its trust root". Policies (`[[trust.policy]]`) are per scope;
   endpoints are not. A machine that consumes public `ocx.sh` packages and
   publishes to a corporate registry cannot express both there.
2. `ocx package push --sign` resolves Fulcio and Rekor through
   `package_sign_common::resolve_sigstore_pair(config, id, None, None)`
   (`crates/ocx_cli/src/command/package_push.rs:489`): flag → `[trust.sigstore]` →
   the public defaults (`package_sign_common.rs:294`, `oci/endpoint.rs:41`). The
   two `None`s are the missing flags — `package sign` already carries
   `--fulcio-url` (`conflicts_with = "key"`) and `--rekor-url`
   (`package_sign.rs:66,72`); `push` carries neither.
3. Signing needs only those two URLs. `SignContext` (`oci/sign/pipeline.rs:87-89`)
   holds no trusted root, so the publish side and the consumer side are genuinely
   separable — and the machine's one `[trust.sigstore]` belongs to the consumer.

Two further facts shape the block. ocx has **no default signing key**: key mode
exists iff `--key REF` (`options/key.rs:78`), and key-mode Rekor upload defaults
**off** (`options/rekor_upload.rs:99`) while keyless upload is mandatory. And ocx's
keyless identity resolution is `OCX_IDENTITY_TOKEN` → ambient (GitHub Actions
exchange, GitLab `SIGSTORE_ID_TOKEN`, CircleCI `CIRCLE_OIDC_TOKEN_V2`;
`oci/sign/oidc_ambient_inline.rs`) → browser, so the CI platform is never modelled
in the mirror.

Verified against ocx 0.6.0: `KeyRef` schemes are `file` (bare path), `env://NAME`,
and the four KMS families — but only **file and env are implemented**; a KMS
reference exits **85** `UnsupportedKeyBackend` (`oci/sign/key_ref.rs:339-345`).

## Considered Options

### Option 1: Minimal passthrough — `key` + `format` only

Two fields. Every secret arrives through ocx's own conventional variables
(`OCX_IDENTITY_TOKEN`, `OCX_KEY_PASSWORD`, `OCX_SIGNING_KEY`) inherited from the
process environment.

| Pros | Cons |
|------|------|
| Smallest possible surface; nothing to keep in step with ocx | Broken under plugin dispatch (F2): all three conventional names are scrubbed |
| No new secret-adjacent field names in a spec | Operator has no way to say "my token is in `$X`" |
| Fewest JSON-schema golden churn points | Silent failure, not a loud one — the run reports "no signing material" with no hint why |

### Option 2: Mode-explicit block — `mode`, `key`, `format`, `identity_token_env`, `key_password_env`, `rekor_upload`

goreleaser/apko shape: an explicit `mode: keyless|key` field plus per-mode fields.

| Pros | Cons |
|------|------|
| `mode:` makes the keyless choice declared, not inferred | `mode:` is derivable from `key:` presence — a second source of truth for one fact |
| Validation can refuse `mode: keyless` + `key:` | Needs contradictory-state validation that only exists because the redundant field exists |
| Matches the strongest external precedent | `rekor_upload` duplicates `[trust.sigstore].rekor_upload`, which already exists in ocx config |

### Option 3: Inferred mode, environment names, no endpoints — `key`, `format`, `identity_token_env`, `key_password_env`

Four fields. Absent `key:` means keyless. The two `*_env` fields name **variables,
never values, never paths**; the mirror copies each named variable's value onto
the conventional ocx name in the child environment of every `ocx` invocation.

| Pros | Cons |
|------|------|
| Survives plugin dispatch (F2): a variable ocx does not know cannot be scrubbed | Deliberately re-supplies a credential ocx chose to strip — a security-relevant act that must be documented, not incidental |
| Both legs behave identically; no flag that works on one and not the other | Two more field names to hold stable forever |
| Secrets are names, matching every surveyed precedent and `spec/dist.rs`'s `identity:` convention | An operator using the conventional names under plugin dispatch still fails, unless they set the fields |
| Endpoints stay in ocx config, so the fleet tutorial applies unchanged | |

### Option 4: Mode tag plus one `Ref` grammar — `keyless{…}` xor `key{…}`

A present `sign:` carries exactly one mode tag, and every value is a `Ref`: a
literal, `env://NAME`, or `file://PATH`. Endpoints are fields under `keyless:` and
are **always** emitted as `--fulcio-url`/`--rekor-url`, so the publish side never
reads the machine's consumer-side `[trust.sigstore]`. Secret-class fields
(`passphrase`, `identity_token`) are resolved by the mirror and exported to every
ocx child on the conventional names. No `format`, no `mode`, no `*_env`.

| Pros | Cons |
|------|------|
| The only shape where the publish-side Sigstore is independent of the machine's single `[trust.sigstore]` table | Needs OCX-C-5 (`push --sign` gains the two endpoint flags), so an ocx release is on the critical path for D1 too |
| The tag declares the mode once, with no second field that can contradict it | One nesting level more than Option 3, and two field names in a secret class |
| One value grammar everywhere, so "where does this value come from" has one answer | Pins ocx's public Fulcio/Rekor defaults as mirror-owned constants — deliberate, and the one place the "do not pin ocx defaults" rule is overridden |
| Survives plugin dispatch (F2) with no operator-named variable: the mirror resolves the secret and re-exports it | |
| Key-mode Rekor explicit both ways, so a fleet `rekor_upload = true` cannot push a private digest to the public log | |

### Weighted criteria

| Criterion | Weight | Opt 1 | Opt 2 | Opt 3 | Opt 4 |
|---|---|---|---|---|---|
| Works on both push and sweep legs identically | 5 | 5 | 4 | 5 | 5 |
| Survives ocx plugin dispatch (F2) | 5 | 1 | 4 | 5 | 5 |
| Secret-leak surface (refs, never values) | 5 | 5 | 5 | 5 | 5 |
| Publish-side Sigstore independent of the machine's consumer-side `[trust.sigstore]` | 5 | 1 | 1 | 1 | 5 |
| Mode declared, not inferred (owner ruling) | 3 | 1 | 5 | 1 | 5 |
| Field count / no redundant source of truth | 3 | 5 | 2 | 4 | 3 |
| One-pasteable-command reachability | 3 | 4 | 4 | 5 | 5 |
| Schema-churn cost (one-way door) | 3 | 5 | 2 | 4 | 3 |
| **Weighted total (max 160)** | | **105** | **109** | **122** | **148** |

## Decision Outcome — D1

**Chosen: Option 4.** ⚠️ **One-way door** — the tag names, the field names and the
`Ref` grammar are a shipped spec surface and a JSON-schema golden.

```yaml
sign:
  keyless:                      # tag. `keyless: {}` = public Sigstore.
    fulcio: <ref>               # optional; default https://fulcio.sigstore.dev
    rekor:  <ref>               # optional; default https://rekor.sigstore.dev
    identity_token: <ref>       # optional; env:// or file:// only. Only for CIs ocx cannot auto-detect.
  # xor
  key: <ref>                    # string form: the ocx --key reference
  key:                          # map form
    ref: <ref>                  # required
    passphrase: <ref>           # optional; env:// or file:// only
    rekor: <ref>                # optional; present = --rekor-upload --rekor-url, absent = --no-rekor-upload
```

One value grammar for every field, `Ref`: a literal, `env://NAME`, or
`file://PATH`. What each spelling means per field:

| Field | literal | `env://NAME` | `file://PATH` |
|---|---|---|---|
| `key` / `key.ref` | a bare path, as ocx | passed verbatim to `--key` (ocx resolves) | passed verbatim |
| `passphrase`, `identity_token` | refused, 64 | resolved by the mirror | resolved by the mirror (≤ `MAX_SECRET_FILE_BYTES`) |
| `fulcio`, `rekor` | the URL | resolved by the mirror | resolved by the mirror |

Resolved secrets are exported to every ocx child as `OCX_KEY_PASSWORD` /
`OCX_IDENTITY_TOKEN` — identically on every leg, which makes the plugin-dispatch
scrub (F2) moot without any operator-named variable. Resolved endpoints become
`--fulcio-url` / `--rekor-url`. URL validation stays ocx's
(`validate_sigstore_url`: https, no userinfo); the mirror passes through and
carries ocx's 64.

**Rationale.** Option 4 is the only shape whose publish side is independent of the
machine's single `[trust.sigstore]` table. That table is per machine, not per
scope, so a host that consumes public `ocx.sh` packages and publishes to a
corporate Sigstore cannot express both — and under Options 1–3 the mirror's
publish leg silently inherits whichever instance the consumer side named. Option 3
bought plugin-dispatch survival with two permanent `*_env` field names; resolving
the secret inside the mirror buys the same survival with none. Option 2's `mode:`
restated what `key:` already said; a tag says it once, and says it declaratively,
which is the owner's ruling.

**Six sub-decisions, recorded so they are not re-derived.**

- **`format` is dropped.** One differentiation does not earn a field: ocx's
  default `bundle` applies, and `simplesigning` is added on request. This retires
  the earlier "the mirror does not default `format`" sub-decision by removing the
  field it governed.
- **KMS references are passed through unvalidated.** ocx 0.6.0 exits 85 on them.
  The mirror does **not** duplicate that check: ocx's implementation status moves,
  and a copy of it in the mirror goes stale and starts refusing references that
  have begun to work. Documented in `mirror-yml.md` instead.
- **`identity_token` *is* a field now**, resolved by the mirror and exported as
  `OCX_IDENTITY_TOKEN`. That dissolves the earlier objection that `push` carries no
  `--identity-token-*` flag: nothing travels on argv, so both legs sign under one
  identity. It exists for CI platforms ocx cannot auto-detect; GitHub Actions,
  GitLab and CircleCI resolve ambiently and leave it unset.
- **Endpoints are always emitted under `keyless`**, from the mirror-owned
  constants `DEFAULT_FULCIO_URL` / `DEFAULT_REKOR_URL` when the spec omits them.
  This pins ocx's public defaults deliberately — the one place the "do not pin ocx
  defaults" rule is overridden, because an omitted flag falls through to the
  machine's consumer-side table, which is the failure this decision exists to stop.
- **Key-mode Rekor is explicit both ways.** `rekor:` present renders
  `--rekor-upload --rekor-url U`; absent renders `--no-rekor-upload`. Silence would
  inherit a fleet `rekor_upload = true` and push a private digest to the public log.
- **`key: {}` is refused.** ocx has no default signing key — key mode exists iff
  `--key REF` — so an empty map names nothing and cannot be honoured.

### Consequences

**Positive:** the publish side and the consumer side are two files, and neither can
silently redirect the other. Signing configuration is one tag plus at most three
refs under one grammar; key material never appears in a spec, in argv, or in a log;
no Sigstore concept beyond two URLs is modelled in the mirror.

**Negative:** the mirror deliberately re-supplies credentials that ocx's plugin
dispatch scrubbed — now from refs it resolved itself rather than from
operator-named variables. Under the threat model the execution environment is
trusted and this is the operator's explicit configuration, so it is in scope and
accepted, recorded here and in `environment.md` rather than left as an emergent
property.

**Negative — an ocx floor bump.** `push --sign` has no endpoint flags today, so D1
now needs OCX-C-5 in a *released* ocx: the mirror's `ocx.toml` floor moves off
0.6.0 and an ocx release precedes the mirror release. This reverses the earlier
"no ocx floor bump for D1, D2 or D4" position, which held only while D1 emitted no
endpoint flags.

**Negative — a wave shift.** The push leg now depends on the satellite, so the
plan's WP 2 moves from wave 2 to wave 3, cascading WP 4 → 4, WP 11 → 5 and
WP 12 → 6. The shippable point moves from wave 2 to wave 3.

**Risk:** wiring the credential deny-list into `load_spec` (C-053) is a *new*
refusal on an *existing* surface. A downstream `mirror.yml` carrying a key named
`token`/`auth`/`secret`/`username` starts exiting 64. The block's own secret-class
fields are named `passphrase` and `identity_token` precisely so they fall outside
that list. Mitigation: run the full `tests/fixtures/` and `tests/golden/` corpus as
an implementation gate before landing, and ship the change with a changelog entry
naming the remedy.

---

# D2 — Where signing runs on each push leg

## Context

Three legs publish, and they are not the same shape:

| Leg | Site | Mechanism |
|---|---|---|
| Archive push | `pipeline/ocx_cli/push.rs::build_push_args` | `ocx package push` subprocess |
| Env (pylock/pypi) push | `pipeline/python_push.rs::build_env_push_args` | `ocx package push` subprocess |
| Patch republish | `command/package/pipeline/patch.rs::patch_push_args` | `ocx package push` subprocess |
| Legacy `package sync` | `pipeline/push.rs::push_and_cascade` ← `orchestrator.rs:850` | **in-process** `ocx_lib::publisher::Publisher` |

`--sign` on a push signs the **platform manifest**, whose digest is final the
moment it is pushed. The image index is not signable that way — its digest is
rewritten on every platform merge — so it is signed afterwards by a
`sign --tags-file` sweep. That composition is ocx's own documented design, and F3
is why: the sweep's `SkippedBareManifest` arm exists *because* push is expected to
have signed the children.

## Considered Options

### Option A: `--sign` on the subprocess legs; sweep for indexes; per-reference sign for the in-process leg

Three argv builders gain `--sign` plus C-052's flag tail. A new
`pipeline/ocx_cli/sign.rs` owns two invocation shapes: `--tags-file` (indexes,
after the tag write) and single-reference with optional `-p` (the in-process leg,
and the D4 backfill).

| Pros | Cons |
|------|------|
| Matches ocx's intended composition exactly; the sweep's skip rule is built for it | Two mechanisms, and a reader must know which object each covers |
| Zero extra ocx invocations on the subprocess legs — the child signs what it just wrote | Three argv builders to keep in step |
| Per-platform outcome arrives in the existing `PushReport` JSON | |
| Covers every leg, including the legacy in-process one | |

### Option B: Sweep only — no `--sign` anywhere

Every published object is signed after the fact by `ocx package sign`.

| Pros | Cons |
|------|------|
| One mechanism, shared with backfill; no argv changes | **Structurally wrong (F3):** `sign --tags-file` skips bare manifests, so platform manifests of multi-platform packages are never signed |
| | Working around the skip means 1 + N invocations per version and a "is this an index" branch the mirror would have to own |
| | Fights the upstream contract rather than composing with it |

### Option C: `--sign` on subprocess legs; `PackageManager::sign_platforms` in-process for the legacy leg

| Pros | Cons |
|------|------|
| No extra subprocess for the legacy leg | Requires building a `PackageManager` + `SignOptions` in-process — deepens exactly the drift ocx's `arch-principles.md` names |
| Signs the digest the publisher just wrote, no re-resolve | Doubles the `sign:` → options mapping: once as argv, once as a struct |
| | The legacy leg is the *least* used path, paying the most architectural cost |

### Option D: `--sign` on subprocess legs; refuse `sign:` on `package sync`

| Pros | Cons |
|------|------|
| Smallest diff; fail-closed and loud | Breaks the one-pasteable-command principle: `package sync` is the non-GitHub CI path, so signing would be GitHub-only in practice |

### Weighted criteria

| Criterion | Weight | A | B | C | D |
|---|---|---|---|---|---|
| Every published manifest is signed | 5 | 5 | 1 | 5 | 2 |
| Composes with ocx's contract rather than fighting it | 5 | 5 | 1 | 3 | 4 |
| One-pasteable-command reachability | 4 | 5 | 5 | 5 | 1 |
| Does not deepen ocx internal reach | 4 | 5 | 5 | 1 | 5 |
| Mechanism count / legibility | 3 | 3 | 5 | 3 | 4 |
| Reporting fidelity | 2 | 5 | 3 | 4 | 3 |
| **Weighted total (max 115)** | | **106** | **69** | **80** | **69** |

## Decision Outcome — D2

**Chosen: Option A.** ⚠️ **One-way door on the exit-code contract only** — the
codes a signing failure produces become script-visible.

**Mechanism, by object class.** Platform manifests are signed by the push that
writes them. Indexes are signed by a sweep over the tags that run wrote. The
legacy in-process leg has no push child to sign inline, so it signs both classes
through the same single-reference wrapper the backfill uses — once with `-p` per
platform, once without for the tag.

**Fail-closed.** With `sign:` set, a signing failure fails the package: the exit
code is non-zero and no further tag advances. `ocx package push` does not roll
back a landed push on a sign failure and neither does the mirror; the manifest is
published and unsigned, and the run says so with a non-zero exit rather than
pretending otherwise. That window is identical under every option considered.

**Exit-code mapping — reuse ocx's, allocate nothing.** The mirror's `ExitCode` is
`ocx_lib::cli::ExitCode`, the *shared* enum, where **83, 84 and 85 are already
taken** (`TransparencyLogUnavailable`, `ReferrersUnsupported`,
`UnsupportedKeyBackend`) and the first free slot is 86. Rather than invent a
number, `MirrorError::SignFailed { target, code }` carries the child's own
classified exit code through unchanged — the precedent `CascadeUnrepaired` already
sets for ocx's 65, and for the same reason: *a signing outcome must not read as
the tool breaking*. An unrecognised child code falls back to `Failure` (1).

| ocx child exit | Meaning | Mirror exit |
|---|---|---|
| 83 | Rekor unavailable | 83, and the push attempt is **retried** |
| 84 | Referrers unsupported and fallback refused | 84 |
| 85 | KMS backend not implemented | 85 |
| 80 / 77 / 78 | Auth / offline-refused / config | 80 / 77 / 78 |
| 65 / 64 | Bad data / bad usage | 65 / 64 |
| 75 | Transient | retried; on exhaustion, 75 |
| anything else | — | 1 |

**One change to the retry predicate.** `push_exit_is_transient`
(`ocx_cli/push.rs:150`) currently matches 75 alone. Extend it to `{75, 83}`: a
transparency-log outage is the textbook retry case, and 83 is reachable from a
push *only* when `--sign` is present, so the widening cannot affect an unsigned
mirror. Everything else stays non-transient, including 84 and 85, which a rerun
cannot fix.

### Consequences

**Positive:** D2 itself needs nothing new from ocx — `--sign`, `sign -p` and
`--tags-file` all exist at 0.6.0. (The floor still moves, but for D1's endpoint
flags: OCX-C-5.) Signing outcomes are script-discoverable by ocx's existing
taxonomy rather than by a mirror-invented number.

**Negative:** two mechanisms to hold in a reader's head. Mitigated by the rule
being stated as one sentence in `subsystem-mirror.md`: *push signs manifests, the
sweep signs indexes.*

**Risk:** the sweep runs after the tag write, so an interrupted run can leave a
tag advanced and its index unsigned. The backfill (D4) is the repair, and it is
convergent — which is why the backfill is in the same plan rather than a
follow-up.

---

# D3 — `registry copy` v2: the referrer and sidecar walk

## Context

`copy_manifest_tree_at` (`registry_copy.rs:888-997`) currently fetches, verifies
the digest, calls `detect_referrers` (line 925) **before** `push_manifest`
(line 992), recurses into children, then pushes. Two constraints make that order
wrong for v2: GitLab rejects a referrer whose subject does not yet exist, and
distribution-spec [#459](https://github.com/opencontainers/distribution-spec) may
make it normative. Subject first, then referrers.

Sidecar tags (`sha256-<hex>.sig`/`.att`/`.sbom`) are already copied as ordinary
`tags{}` entries — but only when the source root names them, which it does not,
because ocx filters reserved tags at render. So in practice they are dropped for
ocx-published sources and passed through for others, untested either way.

## Considered Options

### Option 1: Referrers API only (mirror `ocx package copy`)

Copy referrers through the native API; refuse (exit 84) when the destination has
none. No fallback index, no sidecar sweep.

| Pros | Cons |
|------|------|
| Byte-for-byte the same posture as `ocx package copy`, already ratified upstream | The acceptance harness runs `registry:2`, which has no Referrers API, so **no test can exercise the good path** |
| Avoids the fallback index's lost-update race entirely | GHCR and GitLab — two of the most-used registries — have no Referrers API either; this is [ocx#392](https://github.com/ocx-sh/ocx/issues/392) as a mirror feature |
| No ocx change needed | Leaves cosign sidecars unhandled, which is the shape most upstreams still publish |

### Option 2: Full walk — referrers + fallback merge + verbatim sidecar sweep

Push the subject, then copy its referrer graph (depth-bounded, visited set, count
and byte caps), merging into the destination's `sha256-<hex>` fallback index where
the Referrers API is absent; sweep the three sidecar tags verbatim.

| Pros | Cons |
|------|------|
| The only option with an exit-0 path to `registry:2`, GHCR and GitLab | Inherits the non-atomic fallback merge; concurrent upstream writers can still lose an update |
| Covers both signature shapes, which is what "carries integrity" now has to mean | Largest surface: three new walk functions, three new counters, a capability probe |
| Both legs testable offline in the existing harness | Needs the D5 ocx seam, putting an ocx release on the critical path |

### Option 3: Sidecar sweep only, no referrers

Copy the three cosign sidecar tags verbatim; keep refusing referrers.

| Pros | Cons |
|------|------|
| No ocx change, no fallback index, no lost-update exposure | Drops the shape cosign v3 writes **by default**, which is the direction of travel |
| Small diff | Recreates the silent-drop defect for the modern format — the precise Harbor failure |

### Weighted criteria

| Criterion | Weight | Opt 1 | Opt 2 | Opt 3 |
|---|---|---|---|---|
| Exit-0 path on the harness and on GHCR/GitLab | 5 | 1 | 5 | 4 |
| Covers both signature shapes | 5 | 3 | 5 | 2 |
| No silent drop of anything upstream attached | 5 | 5 | 5 | 2 |
| Bounded against a hostile referrer graph | 4 | 5 | 4 | 5 |
| Implementation surface | 3 | 5 | 2 | 4 |
| Independence from an ocx release | 3 | 5 | 2 | 5 |
| **Weighted total (max 125)** | | **86** | **109** | **80** |

## Decision Outcome — D3

**Chosen: Option 2.** ⚠️ **One-way door** — S-011 inverts, `CopyError::ReferrersPresent`
is removed, and a documented refusal becomes a documented copy.

**The walk, in order.** For each manifest, per copied *tag*:

1. Fetch, verify `sha256(bytes) == digest`, cap at `MANIFEST_FETCH_CEILING`
   (unchanged).
2. Recurse into children / ensure blobs (unchanged).
3. `push_manifest` — **the subject lands first.**
4. `push_canonical_tag` when `canonical_tags:` (unchanged).
5. **`copy_referrers`** — new, and only at the top of a tag's tree plus each
   platform child.
6. **`copy_sidecars`** — new, per subject digest.

**Bounds, chosen not inherited.**

| Bound | Value | Why |
|---|---|---|
| `REFERRER_DEPTH_CEILING` | 2 | A signature's own attestation is the one real second level. Referrers-of-referrers are spec-possible and untested by every mainstream tool (`research_oci_fallback_merge_registries.md` §6); 2 admits the real shape and refuses a chain. |
| `REFERRER_COUNT_CEILING` | 64 per subject | Bounded refusal, not truncation: past it the package fails with `ReferrerBudgetExceeded`. ocx's verifier inspects at most 8 candidates, so 64 is 8× headroom. |
| Response body | `MANIFEST_FETCH_CEILING` | Same ceiling every other manifest read here uses; the native leg is additionally pre-capped by ocx at 4 MiB / 4096 descriptors. |
| Visited-digest set | per package | A digest cycle needs a sha256 preimage, but a *diamond* does not — two referrers naming one subject is ordinary, and the set is what stops re-copying it. |

**Destination capability.** `ReferrersApiCapability::probe` (already `pub`) over
the D5 transport, cached per registry by ocx. **404 and 405 both mean
Unsupported** — that is already ocx's own doc wording, and the 405 arm is the ECR
case. A successful probe is not proof a subject-bearing PUT succeeds, so **every
referrer PUT's status is observed** and a 405 there re-routes the run to the
fallback index.

**ECR — the one place the fallback does not save us.** ECR refuses the *referrer
manifest itself*, so a fallback index would name a manifest that cannot exist.
That is not degradable: the package fails with `CopyError::SubjectRejected
{ registry, status }`, counted and per-package, naming the rejection. Adopts the
discussion's Open-question recommendation unchanged.

**Sidecar sweep — verbatim, and a conflict is a skip, not a refusal.** Sidecar
tags are derived locally as `referrer_fallback_tag(subject) + ".sig"|".att"|".sbom"`
(`referrer_fallback_tag` is already public and already used at
`registry_copy.rs:1451`; ocx's `sidecar_tag` is `pub(crate)` and is **not**
needed). Each is fetched by tag, copied through `copy_manifest_tree` by its own
digest, and tagged at the destination. An absent tag is a no-op, exactly as
`copy_description` treats an absent `__ocx.desc`.

Where the destination already holds that tag at a **different** digest, the PUT is
**skipped and counted** as `sidecar_conflicts` — not refused. This adopts ocx's own
shape from `bf24416a` ("refuses a same-tag PUT that would clobber a
destination-only signature") and the reasoning matters: refusing the package would
wedge a mirror that has ever been signed locally, while skipping loses nothing —
the destination's signature survives and the source's is still discoverable
through the referrer path, which is where cosign v3 puts it by default. Cosign's
own merge semantics for multiple signatures on one `.sig` tag are **not
documented** (`research_oci_fallback_merge_registries.md` §5), so merging is not an
option anyone can implement correctly.

**`If-Match` is out of scope for v1.** The discussion's constraint note asks for
it opportunistically; ocx's `append_referrer_fallback_index` has no ETag support,
so adding it is a second ocx change on an already-federated critical path. Its
bounded read-append-write-readback is ahead of every shipping tool, the mirror
serialises per package, and the residual is an upstream signer writing the same
index concurrently — which the threat model already treats as a trusted writer.
**Recorded as an ocx follow-up, not a gap.** This overturns the discussion's
constraint note with that reason.

**Local fallback merge: contingency, not scope.** A hand-rolled merge against the
fork's public `pull_manifest_raw`/`push_manifest_raw` is written down as the
recovery if the D5 seam slips, and is **not** implemented speculatively. Building
it pre-emptively would ship two implementations of the lost-update-prone step and
guarantee they diverge.

### Consequences

**Positive:** the mirror stops being the Harbor failure. A signed upstream copies
whole, on a registry with the API and on one without.

**Negative:** the mirror now writes a mutable tag (the fallback index) that anyone
with push access can author — the residual ocx records in
`adr_oci_referrers_signing_v1.md` Amendment 10, inherited knowingly.

**Risk:** the lost-update race. Bounded by ocx's read-back retry, the mirror's
per-package serialisation, and an acceptance test seeding a foreign entry and
asserting it survives.

---

# D4 — The backfill command

## Context

Everything already published is unsigned. `sign_tags` is not idempotent across
runs — it has no `--force` and no signed-by-identity skip, so an unconditional
re-run appends another signature every time, and ocx's verifier inspects at most
`MAX_SIGNATURE_CANDIDATES = 8`. A pre-filter is therefore not an optimisation; it
is what stops repeated backfills from pushing real signatures out of the
verifier's window.

## Considered Options — D4a: name and placement

### Option 1: `ocx-mirror package pipeline sign`

Seventh member of the pipeline family, beside `pipeline cascade` and
`pipeline patch`.

| Pros | Cons |
|------|------|
| `pipeline cascade` is the exact precedent: reads `mirror.yml`, drives an ocx verb over the target, dispatch/schedule-triggered, is a repair | Deep verb path for something an operator runs by hand |
| Inherits `SpecSlot`, the renderer conventions and the aux-workflow shape for free | |

### Option 2: `ocx-mirror registry sign`

| Pros | Cons |
|------|------|
| Short | Wrong spec root: the `registry` namespace takes `registry.yml` (`RegistrySpec`), and the backfill needs `mirror.yml`'s `target:` and `sign:` |

### Option 3: `ocx-mirror package sign`

| Pros | Cons |
|------|------|
| Shortest correct path; sibling of `package sync`/`check`/`validate` | Those three are *publish* verbs over a source; this is a repair over the target — it belongs with the other repair |
| | Collides conceptually with `ocx package sign`, one word apart, doing a different thing |

**Chosen: Option 1 — `ocx-mirror package pipeline sign`.**

## Considered Options — D4b: the pre-filter rule

The threat-model tension, stated plainly:
`research_signature_backfill_identity.md` §1 says a keyless SAN/issuer read is
spoofable without chain validation, because anyone with repository write access
can attach a self-signed certificate claiming any identity and force a skip.
`.claude/rules/security-threat-model.md` puts *"whoever can write to the
destination registry"* out of scope by owner ruling. So the attacker that chain
validation defends against is, in this project, trusted.

### Option 1: Listing-only — discover, read identity fields, no crypto

| Pros | Cons |
|------|------|
| The only attacker it admits is one the threat model already trusts | Reading an unvalidated certificate field and acting on it is a pattern that reads wrong out of context |
| Needs one non-verifying ocx seam, no trust-root fetch, no Rekor round trip | Wrong by cosign's `SIGNATURE_SPEC.md`, which requires chain validation before trusting any certificate field |
| Fast enough to run over a whole repository on a schedule | |

### Option 2: Chain-validate (Fulcio root, SCT, validity window; no Rekor)

| Pros | Cons |
|------|------|
| Correct against an out-of-model attacker; matches the research recommendation | Needs a validating API in ocx that does not exist, plus a trust root available offline |
| Cheap relative to full verify | Defends a boundary the owner ruled out of scope — cost with no in-model benefit |

### Option 3: `ocx package verify` per subject

| Pros | Cons |
|------|------|
| Uses only shipped surface; unambiguously correct | Needs a `[[trust.policy]]` the mirror does not have and would have to invent |
| | Turns a backfill into a verification sweep with a Rekor round trip per subject |

### Weighted criteria

| Criterion | Weight | Opt 1 | Opt 2 | Opt 3 |
|---|---|---|---|---|
| Correct under *this* threat model | 5 | 5 | 5 | 5 |
| Buildable without new trust-root machinery | 5 | 5 | 2 | 3 |
| Cost per subject | 4 | 5 | 4 | 1 |
| Residual is cheap to close later | 4 | 4 | 5 | 5 |
| Honest about what it does and does not prove | 4 | 4 | 5 | 5 |
| **Weighted total (max 110)** | | **97** | **83** | **74** |

**Chosen: Option 1 — listing-only**, with the residual recorded and made cheap to
close: the filter is a **pure function over `Vec<SignerCandidate>`**, so swapping a
validating discoverer in later changes the producer and nothing else. Every
identity field on `SignerCandidate` is documented as *unvalidated* at the type
(D5b), which is the mechanism that stops a future caller mistaking discovery for
verification.

## Decision Outcome — D4

**Command:** `ocx-mirror package pipeline sign [SPEC] [--dry-run] [--force] [--format text|json]`,
default spec `./mirror.yml`.

**Inputs and walk.** Spec → `target.registry`/`target.repository` → tags via
`pipeline/target_registry.rs::list_target_tags` (fail-safe: only an authoritative
not-found counts as absent) → platforms per tag via `fetch_published_platforms`.
Scoped to mirror-produced repositories by construction: the only repository it
ever touches is the spec's own `target:`.

**The skip rule, and a named refinement.** The ratified wording is *"drops
subjects already signed by the configured identity."* The mirror has no configured
identity to compare against — under keyless it signs with whatever ambient OIDC
the workflow presents, whose SAN *is* the workflow ref. So the default rule is:
**skip a subject that already carries at least one signature candidate of the
configured `format`**, narrowed by optional `--identity <san>` / `--issuer <url>`
flags, and bypassed by `--force`. Under the threat model — destination writers
trusted — "any signature" and "a signature by us" are the same answer, and the
flags cover rotation when they are not.

Key mode uses the same rule. The self-authenticating alternative
(`verificationMaterial.publicKey.hint` compared against the SHA-256 of the
configured key's DER public key, `research_signature_backfill_identity.md` §2)
needs the *public* half of a key the mirror holds only as a private PEM in an
environment variable, which means deriving a public key — new crypto the mirror
does not own. **Deferred with a reason, not overlooked**; `--force` covers
rotation meanwhile.

**Two passes per tag, because F3 says one is not enough.**

1. **Indexes** — the filtered tag list goes to `ocx package sign --tags-file`, one
   invocation. `SweptOutcome::SkippedBareManifest` rows are collected, not treated
   as failures.
2. **Platform manifests** — for each tag with published platforms, one
   `ocx package sign -p <platform> <ref>` per platform. For each tag that came back
   `SkippedBareManifest`, one `ocx package sign <ref>` with no `-p` (the bare
   manifest *is* the subject, and nothing signed it at publish time).

**Failure policy.** Per-subject fail-closed: one subject's failure is a `failed`
row, never an abort — `sign_tags` never returns `Err` by design, and the
single-reference calls match it. A transient child exit (75, and 83 per D2) is
retried with `push_retry_backoff` + `jitter`, reused unchanged. `--dry-run`
reports the filter's verdict per subject and signs nothing.

**Report.** `BatchReport`-shaped per PKG-21/24/25:

```json
{
  "summary": { "status": "partial_failure", "total": 42, "succeeded": 39,
               "failed": 2, "skipped": 1, "exit_code": 83 },
  "items": [
    { "tag": "3.28.1", "platform": null, "status": "succeeded",
      "subject": "sha256:…", "discovery": "referrers_api" },
    { "tag": "3.28.1", "platform": "linux/amd64", "status": "skipped",
      "reason": "already_signed" },
    { "tag": "3.27.9", "platform": "linux/arm64", "status": "failed",
      "error": { "code": "transparency_log_unavailable", "exit": 83 } }
  ]
}
```

Process exit is the **worst** classified failure among `failed` items, per PKG-24
— never a count, never a new partial-success code.

**ocx floor: moved by D1, not by D4.** `--tags-file`, `sign -p` and `--sign` all
exist at 0.6.0, so the backfill asks nothing new of ocx. D1's endpoint flags
(OCX-C-5) and D3's transport seam (D5) are what put an ocx release on the critical
path, and the `ocx.toml` floor bumps to that release.

---

# D5 — ocx satellite seams

## Context

The mirror needs three things from ocx that it cannot reach today, and one
acceptance-coverage debt.

The referrer vocabulary is **already public**: `OciTransport` is a `pub` trait
(`oci/client/transport.rs:336`) whose `push_referrer_manifest`,
`list_referrers`, `list_referrers_with_fallback`, `pull_referrer_fallback_index`
and `append_referrer_fallback_index` are all public methods, the last two with
correct default implementations carrying `MAX_FALLBACK_ATTEMPTS = 5`,
`MAX_FALLBACK_DESCRIPTORS = 4096` and `MAX_FALLBACK_INDEX_BYTES = 4 MiB`.
`ReferrersApiCapability::probe` is `pub` and takes `&dyn OciTransport`.
`ReferrersListing`, `DiscoveryMethod` and `FallbackAppend` are `pub`.

**The single missing piece is a way to obtain a `&dyn OciTransport` from outside
the crate.** The one implementor, `NativeTransport`, is `pub(super)`
(`oci/client/native_transport.rs:37`) and `Client::transport()` is `pub(crate)`
(`oci/client.rs:223`).

## Considered Options — D5a: the transport seam

### Option 1: One public factory returning a boxed transport

```rust
pub fn native_transport(client: oci::native::Client, auth: auth::Auth) -> Box<dyn OciTransport>
```

| Pros | Cons |
|------|------|
| One new symbol; the concrete type stays private | Hands out an operation surface, against the "operations go through the CLI" doctrine |
| Unlocks the *entire* referrer vocabulary, all of it already `pub` | |
| Touches neither `oci/copy.rs` nor the sign pipeline | |

### Option 2: Make `NativeTransport` public and re-export it

| Pros | Cons |
|------|------|
| No new function | Publishes a concrete type with two constructor parameters and an inherent-method surface nobody outside needs |
| | Every future field on it becomes a semver concern |

### Option 3: Widen `attach_referrer` + `referrers_capability` + `Client::transport()`

The brief's suggestion.

| Pros | Cons |
|------|------|
| Reuses the sign pipeline's own composition | Three symbols instead of one, two of them sign-pipeline internals |
| | `attach_referrer` also *pushes* the referrer manifest, which the mirror does through its own verified-bytes path — half the function is unwanted |
| | `Client::transport()`'s doc states the public API deliberately never exposes a transport; widening it contradicts a written decision rather than extending one |

### Weighted criteria

| Criterion | Weight | Opt 1 | Opt 2 | Opt 3 |
|---|---|---|---|---|
| New public symbols (fewer is better) | 5 | 5 | 4 | 2 |
| Does not touch `oci/copy.rs` (exit-84 contract) | 5 | 5 | 5 | 5 |
| Does not contradict a written ocx decision | 4 | 2 | 2 | 1 |
| Gives the mirror everything D3 needs | 5 | 5 | 5 | 4 |
| Semver surface created | 3 | 5 | 2 | 3 |
| **Weighted total (max 110)** | | **98** | **86** | **68** |

**Chosen: Option 1.**

## Decision Outcome — D5

⚠️ **One-way door** — `ocx_lib` public API; and it puts an ocx release on the
critical path for D3 alone.

### OCX-C-1 — the transport factory

**File:** `crates/ocx_lib/src/oci/client.rs` (beside `Client`, which is where
transport construction already lives).

```rust
/// Builds an [`OciTransport`] over an already-configured fork client.
///
/// The one public constructor for a transport. `ocx-mirror` owns its own
/// registry-client construction policy (`adr_registry_mirror_sync.md`, Open
/// question 1, amended 2026-08-14) and needs the referrer vocabulary this
/// trait already exposes publicly: the capability probe, the referrer manifest
/// PUT, and the tag-schema fallback merge. Without a way to *name* a transport
/// from outside this crate, those public methods are unreachable.
///
/// This is a deliberate, scoped exception to the "registry operations go
/// through the CLI surface" doctrine in `arch-principles.md`: the fallback
/// merge has no CLI spelling, and `ocx package copy` refuses the
/// referrers-less target by ratified decision ([ocx#392]).
///
/// `auth` is consulted per registry, exactly as `Client` consults it.
pub fn native_transport(
    client: crate::oci::native::Client,
    auth: crate::auth::Auth,
) -> Box<dyn OciTransport>;
```

**Rules it must satisfy:** `arch-principles.md` "Core vs Plugin Boundary" (the
exception is named in the doc comment, not left implicit) · "Code Style
Conventions → Type names" (no abbreviations) · `rust-quality/architecture.md`
ARCH-15 (bare `pub` only for a genuine external contract — ocx-mirror is the named
consumer) · ARCH-09 (`Box<dyn>` justified: the trait *is* the dispatch seam and
the concrete type must stay private) · `docs-and-tracing.md` DOC-02 (`# Errors`
where fallible — this one is not).

**Must not touch** `oci/copy.rs::copy_referrers`. Its exit-84 contract is ratified
in `adr_package_copy.md` and `plan_issue_sweep_2026-08-30.md` D4, and
`ocx package copy` must continue never writing the fallback index.

### OCX-C-2 — signature discovery without verification

**File:** new `crates/ocx_lib/src/oci/verify/candidates.rs` (ocx convention: one
concept per file, no `mod.rs`), re-exported from `oci/verify.rs`.

```rust
/// A signature candidate attached to a subject, with the identity fields a
/// caller can match on — and **no verification performed on any of them**.
///
/// Every identity field below was read out of a certificate whose chain was not
/// checked, or out of a bundle whose signature was not verified. A caller
/// deciding policy from these values is trusting whoever could write to the
/// registry. `ocx package verify` remains the only answer to "is this signature
/// good"; this type exists so a sweep can ask the cheaper question "is there
/// already one here".
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SignerCandidate {
    /// How this candidate was found.
    pub discovery: DiscoveryMethod,
    /// The referrer or sidecar manifest digest.
    pub digest: crate::oci::Digest,
    /// The referrer's `artifactType`, when it carried one.
    pub artifact_type: Option<String>,
    /// Certificate SAN, for a keyless signature. **Unvalidated.**
    pub certificate_identity: Option<String>,
    /// Certificate OIDC issuer, for a keyless signature. **Unvalidated.**
    pub certificate_issuer: Option<String>,
    /// `verificationMaterial.publicKey.hint` — the SHA-256 of the DER public
    /// key — for a key-pair signature. Self-authenticating against a
    /// configured key: a forged hint only yields a signature that then fails
    /// verification.
    pub public_key_hint: Option<String>,
}

/// Lists every signature candidate attached to `subject`, verifying none.
///
/// Discovery only: the Referrers API with the OCI tag-schema fallback
/// ([`OciTransport::list_referrers_with_fallback`]), plus the three cosign
/// sidecar tags. No certificate chain is validated, no Rekor entry is fetched,
/// and no trust policy is consulted.
///
/// # Errors
///
/// Whatever the transport raises. An absent signature is an **empty vector**,
/// never an error — "nothing found" and "could not look" stay distinct, the
/// same split [`OciTransport::list_referrers_with_fallback`] already makes.
pub async fn list_signature_candidates(
    transport: &dyn OciTransport,
    image: &crate::oci::native::Reference,
    subject: &crate::oci::Digest,
) -> Result<Vec<SignerCandidate>, ClientError>;
```

**Rules:** `#[non_exhaustive]` on a public struct that will grow
(`rust-quality/data-and-formats.md` DATA-FMT-06 — it *is* matched outside the
crate) · ARCH-04 (the digest is a `Digest` newtype, never a `String`) · every
`Option<String>` identity field carries the "unvalidated" word in its own doc
comment, which is the whole safety mechanism.

### OCX-C-3 — acceptance coverage

Extend, do not build. `test/tests/test_sign.py` and `test_cosign_matrix_*.py`
already cover keyless and key-pair signing and both formats against the
self-hosted stack. Add:

- `ocx package push --sign` writing one signature per **platform manifest**, then
  `ocx package verify` on each platform.
- `ocx package sign --tags-file` sweeping the **index**, then verify on the tag.
- `ocx package sign -p <platform> <tag>` narrowing into an index child, and the
  documented error when the reference resolves to a bare manifest.
- `--key env://NAME` with `OCX_KEY_PASSWORD`, over the same three shapes.
- Both `bundle` and `simplesigning`, on the self-hosted Fulcio/Rekor stack.

### OCX-C-4 — [ocx#391](https://github.com/ocx-sh/ocx/issues/391) read-back

**Out of scope, recorded.** `append_referrer_fallback_index` already reads back;
only `push_referrer_manifest` does not. The mirror's own acceptance test reads the
destination back, so the gap is covered where it matters here. Not a blocker.

---

# D6 — Harness, renderer, and CI

## Considered Options — D6a: the Sigstore stack in the mirror harness

ocx's `test/docker-compose.yml` carries seven `sigstore`-profile services (`dex`,
`sigstore-ct`, `fulcio`, `sigstore-mysql`, `trillian-log-server`,
`trillian-log-signer`, `rekor`) with committed test-only key material. Its default
registry ports are 5000/5001/5003/5004; the mirror's harness binds 5001/5002. The
5001 collision is real and has bitten before.

### Option 1: Compose `include:`/`extends` of ocx's file

| Pros | Cons |
|------|------|
| Declarative, no new service definitions | `include:` merges *all* services, registries included — the 5001 collision, structurally |
| | Silent failure mode: the mirror's tests would run against ocx's registry |

### Option 2: Bring up ocx's stack by explicit service names

`docker compose -f <ocx>/test/docker-compose.yml --profile sigstore up -d fulcio rekor trillian-log-signer`

| Pros | Cons |
|------|------|
| Starts exactly the seven Sigstore services and no registry — compose resolves only the named services plus their `depends_on` closure | A cross-repo path dependency in the mirror's harness |
| Zero duplication of 180 lines of compose and the committed key material | The sibling clone must exist; a machine without it has to skip with a reason |
| Key material, config and readiness semantics stay owned upstream | |

### Option 3: Copy the seven service definitions and the `sigstore/` fixtures

| Pros | Cons |
|------|------|
| Self-contained; no sibling clone needed | Duplicates committed CA keys and 180 lines of compose in a second repository |
| | Drifts on every upstream Sigstore bump, silently |

### Weighted criteria

| Criterion | Weight | Opt 1 | Opt 2 | Opt 3 |
|---|---|---|---|---|
| No port collision with ocx's registries | 5 | 1 | 5 | 5 |
| No duplication of key material | 5 | 5 | 5 | 1 |
| Drift resistance on an upstream bump | 4 | 4 | 5 | 1 |
| Works without a sibling clone | 3 | 2 | 1 | 5 |
| **Weighted total (max 85)** | | **56** | **78** | **52** |

**Chosen: Option 2.** ⚠️ `trillian-log-signer` **must be named explicitly** — it
appears in no `depends_on`, and without it Rekor accepts entries that are never
integrated, so the SET stays unverifiable and every keyless test fails in a way
that reads as a Fulcio bug.

## Decision Outcome — D6

**Harness.**

- **zot service** in `test/docker-compose.yml` for the native-Referrers-API leg,
  pinned by **digest** (not `latest`, not a tag), on host port **5011** — clear of
  ocx's entire 5000–5004 default range, so the two projects never contend.
  ⚠️ **zot garbage-collects untagged manifests, and a referrer is untagged.** The
  mirror's own compose comment already records that zot GCs while `registry:2`
  does not. The mirror ships its own `test/zot-config.json` with GC disabled, or
  the referrer-copy assertions are flaky in a way that looks like a copy bug.
- **`registry:2` keeps the fallback-index leg** (5001/5002, unchanged).
  `registry:3` gains nothing — distribution still does not implement the Referrers
  API as of 3.1.1
  ([distribution#4828](https://github.com/distribution/distribution/pull/4828) is
  unmerged).
- **Sigstore bring-up** in `test/src/helpers.py`: the compose file path from
  `OCX_SIGSTORE_COMPOSE` (default `../ocx/test/docker-compose.yml`), the service
  list in one named constant, and a host-side `wait_for_sigstore()` polling dex
  `/dex/healthz`, Fulcio `/api/v2/trustBundle` and Rekor `/api/v1/log`. Four of
  the seven images are distroless, so a compose `healthcheck` on them would be a
  green that never ran — readiness is polled from the host, as upstream does.
  A missing sibling clone **skips with a reason**, never fails silently.
- **`MirrorRunner.env` is a constructed whitelist**, not an inherited environment
  (`test/src/mirror_runner.py:15-19`), so every signing variable must be added
  explicitly: `SIGSTORE_FULCIO_URL` and `SIGSTORE_REKOR_URL` (the two endpoint
  variables the signing fixture names), the `env://` key variable, and
  `OCX_CONFIG` pointing at a fixture `config.toml` that keeps **only**
  `[trust.sigstore].trusted_root` — the verification side. The split is the point:
  the harness asserts the publish side ignores that config's endpoints (S-061).
  `OCX_CONFIG` is already in `OCX_VARS`, so it reaches the ocx child unchanged.

**Renderer — `generate/ci/permissions.rs`.**

- `id-token: write` is added **registry-agnostically** whenever `sign:` is set, by
  extending the existing block. One `permissions:` block per job, never a second.
- ⚠️ **Deliberate narrowing, and it reaches every downstream repository.** Naming
  any permission sets every unnamed scope to `none`. Today a non-GHCR target
  renders *no* block and keeps the repository's default token scopes; with `sign:`
  set it renders `contents: read` + `id-token: write` and loses everything else.
  That is correct and it is a behaviour change — it needs a golden fixture and a
  changelog line.
- Jobs that gain it: push (signs platform manifests), patch (republishes, so
  signs), and the rendered backfill job if one is emitted. Discover does not sign
  and does not get it.
- **Secret and endpoint mapping:** every `env://NAME` the spec names under
  `passphrase`, `identity_token` or `key` renders `NAME: ${{ secrets.NAME }}`;
  every `env://NAME` under `fulcio` or `rekor` renders `NAME: ${{ vars.NAME }}`.
  Signing steps only. The ref names the variable; the workflow maps it from the
  identically-named secret or variable. No second mapping field.
- **Fork gating:** a `pull_request` run from a fork gets no OIDC token
  ([community#137761](https://github.com/orgs/community/discussions/137761)). Any
  signing step reachable from `pull_request` carries
  `if: github.event_name != 'pull_request'`, pinned by a golden test.

**Docs.**

| File | Change |
|---|---|
| `docs/reference/registry-yml.md:468` | Replace "does not copy signatures or attestations" with carriage: the referrer walk, the fallback merge, the sidecar sweep, the sidecar-conflict skip, and the ECR refusal. Add the relocation note — copies verify under cosign and ocx, while skopeo/podman's default `signedIdentity: matchRepoDigestOrExact` rejects a relocated repository and needs `matchRepository` or a remap. |
| `docs/reference/mirror-yml.md` | New `sign:` section, modelled on `notify:` (804-848) / `announce:` (878+). States that KMS references exit 85 on ocx 0.6.0. |
| `docs/reference/cli.md` | `package pipeline sign` (template: `pipeline patch`, 220-252) plus the exit rows (299-312). |
| `docs/reference/environment.md:89-127` | The deliberate-omission note on `OCX_VARS` (no `env_clear()`, the child inherits), **and** the F2 caveat, now narrowed: under `ocx mirror …` plugin dispatch the three conventional credential variables are stripped, so `key: env://OCX_SIGNING_KEY` fails and the remedy is a rename. `passphrase` and `identity_token` are immune — the mirror resolves them itself and re-exports them onto the child. |

**The four-line non-GitHub job**, documented alongside the rendered workflow so the
one-pasteable-command principle survives:

```yaml
# GitLab CI — keyless, ambient OIDC. mirror.yml carries
#   sign: { keyless: { fulcio: env://SIGSTORE_FULCIO_URL, rekor: env://SIGSTORE_REKOR_URL } }
sign:
  id_tokens: { SIGSTORE_ID_TOKEN: { aud: sigstore } }
  script: [ "ocx-mirror package pipeline push", "ocx-mirror package pipeline sign" ]
```

Jenkins uses `oidc-provider-plugin` with `aud: sigstore`; a cron box uses key mode
(`sign: { key: { ref: file:///run/secrets/mirror.key, passphrase: env://… } }`).

**e2e repository.** `e2e-ocxmirror-signing` under the owner's personal profile,
**permanent**, re-pinned per Deploy Dev build; deletion is the owner's act. Adopts
the discussion's Open-question recommendation. Creation is an outward-facing act
and is confirmed with the owner **at execute time**, never assumed by this ADR.

---

## Technical Details

### Architecture

```
mirror.yml  sign: keyless { fulcio?, rekor?, identity_token? }
                 xor key: <ref> | { ref, passphrase?, rekor? }
     │
     ├── PRODUCE ────────────────────────────────────────────────────────────
     │   ocx_cli/push.rs      ─┐  resolve_sign() once per run → ResolvedSign
     │   python_push.rs        ├─ + --sign + C-052 tail (endpoints or --key)
     │   pipeline/patch.rs    ─┘        → signs each PLATFORM MANIFEST
     │
     │   pipeline/ocx_cli/sign.rs  (new)
     │     ├── invoke_sign_sweep(tags_file)   → signs each INDEX
     │     └── invoke_sign_reference(ref, -p) → the in-process leg, and D4
     │
     │   pipeline/push.rs::push_and_cascade  (in-process Publisher)
     │     └── after the tag write: invoke_sign_reference per platform, then tag
     │
     ├── CARRY ──────────────────────────────────────────────────────────────
     │   registry_copy.rs::copy_manifest_tree_at
     │     1 fetch + verify digest      4 push_canonical_tag
     │     2 recurse children/blobs     5 copy_referrers    (new, depth ≤ 2)
     │     3 push_manifest  ← SUBJECT   6 copy_sidecars     (new, verbatim)
     │        LANDS FIRST                  .sig / .att / .sbom
     │
     │   destination capability → ocx_lib::oci::client::native_transport()
     │     Supported   → push_referrer_manifest
     │     Unsupported → push_referrer_manifest + append_referrer_fallback_index
     │     405 on PUT  → CopyError::SubjectRejected (per-package, counted)
     │
     └── BACKFILL ───────────────────────────────────────────────────────────
         package pipeline sign
           target_registry::list_target_tags / fetch_published_platforms
             → list_signature_candidates (ocx seam, no verification)
             → filter (pure fn over Vec<SignerCandidate>)
             → pass 1: sign --tags-file   (indexes)
             → pass 2: sign -p <platform> (platform manifests)
             → BatchReport { summary, items }
```

### Component contracts

Numbered from **C-050** so they never collide with `registry sync`'s C-001…C-047.
Each is testable without reading implementation code.

| ID | Contract |
|---|---|
| **C-050** | `SignConfig` (`src/spec/sign_config.rs`) deserializes from `sign:` with `deny_unknown_fields` on every struct; shape per the D1 YAML; `Ref` parsed at deserialization; `MirrorSpec.sign: Option<SignConfig>`; the JSON schema golden gains the block; the `KeyConfig` untagged no-variant-matched error message is asserted by a test. An unknown key exits 65. |
| **C-051** | `validate_sign_config` refuses, all as `SpecUsageError` (64), naming the field and never echoing the value: `sign: {}`; both tags present; `key: {}` or a `key` map without `ref`; a `ref` that is empty or contains `BEGIN ` (a literal PEM); a secret-class field (`passphrase`, `identity_token`) given a literal; an `env://` NAME not matching `^[A-Z_][A-Z0-9_]*$`; a `file://` with an empty path. |
| **C-052** | `sign_push_args(&ResolvedSign) -> Vec<String>` is pure. Keyless: `--sign --fulcio-url <U> --rekor-url <U>`, both always present. Key: `--sign --key <ref>` then either `--rekor-upload --rekor-url <U>` or `--no-rekor-upload`. Nothing else, fixed order; `None` yields an empty vector. |
| **C-053** | `scan_for_credentials` runs inside `load_spec` on the merged `mirror.yml`, before deserialization. A key in `CREDENTIAL_DENY_LIST` at any depth exits 64 naming the dotted key path and the `OCX_AUTH_<slug>_TOKEN` remedy. No document value ever appears in the message. The `kind:` check does **not** run for `mirror.yml`. |
| **C-054** | `resolve_sign(&SignConfig, lookup: &dyn Fn(&str) -> Option<OsString>, read: &dyn Fn(&Path) -> io::Result<Vec<u8>>) -> Result<ResolvedSign, MirrorError>` is pure over injected env and file readers (ARCH-12): it resolves every `Ref`; secret refs land in `child_env` under `OCX_IDENTITY_TOKEN` / `OCX_KEY_PASSWORD`; an unset variable or an unreadable file yields `SignMaterialMissing { field, source }` where `source` is the variable name or the path, never a value; file reads are capped at `MAX_SECRET_FILE_BYTES`. `ocx_child_env(&ResolvedSign)` applies `child_env` to every ocx child. Nothing is logged. `OCX_VARS` is not extended (deliberate-omission comment at `ocx_cli.rs`). Called once per pipeline run, before the first push. |
| **C-055** | With `sign:` set and no signing material reachable, the package fails: exit non-zero, no further tag advanced, and the stderr message names the field and the variable name or path (C-054), or carries ocx's own absent-ambient-provider message. Asserted with the exit code and the stream checked separately. |
| **C-056** | `MirrorError::SignFailed { target, code }` maps to the ocx child's own classified exit code; an unrecognised code maps to `Failure` (1). Locked by a table test over `{83, 84, 85, 80, 77, 78, 75, 65, 64, 0xFF}`. |
| **C-057** | `push_exit_is_transient` returns true for exactly `{75, 83}` and false for every other code including `None`. |
| **C-058** | `invoke_sign_sweep(config, tags_file, reference)` builds `--format json package sign --tags-file <path> <ref>` plus C-052's tail minus `--sign`, in that order, bounded by a timeout, `kill_on_drop(true)`. Pure argv assembly is unit-tested without a subprocess. |
| **C-059** | `invoke_sign_reference(config, reference, platform)` emits `-p <platform>` iff `platform` is `Some`. Otherwise identical to C-058's flag tail. |
| **C-060** | `copy_manifest_tree_at` calls `push_manifest` **before** any referrer or sidecar operation. Pinned structurally: a test asserts the destination holds the subject digest at the moment the first referrer PUT is issued. |
| **C-061** | `copy_referrers` walks to `REFERRER_DEPTH_CEILING = 2`, holds a per-package visited-digest set, and fails the package with `ReferrerBudgetExceeded { subject, limit }` past `REFERRER_COUNT_CEILING = 64` per subject. A diamond (two referrers, one subject) copies the subject once. |
| **C-062** | `copy_sidecars` copies `sha256-<hex>.sig`, `.att` and `.sbom` **byte-for-byte** by digest; an absent tag is a no-op; a destination tag at a different digest is skipped and counted in `sidecar_conflicts`, and the package still succeeds. |
| **C-063** | The capability **probe** treats **both** 404 and 405 as Unsupported (fallback-index route). A 405 on the **referrer manifest PUT itself** — the only subject-bearing request the walk makes — fails the package with `SubjectRejected { registry, status }`, counted, per-package, never a run abort, and **writes no fallback index** for that subject. The two 405s are different requests; the PUT never re-routes. *(Amended 2026-09-02, review: the earlier wording gave one request two arms.)* |
| **C-064** | `CopyStats` gains `referrers_copied`, `sidecars_copied`, `sidecar_conflicts`; all three reach `PackageReport` through `record_tag` → `sync_package` → `PackageOutcomeRow` and appear in both report renderings. |
| **C-065** | `missing_descriptors` (`--dry-run`) measures referrer and sidecar bytes and **never fails** on their presence. |
| **C-066** | `CopyError::ReferrersPresent` is removed. `is_whole_run_abort` stays true only for `Abort`; `SubjectRejected` and `ReferrerBudgetExceeded` are aggregating. |
| **C-067** | `already_signed(&[SignerCandidate], &SignFilter) -> bool` is pure. Default: true when any candidate matches the configured `format`. With `--identity`/`--issuer`: true only when a candidate matches both. With `--force`: always false. |
| **C-068** | `package pipeline sign` emits `{ "summary": {...}, "items": [...] }` under `--format json`, never a bare array. `summary.status ∈ {success, partial_failure, failure, cancelled}`, item `status ∈ {succeeded, failed, skipped}`, `summary.exit_code` equals the process exit code. |
| **C-069** | The process exit code is the worst classified failure among `failed` items, never derived from counts. Locked by a test mixing an 83 and a 65 and asserting 83. |
| **C-070** | `--dry-run` on the backfill reports the filter verdict per subject and issues no `ocx package sign` invocation. |
| **C-071** | `render_push_permissions` emits `id-token: write` whenever `sign:` is set, for **every** target registry, inside the single existing `permissions:` block. Pinned by golden fixtures for GHCR and non-GHCR targets, with and without `sign:`. |
| **C-072** | Every rendered signing step reachable from a `pull_request` trigger carries `if: github.event_name != 'pull_request'`. Pinned by a golden test. |
| **OCX-C-1** | `ocx_lib::oci::client::native_transport(client, auth) -> Box<dyn OciTransport>` is public. `oci/copy.rs` is unchanged. |
| **OCX-C-2** | `list_signature_candidates` returns an empty vector for a subject with no signatures and an `Err` only when the transport could not answer; the Referrers API is followed through `Link` pagination to exhaustion, bounded by `MAX_FALLBACK_DESCRIPTORS`, so a heavily-referrer'd subject is never undercounted past one page. Every identity field is documented unvalidated. *(Pagination clause added 2026-09-02, review.)* |
| **OCX-C-3** | ocx acceptance covers push `--sign` per platform, push `--sign --fulcio-url --rekor-url` against the local stack, `sign --tags-file` over an index, `sign -p` narrowing, keyless and `env://` key, both formats, on the self-hosted stack. |
| **OCX-C-5** | `ocx package push --sign` gains `--fulcio-url <URL>` and `--rekor-url <URL>` with `package sign`'s semantics — `--fulcio-url` carries `conflicts_with = "key"`, `--rekor-url` is allowed in key mode together with `--rekor-upload` — plumbed as the two `Option`s into `resolve_sigstore_pair`. Validation is unchanged, help text is ASCII, and a unit test asserts the resolved pair prefers the flag over `[trust.sigstore]`. |

### Boundaries honoured

Named explicitly, because each is a convention a diff can breach silently.

| Boundary | How this design stays inside it |
|---|---|
| **Mirror `lib.rs` public surface is `Command`, `error`, `spec` and nothing else** — a wider surface would silence `dead_code`, which the crate denies on | `SignConfig` lives in `spec/sign_config.rs` and is glob re-exported through `spec.rs`, exactly as `RegistrySpec` and `DistSpec` are (C-008's precedent). `MirrorError::SignFailed` is a variant on an already-public enum. `pipeline/ocx_cli/sign.rs` is `pub(crate)`. `lib.rs` is not edited. |
| **`ocx package copy`'s contract must not change** | D5 touches `oci/client.rs` and adds `oci/verify/candidates.rs`. `oci/copy.rs::copy_referrers` keeps its exit-84 refusal and still never writes the fallback index (`adr_package_copy.md`, `plan_issue_sweep_2026-08-30.md` D4). |
| **Threat model** (`security-threat-model.md`) | Every new input that crossed the network is bounded before use: referrer graphs by depth, count and bytes (C-061); every copied manifest digest-verified before republish (unchanged). The one place the model is *invoked to permit* something — D4's unvalidated identity read — names the trusted actor and records the residual rather than hiding behind the ruling. |
| **Crate module shape** — flat siblings under `pipeline/` with per-module child directories | `pipeline/ocx_cli/sign.rs` sits beside `push.rs` and `announce.rs`; `registry_copy.rs` grows functions, not a new sibling. The backfill command is a seventh `command/package/pipeline/` verb, not a new namespace. |
| **Test layout** — no inline `#[cfg(test)]`; corpora in sibling `tests/` via `#[path]` | New unit tests land in `pipeline/ocx_cli/sign/tests.rs`, `spec/sign_config/tests.rs`, and extend `pipeline/registry_copy/tests/`. |
| **`OCX_VARS` is not extended** | The child inherits the ambient environment (no `env_clear()` anywhere), so any variable an `env://` ref names already reaches ocx. `ocx_cli.rs:42` gains a deliberate-omission comment, not new entries — matching ocx's own credential-exemption reasoning. C-054 *sets* `OCX_IDENTITY_TOKEN` / `OCX_KEY_PASSWORD` on the child from resolved refs; it does not add them to the forward list. |

### Data model — `SignConfig`

```rust
/// `sign:` — publish-side signing. Absent = nothing is signed. Exactly one
/// of `keyless` / `key` — serde cannot express the xor, `validate_sign_config` does.
#[derive(Deserialize)] #[serde(deny_unknown_fields)]
pub struct SignConfig { keyless: Option<KeylessConfig>, key: Option<KeyConfig> }

#[derive(Deserialize)] #[serde(deny_unknown_fields)]
pub struct KeylessConfig { fulcio: Option<Ref>, rekor: Option<Ref>, identity_token: Option<Ref> }

/// `key: <ref>` or `key: { ref, passphrase?, rekor? }`.
#[derive(Deserialize)] #[serde(untagged)]
pub enum KeyConfig { Reference(Ref), Full { #[serde(rename = "ref")] reference: Ref, passphrase: Option<Ref>, rekor: Option<Ref> } }

/// literal | env://NAME | file://PATH — parsed once at the trust boundary (ARCH-04);
/// `Debug`/`Display` print the ref, never a resolved value.
pub struct Ref(RefKind);  enum RefKind { Literal(String), Env(String), File(PathBuf) }

/// Output of `resolve_sign`: endpoints as strings, secrets as `SecretString`,
/// hand-written `Debug` (API-02), never `Serialize`.
pub struct ResolvedSign { mode: ResolvedMode, child_env: BTreeMap<&'static str, SecretString> }
```

---

## User-experience scenarios

**S1 — a GitHub mirror turns signing on.** Adds `sign: { keyless: {} }` to
`mirror.yml`, runs
`ocx-mirror package pipeline generate ci`. The push job's `permissions:` block
gains `id-token: write`; on a non-GHCR target the block appears for the first
time. Next scheduled run: each platform manifest is signed inline by its push,
the index by the closing sweep. `ocx package verify` on the tag passes with the
workflow identity in the policy.

**S2 — no OIDC available.** `sign:` is set; the run is a fork `pull_request`, or a
cron box with no token. Signing steps are trigger-gated out on the fork path. On
the cron box the package **fails**: exit non-zero, no tag advanced, stderr naming
the absent identity source. It does not publish quietly unsigned.

**S3 — the mirror runs as an ocx plugin with a key.** `ocx mirror package pipeline push`
with `sign.key: env://OCX_SIGNING_KEY`. Plugin dispatch scrubbed
`OCX_SIGNING_KEY`, so ocx reports no key. The operator renames the variable to
`MIRROR_SIGNING_KEY`, sets `sign.key: env://MIRROR_SIGNING_KEY`, and it works —
because ocx does not know that name and cannot scrub it. Documented in
`environment.md` so the diagnosis takes one lookup, not one afternoon.

**S4 — copying a signed upstream to `registry:2`.** `registry sync` pushes the
subject, then the referrer manifests, then merges each into the destination's
`sha256-<hex>` fallback index. The report shows `referrers_copied: 3`. Under v1
this package failed with a counted error.

**S5 — copying to ECR.** The referrer PUT returns 405 on a `subject`-bearing
manifest. The package fails with `SubjectRejected`, counted, naming ECR and the
status. Every other package in the run continues; the exit code is the worst
classified failure.

**S6 — a destination-only signature.** The destination already holds
`sha256-abc….sig` at a different digest than the source's — someone signed the
mirror locally. The sidecar PUT is skipped, `sidecar_conflicts` increments, the
package succeeds, and the report names the tag. Neither signature is lost.

**S7 — backfilling a five-year-old mirror.** `ocx-mirror package pipeline sign --dry-run`
reports 210 tags, 178 already signed, 32 to sign. Without `--dry-run` it signs the
32 indexes and their 96 platform manifests. Re-running reports 210 already signed
and signs nothing, so `ocx package verify` still sees fewer than 8 candidates per
subject after any number of runs.

**S8 — a Rekor outage mid-backfill.** Some subjects exit 83. Those rows are
`failed` with `code: transparency_log_unavailable`, the rest succeed,
`summary.status` is `partial_failure`, and the process exits **83** — so a CI
script can tell "the transparency log is down, retry later" from "this signature
is wrong".

---

## Consequences

**Positive**

- Both signature shapes survive a copy, on a registry with the Referrers API and
  on one without. The Harbor silent-drop class is closed.
- Every manifest the mirror publishes is signed, on all four publish legs.
- The publish side and the consumer side are two files: a machine can consume
  public `ocx.sh` packages and publish to a corporate Sigstore at the same time.
- Exactly one new public symbol in `ocx_lib`, plus one new discovery function.
- The exit-code contract gains no new number: ocx's taxonomy is carried through.
- F1's real gap — no credential deny-list on `mirror.yml` — is closed as a
  by-product.

**Negative**

- The mirror writes a mutable fallback-index tag that anyone with push access can
  author, inheriting the residual ocx recorded in
  `adr_oci_referrers_signing_v1.md` Amendment 10.
- The backfill's keyless pre-filter reads unvalidated certificate fields. In-model
  this is sound; out of model it is spoofable, and the ADR says so rather than
  implying more.
- The mirror re-supplies credentials ocx's plugin dispatch deliberately stripped.
- Rendered `permissions:` blocks narrow every downstream non-GHCR mirror's token
  the first time `sign:` is set.

**Risks and mitigations**

| Risk | Mitigation |
|---|---|
| Fallback-index lost update against a concurrent upstream signer | ocx's bounded read-back retry; per-package serialisation; an acceptance test seeding a foreign entry and asserting it survives. `If-Match` recorded as an ocx follow-up. |
| zot GCs untagged referrers, making copy tests flaky | Ship `test/zot-config.json` with GC disabled; pin zot by digest. |
| D5 seam slips, blocking D3 | D1/D2/D4 do not depend on it and ship first. The mirror-local merge is written down as a contingency, not built speculatively. |
| C-053 breaks a downstream `mirror.yml` carrying a denied key name | Run the full fixture and golden corpus as an implementation gate; ship with a changelog entry naming the remedy. |
| `permissions:` narrowing breaks a downstream non-GHCR push job | Golden fixtures for all four combinations; changelog line; the block is only emitted when `sign:` is set, so an unsigned mirror is untouched. |
| A published-but-unsigned window after an interrupted run | The backfill is convergent and is in the same plan, not a follow-up. |

---

## Migration and rollout

**Ordering, and what ships if the satellite slips.**

| Wave | Work | Depends on |
|---|---|---|
| **W1 — lead, independent** | D1's spec block and C-053 deny-list, D6 renderer + docs + harness | nothing |
| **W2 — satellite** | OCX-C-1 (`native_transport`), OCX-C-2 (`list_signature_candidates`), OCX-C-5 (push endpoint flags), OCX-C-3 (acceptance) | nothing |
| **W3 — ocx release + submodule pointer bump** | — | W2 merged and released |
| **W4 — lead, seam-dependent** | D2 (push legs emit `--fulcio-url`/`--rekor-url`, sweep module, exit mapping), D4 (backfill, **presence-only** filter — see below), D3 (referrer walk, fallback merge, sidecar sweep), S-011 inversion, `registry-yml.md:468` rewrite | W3 |

**The W1 filter is presence-only, and that is a deliberate degradation.** Without
OCX-C-2 the mirror cannot read a certificate SAN without parsing Sigstore bundles
itself, which is crypto-adjacent code it must not own. So W1's `already_signed`
asks only *"does this subject carry any referrer or sidecar at all"*, answered
with `pull_referrers_native` plus the fallback tag plus the three sidecar tags —
all already reachable from the fork client the mirror holds. That is enough for
the default skip rule and for the accumulation problem it exists to solve.
`--identity` / `--issuer` narrowing needs OCX-C-2 and lands in W4. C-067's
signature does not change between the two: only the producer of
`Vec<SignerCandidate>` does.

**If only W1 lands:** `mirror.yml` accepts and validates `sign:`, the renderer
emits `id-token: write` and the harness can bring up Sigstore, but nothing signs
yet — the push leg needs OCX-C-5. That is an inert partial state: no unsigned
package is published differently than today, and `registry sync` still refuses
referrer-bearing packages, with no half-copied signatures.

**Existing mirrors, unsigned → signed.** Adding `sign:` changes nothing already
published. `package pipeline sign` is the one-shot repair; `--dry-run` first. No
tag moves and no digest changes: a signature is a *new referrer*, so pinned
consumers are unaffected.

**v1 refusal → v2 copy.** The removal of `CopyError::ReferrersPresent` is the
user-visible break. S-011 inverts from "a package carrying a referrer fails with a
counted error" to "referrers are copied and discoverable at the destination", read
back through the API on zot and through the `sha256-<hex>` index on `registry:2`.
`registry-yml.md:468` is rewritten in the same change; a stale "does not copy
signatures" line beside working carriage is worse than either state alone.

---

## Validation

- [ ] Rust unit tests: C-051, C-052, C-056, C-057, C-058, C-059, C-061, C-067,
      C-069 — all pure, no network.
- [ ] Acceptance (`test/tests/test_registry_sync.py`): S-011 inverted; a sidecar
      fixture copies byte-for-byte; a destination-only same-tag signature is
      skipped and counted; the subject lands before its referrers; a seeded
      fallback index with a foreign entry survives the merge.
- [ ] Acceptance (new `test/tests/test_signing.py`): a spec with `sign:` and no
      identity fails the package, exit code asserted separately from stderr; a
      keyless run against the self-hosted stack signs and verifies; the same run
      with `--key env://…` and a password passes; the backfill signs exactly the
      unsigned subject, `--force` signs both, and verify shows ≤ 2 candidates per
      subject after repeated runs.
- [ ] Golden corpus: `id-token: write` present for GHCR and non-GHCR with `sign:`,
      absent without it; signing steps trigger-gated; JSON schema regenerated for
      `sign:`.
- [ ] ocx satellite: OCX-C-3 scenarios green on the `sigstore` profile.
- [ ] `/e2e-test` tier 2: `e2e-ocxmirror-signing` renders, signs keyless in GitHub
      Actions, pushes to `dev.ocx.sh`; `ocx package verify` passes with the
      workflow identity in the policy.
- [ ] Security review reads `.claude/rules/security-threat-model.md` first and
      names an attacker for every finding.
- [ ] `task verify` green on the final state.

---

## Open questions

Three, each with a recommendation. Everything else in the discussion's
Open-questions list is decided above.

1. **[NEEDS CLARIFICATION: does the mirror re-supplying `OCX_IDENTITY_TOKEN` /
   `OCX_KEY_PASSWORD` from resolved refs (D1) overstep a deliberate ocx security
   decision?]** ocx's plugin dispatch strips these on purpose; D1 routes around it
   by resolving the operator's `env://`/`file://` ref itself and exporting the
   value onto the conventional name in the child. Under the mirror's threat model
   the execution environment is trusted, so this is the operator's own
   configuration and in scope — but it is ocx's decision being worked around, and
   the owner may want it raised upstream instead.
   **Recommended:** proceed as designed, and open an ocx issue asking whether a
   plugin may opt into a named credential passthrough, so the mirror's workaround
   can later become a supported path rather than a permanent bypass.

2. **[NEEDS CLARIFICATION: `REFERRER_DEPTH_CEILING = 2` or depth 1?]** The
   ratified decision says "the whole referrer graph, depth-bounded"; no mainstream
   tool walks past depth 1 and none documents a bound. Depth 2 admits an
   attestation attached to a signature; depth 1 is what oras and cosign do in
   practice and is trivially cheaper to reason about.
   **Recommended:** 2, with the visited-digest set and the per-subject count cap
   doing the real bounding work. Narrowing to 1 later is a one-constant change;
   widening after an incident is not.

3. **[NEEDS CLARIFICATION: does the backfill get a rendered workflow of its own?]**
   `pipeline cascade` gets `cascade.yml` when `cascade.schedule` is set, and the
   backfill is the same shape of repair. A rendered `sign.yml` would need a
   `sign.schedule` field, which D1 does not have — so adding it reopens the
   one-way door on the spec block.
   **Recommended:** no rendered workflow in v1. The backfill is dispatch-only, run
   by hand or from an operator's own job; the four-line non-GitHub snippet in
   `cli.md` covers it. Add `sign.schedule` in a later minor version if a repeated
   need appears, which is additive and safe.

---

## Links

- [`adr_registry_mirror_sync.md`](./adr_registry_mirror_sync.md) — v1 referrer
  refusal (Open question 3), referrer bounds (~586-600), the blob-copy seam
  amendment
- [`research_ocx_060_semantics.md`](./research_ocx_060_semantics.md) §9e — the
  signing env vars, owner call, answered here by D1 and F2
- `.agents/discussions/mirror-signing.md` — the ratified discussion
- `.claude/state/plans/notes_mirror_signing_discover.md` — hook points with
  `file:line`
- `external/ocx/.claude/artifacts/adr_oci_referrers_signing_v1.md` — ocx's signing
  design, Amendment 10 (fallback index) and the D4 optimistic read-back
- `external/ocx/.claude/artifacts/adr_package_copy.md` — `ocx package copy`:
  referrers default-on, exit 84 without the API, never the fallback index
- [ocx#391](https://github.com/ocx-sh/ocx/issues/391) ·
  [ocx#392](https://github.com/ocx-sh/ocx/issues/392) — open upstream copy defects
- [OCI distribution-spec](https://github.com/opencontainers/distribution-spec/blob/main/spec.md)
  — referrers API, tag-schema fallback, the non-atomic merge warning
- [containers-roadmap#2783](https://github.com/aws/containers-roadmap/issues/2783)
  — ECR 405 on `subject`
- [harbor#23210](https://github.com/goharbor/harbor/issues/23210) — the
  replication silent-drop this design exists to avoid

---

## Changelog

| Date | Author | Change |
|------|--------|--------|
| 2026-09-02 | architect | Initial draft — D1…D6, 23 lead contracts (C-050…C-072), 4 satellite contracts, three code findings (F1 deny-list gap, F2 plugin-dispatch credential scrub, F3 sweep cannot cover platform manifests) |
| 2026-09-02 | owner ruling | D1 Option 3 → Option 4 (mode tag, `Ref` grammar, publish-side endpoints, `format` dropped); OCX-C-5 added; WP 2 → wave 3; ocx floor bump |
| 2026-09-02 | hex-plan review | C-063 split into probe-405 (fallback) vs PUT-405 (`SubjectRejected`, no fallback write). D5a "does not contradict a written ocx decision" rescored 4→2 for Options 1 and 2: `native_transport` returns `Box<dyn OciTransport>`, which `oci/client.rs:223`'s doc says the public API never exposes — Option 1 still wins (98 vs 86 vs 68), the argument is now honest. Deferred: D3's harness criterion is circular with D6's zot (rescored Opt 1 ≈ 101 < 109, decision holds); D1 never considers a bare `--sign` push flag with no spec block. |
