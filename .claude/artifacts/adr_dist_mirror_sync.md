# ADR: Distribution mirroring — `ocx-mirror dist sync`

## Metadata

**Status:** Accepted
**Date:** 2026-08-18
**Deciders:** Michael Herwig (maintainer)
**GitHub Issue:** N/A
**Related Design Spec:** N/A — the decisions below are the design; the surface is one verb and one spec file
**Stack Alignment:**
- [x] Fits the existing stack (Rust 2024 + Tokio, clap, reqwest) and the conventions in `.claude/rules/subsystem-mirror.md`
**Domain Tags:** cli, mirroring, supply-chain
**Amends:** [adr_cli_namespace_restructure.md](./adr_cli_namespace_restructure.md) — adds the third top-level namespace (sub-decision 4 there)
**Superseded By:** N/A

## Context

A corporate operator can already mirror every OCX package their fleet consumes:
`package sync` for a tool they package themselves, `registry sync` for whole
upstream indexes. Neither mirrors the **bootstrap layer** — ocx's own release
archives and the `dist.json` manifest naming them — so a network with no route
to `github.com` could mirror everything ocx installs and still have no way to
install `ocx` itself.

That layer is not an OCX package and cannot become one. It is consumed *before
any ocx binary exists*, by five shell installers that run `curl` with no token
and no `jq`, by `rules_ocx` through Bazel's `ctx.download`, by `find_ocx`
through CMake's `file(DOWNLOAD)`, and by the SDKs still to be written. Every one
of those speaks plain HTTP and nothing else.

The reachability requirement is also stricter than a proxy satisfies. A
pull-through remote covers today's network, but the operator's requirement is
retention measured in years, over which upstream may retag or delete a release.
The bytes are therefore copied, not proxied.

## Decision Drivers

- **Every consumer works unchanged.** Whatever shape the mirror takes, the five
  installers, Bazel, CMake and the SDKs must reach it with the two environment
  variables they already read.
- **Corporate stores are not one store.** Artifactory generic, Nexus raw, GitLab
  generic packages, Azure Blob, S3, plain nginx, GitHub Pages.
- **Reproducibility.** An operator must be able to pin an install so it resolves
  the same bytes years later.
- **Clobber-safety.** A mirror that half-publishes is worse than one that fails.
- **No new dependencies**, per the project's boring-technology stance.

## Decision Outcome

`ocx-mirror dist sync`, driven by a `dist.yml` spec: fetch the upstream
`dist.json`, filter it, copy every surviving archive into `output:` at a
configurable layout, rewrite each row's `url` to the mirror, and write the
manifest as three documents — a rolling `dist.json`, a `dist.json.sha256`
sidecar, and a content-addressed `dist/<sha256>.json` snapshot. Uploading is
optional; the emitted tree is always written.

### Sub-decision 1: the manifest is fetched, never re-derived

`dist.json` is generated upstream by `www-setup/scripts/gen-dist.sh` from the
GitHub Releases API. That script owns target extraction from each release's
`sha256.sum`, the channel semantics, and the `latest` / `latest_next` pointer
rules.

**Decided:** fetch and transform the published manifest; never re-derive it from
the Releases API.

**Rationale:** a second implementation of those rules would drift from the first
silently — the two would agree on every release until the day they did not, and
the disagreement would surface as an installer resolving a version the mirror
does not hold. Filtering and re-pointing are the only transformations applied,
and both are decidable from the manifest alone.

**Cost:** the mirroring job must reach `setup.ocx.sh`. Acceptable — that job is
online by definition, since it also reaches GitHub for the archives.

### Sub-decision 2: row URLs are rewritten at mirror time

The installers compose a mirror URL as
`${OCX_INSTALL_MIRROR_URL%/}/${tag}/${filename}` (`install.sh`). A plain file
store satisfies that shape. A GitLab generic package registry does not — it
addresses files as `/packages/generic/<package>/<version>/<file>`.

**Decided:** the mirrored manifest carries the mirror's URLs. Applied
unconditionally, including for stores where `OCX_INSTALL_MIRROR_URL` alone would
have worked.

**Rationale:** the alternative is teaching a URL template to five shell
dialects, Bazel, CMake and every future SDK, and keeping those seven
implementations agreeing forever. Rewriting once, in the one place that already
parses the manifest, leaves every consumer needing only
`OCX_INSTALL_DIST_URL`. Applying it unconditionally means one code path rather
than two, and no store-shape-dependent behaviour to reason about.

**Cost:** the mirrored manifest is no longer byte-comparable with upstream's, so
a consumer cannot diff one against the other to detect tampering. Out of scope
per `security-threat-model.md` — an actor who can rewrite the mirror's output
already controls everything downstream of it. `sha256` is never touched, so the
archives themselves remain verifiable against upstream's assertion.

**Consequence:** `publish.base_url` must carry no query or fragment. Two
composers act on it — the published URL and the upload target — and they treat a
query differently. It is also the shape an Azure SAS arrives in, which would
otherwise be republished to every consumer.

### Sub-decision 3: the JSON rendering is hand-rolled

`install.sh` parses the manifest with `grep -o '{[^{}]*}'`, and `grep` is
line-based.

