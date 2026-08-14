# ADR: `ocx-mirror registry sync` — whole-registry mirroring into a corporate registry

## Metadata

**Status:** Proposed
**Date:** 2026-08-13
**Deciders:** Michael Herwig (maintainer), architect
**GitHub Issue:** N/A — closes the `TODO(registry ADR)` at `src/command/registry/mod.rs:18-19` and fills
item 5 of [`adr_cli_namespace_restructure.md`](./adr_cli_namespace_restructure.md)'s implementation plan
(`:170`)
**Related Design Spec:** N/A (to follow)
**Stack Alignment:**
- [x] Decision fits existing stack (Rust 2024 + Tokio, clap) and conventions in
  [`.claude/rules/subsystem-mirror.md`](../rules/subsystem-mirror.md)
**Domain Tags:** cli, spec, oci, index, security
**Supersedes:** the **content-copy half** of `external/ocx/.claude/artifacts/adr_oci_registry_mirror.md`
(see *Reconciliation* below)
**Superseded By:** N/A
**Depends on:** ocx ≥ **0.5.8** (`external/ocx` submodule bump — hard prerequisite, see *Migration*)

**Citation convention.** A bare `crates/…` path is **ocx@v0.5.8** (`/home/mherwig/dev/ocx`, current
`main`), which is what this design targets. `external/ocx/…` is this repo's **pinned v0.5.6**
submodule, which does *not* contain `regenerate.rs` or `file_transport.rs`. Unprefixed `src/…` is
ocx-mirror. Every `path:line` below was read before it was written; where an earlier revision or a
review cited a line that does not hold, the corrected one is used and the correction is stated.

---

## Context

### The deployment this exists for

A corporate network blocks egress to `ocx.sh`, `ghcr.io`, `docker.io`. An operator runs an internal
OCI registry (JFrog Artifactory in the motivating case). They want their developers to
`ocx install kubernetes/kubectl:1.31` and have every byte — index documents *and* OCI content — come
from inside the perimeter. Nothing may reach out.

### What already ships, and why it is not enough

Two thirds of this problem are already solved, in ocx, and the honest baseline is to say so:

| Shipped | What it does | Why it does not close the air-gap |
|---|---|---|
| `[mirrors]` config (`adr_oci_registry_mirror.md`) | Rewrites the OCI read path per upstream host onto a corporate proxy | Requires the corporate registry to be a **pull-through proxy with egress**. An air-gapped registry has none — there is nothing to proxy through |
| `ocx index sync <REGISTRY>...` (v0.5.8) | Snapshots a source's whole catalog into a locally-servable index tree. `adr_servable_index_snapshot.md:535`: *"An air-gapped mirror is one command, then a static file server. Zero `ocx-mirror` code."* | Copies **index documents only**. Every `repository` still points at `ghcr.io`. A consumer resolves the tag correctly and then fails to fetch a single blob |

**The remaining third is the whole of this ADR**: copy the OCI content by digest into the corporate
registry, and rewrite each root document's `repository` so the index the consumer reads points at
where the bytes actually are.

### Neither shipped verb is a shortcut past the copy engine — stated, because a reader will assume otherwise

`ocx index sync` is **index-pointer-only**. It enumerates a registry's catalog and feeds every package
through the same merge-only write path as `ocx index update <pkg>`, refreshing roots and dispatch
objects into `$OCX_HOME/index/<source>/`. Its purpose is a *truthful local cache of what the registry
already says* — so by construction it never copies a blob and never rewrites `repository`.
`adr_servable_index_snapshot.md:282-286`: *"a merge-never-deletes operation is not a mirror."*

`regenerate_catalog` does not help either. Its stated invariant (`regenerate.rs:19-22`) is that it
re-derives `c/index.json` from **unchanged root bytes already on disk**; `c/index.json` is the only
path whose bytes it writes, it never writes `config.json`, and it removes no root and no `o/` object.

**Consequence for the design:** the rewritten root is bytes *this* tool constructs, written through
this tool's own call to the index store. Nothing upstream produces them.

### Why this is not "wrap `skopeo sync`"

