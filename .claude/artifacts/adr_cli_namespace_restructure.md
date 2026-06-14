# ADR: CLI namespace restructure — `package` / `registry`

## Metadata

**Status:** Accepted
**Date:** 2026-06-14
**Deciders:** Michael Herwig (maintainer)
**GitHub Issue:** #5 (Mirror-Daemon ADR — this restructure is its prerequisite)
**Related Design Spec:** N/A
**Stack Alignment:**
- [x] Decision fits existing stack (Rust 2024 + Tokio, clap) and conventions in `.claude/rules/subsystem-mirror.md`
**Domain Tags:** cli
**Supersedes:** the *CLI Design* slice of `adr_ocx_mirror.md` (the original flat `sync` / `check` / `validate` / `pipeline` surface, incl. the deferred `sync-all`)
**Superseded By:** N/A

## Context

`ocx-mirror` exposed package-mirroring as the **top-level** CLI surface: `sync`,
`check`, `validate`, `pipeline …`, `schema`. That fit when mirroring upstream tool
releases was the entire job. It stopped scaling once three new directions appeared:

1. **Mirror daemon (#5).** A continuous-sync command (`watch` — poll loop, per-spec
   cursor, ETag) needs a home. It is a *package* operation, not a new top-level verb.
2. **Registry mirroring.** The maintainer wants registry-to-registry mirroring (two
   kinds, deliberately under-specified) as a sibling capability.
3. **Python packages.** Near-future mirroring of Python distributions needs a home.

With package verbs occupying the root, there was nowhere clean to add `registry` as a
sibling. This ADR records the decision to split the CLI into a `package` namespace
(all existing commands) and a reserved `registry` namespace, recording how `watch`,
registry mirroring, and Python sources slot in — without committing to the still-open
registry design.

## Decision Drivers