**Decided:** render every leaf object on exactly one line, and emit `latest`
before `latest_next` before `releases`. Unknown keys — top level, pointer, and
row — round-trip through `#[serde(flatten)]`.

**Rationale:** `serde_json::to_string_pretty` splits objects across lines, which
makes them invisible to that parse; the failure is silent, and it is silent on
every shell at once. Emitting the pointers first matters because
`get_latest_version` takes the *first* leaf object whose channel is `stable`.
Round-tripping unknown keys matters because the mirror is the one hop in the
chain with no way to know who reads what.

**Cost:** this repository's output is coupled to a parser in `ocx-sh/www-setup`,
with no shared CI. Mitigated from this side: an acceptance test runs that exact
`grep`/`sed` pipeline against the emitted bytes, so a regression here fails here.
It cannot catch a change on the other side, which is www-setup's to own.

### Sub-decision 4: one native PUT, and the tree is always emitted

**Decided:** always write the tree to `output:`. Offer an optional native HTTP
`PUT` — one implementation, no trait, no per-store backends — with a `headers:`
escape hatch. Do not shell out to `curl`.

**Rationale:** Artifactory generic, Nexus raw and GitLab generic packages are
the same `PUT`; Azure Blob differs by one header, which `headers:` covers. Stores
needing request signing (S3, GCS) are served by the emitted tree and the
operator's own CLI, which already has their credentials wired. A trait with one
implementation is an abstraction with nothing to abstract; the trigger to extract
one is a target that genuinely cannot be expressed as a header — GitHub Releases,
whose two-step upload API would be the first.

Shelling out to `curl` was rejected on three counts: `curl` is absent from
distroless images, credentials in `argv` are readable from `/proc` for the
process lifetime, and a template script per artifact buys a process spawn and
quoting bugs in exchange for no control over retries.

**Cost:** WebDAV does not work under a nested layout — RFC 4918 §9.7.1 forbids
`PUT` from creating collections. Documented rather than worked around.

## Invariants

All are enforced in code and pinned by tests:

1. **Clobber-safety.** A run that cannot place every selected archive publishes
   **no manifest at all**. The destination keeps its previous, internally
   consistent manifest; archives that did land stay on disk, so the corrected
   re-run is cheap. This mirrors the rule `gen-dist.sh` enforces upstream.
2. **Clobber-safety's second half.** A run that selected **nothing** publishes
   nothing either. It satisfies invariant 1 trivially — every selected archive
   landed — so that guard alone reads a mistyped `select.min_version` as a
   success and overwrites a working manifest with one naming no releases,
   leaving every consumer resolving `latest` to `null`. Found in review; the
   original single-guard formulation had an acceptance test asserting exactly
   the bad behaviour, which is how a hole this shape survives a first pass.
3. **Publish order.** Archives, then the content-addressed snapshot, then the
   sidecar, then the rolling `dist.json` **last**. A consumer reading mid-run
   resolves either the old manifest or the new one, and both are fully backed.
4. **One URL composition.** The `url` stamped into a published row and the
   uploader's PUT target are the same function call, not two agreeing
   implementations. They were two, and the two diverged twice — on a query
   string (now refused outright in `publish.base_url`, since an Azure SAS there
   is a credential leak either way) and on percent-encoding, which no
   validation can rule out because `filename` is foreign data.

## Consequences

**Positive:**
- An air-gapped fleet can bootstrap ocx with two environment variables.
- Pinning `OCX_INSTALL_DIST_URL` at a `dist/<sha256>.json` snapshot pins the
  entire closure, since every row carries an inline digest.
- Filtering is extensible without a precedence rule: `select:` combines
  subtractively with AND, so `targets:`, `max_version:` or `channels:` are
  additive.

**Negative / accepted:**
- Downloads and uploads are sequential. Acceptable for a scheduled batch job;
  the seam for bounded concurrency exists if a fleet outgrows it.
- Consumers cannot authenticate their *reads*, so the mirror must allow
  anonymous `GET`. Recorded as follow-up work in the three consumer repositories
  (`find_ocx`, `rules_ocx`, `www-setup` — see their `mirror-auth.md` rules).
- The rolling `dist.json` is the one mutable object; a caching proxy in front of
  it can serve a stale manifest. Harmless by construction — a stale manifest
  names archives that are still present — but worth knowing.

## Links

- [adr_cli_namespace_restructure.md](./adr_cli_namespace_restructure.md) — amended by this ADR
- [adr_registry_mirror_sync.md](./adr_registry_mirror_sync.md) — the sibling namespace, same tier
- [security-threat-model.md](../rules/security-threat-model.md) — the defended boundary
- [subsystem-mirror.md](../rules/subsystem-mirror.md) — module map
- `docs/reference/dist-yml.md` — the operator-facing spec reference

---

## Changelog

| Date | Author | Change |
|------|--------|--------|
| 2026-08-18 | Michael Herwig | Initial draft, accepted; records the four decisions the implementation made |
| 2026-08-18 | Michael Herwig | Round-two review: added invariants 2 and 4 (empty selection publishes nothing; one URL composition) and the 8 MiB cap on the manifest fetch |