Every comparable tool that can mirror "a whole registry" enumerates through the Docker Distribution
`_catalog` API, and that endpoint fails in the field — Quay rejects the wildcard auth scope
([xelalexv/dregsy#57](https://github.com/xelalexv/dregsy/issues/57)), Docker Hub does not expose it,
skopeo never implemented registry-wide sync at all
([containers/skopeo#364](https://github.com/containers/skopeo/issues/364), open six years).
OCX enumerates from `c/index.json`, a static file: no auth-scope negotiation, no rate limit on
discovery, and the list contains exactly the packages someone deliberately published.
[`research_registry_mirror_tooling.md`](./research_registry_mirror_tooling.md) §1.

### Reconciliation: the superseded ADR

`external/ocx/.claude/artifacts/adr_oci_registry_mirror.md` still reads **Status: Accepted** with
**Superseded By: N/A**. The owner has verbally deprecated it; the file does not say so, and
`adr_servable_index_snapshot.md:691` already prescribes setting its `Superseded By` for the
index-tree half. Recording the full picture here, since the reconciliation now spans three ADRs:

| Half of `adr_oci_registry_mirror.md` | Disposition |
|---|---|
| Index-tree half | Superseded by `adr_servable_index_snapshot.md` (already prescribed at `:691`; the vendored copy does not yet carry the amendment) |
| Client-declared `[mirrors]` transport rewrite | **Shipped and live.** Not superseded — it remains the right answer for a proxying corporate registry |
| Content-copy half (the mirror as a *replacement* rather than a proxy) | **Superseded by this ADR** |

**R1–R6 survive as constraints on this design**, not as history:

| # | Original requirement | How this ADR honours it |
|---|---|---|
| R1 | Destination is host + repository-path prefix (Artifactory repo-key method) | `target.repository` is a **prefix**; `destination:` template expands beneath it |
| R2 | Replace semantics — never contact the upstream origin | The consumer's index names only the corporate registry. There is no fallback to fall back to |
| R3 | Per-source mapping | One `sources[]` entry per upstream index, each with its own `as:` and filters |
| R4 | `ocx.lock` portable across mirrored and direct-egress hosts | **Verified.** The lock records logical identity only; the rewrite lives exclusively in index documents ([`research_mirror_lock_portability.md`](./research_mirror_lock_portability.md), verdict) |
| R5 | Mirror needs its own credentials | `OCX_AUTH_<dest-slug>_*`, resolved by ocx. Zero credentials in the spec |
| R6 | Tamper resistance | Digest-pinned transfer, and **nothing more** — see *Trust scope*, where the residual is named rather than claimed away |

---

## Decision Drivers

- **Air-gap correctness is binary.** A mirror that copies 99% of what a consumer needs is a mirror
  that does not work.
- **The published index tree is an external contract.** Once an operator points
  `[registries."ocx.sh"] index = "https://pages.corp/ocx.sh"` at it, its layout is load-bearing for
  every machine in the fleet. One-way door.
- **Reuse the shipped index machinery, do not re-implement it.** `serialize_root`, `IndexStore`,
  `regenerate_catalog` exist and are byte-parity-tested. A second implementation of a frozen wire
  format is Block-tier under `quality-core.md`.
- **Do not duplicate the OCI transport layer.** This repo has deliberately never owned one.
- **Blast radius honesty.** The mirror becomes the fleet's trust root. Say so in writing rather than
  implying that digest-pinning covers it. (Whoever can write to that tree is out of scope per
  [`security-threat-model.md`](../rules/security-threat-model.md); saying it plainly is still the
  driver.)
- **A release enters the index only after the mirror succeeded** *(owner requirement)*. Visibility is
  the last write, and every partial state a crash can leave must be repairable by the next ordinary
  run — not by an operator with a runbook.
- **Everything the upstream publishes travels, including tags nothing can parse** *(owner
  requirement)*. A version cascade must survive the copy; a `nightly` or a git SHA must not be an
  error. The design that satisfies both is the one that classifies nothing.
- **Foreign bytes are validated before they reach a path, a URL or a credential.** The upstream
  registry and the network are in-scope attackers; the machine running this is not.

---

## Industry Context & Research

**Research artifacts** (all 2026-08-13):
[`research_registry_mirror_tooling.md`](./research_registry_mirror_tooling.md) ·
[`research_mirror_supply_chain.md`](./research_mirror_supply_chain.md) ·
[`research_mirror_lock_portability.md`](./research_mirror_lock_portability.md) ·
[`research_mirror_operability.md`](./research_mirror_operability.md)

**Key insights driving the decision:**

1. **Glob for names, regex for tags** is where the ecosystem converged (Harbor, Artifactory, zot).
   Client tools that chose regex for names predate the convention. This design has no tag filter at
   all in v1, so glob-only means **no regex anywhere in the schema**.
2. **Exclude-is-a-veto must be stated, not inherited.** Artifactory publishes it cleanly; regsync's
   published documentation contradicts itself on exactly this point (tooling research §2).
3. **Destination collision is the universal footgun.** skopeo flattens to basename and silently
   overwrites; Harbor's flatten-N-levels is the same defect from the other side; **collision policy is
   undocumented by all seven surveyed products** (§3). Mandating `{registry}` makes it structurally
   unreachable — a validation rule no competitor has.
4. **Append-only is the industry default**, not a limitation (§4): Harbor never propagates deletion
   for scheduled rules, zot has not implemented it, Quay is archival by design.
5. **Referrers do not travel by default and the failure is silent** — Harbor
   [#23210](https://github.com/goharbor/harbor/issues/23210) is exactly this bug (supply-chain research §3b).
6. **A rewriting mirror is the 2008 Cappos attack surface.** Digest-pinning secures a transfer, not a
   claim (supply-chain research, *The crux*).

---

## Considered Options — the overall shape

### Option A: Index-only mirror (`ocx index sync` + static file server)

Ship nothing. Tell operators to run the shipped ocx verb and serve the tree.

| Pros | Cons |
|------|------|
| Zero new code, zero new spec, zero new trust root | **Does not close the air gap** — every `repository` still names `ghcr.io` |
| Already parity-tested and released | Useless for the motivating deployment |

### Option B: Transparent client-side `[mirrors]` redirect (the superseded ADR)

Consumers set `[mirrors]` so OCI reads route to the corporate registry's repositories.

**Correction to the previous revision.** This ADR asserted that `[mirrors]` "requires a proxying
registry with egress". That is **false**, and the review was right to call it. `[mirrors]` is a
purely **client-side base-URL rewrite**: `MirrorMap::rewrite_repository`
(`crates/ocx_lib/src/oci/client/mirror_map.rs:67-75`) maps `(registry, repository)` to
`(mirror.host, "<path_prefix>/<repository>")` and the client dials that instead — nothing proxies,
nothing needs egress. It also covers **two roles**, `index` and `registry`, either separately or
together (`website/src/docs/in-depth/indices.md:429-437`), so it can redirect index traffic as well
as OCI traffic. Its real limitation is much narrower than claimed, and is stated below.

| Pros | Cons |
|------|------|
| Shipped; client-side only; no proxy and no egress required | **It is the addressing half only.** Something still has to put the bytes inside the perimeter — `[mirrors]` never copies anything |
| Consumers keep the public index verbatim | Per-host configuration on every consumer machine, distributed and kept correct by the operator |
| No new trust root; no mirror-side spec | On its own it closes nothing: a cold air-gapped host rewrites its request onto an empty corporate registry |

### Option C: Full copy with `repository` rewrite (**chosen**)

Copy OCI content by digest into the corporate registry; rewrite each root's `repository`; write the
resulting index tree to a directory the operator serves.

| Pros | Cons |
|------|------|
| The only option that actually works air-gapped | The published tree becomes the fleet's trust root (residual, named below) |
| Enumerates from a static catalog, not `_catalog` | Bytes are real: ~210 GB for the full 121-package public catalog |
| Real atomic-visibility guarantee (root written last) | Needs a blob-copy seam this repo does not have today |
| `ocx.lock` stays byte-identically portable (R4) | New spec schema = new external contract |

### Option D: Shell out to `oras cp --recursive` per package, own only the index tree

Let ORAS do the content copy (it walks the referrers graph natively); ocx-mirror writes the index tree.

| Pros | Cons |
|------|------|
| Referrers-aware copy for free — the one requirement that is genuinely hard | **Credential model splits.** Settled: auth is delegated entirely to ocx (`OCX_AUTH_<slug>_*` → docker store → anonymous). ORAS reads only the docker credential store, so an `OCX_AUTH_*`-configured destination silently fails |
| No transport code in this repo | New binary dependency in `ocx.toml` for every mirror repo and every CI runner |
| Battle-tested copy semantics | oras-go carries the same credential-forwarding CVE class ([GHSA-jxpm-75mh-9fp7](https://github.com/oras-project/oras-go/security/advisories/GHSA-jxpm-75mh-9fp7)) — not a security win |
| | Error classification becomes exit-code archaeology over a foreign tool's stderr |

### Option E: Copy content, publish the source index **verbatim**, consumers redirect via `[mirrors]`

Added in this revision because all three reviewers proposed it independently and the previous
revision did not consider it at all. Copy the OCI content exactly as C does, but write the index tree
with **unmodified** root documents. Consumers point `index=` at the mirrored tree *and* set
`[mirrors]` so the untouched `ghcr.io` pointers are rewritten client-side onto the corporate registry.

| Pros | Cons |
|------|------|
| **Roots stay byte-identical to the source**, so `sha256(root)` still matches the source catalog — the O(1) catalog short-circuit survives unchanged and Defect 2 does not exist | Every consumer needs `[mirrors]` config distributed and kept correct; a machine that misses it reaches for `ghcr.io` and fails closed |
| No destination-template grammar, no collision policy, no path-escape surface — the whole `{registry}/{namespace}/{package}` design disappears, and with it the trust boundary it creates | The destination repository layout is then dictated by `path_prefix + upstream repository`, not chosen — an Artifactory repo-key shape that cannot accommodate it is stuck |
| The mirror mints no wire-contract pointer, so it never has to satisfy `parse_physical_repository` | Two coupled configurations (index home + mirror map) instead of one, and they can drift independently |
| Verbatim roots keep a future signed-root retrofit possible; C's rewrite forecloses it | Depends on ocx-side `[mirrors]` behaving exactly right on the path the mirror serves — one more shipped behaviour to be correct about |

**The panel unanimously recommended Option E. The owner chose Option C.** Recorded honestly: E's
advantages above are real, it scores highest below, and the short-circuit and path-escape defects this
ADR spends pages on are E's for free. The owner's reason is that the mirrored index must be
self-contained — one configuration on the consumer, not two that can drift. The ruling stands and is
not re-litigated; the rest of this document designs C.

### Evaluation — binary gate first, then score the survivors

The previous revision used one compensatory weighted sum across all four options. That was wrong on
two counts: the arithmetic did not add up, and it let *"closes the air gap"* — which the Decision
Drivers already state is **binary** — be outvoted by soft criteria, which is how A (a do-nothing
option) came out ahead of D. Replaced with the gate the drivers actually state.

**Gate (pass/fail, not scored): does the option put the bytes inside the perimeter?**

| Option | Gate | Why |
|---|---|---|
| A — index-only | **FAIL** | Copies index documents only; every `repository` still names `ghcr.io` |
| B — `[mirrors]` alone | **FAIL** | Addressing half only; copies nothing, so the corporate registry stays empty |
| C — copy + `repository` rewrite | pass | |
| D — `oras cp --recursive` + own index tree | pass | |
| E — copy + verbatim index + `[mirrors]` | pass | |

A and B are eliminated here and are not scored. They are not weak options; they are answers to a
different question, and both remain correct for the deployments they fit (recorded in *Migration*).

**Survivors, weighted:**

| Criterion | W | C | D | E |
|---|---|---|---|---|
| One auth path; no credential-model split | 4 | 5 | 1 | 5 |
| Reuses shipped index machinery unchanged | 3 | 3 | 3 | 5 |
| Bounded new maintenance surface | 3 | 3 | 3 | 4 |
| No new trust root introduced | 3 | 1 | 1 | 1 |
| Operator steps to a working fleet | 2 | 4 | 3 | 2 |
| Reversibility (pre-1.0 verb, additive spec) | 2 | 3 | 3 | 4 |
| **Total** | | **55** | **37** | **62** |

Arithmetic, so it can be checked: C = 20+9+9+3+8+6; D = 4+9+9+3+6+6; E = 20+15+12+3+4+8.
*No new trust root* scores 1 for every survivor — each of them serves the fleet's index — so it
discriminates nothing here. It is kept rather than dropped because dropping the criterion that scores
the chosen option worst would be exactly the kind of quiet gaming this rewrite exists to remove.

**E ranks highest, and the owner chose C.** Recorded plainly: the three-reviewer panel recommended E
unanimously, E's advantages in the table above are real, and the catalog short-circuit defect
(Defect 2) and the destination path-escape surface are both artefacts of C's rewrite that E would not
have. The owner's ruling is that the mirrored index must be self-contained — one configuration on the
consumer, not two that can drift — and that ruling is binding. D loses regardless: it splits the
credential model, which the settled auth doctrine exists to prevent.

**Reversibility note.** Option C's 3/5 is honest. The verb and spec schema are pre-1.0 and additive;
the **published tree layout** is not — once a fleet's `[registries]` names `<output>/<as>`, moving it
is a coordinated fleet change. That is why `output:` is a parent home with one subtree per source
(the tree's address *is* the registry's identity) rather than a template with segments inside it.

---

## Decision Outcome

**Chosen Option: C — full copy with `repository` rewrite, published as a static index tree.**

### Settled with the owner (recorded, not re-litigated)

1. **Separate `RegistrySpec` in its own `registry.yml`.** Not folded into `MirrorSpec`. They share
   `target` only (see decision 9 for why `concurrency` is *not* shared). A `kind:` discriminator on
   both documents gives a clear error for a misplaced file — `MirrorSpec` carries
   `#[serde(deny_unknown_fields)]` (`src/spec.rs:66-68`), so without a discriminator a misplaced
   `registry.yml` reports `unknown field 'sources'` and buries the real problem.
   **The `kind` read is part of the raw-`Value` pre-scan, not a schema field check** — see Defect 1;
   a `kind` field on a `deny_unknown_fields` struct is checked *after* deserialization has already
   failed on `sources:`, so reading it there would be unreachable in exactly the case it exists for.
2. **One verb:** `ocx-mirror registry sync [SPEC]`, optional positional defaulting to `./registry.yml`.
   Asymmetric with `package sync <SPEC>` on purpose — a package mirror repo holds many specs, a
   corporate registry mirror repo holds exactly one.
3. **`output:` is a parent index home**, one subtree per source named by `as:`. Not a template.
4. **Destination template `{registry}/{namespace}/{package}`**, plain string substitution, no template
   engine. Derived from the **package name** (the catalog key), never from the physical `repository`.
   `{registry}` mandatory when more than one source is configured. **The expansion is refused, never
   normalised** — no lowercasing, no slugification, no path cleaning; see *Destination expansion is a
   trust boundary*.
5. **`as:` per source**, defaulting to `registry:` verbatim. **Hard error — never silent
   slugification — when the value is not a legal OCI path component.** It doubles as the output
   subdirectory and the `{registry}` expansion.
6. **Glob include/exclude over the two-segment package name** (`*` only; no `**`, no `?`, no `{a,b}`
   alternation). Exclude is a veto: a package passes iff it matches some include **and** no exclude.
   Empty include means everything. No regex anywhere in the schema.
   *`{a,b}` was cut in this revision:* `include:` is already a list, so `["{hashicorp,ocx}/*"]` and
   `["hashicorp/*", "ocx/*"]` are the same filter with two grammars. One grammar, and the one that
   needs no brace parser.
7. **No version filter in v1.** A verbatim root copy carries the rolling-alias cascade for free; a
   version subset makes `latest` resolve to a digest nothing copied. Version windows are v2 and need
   `ocx package cascade repair`.
8. **The root document is written only after every byte it names is confirmed at the destination.**
   Restated as an **owner requirement**, not an implementation note: *a release enters the mirrored
   index only after the mirror succeeded.* The index write is the atomic visibility point — see
   *Failover, repair, and the visibility guarantee*.
9. **Blob dedup:** unconditional HEAD against the destination repository, plus an in-run
   `digest → repository already written` map used for opportunistic cross-repository mounts. A mount
   is an optimistic attempt — any non-success outcome (`MountOutcome::UploadRequired`,
   `transport.rs:196-204`) falls through to an ordinary upload.
   **`target.blob_anchor` is cut from v1** — see *Cut from v1: `blob_anchor`*. The in-run map stays.
   **Concurrency is a `RegistryConcurrency` of its own**, not `ConcurrencyConfig` shared verbatim.
10. **Auth is delegated entirely to ocx** (`OCX_AUTH_<slug>_{TYPE,USER,TOKEN}` → docker credential
    store → anonymous). **Zero credentials in the spec**; credential-shaped fields refused at load
    with exit 64 by a raw-`Value` pre-scan (Defect 1), whose message **names the key path and the
    environment variable, never the value** — the opposite of what `policy_check_notify` does today.
11. **No GitLab/CI renderer, no merge-request creation.** The mirror writes files and stops;
    `git add/commit/push` is the operator's two lines.
12. **The corporate mirror never announces.** It owns its index and writes the tree directly.
13. **Failure policy is configurable, defaulting to continue-on-error.** `on_error: continue |
    fail_fast` on the spec, `--fail-fast` overriding on the CLI. It governs **per-package** failures
    only; a non-authoritative destination answer aborts the whole run regardless (see *Error model*).
14. **Every tag in a source root's `tags{}` is copied, whatever its text.** The mirror computes no
    cascade — the source root already carries the cascade result. An unparseable tag is a
    copy-it-and-move-on case, never a failure. See *Cascade correctness and non-version tags*.

### Sub-decision: the destination reads `target.repository` as a *prefix*

`Target` (`src/spec/target.rs:6-10`) is shared verbatim, but `repository` means something different
here: in `MirrorSpec` it is the full destination repository, in `RegistrySpec` it is the Artifactory
repo-key prefix that `destination:` expands beneath. `Target::reference()` (`:20-23`) still composes
correctly because it only joins with `/`. Recorded because a shared type read two ways is exactly the
kind of thing a later reader re-derives wrongly.

### Cut from v1: `target.blob_anchor`

The previous revision put an optional `blob_anchor` on `Target` — "one named source repository for
cross-repo mounts, the ECR shape". **Cut.** Two independent reasons, either sufficient:

- **It cannot work as designed.** Nothing anywhere in this design ever *writes* a blob to the anchor
  repository. A mount request names a source repository the registry must already hold the blob in;
  against an empty anchor every mount 404s and falls through to an upload, forever. It is
  configuration that changes no behaviour.
- **It would land on a shared type with no `deny_unknown_fields`.** `Target`
  (`src/spec/target.rs:6-10`) carries only `registry` and `repository` and has **no**
  `#[serde(deny_unknown_fields)]` — unlike `MirrorSpec` (`src/spec.rs:66-68`). Adding a field there
  makes `blob_anchor:` silently accepted and silently ignored in every existing `mirror.yml`.

The in-run `digest → repository already written` map stays: it is free, it is scoped to one process,
and it makes the second package that shares a blob mount instead of re-upload.

**v2 trigger, so this is a deferral and not a deletion:** if a real run shows the same digest
uploaded across ≥3 destination repositories often enough to matter, the answer is a *seeded* anchor —
push each blob to the anchor repository first, then mount every real repository from it — and that
design must also say who garbage-collects the anchor. Whatever ships then goes on `RegistrySpec`, not
on the shared `Target`.

### Destination expansion is a trust boundary

`{namespace}` and `{package}` come from a **foreign catalog key** — a string this tool read off an
index someone else authored. Under `security-threat-model.md` the attacker is the upstream registry,
which is in scope. A key of `foo/../../prod-images` normalises client-side, so the registry never sees
a `..` and never gets the chance to refuse it: the mirror would write to a repository outside the
prefix the operator configured, with the operator's credentials.

**The rule, and it is one function call.** Compose the full physical repository path, then run it
through the grammar ocx already applies to exactly this case:

```
physical_repository = "{target.repository}/{expanded destination}"
Identifier::validate_repository(&physical_repository)?          // crates/…/oci/identifier.rs:102
assert physical_repository.starts_with(&format!("{}/", target.repository))
```

`Identifier::validate_repository` exists for this and says so in its own doc comment — *"for callers
that take a repository from a foreign source and use it verbatim — a catalog key read off an index
someone else authored"* (`identifier.rs:86-88`). Its rules, read from `repository_error_kind`
(`:491-521`):

| Guard | Line | Effect here |
|---|---|---|
| any ASCII uppercase ⇒ `UppercaseRepository` | `:492-494` | **Refuse, do not lowercase.** `str::to_lowercase` folds U+212A KELVIN SIGN onto `k`, so lowercasing manufactures collisions between two distinct upstream keys. The `Identifier` grammar already takes the refuse line; this design takes the same one |
| length cap | `:495-497` | Bounds the expanded path |
| `.` / `..` segment ⇒ `DirectoryTraversal` | `:498-503` | The escape above, refused outright rather than normalised |
| every byte in `[a-z0-9._-]`, no empty segment | `:515-519`, `:524-525` | Non-ASCII, whitespace, control characters, `:` and `@` smuggling all refused |

Because uppercase is *refused* rather than folded, the "two packages expanding to the same lowercased
destination" collision rule from the previous revision reduces to a plain string-equality check over
the expanded set — no case normalisation anywhere.

**What gets written into the root is `"oci://" + physical`, and it must round-trip.**
`IndexRoot.repository` is a strict wire contract: `parse_physical_repository`
(`crates/…/oci/index/ocx_index.rs:301-327`) hard-fails a missing scheme
(`Error::MalformedPhysicalRef`), and beyond the scheme it re-parses
`host/path` through `Identifier::parse_with_default_registry` and demands an **exact** round-trip —
`registry() == host`, `repository() == path`, no tag, no digest (`:322-325`). So the mirror writes

```
"oci://" + target.registry + "/" + target.repository + "/" + <expanded destination>
```

and validates the composed string by calling `parse_physical_repository` on it **before** writing,
getting the same verdict every consumer will reach. A scheme-less value — which is what the previous
revision's template expansion produced — ships a tree that fails 65 on every package.

`target.registry` and `target.repository` are themselves validated at load against the same grammar
(`Identifier::validate_repository` for the repository; the registry through
`parse_with_default_registry`), so a typo in the destination is a load-time 65 rather than a run-time
push failure.

### Cascade correctness and non-version tags

**The mirror computes no cascade. It copies one.** A source root's `tags{}` map already contains
`1.31.2`, `1.31`, `1`, `latest` — each an entry the source's own publish computed — and each value is
a `content` digest. Copying every entry by digest reproduces the cascade exactly, because `1.31` and
`1.31.2` at the destination end up pointing at the same manifest bytes they point at upstream. This
is decision 7's reasoning stated as a mechanism.

Three tag classes, and only one of them involves parsing a version:

| Class | Which tags | What happens |
|---|---|---|
| **Copied by digest, verbatim** | *Every* key in `root.tags{}` — `1.31.2`, `1.31`, `1`, `latest`, `nightly`, `edge`, `stable`, `2026-08-14`, `a1b2c3d`, anything | Pull the manifest named by `content` **by digest**, push it at the destination under the same tag name. No parse, no classification, no error path |
| **Ordering only** | Whichever of those parse | `pep440_sort_key` (`src/filter.rs:225-232`) orders the copy within a package so the log reads chronologically and an interrupt is deterministic. It is a *sort key*, never a gate: its `None` arm sorts **first** (`filter.rs:187-193`), so an unparseable tag is copied first and can never be mistaken for the newest |
| **Administrative** | `__ocx.desc` | Never appears in `root.tags{}` — ocx filters reserved tags once at render (`CHANGELOG.md:173`) — so it is fetched by tag name explicitly, per package |

**No tag is ever rejected for being unparseable.** This is the owner requirement, and it is also what
falls out of the design: nothing in the copy path asks a tag what version it is.

**Why the mirror must not re-derive the cascade, stated so it is not "improved" later.** ocx's
cascade computation is `decompose` / `resolve_cascade_tags`
(`crates/ocx_lib/src/package/cascade.rs:108`, `:207`). `resolve_cascade_tags` is **platform-aware and
blocker-aware against the target registry's own tag list** (`:214-219`) — it decides which rolling
tags a version may take given what else is published *there*. Run against the mirror's filtered
subset it would produce a different cascade than the source published: a mirror holding `1.31.2` but
not `1.32.0` would compute `latest → 1.31.2`, contradicting the source's `latest → 1.32.0` and
silently downgrading every consumer that resolves `latest`. Copying the source's `tags{}` verbatim is
not a shortcut past cascade logic; it is the only answer that stays faithful.

This repo's own version handling agrees on the fail-open convention: `version_cmp`
(`src/filter.rs:157-164`) returns `None` when neither parser relates two strings, and `within_bounds`
(`:173-185`) then leaves the version **unbounded** rather than dropping it — *"an unrecognisable
upstream tag is surfaced as work rather than silently skipped"*. Same instinct, applied here as
"copy it".

**Ordering constraint the mirror does owe.** Every tag of a package lands at the destination *before*
that package's root document is written (decision 8), so a partially-copied `tags{}` map is never
visible. Order *among* tags is therefore free — which is why `pep440_sort_key` can be used for
legibility without carrying any correctness weight.

**Failure of one tag drops that tag, not the package.** If a tag's `content` digest cannot be pulled
— upstream deleted the manifest but left the index entry — that tag is simply absent from the
`confirmed` set and does not enter the root. The package is counted failed and the summary names the
tag, but the root **is** written, carrying the tags that did copy.

> **Amended after implementation.** This paragraph originally read "fails the package, not the tag …
> no root is written", on the reasoning that a root with fewer tags than the source is a silent
> cascade break. That reasoning does not survive contact with the merge: the written root is the
> **union** of what the destination already published and what this run confirmed (C-047), so a
> failed tag falls back to the digest the last good run published rather than vanishing. Nothing is
> ever deleted, so there is no cascade break to prevent. The original rule would have let one broken
> upstream pointer hold every other tag of that package hostage indefinitely, and it is contradicted
> by the design's own `confirmed` set — a set that is *filtered* only makes sense if partial
> publication is the intent. Two guards keep the weakened rule honest: nothing confirmed **and** no
> destination root ⇒ no root is written at all (an empty `tags{}` root would advertise content the
> mirror does not hold and satisfy every `should_skip` condition forever), and the run still exits
> non-zero.

### Failover, repair, and the visibility guarantee

**Guarantee (owner requirement).** *A release enters the mirrored index only after the mirror
succeeded.* Concretely, and **per tag rather than per package**: a tag appears in the written root
only once every manifest and blob its `content` digest names is confirmed present at the destination
— that is exactly what the `confirmed` set is, and a tag joins it only inside the success arm of the
copy. `c/index.json` is then published only by `CatalogTransaction::commit` at the end of the
source's pass. The published tree therefore never names content that is not there.

Stating the guarantee per tag is not a weakening of the owner's requirement — it is the requirement
applied at the granularity a release actually has. A package is not a release; its tags are. Holding
back forty-three confirmed releases because a forty-fourth is broken upstream would publish *less*
verified content, not more.

**The converse is deliberately not guaranteed**, and that is the whole subject of this section:
content *can* be at the destination with no root naming it. That is the interrupted state, it is
harmless to consumers, and the next run repairs it.

**Repair is the skip predicate, not a verb.** A package is skipped iff **all** of:

1. its root document exists under `p/<ns>/<pkg>.json`; **and**
2. the local root's `tags{}` key set ⊇ the source root's `tags{}` key set; **and**
3. its repository is a key in the local `c/index.json` **and** that entry's digest equals
   `sha256(local root bytes)`.

Anything else re-copies. Conditions 2 and 3 are the fixes for two live defects: without 2 a tag added
upstream to an existing package is never noticed, and without 3 an interrupted run leaves completed
roots in `p/` that the next run skips — so `commit` publishes a catalog missing them, permanently.
Condition 3 also costs nothing: `begin_catalog_transaction` has already read the catalog map, and
`IndexStore::root_catalog_entry` is the same derivation `write_root` performs (`index_store.rs:1121-1122`).

| Damage state | Detected by | Repair | Reuses |
|---|---|---|---|
| Partially-copied package — blobs pushed, no root | predicate 1 | Re-copy the package. Every already-pushed blob HEAD-skips, so the repair costs requests, not bytes | destination HEAD skip |
| Uncatalogued root — root on disk, no catalog entry | predicate 3 | `write_root` the bytes already on disk inside this run's transaction; the entry is re-derived from them | `CatalogTransaction::write_root` (`index_store.rs:1103-1124`) |
| Drifted catalog — entry digest ≠ `sha256(root)` | predicate 3 | identical to the above | same |
| Tag added upstream to a package already mirrored | predicate 2 | Re-copy the package; existing tags HEAD-skip | — |
| Destination blob vanished (registry GC) | the HEAD miss on the next run | Re-upload. The mirror keeps **no** state about the destination — it HEADs every blob every run | — |
| Catalog holds entries with no root on disk, or the whole catalog is unusable | not detected per-package | `regenerate_catalog(&store, &as_name)` (`crates/…/oci/index/regenerate.rs:121`) behind an explicit `--repair-catalog` flag | `RegenerateOutcome` (`:232-237`) supplies the counts to report |

**Where `regenerate_catalog` fits, and where it does not.** It re-derives `c/index.json` from
**unchanged root bytes already on disk** (`regenerate.rs:19-22`) and writes no other path. So it
repairs catalog↔root disagreement in both directions, and nothing else: it cannot notice that a
root's content is missing at the destination, cannot repair a truncated root, and removes no root. It
is also **wholesale** — it takes every root under `p/`, including ones the current filter excludes,
which is right for a tree this tool owns end to end and wrong the moment an operator hand-edits the
tree. That is why it is a flag and not part of the default run: the per-package predicate above
already covers every state a normal interrupt produces, at zero extra cost, and `regenerate_catalog`
is the bigger hammer for the case where the catalog itself is the thing that is wrong.

**Failure policy (decision 13).**

```yaml
on_error: continue     # default; `fail_fast` is the other value
```

`--fail-fast` overrides the spec. `continue` is the default for the same reason skopeo's
`--keep-going` is: one broken package must not abort 120 healthy ones, and the run exits non-zero iff
anything failed. **It governs per-package failures only.** A destination read whose answer is not
authoritative — 503, timeout, auth failure — aborts the entire run immediately under either setting,
because the fail-safe doctrine forbids reading a non-answer as "absent". The two are different error
classes with different exit codes; see *Error model*.

### Source-side input validation — the attacker is upstream or the network

Scoped by [`security-threat-model.md`](../rules/security-threat-model.md): the execution environment
is trusted, everything arriving over the network is not. Each item below names its attacker.

**SSRF — a source root's `repository` is remote-controlled data this tool dials from inside the
perimeter.** *Attacker: a compromised or malicious source registry.* A root can name
`oci://169.254.169.254/x` or `oci://vault.internal:8200/x`, and the mirror connects to it with
whatever the runner's network reaches. ocx already solved this — `oci::ssrf::resolve_and_validate`
(`crates/…/oci/ssrf.rs:234`) and the `ClientBuilder::ssrf_guard` DNS resolver
(`crates/…/oci/client/builder.rs:217-223`), which re-validates at connect time so an approved address
cannot rebind before the socket opens — but it is wired only at index-layer call sites, and **this
design touches none of them**. Required:

- The source-side client is built with `.ssrf_guard(trusted_hosts)`. Not optional, not a flag.
- Each source root's physical host is validated **before the first fetch**, so a forbidden host is
  refused before any request is made — the ordering ocx's own regression test pins
  (`ocx_index.rs:1574`, *"refuses forbidden physical host before any transport call"*).
- `sources[].trusted_hosts: []` is the per-source escape hatch, for the legitimate case of an
  upstream index that points at a private registry. Empty by default.
- A refusal maps to `SourceError` (69) naming the host and the root that named it — not a per-package
  failure, because a source index steering the mirror at link-local addresses is not a package
  problem.

**`sources[].index` may carry userinfo, and a failed fetch prints the URL.** *Attacker: none needed —
this leaks the operator's own credential outward.* `reqwest`'s error `Display` includes the request
URL unredacted, so `https://user:pass@index.corp/` in a spec puts the password into any CI log that
records a fetch failure, and logs leave the trusted boundary routinely. **Reject a non-empty userinfo
component at load**, `SpecUsageError` (64), message naming the field and the `OCX_AUTH_*` variable to
use instead — never the URL.

**Digest verification is the mirror's job, not the peer's.** *Attacker: a malicious, compromised or
MITM'd source registry.* `OciTransport::push_blob` (`transport.rs:178-184`) takes the digest as a
**caller parameter** and does not compute it; a mirror that passes through the digest the source
claimed would republish attacker bytes under a digest the attacker chose, at the mirror's own origin.
Required: after pulling each blob and each manifest, assert `sha256(pulled_bytes) == expected_digest`
before any push; a mismatch fails the package and names both digests. The manifest half is partly
covered upstream — `fetch_manifest_raw_bytes` already refuses a registry-claimed digest that does not
match its bytes (`client.rs:2254`, `fetch_manifest_raw_bytes_rejects_registry_claimed_digest_mismatch`)
— but the blob half has no such check and the mirror must not assume one.

**Bound the referrers detection.** *Attacker: upstream, via an unbounded or cyclic referrer graph.*
v1 **detects** referrers rather than copying them (Open question 3), which keeps the bound trivial and
it is stated so nobody widens it silently:

- **Depth 1, no recursion.** One `/v2/<name>/referrers/<digest>` request per copied manifest. A
  referrer's own referrers are never queried, so there is no graph to walk and no cycle to detect.
- **Response body capped** at the same `MAX_INDEX_DOCUMENT_BYTES` ceiling the manifest fetch uses
  (`crates/…/oci/client.rs`, `fetch_manifest_raw_bytes_capped`), so an oversized response is refused
  rather than buffered.
- **The error names at most the first 10 referrer digests** and reports the total count; a manifest
  with 10,000 referrers produces a bounded message.

When v2 copies referrers, the walk it introduces needs its own depth cap, a visited-digest set, and a
per-manifest count cap — recorded here so that design starts from the bound rather than adding it
after an incident.

**Bytes republished under the mirror's own origin.** `__ocx.desc` carries a readme and a logo authored
upstream, copied verbatim, and rendered by whatever catalog UI the operator points at the corporate
registry. v1 copies them by digest without interpreting them — the same posture ocx has — and this is
recorded as a known residual rather than mitigated: sanitising foreign markdown is the renderer's
problem, and inventing a second answer here would be the wrong place for it.

---

## Open question 1 (resolved): where the blob-copy seam lives

> **AMENDED 2026-08-14 — Seam 1 is superseded. Everything in this section below this box, and
> Implementation-Plan item 2, describes work that is CUT.** There is no upstream `ocx` change. The
> heading below calling the wrapper list "the single authoritative list" no longer holds; the
> authoritative surface is now the plan's A-009 table
> (`.claude/state/plans/plan_registry_mirror_sync.md`), verified by compiling against it.
>
> **What ships instead.** Reads: `Index::fetch_manifest_raw_bytes` (`oci/index.rs:442`) — already
> public, returns verbatim bytes + digest + parsed manifest, the same seam `persist_dispatch` uses.
> Writes: the fork's own `native::Client` (`push_blob`, `mount_blob`, `push_manifest_raw`,
> `blob_exists`, `fetch_blob_size`, `pull_referrers`), constructed in ocx-mirror. Auth:
> `ocx_lib::auth::Auth::get_or_fallback` — ocx's own `OCX_AUTH_<slug>_*` → docker-credential-store →
> anonymous chain, unchanged. Registry-side SSRF: the public `ssrf::GuardedResolver` installed on the
> fork's public `ClientConfig.dns_resolver` field.
>
> **This is materially Seam 2, which scored 72 against Seam 1's 118. Recorded plainly rather than
> re-scored.** Seam 2 lost on exactly two criteria and both are now mitigated concretely: *one auth
> path* — auth still routes through ocx's own public `Auth`; *SSRF policy in one place* — the policy
> is still ocx's `GuardedResolver` and `resolve_and_validate`, merely installed by this repo instead
> of by `ClientBuilder`. What changed the answer is a constraint the matrix never scored: **the PR
> must stand alone.** A submodule pointer must reference a commit that exists upstream, so Seam 1
> makes an `ocx-sh/ocx` merge a *build* prerequisite and this PR cannot merge or release on its own.
>
> **Residual, accepted and bounded.** ocx-mirror now owns registry-*client construction policy* — TLS
> roots, timeouts, chunk size, protocol, resolver wiring. That is a `quality-core.md` *Don't Own
> Non-Domain Code* Warn. It is bounded by a test asserting the three settings `ClientBuilder::new`
> sets that `ClientConfig::default()` does not (`push_chunk_size`, `read_timeout`, `connect_timeout`
> — `builder.rs:98-114`). The copy ladder itself still delegates to the fork.
>
> **The SSRF guard is source-side only** — as this ADR already mandates at *Source-side input
> validation*. A guarded **destination** client refuses `localhost:5002` and an RFC1918 Artifactory,
> i.e. the acceptance harness and the motivating deployment both (`ssrf.rs:88-91` rejects loopback,
> RFC1918, link-local and CGNAT). The destination registry is operator-authored config, inside the
> threat model's trusted boundary.
>
> **Both halves pin. Corrected 2026-08-15 — an earlier revision of this box claimed the index half
> could not, and that was wrong.**
>
> Registry half: `GuardedResolver` on the fork's `ClientConfig.dns_resolver`. Index half:
> `resolve_and_validate(host, port, trusted_hosts)` pre-flight, then
> **`ClientBuilder::resolve_to_addrs(&host, &addresses)`** with the addresses that call already
> returned (`ssrf.rs:234` returns them; the earlier design discarded them), skipped only when the host
> is an IP literal. `redirect::Policy::none()` on both.
>
> **The retracted claim, and why it was wrong.** The box previously said `GuardedResolver` implements
> reqwest **0.13**'s `dns::Resolve` while ocx-mirror is on 0.12.28, therefore no pin was available,
> therefore pre-flight validation alone was accepted — with "add reqwest 0.13" recorded as the
> rejected alternative. The first half is true and irrelevant: **reqwest 0.12 has
> `ClientBuilder::resolve_to_addrs`** (`reqwest-0.12.28/src/async_impl/client.rs:2278`), which pins
> the connect without any resolver trait. No version bump was ever needed, so the rejected-alternative
> framing answered a question that does not arise, and is withdrawn.
>
> **What the false premise licensed.** The client re-resolves on the first GET, so a malicious source
> registry — which controls authoritative DNS for its own index hostname and needs no network position
> at all — serves a short-TTL record and flips it to `169.254.169.254` or an RFC1918 address between
> validation and connect. Blind SSRF, CWE-918 via CWE-367, against exactly the perimeter this floor
> exists to protect.
>
> **The reasoning error, named so it is not re-derived.** The second argument — *"the index base URL is
> operator-authored spec config, inside the trusted boundary"* — **conflates the string with the DNS
> answer.** The operator authored the hostname. They did not vouch for the source's nameserver, and it
> is the nameserver that decides what address the socket opens against. An operator-authored hostname
> is a trusted *string*, never a trusted *resolution*.
>
> That upstream's own `ReqwestIndexTransport::new()` (`ocx_index.rs:203-208`) installs no resolver is
> true and is not a licence — it is upstream's gap, not a standard to match.

**Two corrections to the framing, both verified in the trees.**

**Correction 1 — the primitives already exist, and the trait is `pub`.** `OciTransport`
(`crates/ocx_lib/src/oci/client/transport.rs:49`) already declares `head_blob` (`:99`),
`push_manifest_raw` (`:166`), `push_blob` (`:178`) and `mount_blob` (`:196`), and the trait itself is
re-exported publicly (`client.rs:116`). What is missing is a **route to an instance**: `Client`'s
transport field is private (`client.rs:151`), `Client::with_transport` is `#[cfg(test)] pub(crate)`
(`:182-183`), and `native_transport` is `pub(crate)` (`:105`). The read half is *already* public —
`Client::head_blob` (`:600`) and `Client::pull_blob` (`:652`). So the upstream diff is **four thin
`Client` wrappers plus two re-exports**, not a new transport layer.

**Correction 1b — `ProgressFn` is not reachable from outside `ocx_lib`, and that shapes the API.**
`OciTransport::push_blob` takes `on_progress: ProgressFn` (`transport.rs:183`), and `ProgressFn` /
`no_progress()` live in `mod transport` — declared **private** at `client.rs:109`, with only
`pub use transport::{MountOutcome, OciTransport};` at `:116`. An external caller therefore cannot
construct the argument the trait method demands. The upstream PR resolves this the lazy way: the
`Client` wrapper takes no progress parameter and passes `transport::no_progress()` internally,
exactly as `push_description` already does (`client.rs:1444`, `:1451`, `:1469`). No re-export of
`ProgressFn` is needed and none should be added — a progress-reporting variant can be added later if
a real run wants one.

**Correction 2 — referrers are further along than assumed.** The vendored fork already implements the
referrers API *with* the fallback-tag schema: `pull_referrers` at
`external/rust-oci-client/src/client.rs:2002`, documented at `:1981-2001`. `ocx_lib` does not surface
it (no `referrers` symbol anywhere in `crates/ocx_lib/src`), so the referrers-aware copy is a
**wrapper** exercise, not a protocol implementation.

### Options

**Seam 1 — thin `Client` wrappers upstream in `ocx_lib`** (submodule change + upstream PR). The
exact surface is specified once, in *The upstream contract* below — this ADR is the only
specification for a PR against another repository, so it gives signatures, not names.

**Seam 2 — drive the vendored `oci-client` fork directly from ocx-mirror.**

**Seam 3 — shell out to `ocx`.** There is no `ocx` verb that copies blobs across registries, so this
is Seam 1 plus a subprocess boundary.

**Seam 4 — shell out to `oras cp --recursive`.** Option D above, at the seam level.

| Criterion | W | **Seam 1** | Seam 2 | Seam 3 | Seam 4 |
|---|---|---|---|---|---|
| One auth path (settled decision 10) | 5 | **5** | 2 | 5 | 1 |
| Credential/SSRF policy stays in one place (ocx#272) | 5 | **5** | 1 | 5 | 2 |
| Referrers reachable (detect in v1, copy in v2) | 4 | **4** | 4 | 4 | 5 |
| No duplicated transport layer in this repo | 4 | **5** | 1 | 5 | 5 |
| Error classification into `MirrorError`/`ExitCode` | 3 | **5** | 4 | 2 | 1 |
| No new external tool dependency | 3 | **5** | 5 | 5 | 1 |
| Independence from upstream release cadence | 2 | **1** | 5 | 1 | 5 |
| **Total** | | **118** | **72** | **109** | **71** |

Arithmetic, recomputed in this revision (the previous totals — 106 / 60 / 97 / 62 — were all wrong,
though the ranking was not): Seam 1 = 25+25+16+20+15+15+2; Seam 2 = 10+5+16+4+12+15+10;
Seam 3 = 25+25+16+20+6+15+2; Seam 4 = 5+10+20+20+3+3+10. The SSRF criterion is no longer hypothetical
— *Source-side input validation* makes the guard a hard requirement, and Seam 1 is the only option
that gets it without re-implementing `GuardedResolver` in this repo.

**Recommendation: Seam 1.** The submodule bump to ≥0.5.8 is a hard prerequisite anyway, so the
upstream dependency is already on the critical path and costs nothing extra. Seam 1 keeps auth,
redirect/SSRF handling, retry and error classification in the single place this project has always
kept them — which is the whole reason ocx-mirror has never owned a transport. Seam 3's only loss
against Seam 1 is error classification through a subprocess, and it buys nothing Seam 1 does not
already have.

**Risk on Seam 1, and its mitigation.** It couples this feature to an upstream merge. Mitigation: the
additions in *The upstream contract* are additive and mechanical — four `Client` wrappers over
existing `OciTransport` methods, one visibility promotion, and one trait method with a default
implementation so no existing implementor changes. Each has an in-tree caller pattern to copy. They
can land in the submodule ahead of everything else in this ADR and be verified in isolation.

**Named consequence — memory, not a detail.** `OciTransport::push_blob` takes `Vec<u8>` (`:178`) and
`pull_blob` returns `Vec<u8>` (`:91`). There is no streaming blob-to-blob path in `ocx_lib` today.
Peak resident memory is therefore `blob_concurrency × largest_blob`. The largest sampled catalog
asset is bazel's `dist.zip` at 221 MB (operability research §6), so `buffer_unordered(8)` would peak
near 1.8 GB. **`RegistryConcurrency::max_blobs` defaults to 4** for this reason — and it is a knob on
a type this design owns, not a re-defaulting of `ConcurrencyConfig::max_downloads`, whose 8 stays what
every existing package mirror gets (`src/spec/concurrency_config.rs:39-41`).

**Corrected 2026-08-14 — the stated reason for buffering was wrong.** "A streaming `push_blob` does
not exist" is false: the fork exposes public `push_blob_stream` and `pull_blob_stream`. The real
reason to keep buffering is that **`sha256(pulled) == expected` before any push cannot be satisfied
by a naive tee-stream** — the bytes must be complete and verified before the first one is sent.
Recorded explicitly, because a future optimisation reading the old justification would delete the
buffer and verify-before-push together. Streaming is deferred on that basis; trigger unchanged (a
catalog blob exceeds ~500 MB, or a real run is observed OOMing), and any streaming design must say
how it preserves verify-before-push.

**The ceiling also depends on two conditions the implementation must hold**: one **run-scoped**
`Arc<Semaphore>` (not one per call) and **sequential** package processing — concurrency inside a
package, never across them.

**And the digest check does not change this.** `sha256(pulled_bytes)` is computed over the buffer that
is already resident, so the mandatory verification costs CPU, not memory.

---

## Open question 2 (resolved): pruning

**Append-only forever in v1. No `registry prune` verb.**

This lands on the industry default rather than choosing a side: Harbor never propagates deletion for
manual or scheduled rules, Artifactory's is opt-in, zot's is unimplemented
([project-zot/zot#3102](https://github.com/project-zot/zot/issues/3102)), Quay is archival by design
(tooling research §4). It is also forced by ocx's own doctrine — `adr_servable_index_snapshot.md`
encodes *"No delete verb on `IndexStore`"* as a ratified constraint.

The consequence to state plainly: **narrowing an `exclude:` glob does not remove anything already
copied.** The repair is to delete the output subtree and re-run, which re-copies from scratch; the
registry-side blobs stay. If a `registry prune` verb is ever built it is an explicit, opt-in,
separately-designed verb — not a flag on `sync`.

---

## Open question 3 (RESOLVED — owner ruling, 2026-08-14): trust scope for v1

**Ruling: integrity first. This mirror carries integrity, not trust, and says so.**

Trust — authenticity and provenance, as opposed to "these bytes are the bytes the digest names" —
arrives with **signing over the OCI referrers API**. That work is in active development in `ocx` and
is **not ready** (milestone *Signing & Trust v1*: [ocx#195](https://github.com/ocx-sh/ocx/issues/195)
referrers-capable acceptance registry, [ocx#196](https://github.com/ocx-sh/ocx/issues/196)
offline/air-gapped verify). Until it lands there is nothing signed for a mirror to preserve, and a
bespoke trust mechanism built here would be a second, competing answer that signing would then have
to displace. **So v1 mirrors content and index faithfully, claims integrity only, and a future
revision mirrors referrers alongside them** — at which point the trust story is signing's, not this
document's ([ocx-mirror#7](https://github.com/ocx-sh/ocx-mirror/issues/7)).

**The one thing this defers that must not be deferred silently.** Referrer copy is safe to omit
*only while nothing upstream is signed*. The moment a source publishes a signed artifact, a copy that
walks only tagged manifests drops that signature with no error and no diagnostic — Harbor's
[#23210](https://github.com/goharbor/harbor/issues/23210) is that exact failure. So v1 does not copy
referrers, but it **must detect them**: after copying a manifest, query
`/v2/<name>/referrers/<digest>` (the vendored fork already implements this, including the
`sha256-<digest>` fallback-tag schema) and, when the response is non-empty, **fail the package with a
counted error naming what was not copied**. That converts "signatures vanished" from a silent future
regression into the trigger that says referrer mirroring is now required. Cheap now, and it is the
difference between a deferral and a latent defect.

**Superseded recommendation, retained for the record:** accept the residual for v1 in writing with the
threat named, and do not scope a signed freshness manifest into this ADR. The ruling above supersedes
the framing but keeps both conclusions — no freshness manifest here, and the residual stated plainly.

**The residual, stated once and then scoped out.** The mirror's git repository and static host are
the root of trust for every consumer pointing `index=` at them: whoever can write there can roll a
package back, freeze the mirror while it looks current, or re-point any tag, and digest-pinning does
not touch any of it (Cappos et al. 2008; supply-chain research §1). **Every attacker in that sentence
is an insider** — compromised CI, a leaked deploy credential, a malicious maintainer — and
[`security-threat-model.md`](../rules/security-threat-model.md) rules them **out of scope for this
project**, in the ruling it says it generalises from this very question: *"the corporate mirror's host
and CI are the fleet's root of trust, and that is accepted, not mitigated."* So it is recorded here as
an accepted consequence, not analysed further, and a reviewer filing it is filing an out-of-scope
finding.

The paragraphs of analysis that stood here — freshness-manifest sketches, rollback taxonomy, PEP 458
precedent — were cut in this revision for that reason. They are not wrong; they are answers to a
question this project has decided not to ask, and keeping them crowded out the input-validation
obligations that *are* in scope (*Source-side input validation*). The one thing that survives, because
it is cheap and because it converts a future silent regression into a loud one, is the referrers
detection above.

**What v1 must not claim.** That digest-preservation gives the mirror supply-chain integrity.
"Digests are byte-identical" must not read as "trustworthy" in the docs or in any security review.

---

## Consequences

### Two defects found in the settled decisions — flagged, not silently redesigned

**Defect 1 — the exit-64 credential-rejection mechanism does not work as specified.**
Settled decision 10 says credential-shaped fields are refused at load with exit 64, *"same doctrine as
`policy_check_notify`"*. That doctrine works for `notify` because `webhook_secret` **is a schema
field** whose *value* is policy-checked after deserialization (`src/spec/load.rs:64-69` runs the policy
check before `validate`). A credential-shaped field in `registry.yml` is by construction **not** in
the schema, so `#[serde(deny_unknown_fields)]` rejects it during deserialization and produces
`SpecInvalid` → **exit 65**, with the message `unknown field 'password'`. The policy check never runs.

**Fix — one pre-scan over the merged raw `Value`, doing three jobs.** Between the merge and
`serde_yaml_ng::from_value` (`src/spec/load.rs:59` → `:61`), walk the merged mapping at any depth and:

1. **Reject credential-shaped keys** — a fixed deny-list (`password`, `token`, `username`, `auth`,
   `credentials`, `secret`, `api_key`) ⇒ `SpecUsageError` → 64.
2. **Read `kind`** ⇒ absent or not `registry` ⇒ `SpecUsageError` → 64. This is the *only* place it can
   be read: a `kind` field on the schema is checked after deserialization, and deserialization has
   already failed on `sources:` in exactly the misplaced-file case the discriminator exists for.
3. **Reject userinfo in `sources[].index`** ⇒ `SpecUsageError` → 64.

**The message must not print the value.** This is where the previous revision was actively dangerous:
it said "the same doctrine as `policy_check_notify`", and `policy_check_notify` **echoes the offending
value** into its error (`src/spec/validate.rs:412`, `:417` — both `(got '{secret}')`). Followed
literally in a credential pre-scan, that writes the token into the CI log, and logs leave the trusted
boundary routinely (`security-threat-model.md`, *"Report the offending key or variable name, never the
value"*). The pre-scan reports **the key path and the `OCX_AUTH_<slug>_*` variable to use instead**,
and nothing else:

```
registry.yml: sources[0].password: credentials must not appear in a spec;
set OCX_AUTH_<slug>_TOKEN in the environment instead
```

Because the scan runs **post-merge**, it covers every file in an `extends:` chain automatically —
see *the pre-scan runs post-merge* under the wire contract.

**Defect 2 — the catalog-digest short-circuit does not survive the rewrite.**
The operability research's headline result — *"a no-op sync of N packages costs exactly 1 HTTP
request"* (§3) — assumes the local roots are byte-identical to the source's, so their digests can be
compared against the source catalog's `packages[id] = sha256(root_raw)`. **This mirror rewrites
`repository`, so every local root hashes differently by construction.** The short-circuit as written
compares two things that can never be equal and would report every package as changed on every run.
(Option E does not have this defect. It was weighed and the owner chose C anyway.)

**The short-circuit, restated so it is sound.** It fires iff **both**:

- `sha256(source catalog bytes)` equals the value the previous fully-successful run recorded; **and**
- the **filtered package-name set equals the local `c/index.json` key set**.

The second condition is the fix for a defect the previous revision would have shipped: **a widened
`include:` is invisible to a catalog-digest comparison.** The source catalog has not changed, so the
digest matches, so zero deltas — and the newly-requested packages are never copied, silently, until
something upstream happens to change. Comparing the name sets catches it with **zero extra requests**:
the source catalog was just fetched and the local catalog was read when the transaction opened. Both
sets are already in memory. A narrowed `include:` is caught by the same comparison and correctly
produces no copy work (append-only, open question 2).

When the short-circuit does not fire, the fallback is the per-root pass: fetch each filtered package's
source root and compare its `tags{}` key set against the local root's. O(N) small JSON GETs, zero
bytes, ~121 requests at full public-catalog scale, which is seconds. This is the same predicate the
repair path uses, so there is one comparison in the design, not two.

**Where the cached digest lives — outside `output:`.** The previous revision put it at
`<output>/.ocx-mirror/<as>/source-catalog.json`, inside the tree the operator serves and commits —
which this ADR forbids twice (the `locks_root` section, and the validation item asserting nothing but
wire content lands under `output:`). It goes in
`<cache-dir>/registry-sync/<sha256(canonicalized output path)>/<as>.digest`, alongside the lock
directory, defaulting to `${XDG_CACHE_HOME:-~/.cache}/ocx-mirror/` and overridable with `--cache-dir`.

It stores a **single digest per source**, not the catalog. That is all the short-circuit needs: if the
digest matches, nothing upstream changed at all; if it differs, the per-root pass runs regardless of
what changed. Caching the whole catalog to compute per-package deltas would be a bigger file for a
saving the per-root pass already delivers in seconds.

This is **not** the resume journal decision 8 rejected, and the difference is testable: losing,
staling, or corrupting the file costs one extra comparison pass and never correctness. A CI runner
with no persistent cache simply takes the per-root path every run. Recording that explicitly is what
keeps a later reader from re-deriving it as a journal and rejecting it — or, worse, from making it
load-bearing.

### Positive

- The only design in the surveyed field that enumerates from a published static catalog instead of
  `_catalog`, states its destination-collision policy **and enforces it structurally**, and offers a
  real atomic-visibility boundary (tooling research §10).
- `ocx.lock` is byte-identically portable between a mirrored host and a direct-egress host — verified
  against the lock schema, not assumed (`lock.rs:170`, `resolve.rs:516`).
- **Resumability and repair are the same mechanism, and it is one predicate.** The output directory is
  the checkpoint: on interrupt at most one package has blobs copied and no root written, and the
  three-condition skip predicate re-copies exactly the packages that are not fully landed. No journal,
  no resume flag, no state file that can go stale — and every already-pushed blob HEAD-skips, so a
  repair costs requests rather than bytes.
- Reuses `serialize_root` / `serialize_catalog` / `serialize_config` — the byte-parity-tested writers —
  so the tree cannot drift from what ocx and `indexbot render` produce.

### Negative / accepted

- **The mirror becomes the fleet's trust root** (open question 3) — accepted, not mitigated, and
  out of scope per [`security-threat-model.md`](../rules/security-threat-model.md).
- **Append-only forever**: a narrowed filter does not un-copy anything.
- **Peak memory is `concurrency.max_blobs × largest blob`** until a streaming copy exists.
- **No `blob_anchor` in v1** — a blob shared by two destination repositories is uploaded twice unless
  the second lands in the same run and hits the in-run map. Registry-side storage still deduplicates
  by checksum; the cost is wire transfer only.
- **The consumer sees a rewritten pointer, so a source-published signature over the root document
  (should one ever exist) would not verify against the mirrored copy.** Structural consequence of
  Option C that Option E does not have; recorded because a future signing design must know it.
- **A mirrored-only host cannot resolve a package the mirror never copied**, even though the same
  package resolves fine publicly. True of any curated mirror; worth saying because the failure reads
  as "package does not exist".
- **`__ocx.desc` needs explicit handling** — readme and logo live at `<repository>:__ocx.desc`, and
  that tag is classified administrative, so *any* `root.tags{}` walk skips it automatically
  (lock-portability research). Not an install-path break (ocx models no `desc` on the install path);
  it breaks catalog/website rendering only.
- **Never be first mover on `format_version`.** An unrecognized version is a hard 65 at every reader
  (`wire.rs:49`, `:68`) — flag-day by design.
- **Overlapping runs are the operator's problem, not the tool's.** Harbor ships a *single active
  replication* toggle for exactly this, and this repo already enforces the equivalent invariant on the
  surface it owns — the generated workflows' `concurrency:` block with `cancel-in-progress: false`
  ([`subsystem-mirror.md`](../rules/subsystem-mirror.md) § R1). Settled decision 11 means
  `registry sync` renders no CI, so it cannot emit that block. `CatalogTransaction` serialises the
  catalog write, but two concurrent runs against one output tree still race on root documents.
  **Document it as a required `concurrency:` group on the operator's own job**; do not add a lockfile
  to a tree whose whole point is being a clean servable artifact.

### Risks

- *Referrers dropped silently.* If the copy walks only `root.tags{}`, every cosign signature and
  attestation vanishes with no error and the copy reports success — Harbor
  [#23210](https://github.com/goharbor/harbor/issues/23210) verbatim. Blocks
  [ocx-sh/ocx-mirror#7](https://github.com/ocx-sh/ocx-mirror/issues/7). **Mitigation:** v1 does not
  copy referrers but **detects** them and fails the package (open question 3), using the fork's
  existing `pull_referrers` (fallback-tag schema included), bounded as specified under *Source-side
  input validation*. So the failure mode is loud, not silent.
- *Credential forwarding across a host boundary on the push path.* [GHSA-jxpm-75mh-9fp7](https://github.com/oras-project/oras-go/security/advisories/GHSA-jxpm-75mh-9fp7)
  (CVSS 7.5, CWE-918) is the same defect class in the sibling implementation, tracked here as
  [ocx-sh/ocx#272](https://github.com/ocx-sh/ocx/issues/272). This ADR's push path is the first
  ocx-mirror code to open blob upload sessions against a registry the operator does not control the
  redirect behaviour of. **Mitigation:** Seam 1 keeps this in `ocx_lib`, where the fix lands once.
- *A tree written without `config.json`.* An ocx < 0.5.8 reading it fails **silently** on every
  package — `NotAnIndex` → `resolve_root` returns `Ok(None)`. **Mitigation:** always write it (below).
- *Artifactory repository-type mismatch.* The destination must be an **OCI-type** repository (7.74+),
  not a legacy Docker-type repo-key; the latter answers `unable to upload blob … unknown: Not Found`.
  Referrers need 7.90.1+ and are scoped to OCI/Helm-OCI types only. **Mitigation:** document as a
  prerequisite; choosing OCI-type now avoids a later repo migration.

---

## Technical Details

### Architecture

```
registry.yml ──► raw Value: credential deny-list · kind · index userinfo   (exit 64)
             ──► RegistrySpec (schema + grammar validated at load)          (exit 65)
                     │
   per source ───────┤   client built with .ssrf_guard(trusted_hosts)
                     ▼
        ┌─ GET <index>/config.json      ── gate format_version, keep raw bytes
        ├─ GET <index>/c/index.json     ── enumerate (static file; no _catalog)
        │        │
        │        ├── filter: include ∧ ¬exclude over "<ns>/<pkg>"
        │        ├── expand + validate destination (Identifier grammar, prefix containment)
        │        └── short-circuit iff  catalog digest unchanged
        │                          AND  filtered names == local catalog keys
        │                          else per-root compare (the repair predicate)
        │
        └─ per package needing work:
             GET  p/<ns>/<pkg>.json                     ── raw bytes + typed root
             validate root.repository host (SSRF) before any physical fetch
             for EVERY tags{} entry (version or not), and __ocx.desc:
                 pull manifest by digest ─► verify sha256(bytes) == digest
                 for each referenced blob + child manifest:
                     HEAD dest ──hit──► skip
                          │  └─ non-authoritative (503/timeout/auth) ─► ABORT RUN (69)
                          └─miss─► try mount (in-run digest→repo map)
                                     └─ UploadRequired ─► pull ─► verify sha256 ─► push
                 push manifest bytes verbatim (digest preserved)
                 GET /v2/<name>/referrers/<digest> ── non-empty ⇒ fail THIS package
             ── all content confirmed at destination ────────────────► THEN:
             parse root bytes → serde_json::Value
             mutate exactly one key: "repository" := "oci://" + dest (round-trip checked)
             serialize_root(&Value) → CatalogTransaction::write_root
                                       (hook = parse_physical_repository;
                                        catalog entry = sha256 of the REWRITTEN root)
        commit transaction ─► c/index.json ; write config.json
        on full success ────► record sha256(source catalog) in <cache-dir>, outside output:
```

### `RegistrySpec` — the wire contract

```yaml
kind: registry                       # discriminator; MirrorSpec carries `kind: package`
                                     # read by the raw-Value pre-scan, NOT as a schema field

target:                              # crate::spec::Target, shared verbatim — two fields, no more
  registry: artifactory.corp.example
  repository: ocx-mirror             # PREFIX (Artifactory repo-key), not a full repository

output: ./public                     # PARENT index home; one subtree per source

destination: "{registry}/{namespace}/{package}"   # plain substitution; no template engine
                                                  # {registry} REQUIRED when sources.len() > 1

on_error: continue                   # continue | fail_fast     (decision 13; --fail-fast overrides)

sources:
  - registry: ocx.sh                 # logical registry name — the `<ns>` consumers configure
    index: https://index.ocx.sh      # where that registry's index tree is served;
                                     # userinfo (user:pass@) REFUSED at load
    as: ocx.sh                       # OPTIONAL; default = `registry` verbatim.
                                     # Output subdir AND the {registry} expansion.
                                     # Hard error if not a legal OCI path component.
    include: ["kubernetes/*", "hashicorp/*", "ocx/*"]   # empty ⇒ everything
    exclude: ["*/internal-*"]                           # veto: match ⇒ excluded, always
    trusted_hosts: []                # SSRF escape hatch for roots pointing at a private
                                     # registry; empty by default

concurrency:                         # RegistryConcurrency — NOT crate::spec::ConcurrencyConfig
  max_blobs: 4                       # blob-copy semaphore; see the memory ceiling
  max_retries: 3                     # reactive 429 / Retry-After backoff (push_with_retry shape)
```

**Why `concurrency` is a new type and not the shared one.** The previous revision said
"`crate::spec::ConcurrencyConfig`, shared verbatim" **and** `max_downloads: 4`. Those are mutually
exclusive: `default_max_downloads()` returns **8** (`src/spec/concurrency_config.rs:39-41`), and
changing it would change `MirrorSpec`'s default for every existing package mirror — out of scope and
not wanted. Three of its five knobs are also dead here (`max_bundles`, `rate_limit_ms`,
`compression_threads` have no meaning in a blob copy), and carrying dead knobs into a new external
contract is how a schema accumulates lies. So:

```rust
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryConcurrency {
    #[serde(default = "default_max_blobs")]   // 4
    pub max_blobs: usize,
    #[serde(default = "default_max_retries")] // 3
    pub max_retries: u32,
}
```

Two knobs, both meaningful. `max_retries` reuses the **retry shape** `push_with_retry` already
implements (1s doubling to a 30s cap) — the shape, not the type — but **not** its ±10% jitter:
`registry_copy.rs::retry_delay` is a plain deterministic doubling ladder, because it stands in for a
`Retry-After` header the fork never surfaces to its caller (`is_rate_limited`'s doc comment,
`registry_copy.rs`), leaving nothing for jitter to desynchronise.

**Validation rules, all at load unless noted:**

| Rule | Error | Exit |
|---|---|---|
| Credential-shaped key anywhere in the document (pre-scan, Defect 1) — message names key path + env var, **never the value** | `SpecUsageError` | 64 |
| `sources[].index` carries userinfo | `SpecUsageError` | 64 |
| `kind:` absent or not `registry` (pre-scan) | `SpecUsageError` | 64 |
| `target.registry` / `target.repository` fail the OCI grammar (`Identifier::validate_repository`, `parse_with_default_registry`) | `SpecInvalid` | 65 |
| `as:` not a legal OCI path component | `SpecInvalid` | 65 |
| Duplicate `as:` across sources | `SpecInvalid` | 65 |
| `{registry}` missing from `destination` with >1 source | `SpecInvalid` | 65 |
| Unknown placeholder in `destination` | `SpecInvalid` | 65 |
| Glob containing `**`, `?`, or `{` | `SpecInvalid` | 65 |
| `on_error` not `continue` \| `fail_fast` | `SpecInvalid` | 65 |
| *(plan time)* An expanded destination fails `Identifier::validate_repository`, or does not start with `target.repository/` | `SpecInvalid` | 65 |
| *(plan time)* Two packages expanding to the same destination | `SpecInvalid` | 65 |

`ocx.sh` and `ghcr.io` pass the `as:` grammar (dots are legal per the distribution-spec name grammar);
`localhost:5001` does not, and gets a hard error naming `as:` as the fix.

**The pre-scan runs post-merge and therefore covers `extends:`.** `load_spec` builds a merged
`serde_yaml_ng::Value` — directly at `src/spec/load.rs:37-38` when there is no `extends:`, or by
shallow-merging the whole chain at `:41-58` when there is — and only deserializes at `:61`. Both
paths converge on one `merged` value, so a pre-scan inserted between `:59` and `:61` sees every key
from every file in the chain with no chain-walking of its own. `registry.yml` therefore **supports
`extends:`** on the same terms `mirror.yml` does, and the credential and `kind` checks cannot be
evaded by hiding a key in a base file. (The existing `policy_check_notify` call sits at `:67-69`,
*after* deserialization — which is precisely why Defect 1 exists.)

### CLI surface

```
ocx-mirror registry sync [SPEC] [--dry-run] [--fail-fast]

  SPEC          Path to the registry spec  [default: ./registry.yml]
  --dry-run     Report what would be copied — package counts AND estimated bytes — and copy nothing
  --fail-fast   Abort on the first package failure (default: continue-on-error, fail at end)
```

`--fail-fast` reuses the flag already on `SyncOptions` (`src/command/package/options.rs`) and
overrides the spec's `on_error:`. Default is **continue-on-error / fail-at-end** (skopeo's
`--keep-going` semantics): one broken package must not abort 120 healthy ones, and the process exits
non-zero iff anything failed. Confirmed as Harbor's real behaviour too, read from its source rather
than its docs — an `Execution` carries live `Total` / `Succeed` / `Failed` / `InProgress` / `Stopped`
counters with `Task` at per-artifact grain.

**Summary line, borrowing that counter set:** `N total, M copied, K skipped, J failed`. A no-op run is
**never silent** — it prints the same line with `0 copied`, because silence is indistinguishable from
"did not run" in a CI log.

**How `--dry-run` computes its byte estimate**, because "estimated bytes" without a method is a number
nobody can trust. It runs the whole plan except the transfers: enumerate, filter, fetch each changed
package's root, fetch each referenced manifest **by digest**, HEAD every **blob** digest at the
destination *(corrected 2026-08-15 — this paragraph said "every descriptor"; the shipped
`missing_descriptors` probes blobs only and excludes nested manifest bodies, which would need a
manifest `HEAD` the fork does not expose separately from a full fetch)*. The estimate is then the sum
of the `size` field of every *blob* descriptor whose HEAD missed, counting each distinct digest once.
That is the exact byte count a real run would transfer for the bytes that dominate it, short by the
manifest bodies — not a guess for what it does cover, since descriptor `size` is authoritative in an
OCI manifest. Cost: the same request count as a real run minus every blob GET and every push, which
is the point.

`registry validate` is deliberately **not** added. `--dry-run` parses and validates. Trigger to
reconsider: an operator wants a network-free pre-commit gate.

### The copy engine seam — concrete functions, no trait

**`src/pipeline/registry_copy.rs`** holds free `async fn`s over `&ocx_lib::oci::Client`. It sits under
`pipeline/` for the same reason `pipeline/target_registry.rs` does — it is shared machinery below the
command layer, and a fourth top-level module shape (`src/registry/`, which the previous revision
invented) buys nothing the module map does not already give. `src/command/registry/` stays the CLI
surface only, matching `command/package/` ↔ `pipeline/` throughout this crate.

**No `BlobSink` trait.** There would be exactly one production implementation, and the test seam
already exists: the acceptance harness runs a real Docker registry on `:5001` (`test/`), which is a
better oracle for digest preservation and mount fallback than any in-process double.
`quality-core.md` YAGNI: extract an abstraction when a second genuinely different implementation
appears.

#### The upstream contract — the single authoritative list

This ADR is the only specification for a PR against `ocx`, so the surface is given as signatures.
Two of the six are already public and need no change; four are new `Client` methods; two re-exports
close the type gaps. All of it is additive and independently verifiable.

**Already public — use as-is:**

```rust
pub async fn head_blob(&self, identifier: &Identifier, digest: &Digest) -> Result<u64>;  // client.rs:600
pub async fn pull_blob(&self, blob_ref: &oci::PinnedIdentifier)                           // client.rs:652
    -> std::result::Result<Vec<u8>, ClientError>;
```

`head_blob` is the destination existence probe; there is no need for a second `blob_exists` wrapper
(the previous revision listed one — cut, it duplicates an already-public method).

**New on `Client`, each a thin delegation to the existing `OciTransport` method:**

```rust
/// Raw manifest bytes by digest, byte-identical to what the registry served.
/// Promotes `fetch_manifest_raw_bytes` (client.rs:1886, `pub(crate)`) to `pub`.
/// Raw bytes are mandatory: a typed round-trip re-serializes and changes the digest.
pub async fn fetch_manifest_raw_bytes(&self, identifier: &Identifier)
    -> std::result::Result<Option<(Vec<u8>, Digest, oci::Manifest)>, ClientError>;

/// Wraps OciTransport::push_blob (transport.rs:178-184), passing
/// `transport::no_progress()` internally — see Correction 1b.
pub async fn push_blob_bytes(&self, identifier: &Identifier, data: Vec<u8>, digest: &Digest)
    -> Result<String>;

/// Wraps OciTransport::push_manifest_raw (transport.rs:166-171). Verbatim bytes,
/// so the destination digest equals the source digest.
pub async fn push_manifest_bytes(&self, identifier: &Identifier, data: Vec<u8>, media_type: &str)
    -> Result<String>;

/// Wraps OciTransport::mount_blob (transport.rs:196-204). The default trait impl
/// already returns `MountOutcome::UploadRequired`, so a transport that cannot mount
/// degrades correctly with no caller branch.
pub async fn mount_blob(&self, identifier: &Identifier, source_repository: &str, digest: &Digest)
    -> Result<MountOutcome>;
```

**Referrers** — the vendored fork already implements the API *with* the `sha256-<digest>`
fallback-tag schema (`external/rust-oci-client/src/client.rs:2002`, documented at `:1981-2001`), and
`ocx_lib` surfaces nothing (no `referrers` symbol anywhere in `crates/ocx_lib/src`). It therefore
needs **both** a new `OciTransport` method and a `Client` wrapper:

```rust
// on trait OciTransport, with a default impl returning Ok(Vec::new()) so no
// existing implementor (including the test transport) has to change:
async fn list_referrers(&self, image: &oci::native::Reference, digest: &oci::Digest)
    -> Result<Vec<oci::Descriptor>> { let _ = (image, digest); Ok(Vec::new()) }

pub async fn list_referrers(&self, identifier: &Identifier, digest: &Digest)
    -> Result<Vec<oci::Descriptor>>;
```

**Re-exports.** `MountOutcome` and `OciTransport` are already re-exported (`client.rs:116`).
`ProgressFn` is **not**, and must stay unexported — the wrappers above are shaped so it is never
needed at the boundary (Correction 1b).

### Destination skip check — reuse, do not reinvent

Copy-what-is-missing is settled ecosystem practice, not an invention — regsync spells it
`once --missing`, and pairs `fastCopy` with `forceRecursive` for the same skip-versus-rewalk choice
this design makes between the HEAD skip and the referrers walk.

`src/pipeline/target_registry.rs` already solves this exact problem and its module doctrine (`:1-13`)
is the rule to follow: **only an authoritative registry not-found may classify content as absent;
every other error aborts.** `ClientError::BlobNotFound` / `RepositoryNotFound` /`ManifestNotFound` are
distinct variants for precisely this (`oci/client/error.rs:69`, `:76`, `:85`). A HEAD that fails with a
503 must **not** be read as "blob absent and therefore needs uploading" — that is the mirror-side twin
of issue #157, and it would re-upload the whole catalog on a flaky link.

**And it aborts the run, not the package.** The previous revision left this contradicting itself: a
503 was said to produce `TargetError` → 69 while the default `on_error: continue` would have folded it
into the per-package aggregate and exited 1. Resolved in *Error model* by naming the two classes
apart — a non-authoritative destination answer is never a package failure.

### Index-store construction — `locks_root` must be redirected, and this is not a footnote

```rust
// output: ./public   as: ocx.sh   ⇒   ./public/ocx.sh/{config.json,c/,p/}
let store = IndexStore::new(&spec.output)          // root = the PARENT index home
    .with_locks_root(&locks_dir);                  // MANDATORY — see below
let mut transaction = store.begin_catalog_transaction(&source.as_name).await?;
```

`IndexStore::new` defaults `locks_root` to `root.join("locks")` (`index_store.rs:62-66`). Left
alone, `registry sync` would create a `locks/` directory **inside the very tree the operator serves
statically and commits to git**. The doc comment on `with_locks_root` (`:68-75`) says why in as many
words: *"Locks must never land inside a redirected … or shipped index home."*

**Correction to the previous revision's citation.** `regenerate.rs:283-285` is `store_at`, a **test
helper** inside `regenerate.rs`'s `#[cfg(test)]` module (`:280-285`); its own comment says it matches
the production sites, which is what misled the citation. The two real production sites are
`crates/ocx_cli/src/app/context.rs:234-238` (a `--index` / `OCX_INDEX`-redirected home, locks kept
machine-global) and `crates/ocx_lib/src/file_structure.rs:117`.

**`locks_dir` is stable, not per-run.** It is
`<cache-dir>/registry-sync/locks/<sha256(canonicalized output path)>/`, derived from the output tree
so every run against one tree lands on the same lock and different trees never contend. A per-run
directory would give each run its own lock file and defeat serialization entirely — two concurrent
runs would both enter `CatalogTransaction` and one catalog write would be lost. The stable directory
means the second run **blocks** for up to `SOURCE_LOCK_TIMEOUT` (60s, `index_store.rs:40`) on the
catalog write and then errors if the first is still holding it. That is the correct outcome, not a
regression: the alternative is silent catalog corruption.

The lock covers the **catalog write window only**, not the whole run, so two overlapping runs still
race on individual root documents. That is why the operator-side `concurrency:` group stays a
documented requirement (below) rather than something this tool claims to solve.

**Which layout, and why it is not reopened.** This is the **wrapped** case —
`IndexStore::new("./public")` with `source = "<as>"` — which is the store's ordinary shape and needs
no special-casing. `adr_servable_index_snapshot.md` OQ1 (root the store one level up, pass the
checkout directory name as `source`) addresses the *unwrapped* layout, where the tree root *is* the
served root. Settled decision 3 chose the wrapped layout, so OQ1 does not apply here.

**Constraint on how the tree is written: no symlinks under `p/`.** `list_wire_repositories`
(`index_store.rs:832`) branches on `file_type.is_dir()` (`:886`) and then `!file_type.is_file()`
(`:894`); `DirEntry::file_type` does not follow symlinks, so a symlink is neither — a symlinked root
is skipped, and a symlinked *directory* is skipped along with every root beneath it. Because
`regenerate_catalog` replaces the catalog wholesale, that is not one missing entry but silent bulk
removal (`regenerate.rs:50-61`). The mirror writes real files and real directories only.

**One inherited side effect worth knowing before running against a git checkout:**
`CatalogTransaction::commit` unconditionally `remove_file`s `c/index.json.etag` *before* its
unchanged-catalog early return, failure ignored on purpose (`regenerate.rs:42-46`). If an output tree
ever tracks that file in version control, a sync deletes it and cannot fail on it. Otherwise `commit`
writes nothing when the catalog is unchanged (`index_store.rs:1156-1158`), so a no-op run leaves the
tree byte- and mtime-identical — which is what makes it safe to run on a schedule against a
committed tree.

### Index-tree writing — every document, and who writes it

| Document | Action | Writer |
|---|---|---|
| `p/<ns>/<pkg>.json` | Parse raw bytes to `serde_json::Value`, mutate **exactly** the `repository` key, re-serialize. `IndexRoot` is `Deserialize`-only (`wire.rs:144-159`) and has no `deny_unknown_fields` for fleet forward-compat, so a *typed* round-trip would silently drop newer fields the mirror must pass through | `wire_writer::serialize_root(&Value)` (`:59`) via `CatalogTransaction::write_root` (`index_store.rs:1103-1124`) — **not** the bare `write_root_document` (`:757`): `write_root` writes the bytes atomically *and* upserts the derived catalog entry under the one held lock. See *the `repository_check` hook* below |
| `c/index.json` | **Derived, never copied** — ratified constraint: *"the catalog is authored, never mirrored"*. The mirror's catalog is the filtered subset, keyed on `sha256(rewritten root)` | `CatalogTransaction::commit`; `regenerate_catalog` (`regenerate.rs:121`) is the repair path |
| `config.json` | **Write-if-absent** *(corrected 2026-08-14 — this row said "always written")*. Fetch the source's, parse to gate `format_version` against `SUPPORTED_FORMAT_VERSION` (`wire.rs:49`), then write `{"format_version": 1}` **only when the file does not already exist**, matching `IndexStore::ensure_source_config`'s own write-if-absent semantics *(corrected 2026-08-15 — this cell previously specified the content as "the raw source bytes verbatim" when the source served one, parse-to-check-write-raw, synthesizing only when it did not; the shipped `write_config_json` takes no source-bytes parameter at all and always synthesizes — `config.json` is this tree's own fleet-facing name-grammar declaration, not mirrored content, and a parse→`serialize_config` round trip of foreign bytes would drop any sibling field `IndexFormatConfig` does not model)*. Unconditional writing breaks two things: the atomic rename churns mtime even for byte-identical content, destroying the no-op-run mtime stability this ADR sells three rows below as what makes a scheduled run safe against a committed tree; and it clobbers an operator-authored `name_segments`, which is jurisdiction-affecting and which ocx explicitly refuses to guess. **Neither `--repair-catalog` nor an ordinary sync repairs a corrupt or missing file** *(corrected 2026-08-15 — this cell previously said a corrupt file "is `--repair-catalog`'s job"; `regenerate_catalog` never writes `config.json` at all, and the mirror's own writer fires only from inside a package write, which a fully-skipped or short-circuited run never reaches — the remedy is deleting the file by hand, recreated only once some package's copy next proceeds to write)* | `serialize_config` (`:106`), write-if-absent |
| `o/<algo>/<hex>.json` dispatch objects | Copied byte-for-byte; the digest pins them | `IndexStore::write_dispatch_object` (`:393`) |
| `<repository>:__ocx.desc` | **Explicitly copied** per package. Skipped by any `root.tags{}` walk because the tag is classified administrative | copy engine |

Root fields other than `repository` — `name`, `owners`, `created`, `upstream`, `desc`, and critically
`status` / `deprecated_message` / `superseded_by` — are copied verbatim and **never fabricated**. The
last three are consumed by ocx and drive yank/deprecation warnings on every resolve
(`surface_root_status`, `crates/…/oci/index/ocx_index.rs:1004`).

#### The `repository_check` hook — specified, not assumed

`CatalogTransaction::write_root` parses the bytes into an `IndexRoot` itself (`index_store.rs:1110-1115`)
and then hands that value to a caller-supplied
`repository_check: impl FnOnce(&IndexRoot) -> Result<()>` (`:1107`, invoked at `:1116`). The mirror
passes **the same closure ocx's own published-root writer passes**:

```rust
transaction
    .write_root(&repository, &rewritten_bytes, |root| {
        parse_physical_repository(&root.repository).map(|_| ())
    })
    .await?;
```

That is `commit_published_root`'s hook verbatim (`crates/…/oci/index/local_index.rs:791-792`, fed to
`write_root` at `:804`). It checks the **grammar** of the pointer, not its identity, so a rewritten
`oci://artifactory.corp.example/ocx-mirror/…` satisfies it exactly as an upstream
`oci://ghcr.io/…` does — and it is the same C3 check every consumer applies on read, so a root that
passes here cannot fail there.

**Correction to a review finding.** The review reported this hook's only production exemplar as
`local_index.rs:435`, asserting `repository == format!("oci://{source}/{repository}")` and therefore
"unsatisfiable for a rewritten root". That line is real but is not a `repository_check` hook: it is
`expected_repository` inside `commit_root_tags`, the **derived**-index authoring path, which compares
against an existing document at `:456` and writes through the bare `write_root_document` (`:501`) —
never through `write_root`. Every actual `write_root` call site passes the `parse_physical_repository`
closure (`local_index.rs:791-792`, `:875-877` in `#[cfg(test)]`). No no-op hook is needed and none is
used: passing `|_| Ok(())` would give up the one cheap guarantee that a rewrite which produced an
unparseable pointer fails at the write instead of shipping.

**`preserve_order` is load-bearing and currently implicit.** `serialize_root` requires an
order-preserving `Value`; `ocx_lib` enables the feature (`crates/ocx_lib/Cargo.toml:50`) but
ocx-mirror's own `serde_json = "1.0.150"` (`Cargo.toml:51`) does not. It works today only through
Cargo feature unification. **Declare `features = ["preserve_order"]` explicitly** — an implicit
dependency on unification for a byte-exactness-critical path is a latent field-reordering bug.

### Error model — two failure classes, then two new variants

**The distinction `on_error` needs, stated first.** The previous revision contradicted itself: a
destination HEAD failing 503 was specified as `TargetError` → 69, while the default
continue-on-error would have folded it into the per-package aggregate and exited 1. Resolved by
naming the classes apart, and the rule is one line: **`on_error` governs per-package failures only.**

| Class | Members | Behaviour | Exit |
|---|---|---|---|
| **Whole-run abort** — a non-answer, never a package's fault | `TargetError` (a destination read whose answer is not authoritative: 503, timeout, auth, connection reset), `SourceError` (source index or source registry unreachable; an SSRF refusal), `IndexFormatUnsupported`, `IndexWriteError`, every `Spec*` | Aborts immediately under **both** `continue` and `fail_fast`. Never counted in the per-package aggregate | 69 / 65 / 74 / 64 / 79 |
| **Aggregating** — one package failed for a reason specific to it | manifest or blob that will not pull, digest mismatch, referrers detected, a 4xx push rejection, a tag whose `content` is gone upstream | Under `continue`: counted, reported in the summary, run continues. Under `fail_fast`: aborts at the first one | `ExecutionFailed` → 1 |

The fail-safe doctrine is what forces the split: `target_registry.rs`'s rule is that only an
authoritative not-found may classify content as absent, so a 503 is *"I do not know"* — and a run that
does not know cannot correctly skip or copy anything. Continuing past it would re-upload the catalog
on a flaky link, which is exactly issue #157's failure inverted.

**Reused unchanged:** `SpecNotFound` (79), `SpecInvalid` (65), `SpecUsageError` (64),
`SourceError` (69), `TargetError` (69), `ExecutionFailed` (1).

**New — justified individually:**

| Variant | Exit | Why not an existing variant |
|---|---|---|
| `IndexWriteError(String)` | 74 `IoError` | The output tree is a distinct failure surface with a distinct remedy (disk full, permissions). `TemplateError` is also 74 but means "workflow render failed" — reusing it would make the message lie |
| `IndexFormatUnsupported(u64)` | 65 `DataError` | A source declaring `format_version: 2` is **not** transient. `SourceError` (69) would make CI retry forever on something a retry can never fix. Follows `CascadeUnrepaired`'s precedent of a variant existing to carry a non-transient 65 through unchanged |

Both go in `src/error.rs` with a `kind_exit_code` arm (`:75-95`) and a unit test in the existing
per-variant style (`:146-218`).

### NFR coverage

| NFR | Position |
|---|---|
| **Scalability** | Full public catalog ceiling, cold: **121 packages** ⇒ ~1,585 release-builds, ~9,510 blobs, ~210 GB, ~60,000 requests — **plus referrers detection, which this figure omitted** *(corrected 2026-08-14)*: one `/referrers/<digest>` query per copied manifest, ~308 per cmake-sized package, so a full-catalog cold run adds tens of thousands of small requests. It is bounded (depth 1, no recursion) but it is not free, and a rate-limited registry will feel it. A real filtered corporate run is tens of packages. Enumeration is a static file — it does not scale with registry size, only with catalog size |
| **Latency** | No-op run: 1 request with the source-catalog cache, ~121 small GETs without. K changed packages: 1 + K root fetches + only the blobs those roots newly reference |
| **Availability** | Continue-on-error / fail-at-end by default (`on_error`). **Reactive** 429 / `Retry-After` backoff only, reusing `push_with_retry`'s 1s-doubling-to-30s ±10% jitter shape. The distinction is load-bearing: regsync's `ratelimit` is **proactive** — a pre-flight quota check reading a remaining-pulls header before a step, not a 429 handler — and GHCR publishes no such header, so the proactive mechanism has nothing to read. Defer it until a Docker Hub source exists |
| **Security** | Zero credentials in the spec, refused by a pre-scan that reports the key path and never the value; auth entirely ocx's; SSRF guard on the source client with per-source `trusted_hosts`; every destination path routed through `Identifier::validate_repository` and containment-checked against `target.repository`; `sha256(pulled) == expected` before every push; bounded referrers detection; credential non-forwarding across host boundaries is ocx#272, kept in `ocx_lib` by Seam 1. Full treatment: *Source-side input validation*. Trust residual: open question 3 |
| **Cost** | Bytes are the cost. `--dry-run` reports estimated bytes, not just counts — summed from descriptor `size` over the digests that missed at the destination, so it is the real transfer figure an operator on a metered or scheduled link needs before committing |
| **Operability** | Output tree is its own checkpoint; no journal. Non-silent no-op. Bounded blob concurrency, no KB/s cap and no chunked upload (deferred with triggers from operability §6) |
| **Portability** | `ocx.lock` unaffected — logical identity only. On-disk caches key on logical registry, so one machine switching between mirrored and direct config reuses one cache |

---

## Implementation Plan

1. [ ] **Bump `external/ocx` to ≥ 0.5.8**, `ocx.toml:6` (`ocx = "ocx.sh/ocx/cli:0.5.6"`) and
       `ocx.lock` in the same commit — `subsystem-mirror.md` makes the version floors an invariant
       *"enforced by keeping this repository's own `ocx.toml` / `ocx.lock` current"*, so the pin moving
       with the pointer is not optional. Fix the now-false `reqwest` sentence (below) and declare
       `serde_json` `preserve_order` explicitly while here. Blast radius in the next section.
2. [x] ~~**Upstream (ocx)**: the four new `Client` wrappers, `fetch_manifest_raw_bytes` promoted to
       `pub`, and `list_referrers` on `OciTransport` (default impl) plus its `Client` wrapper.~~
       **CUT 2026-08-14 — no upstream change is needed and none may be made.** Every capability is
       already public at the v0.5.8 pin; see the amendment box on Open question 1. Seam 1 would have
       made an `ocx-sh/ocx` merge a *build* prerequisite for a PR that must stand alone.
3. [ ] **`RegistrySpec`** in `src/spec/registry.rs` + `RegistryConcurrency` + the raw-`Value` pre-scan
       (credentials / `kind` / index userinfo, none of which echo a value) wired between
       `src/spec/load.rs:59` and `:61` + the grammar validations. Fixture per rejected document under
       `tests/fixtures/invalid/`, matching the existing one-file-per-rule convention — including one
       whose credential hides in an `extends:` base.
4. [ ] **Enumerate + filter + destination expansion**: catalog fetch behind the SSRF-guarded client,
       glob engine, `Identifier::validate_repository` + prefix containment on every expanded
       destination, the short-circuit with its name-set condition, the cached digest **outside**
       `output:`, `--dry-run` reporting counts and descriptor-summed bytes.
5. [ ] **Copy engine** (`src/pipeline/registry_copy.rs`): every-tag copy by digest with
       `sha256(pulled) == expected` before each push, destination HEAD skip reusing
       `target_registry.rs`'s pattern, mount attempt with upload fallback, `__ocx.desc`, bounded
       referrers detection, and the two-class error split.
6. [ ] **Index writer + repair**: root rewrite to `"oci://" + dest` validated by
       `parse_physical_repository` before writing, `serialize_root`, `CatalogTransaction::write_root`
       with the `parse_physical_repository` hook, `config.json`, write-root-last ordering, the
       three-condition skip predicate, and `--repair-catalog` over `regenerate_catalog`.
7. [ ] **Wire the verb**: `RegistryCommand` + the `Registry` arm on `Command`, deleting the placeholder
       comment at `src/command/registry/mod.rs:18-19`.
8. [ ] **Docs + rule**: `docs/`, and a `registry sync` section in
       [`subsystem-mirror.md`](../rules/subsystem-mirror.md) (module map rows, error-model rows).
9. [ ] **Amend the vendored ADR**: set `Superseded By` on
       `external/ocx/.claude/artifacts/adr_oci_registry_mirror.md` per the reconciliation table — the
       index-tree half to `adr_servable_index_snapshot.md`, the content-copy half to this ADR.

### The v0.5.6 → v0.5.8 submodule bump — measured, not assumed

**The CLI contract is low risk.** Both suspected breaking changes are **out of range** — the
`package inspect --format json` reshape (`CHANGELOG.md:192`) and the index physical-transport fix
(`:248`) both sit below the v0.5.5 boundary (`:85`), so they are already baked into the current 0.5.6
pin; ocx-mirror never invokes `ocx package inspect` in any case.

**Correction — the in-range BREAKING scan was wrong.** The previous revision claimed "the only
in-range BREAKING entry is `(lazy)`-scoped". Re-run against `/home/mherwig/dev/ocx/CHANGELOG.md`, with
the range being everything **above** the `## [0.5.6]` heading (`:62`) up to and including `## [0.5.8]`
(`:8`), there are **three**:

| Entry | Line | Version | Touchpoint here |
|---|---|---|---|
| *Every `${…}` in package metadata follows one grammar* *(package)* | `:14` | 0.5.8 | **Real.** `src/spec/metadata_config.rs` parses every mirror's metadata through `ocx_lib`'s `AuthoringMetadata` and calls `classify_install_path_rooted_dir` (`:10`, `:98-104`), and `${installPath}` literals appear throughout its fixtures (`:218`, `:234`, `:246`). Every push goes through this file. **Must be exercised before the bump lands**: load each in-repo spec and diff the resolved metadata against the 0.5.6 output |
| *Tools can join PATH without downloading their content* *(lazy)* | `:17` | 0.5.8 | None. Tool materialization; no touchpoint with `oci` / `package push` / `package announce` / `index` |
| *Snapshot keys companions by tag so two tags of one repository survive a freeze* *(patch)* | `:56` | 0.5.7 | None. Patch-tier companion state; ocx-mirror drives no patch verb |

Two of the three are inert. The first is not, and calling it inert is what the previous revision did.
It does not block the bump — the grammar change is a *unification*, and the shapes ocx-mirror emits
(`${installPath}`, `${installPath}/<dir>`) are the ones it unifies onto — but the bump commit carries
the check rather than the assumption.

**Both JSON reports ocx-mirror parses were checked field-by-field and are compatible.** Push gains
`canonical_tags_written` (`ocx_cli/src/api/data/push.rs:20-36`); ocx-mirror's `PushReport`
(`src/pipeline/ocx_cli/push.rs:29-44`) reads four fields, every one `#[serde(default)]`, with no
`deny_unknown_fields` on either side — the extra field is ignored. Announce gains
`pull_request_number` / `fork` (`api/data/announce.rs:20,50,55`), same tolerant shape.
`ExitCode::TempFail = 75` is unchanged (`ocx_lib/src/cli/exit_code.rs:46`), so
`push_exit_is_transient()`'s hardcoded 75 stays correct. `[patch.crates-io]` still resolves: both
nested fork submodules exist at v0.5.8, which pins `oci-client = "0.17"` and
`docker_credential = "1.3"`.

**One real drift, and it makes a checked-in doc false.** `reqwest` is **no longer mirror-owned**: ocx
v0.5.8 re-added it directly (`Cargo.toml:100`, `reqwest = { version = "0.13", features = ["rustls"] }`,
plus a new `webpki-root-certs = "1"`) to drive `ReqwestIndexTransport`. ocx-mirror is still on
`reqwest` **0.12** with different features (`Cargo.toml:68`). So the sentence carried in
**`CLAUDE.md:51-53`** § Dependency model (and echoed in the `Cargo.toml` comment block) — *"Since
v0.4.1 `reqwest`, `rustls`, `octocrab`, `url` are mirror-owned — ocx dropped them, so there is no
upstream source of truth for these four"* — is now **wrong for `reqwest`**. (The previous revision
cited `Cargo.toml:42-45` for that sentence; `:42-45` is not where it lives.) `octocrab` and `url`
remain absent upstream. `rustls` is no longer a bare
top-level ocx dependency at all (only a reqwest feature), while ocx-mirror pins it explicitly
(`Cargo.toml:69`) because `main.rs:31-33` installs the aws-lc-rs provider directly.

Two semver-incompatible `reqwest` majors coexist in one lockfile without error, so this is **not a
hard blocker** — but the bump commit must carry **(a)** the corrected wording in both places, and
**(b)** a `cargo tree -i reqwest -i rustls` verification, which nobody has run yet. Getting two TLS
stacks in one binary through inattention is exactly what that comment block exists to prevent.

### Migration / rollout — what existing consumers see

| Population | What happens |
|---|---|
| Existing `mirror.yml` specs | **Nothing.** `kind:` is read by the pre-scan and is optional there; an existing spec parses unchanged. Cutting `blob_anchor` means nothing is added to `Target`, so the shared type is untouched |
| Existing `package` verbs and generated workflows | **Nothing.** No shared code path is modified; `Target` is read, not changed, and `ConcurrencyConfig` is not touched at all now that `RegistryConcurrency` is its own type |
| A repo that bumps the binary but not the submodule | Does not build. The submodule bump is a prerequisite, not a runtime check |
| A fleet already using `[mirrors]` | **Unaffected and still supported.** It is the addressing half of a different deployment shape (Options B and E), not a thing this replaces |
| A consumer pointed at a mirror tree, on ocx < 0.5.8 | Resolves correctly **because the mirror always writes `config.json`**. Without it, silent not-found on every package |
| A mirror tree copied before referrer **copying** exists (i.e. every v1 tree) | Roots and content resolve; signatures are absent. Under v1 a package that *has* referrers fails loudly rather than shipping silently incomplete. Re-running after the v2 referrers walk ships backfills them — HEAD-skip means only the referrers are transferred |

**Rollout order:** steps 1–2 are independently shippable and should land first. Steps 3–6 are the
feature. Step 7 makes it reachable. Nothing before step 7 is user-visible.

---

## Validation

- [ ] A tree produced by `registry sync`, served statically, resolves every package it contains from a
      host with **no route to the public internet** (the acceptance criterion the feature exists for).
- [ ] Every copied manifest's digest at the destination equals its digest at the source — byte
      preservation, asserted on manifests and blobs alike.
- [ ] `ocx.lock` produced on a mirrored host is **byte-identical** to one produced on a direct-egress
      host for the same `ocx.toml`.
- [ ] **The written `repository` is an `oci://` reference ocx accepts** — `parse_physical_repository`
      on every root in the produced tree returns `Ok`, and a real `ocx` binary resolves a package from
      the tree. The scheme-less form must be shown to fail: a fixture writing a bare
      `host/path` value exits 65 at the consumer.
- [ ] **A multi-platform root copies every platform manifest**, not just the host's — the `oc-mirror`
      pitfall. Assert the destination image index lists the same descriptor set as the source's, by
      digest, for a package with ≥3 platforms.
- [ ] A package carrying a cosign referrer **fails the package with a counted error naming what was
      not copied** — the v1 detect-don't-drop contract. The Harbor #23210 guard flips to "referrer
      present at the destination" when v2 copies them.
- [ ] `__ocx.desc` is present at the destination for a package that has one.
- [ ] **A root whose `tags{}` carries `nightly`, `2026-08-14` and `a1b2c3d` alongside `1.2.3` copies
      all four**, and the destination resolves each to the same digest as the source. No tag is
      rejected for being unparseable.
- [ ] **A rolling alias survives the copy**: `X.Y` and `latest` at the destination resolve to the same
      manifest digests they resolve to at the source, for a package with a full cascade.
- [ ] `config.json` is present in every written subtree; a source declaring `format_version: 2` exits
      **65** and writes nothing.
- [ ] Interrupting a run mid-package leaves no root document for that package, and a re-run completes
      it while HEAD-skipping every blob the interrupted run already pushed.
- [ ] **An uncatalogued root self-heals**: delete a package's entry from `c/index.json` leaving the
      root on disk; the next run re-`write_root`s it and `commit` publishes a catalog containing it.
      Same assertion with a corrupted entry digest.
- [ ] **A widened `include:` copies the newly-matched packages** even though the source catalog is
      byte-identical to the previous run's — the short-circuit's name-set condition.
- [ ] A no-op re-run prints a non-silent report and issues exactly one catalog request (with the cache)
      — **and both outcomes are demonstrated**: the same assertion must go red on a run with one
      changed package.
- [ ] A destination HEAD failing with 503 aborts the **whole run** with `TargetError` (69) and does
      **not** re-upload — under `on_error: continue` as well as `fail_fast`. A per-package failure
      under `continue` exits **1** and the run reaches the last package.
- [ ] `registry.yml` carrying `password:` exits **64**, not 65 — the Defect 1 guard — **and the error
      output does not contain the value**. Same for a `password:` hidden in an `extends:` base file.
- [ ] `sources[].index` carrying `https://user:pass@host/` exits 64 and the password is absent from
      stdout, stderr and any log line.
- [ ] **A catalog key of `foo/../../prod-images` is refused**, not normalised; an uppercase key is
      refused rather than lowercased. Neither produces a write outside `target.repository/`.
- [ ] **A source root pointing at `oci://127.0.0.1/x` or `oci://169.254.169.254/x` is refused before
      any physical request is made**, and listing the host in `trusted_hosts` lets it through.
- [ ] **A blob whose bytes do not hash to the digest the source claimed fails the package** and is
      never pushed.
- [ ] Two sources whose packages expand to the same destination are refused at plan time.
- [ ] A single-source spec with no `{registry}` in `destination` is accepted; adding a second source
      makes the same spec exit 65.
- [ ] `cargo tree -i reqwest -i rustls` post-bump shows one TLS stack and no unexpected second
      `reqwest` major reaching the binary.
- [ ] The resolved metadata for every in-repo spec is unchanged across the 0.5.6 → 0.5.8 bump — the
      `${…}` grammar unification guard.
- [ ] **Nothing but wire content lands under `output:`** after a run: no `locks/`, no
      `c/index.json.etag`, no `.ocx-mirror/`, no cache file of any kind. Assert on the full recursive
      listing, not on a name list.
- [ ] `task verify` passes.

---

## Open Questions

- ~~**[NEEDS CLARIFICATION: trust scope for v1.]**~~ **Resolved by owner ruling, 2026-08-14** — see
  Open question 3. Integrity first: trust arrives with signing over the referrers API, which is in
  active development and not ready, so v1 claims integrity only and a future revision mirrors
  referrers. No freshness manifest here. The residual stays stated plainly, and v1 **detects**
  referrers it does not copy rather than dropping them silently.

- *Raised and ruled out of scope (owner, 2026-08-14): who runs `registry sync` in CI and under what
  identity.* An actor able to write to the mirror job's output already controls the corporate registry
  and the host serving the index — there is no configuration of this tool that changes that, so it is
  not a threat this design models. Generalised since into
  [`security-threat-model.md`](../rules/security-threat-model.md). Recorded so it is not re-raised:
  settled decision 11 leaves the job to the operator, and that remains deliberate.

- *Raised and ruled out of scope (owner, 2026-08-14): whether the destination registry supports
  cross-repository blob mount as a capability to detect.* Same answer as the Artifactory question
  below, and now moot for a second reason: `blob_anchor` is cut, so the only mounts attempted are
  within-run against a repository this run demonstrably wrote.

- **Resolved in this revision, recorded so it is not reopened:** `registry.yml` **supports
  `extends:`**. The raw-`Value` pre-scan runs post-merge (`src/spec/load.rs:59` → `:61`), so every
  file in the chain is covered by the credential, `kind` and userinfo checks with no chain-walking of
  its own. Settled decision 2 ("one spec per repo") is about how many `registry.yml` files a repo
  holds, not about whether one may inherit.

- *Raised and ruled out of scope (owner, 2026-08-14): whether Artifactory supports cross-repository
  blob mount.* Whether a registry honours a mount is the registry operator's configuration problem,
  not this tool's, and no probe gates anything: **mount is an optimistic attempt and any non-success
  falls back to an ordinary upload** (decision 9). The one addition the ruling makes explicit — a mount
  that fails *although the source blob demonstrably exists* emits a **warning** and then uploads, so a
  misconfigured destination is visible in the run log rather than silently costing bandwidth on every
  package, forever. Nothing is gated, nothing is probed, and a registry that declines every mount
  produces a correct mirror at the cost of wire transfer only — never storage, since checksum-based
  storage deduplicates server-side regardless.

---

## Links

- [`adr_cli_namespace_restructure.md`](./adr_cli_namespace_restructure.md) — reserves this namespace (`:170`)
- [`research_registry_mirror_tooling.md`](./research_registry_mirror_tooling.md) — competitor schemas, filter grammar, collision precedent, Artifactory requirements
- [`research_mirror_supply_chain.md`](./research_mirror_supply_chain.md) — TUF taxonomy under the rewrite, referrers, credential-forwarding CVE class
- [`research_mirror_lock_portability.md`](./research_mirror_lock_portability.md) — the R4 verdict, wire-format skew, the `__ocx.desc` gap
- [`research_mirror_operability.md`](./research_mirror_operability.md) — sizing, resumability, incremental cost, rate limits, failure semantics
- ocx: `adr_servable_index_snapshot.md` — the index-tree half this builds on (decisions A, C, F)
- ocx: `adr_index_indirection.md` — wire grammar; `repository` as a routing pointer expected to be rewritten
- `external/ocx/.claude/artifacts/adr_oci_registry_mirror.md` — R1–R6; superseded per the reconciliation table
- [ocx-sh/ocx-mirror#7](https://github.com/ocx-sh/ocx-mirror/issues/7) — re-sign + attest mirrored bundles (blocked on the referrers walk)
- [ocx-sh/ocx#272](https://github.com/ocx-sh/ocx/issues/272) — credential forwarding across host boundaries
- [goharbor/harbor#23210](https://github.com/goharbor/harbor/issues/23210) — referrers silently dropped under replication

---

## Changelog

| Date | Author | Change |
|------|--------|--------|
| 2026-08-15 | docfix (sonnet), per opus spec-compliance review | **Two more corrections found applying the same review's fixes to the plan.** The `config.json` row of *Index-tree writing* still described the superseded "raw source bytes, synthesize only if absent" design and still said a corrupt file "is `--repair-catalog`'s job" — both corrected to match the shipped write-if-absent-always-synthesized writer, and to the fact that neither `--repair-catalog` nor an ordinary sync restores a corrupt or missing file (delete-and-rerun is the only remedy, since `regenerate_catalog` never writes `config.json` and the mirror's own writer runs only from inside a package write a fully-skipped or short-circuited run never reaches). The dry-run byte-estimate paragraph said "every descriptor"; corrected to blobs only, matching `missing_descriptors`' own documented scope (nested manifest bodies are excluded). Status stays **Proposed**. |
| 2026-08-15 | architect (opus) | **Correction: the index-HTTP SSRF gap was accepted on a false premise. Both halves now pin.** The prior amendment claimed no resolver pin was available on this crate's own HTTP client because `GuardedResolver` implements reqwest **0.13**'s `dns::Resolve` while ocx-mirror is on 0.12.28. True, and irrelevant: reqwest 0.12 has `ClientBuilder::resolve_to_addrs` (`reqwest-0.12.28/src/async_impl/client.rs:2278`), and `resolve_and_validate` already returns the validated `Vec<SocketAddr>` (`ssrf.rs:234`) — the design simply discarded them. Index half now pins with `.resolve_to_addrs(&host, &addresses)`, skipped only for IP literals; `redirect::Policy::none()` unchanged; **no version bump was ever needed**, so the "add reqwest 0.13" rejected-alternative framing is withdrawn rather than retained. What the premise licensed: a malicious source registry, authoritative for its own index hostname and needing no network position, serves a short-TTL record and flips it to `169.254.169.254` or RFC1918 between validation and connect — blind SSRF, CWE-918 via CWE-367. The secondary argument (*"the index base URL is operator-authored spec config, inside the trusted boundary"*) is recorded as **rejected**, with its specific error named: it **conflates the string with the DNS answer** — the operator authored the hostname, not the source's nameserver, and an operator-authored hostname is a trusted string, never a trusted resolution. Status stays **Proposed**. |
| 2026-08-14 | architect (opus) | **Amendment: Seam 1 superseded, plus five corrections found while decomposing and building.** *Seam:* Open question 1 carries an amendment box — there is **no upstream `ocx` change** and Implementation-Plan item 2 is **cut**. Reads go through the already-public `Index::fetch_manifest_raw_bytes` (`oci/index.rs:442`), writes through the fork's own `native::Client`, auth through `ocx_lib::auth::Auth::get_or_fallback`, registry-side SSRF through the public `GuardedResolver` + `ClientConfig.dns_resolver`. This is materially **Seam 2** (scored 72 vs Seam 1's 118); recorded plainly rather than re-scored, with both losing criteria mitigated concretely and the deciding constraint named — a submodule pointer must reference an upstream commit, so Seam 1 makes an `ocx` merge a *build* prerequisite for a PR that must stand alone. Residual accepted: ocx-mirror owns registry-client *construction policy* (a *Don't Own Non-Domain Code* Warn), bounded by a test on the three settings `ClientBuilder::new` sets that `ClientConfig::default()` does not. *SSRF split:* the guard is **source-side only**, as *Source-side input validation* already mandated — a guarded destination refuses `localhost:5002` and RFC1918 Artifactory alike; and the index-HTTP half takes **pre-flight `resolve_and_validate` + `redirect::Policy::none()`** rather than a pinned resolver, because `GuardedResolver` implements reqwest **0.13**'s `dns::Resolve` and this crate is on 0.12.28 (adding 0.13 directly rejected per `CLAUDE.md`). *Corrections:* `config.json` is **write-if-absent**, not always-written — an unconditional atomic rename churns mtime against this ADR's own no-op-stability claim and clobbers operator `name_segments`; the `max_blobs: 4` justification "a streaming `push_blob` does not exist" is **false** (the fork has public `push_blob_stream`/`pull_blob_stream`) and the real reason is that verify-before-push cannot be satisfied by a naive tee — recorded so an optimisation does not delete the verification with the buffer, along with the two conditions the ceiling depends on (run-scoped semaphore, sequential packages); the ~60,000-request budget **omitted referrers detection** (~308 queries per cmake-sized package). *Unchanged and confirmed:* the lock claim at *Index-store construction* ("the lock covers the catalog write window only, not the whole run") was already correct — the plan's run-scoped transaction was the thing that disagreed, and the plan was fixed. Status stays **Proposed**. |
| 2026-08-14 | architect (opus) | **Revision against a three-reviewer panel (21 Block findings) and two new owner requirements.** *Shape:* Option C reaffirmed by owner ruling; Option E added to the options table with its real advantages and its real score (62 vs C's 55), Option B's false "needs a proxying registry" con corrected against `mirror_map.rs:67-75`, and the compensatory weighted sum replaced by the binary air-gap gate the Decision Drivers already state — A and B eliminated by the gate rather than outvoting it. *New requirement A (failover / auto-repair):* write-root-last restated as an owner **guarantee**; repair folded into a three-condition skip predicate (root present ∧ tags ⊇ source ∧ catalogued with matching digest) so an interrupted run self-heals with no verb and no journal; `regenerate_catalog` scoped to `--repair-catalog` with its limits named; `on_error: continue \| fail_fast` added, defaulting to `continue`. *New requirement B (cascade / non-version tags):* the mirror computes **no** cascade — three tag classes specified, every `tags{}` key copied by digest whatever its text, `pep440_sort_key` used as an ordering key only (`src/filter.rs:225-232`, `None` sorts first), and `resolve_cascade_tags` (`crates/…/package/cascade.rs:207`) recorded as the thing that must **not** be re-run against a filtered subset. *Ships-broken defects:* the written pointer is `"oci://" + destination` and round-trip-checked through `parse_physical_repository`; the `repository_check` hook specified as `parse_physical_repository` (correcting the review's `local_index.rs:435` premise — that is `commit_root_tags`, which writes through the bare `write_root_document`). *Correctness:* `kind` folded into the raw-`Value` pre-scan; catalog membership added to the skip predicate; the widened-`include:` blind spot closed by a name-set condition costing zero requests; cache moved out of `output:` and reduced to one digest; the 503 contradiction resolved by naming the whole-run-abort and aggregating error classes apart; `RegistryConcurrency` replaces the incompatible shared `ConcurrencyConfig`; **`blob_anchor` cut** (nothing writes to it, and it would land on a `Target` with no `deny_unknown_fields`) with a v2 trigger. *Security, all in scope under `security-threat-model.md`:* SSRF guard wired with per-source `trusted_hosts`; destination expansion routed through `Identifier::validate_repository` with prefix containment, refusing uppercase rather than lowercasing; the credential pre-scan explicitly forbidden from echoing the value (`policy_check_notify` at `validate.rs:412`,`:417` does, and following it literally would log the token); `sources[].index` userinfo rejected; `sha256(pulled) == expected` mandated before every push; referrers detection bounded; `extends:` resolved as supported via the post-merge pre-scan. *Honesty:* sizing re-derived from **121 packages** (~1,585 builds, ~9,510 blobs, ~210 GB, ~60,000 requests); the in-range BREAKING scan redone and corrected from one entry to **three**, with the `${…}` metadata-grammar change flagged as a real touchpoint on `src/spec/metadata_config.rs`; miscited lines fixed (`index_store.rs:832`/`:886`/`:894`, `ocx_index.rs:1004`, `CLAUDE.md:51-53`, `regenerate.rs:283-285` identified as a test helper with `context.rs:234-238` + `file_structure.rs:117` as the production sites); the Seam-1 surface given as exact signatures with `ProgressFn`'s private-module problem resolved; `run_locks_dir` defined as stable-per-output-tree; insider-attacker trust analysis cut to a pointer per the threat model. *Trims:* `{a,b}` glob alternation, `blob_exists`, the freshness-manifest analysis. Copy engine relocated to `src/pipeline/registry_copy.rs`. Validation checklist rebuilt around the new obligations. |
| 2026-08-13 | architect (opus) | Submodule-bump blast radius folded into Migration: CLI contract verified low-risk (both suspected breakages predate the current pin; both parsed JSON reports gained fields tolerantly), `ocx.toml` pin bump made non-optional, and the `reqwest`-is-mirror-owned sentence in `Cargo.toml:42-45` + `CLAUDE.md` recorded as **now false** with a `cargo tree` verification step. Added the `locks_root` redirect to the component contract — `IndexStore::new` defaults it to `root/locks`, which would ship a lock directory inside the served, committed tree — plus the wrapped-layout choice (servable-ADR OQ1 does not apply), the no-symlinks-under-`p/` constraint, and `commit`'s unconditional `c/index.json.etag` removal. Kept `CatalogTransaction::write_root` over the bare `write_root_document` and recorded why. Operability: Harbor's `Execution` counter set adopted as the summary line, its single-active-replication toggle recorded as an operator responsibility this tool cannot render (decision 11), and regsync's `ratelimit` corrected to proactive — which is why reactive-only is right against a header-less GHCR. |
| 2026-08-13 | architect (opus) | Initial draft. Records the twelve settled decisions; resolves the blob-copy seam to thin `ocx_lib` wrappers (Seam 1) after finding the primitives already exist and are `pub` at the `OciTransport` level with only the *instance route* missing, and that the vendored fork already implements `pull_referrers` with the fallback-tag schema; resolves pruning to append-only; recommends accepting the trust residual in writing. Flags two defects in the settled decisions: the exit-64 credential rejection is unreachable behind `deny_unknown_fields`, and the catalog-digest short-circuit cannot survive a rewrite that changes every root's digest by construction. Names the `push_blob(Vec<u8>)` memory ceiling and drops default blob concurrency to 4. |