- **Unambiguous homes** for `watch` (#5), registry mirroring, and Python sources.
- **Downstream blast radius**: the generated GHA workflow YAML baked into every
  consumer mirror repo pins the binary's subcommand path.
- **Namespace symmetry**: package CI (`pipeline`) belongs with the other package verbs.
- **YAGNI**: reserve the registry namespace without building empty scaffolding.

## Considered Options

### Option 1: Keep the flat surface; add `watch` / registry verbs at the top level

**Description:** Leave `sync`/`check`/… at root; add `watch`, `registry-sync`, etc. as
new top-level verbs.

| Pros | Cons |
|------|------|
| Zero churn to existing commands | Root namespace becomes a flat grab-bag |
| No downstream template changes | No grouping signal between package vs registry ops |
|  | `pipeline` (package CI) sits next to unrelated registry verbs |

### Option 2: `package` / `registry` namespace split (chosen)

**Description:** Move all package-mirroring verbs under `package`; reserve `registry`
as a sibling for registry-to-registry mirroring; keep `schema` top-level.

| Pros | Cons |
|------|------|
| Clear grouping; obvious home for `watch`, registry, python | One-time churn: templates, ci.rs assertions, tests, docs |
| Symmetric, discoverable `--help` tree | Extra nesting level in generated workflows (`package pipeline …`) |
| Reserves registry direction without over-building |  |

### Option 3: Separate binaries (`ocx-mirror`, `ocx-registry-mirror`)

**Description:** Ship registry mirroring as a distinct binary.

| Pros | Cons |
|------|------|
| Hard isolation between concerns | Two release artifacts, two install paths, duplicated auth/config |
|  | Splits shared spec/pipeline/auth code across crates prematurely |

## Decision Outcome

**Chosen Option:** Option 2 — `package` / `registry` namespace split.

**Rationale:** It gives every planned capability an unambiguous home, keeps the package
CI pipeline grouped with the package verbs, and reserves the registry direction at
near-zero cost. The one-time churn is mechanical and guarded by the renderer's
self-assertions and the downstream drift guard.

**Sub-decisions:**

1. **`pipeline` moves under `package`** → `ocx-mirror package pipeline …`. The pipeline
   is the package-mirroring CI engine (plan/prepare/push for *packages*); leaving it at
   the root while `sync` nests would split one concern across two levels. Cost: one
   extra nesting level in generated workflows.
2. **Clean hard break — no back-compat aliases.** Pre-1.0 (v0.4.0). The only out-of-tree
   consumer is the generated GHA workflow YAML, which the `verify-generated.yml` drift
   guard forces downstream repos to regenerate. Hidden aliases were considered and
   rejected as deprecation debt for a surface with a single, self-correcting consumer.
3. **`schema` stays top-level** — it generates JSON Schema for spec types
   (source-agnostic, cross-cutting); it is not a package operation.

### Python package mirroring — plausibility finding

"A package mirror, just a different type" is architecturally accurate. All sources
converge to a canonical `VersionInfo { version, assets, is_prerelease }`
(`src/source.rs`); packaging, variants, metadata, filtering, cascade, and push are
**source-agnostic**. Therefore:

- **One PyPI distribution → one OCX package** is a new `Source` enum arm
  (`src/spec/source.rs`) plus one `src/source/pypi.rs` module. It lives under `package`
  and changes nothing downstream. (The binary already mirrors Python interpreters via
  `github_release` + variants — `python-build-standalone`.)
- **Mirroring an entire index** (all of PyPI / many packages at once) is registry-shaped
  and belongs under `registry`, not `package`.

Neither is implemented by this ADR; both homes are reserved.

### `watch` home

The mirror daemon (#5) lands as `ocx-mirror package watch` once #5 settles the open
`watch` vs `serve` question. It is not added in this restructure.

### Consequences

**Positive:**
- Discoverable, symmetric CLI tree; clear extension points for registry + python + daemon.
- Package CI grouped with package verbs.

**Negative:**
- Breaking change for anyone invoking the old top-level verbs (mitigated: pre-1.0; drift
  guard regenerates downstream workflows).
- One extra nesting level in generated workflow YAML.

**Risks:**
- A downstream repo on a new binary with old, not-yet-regenerated YAML fails until it
  regenerates. Mitigation: the drift guard reds the workflow until regeneration, making
  the required action explicit.

## Technical Details

### Architecture (CLI tree)

```
ocx-mirror
├── package                       # all package-mirroring operations
│   ├── sync                      # mirror a spec → registry
│   ├── check                     # dry-run of sync
│   ├── validate                  # validate a mirror.yml
│   ├── pipeline                  # pre-publish CI engine
│   │   ├── generate ci
│   │   ├── plan | prepare | push | notify | describe
│   └── watch                     # DIRECTION ONLY (#5) — not yet implemented
├── registry                      # RESERVED skeleton — no live subcommand yet
└── schema                        # top-level utility (source-agnostic)
```

### API Contract

- `Command` (top-level): `Package(PackageCommand)`, `Schema` (feature-gated).
- `PackageCommand::execute(&self, printer, progress)` — threads both because `sync`
  needs progress; mirrors the existing `PipelineCommand` dispatch pattern.
- `src/command/registry/mod.rs` is a documented placeholder declared in the module tree
  (`mod registry;`) with **no** `Registry` arm on `Command` — clap rejects empty
  subcommand enums and a live-but-empty verb would mislead.

## Implementation Plan

1. [x] `git mv` package verbs under `src/command/package/`; add `PackageCommand`; rewrite
   top-level `Command`; re-point `crate::command::*` paths. *(commit `08119e0`)*
2. [x] Render `package pipeline …` in baked GHA templates; update `ci.rs` assertions;
   update pytest call sites, CLI docs, `CATALOG.md`. *(commit `08119e0`)*
3. [x] Reserve `src/command/registry/` placeholder; scaffold this ADR; refresh
   `subsystem-mirror.md` + serena `core` memory. *(this commit)*
4. [ ] `ocx-mirror package watch` — deferred to #5 (after watch-vs-serve resolves).
5. [ ] `registry` subcommands + the two-kinds design — deferred to a dedicated ADR.
6. [ ] Python `pypi` source type — deferred to a future feature.

## Validation

- [x] `task rust:verify` passes on the refactor (397 unit tests; 38 renderer tests
  confirm templates render the new verbs).
- [x] Manual CLI shape: top-level shows `package` + `schema`; `package pipeline` lists
  the six subcommands; old `sync` is rejected (hard break confirmed).
- [x] Binary-probe acceptance tests pass against the rebuilt binary.
- [ ] Full Docker acceptance (`task test:parallel`) — CI gate.

## Links

- [adr_ocx_mirror.md](./adr_ocx_mirror.md) — superseded CLI Design slice
- [adr_ocx_mirror_test_pipeline.md](./adr_ocx_mirror_test_pipeline.md) — pipeline contracts
- [subsystem-mirror.md](../rules/subsystem-mirror.md) — module map / command contracts

---

## Changelog

| Date | Author | Change |
|------|--------|--------|
| 2026-06-14 | Michael Herwig | Initial draft, accepted; refactor landed in `08119e0` |
