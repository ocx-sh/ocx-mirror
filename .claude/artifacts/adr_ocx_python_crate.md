# ADR: `ocx_python` as a day-one workspace library crate

## Metadata

**Status:** Proposed
**Date:** 2026-07-04
**Deciders:** Michael Herwig
**GitHub Issue:** N/A (pre-implementation)
**Related Design Spec:** [design_spec_ocx_python.md](./design_spec_ocx_python.md)
**Stack Alignment:**
- [x] Decision fits existing stack (Rust 2024 + Tokio, see CLAUDE.md) and conventions in `.claude/rules/subsystem-mirror.md`
**Domain Tags:** pipeline | spec | source
**Supersedes:** N/A
**Superseded By:** N/A

## Context

ocx-mirror will gain a pylock-driven Python source type: mirror each locked
wheel into the target registry (content-addressed by lock hash) and compose a
runnable environment package from prefix-annotated wheel layers plus a
private interpreter dependency (see
[research_python_wheel_oci.md](./research_python_wheel_oci.md)).

A second consumer exists on the roadmap: **ocx-dist** (separate repository,
distribution-oriented) also needs Python support. Two independent
implementations of the translation logic would fork the on-registry
conventions (naming, repack determinism, layer layout, entrypoint synthesis)
— the conventions are the actual cross-repo contract, and drift there
corrupts a shared registry namespace irreversibly.

Today ocx-mirror is a single binary crate with its manifest at the repo root
(degenerate one-member workspace anchoring `[patch.crates-io]`). The decision:
where does the Python translation layer live, and in what workspace shape?

## Decision Drivers

- **Convention integrity**: exactly one code implementation of the
  one-way-door conventions, consumed by all writers of the registry namespace.
- **Extraction cost**: the crate must move to the ocx mono-repo
  (`crates/ocx_python`) with near-zero migration cost once ocx-dist is real.
- **No new pinning strategy**: dependency handling must reuse the existing
  submodule/git-pin discipline (CLAUDE.md dependency model).
- **Mirror pipeline fit**: slot into the existing two-phase pipeline
  (prepare concurrent / push sequential) without restructuring it.

## Industry Context & Research

**Research artifact:** [research_python_wheel_oci.md](./research_python_wheel_oci.md)
(F1–F7 feasibility; F8 verified test corpus + 19-property edge-case
catalog; F9 lock provenance for published wheels)
**Trending approaches:** wheels-in-OCI (PyOCI), content-addressed Python
stores (uv cache, rattler CAS proposal), lock-driven env reconstruction
(uv2nix). Nothing in the ecosystem covers lock→OCI-environment translation —
genuine net-new territory.
**Key insight:** a valid resolved lock is collision-free by construction, so
OCX's overlap-free prefix-layer union composes a correct site-packages with
zero core changes (ocx ≥ v0.4.1).
**Corpus:** `pycowsay` (easy) · `yt-dlp`, `black` (medium) · `streamlit`,
`google-cloud-aiplatform[full]` (hard) · `uwsgi`, `psycopg2` (negative
fixtures — no-wheel-anywhere vs no-wheel-for-triple failure paths).
**Published wheels ship no lock** (verified): v1 requires a pylock input;
v2 `source.type: pypi` derives one per target via `uv pip compile
--format pylock.toml --exclude-newer <stamp>` (reproducible resolution).

## Considered Options

### Option 1: Workspace conversion — `crates/ocx_mirror` + `crates/ocx_python`

**Description:** Convert the root manifest into a virtual workspace
(`[workspace] members = ["crates/*"]`, `[workspace.dependencies]`,
`[workspace.package]`, `[workspace.lints]`), move the existing binary crate
to `crates/ocx_mirror/`, add `crates/ocx_python/` as a pure library crate.
This mirrors the ocx mono-repo's own workspace shape — the vendored
`external/ocx/crates/ocx_mirror/` snapshot is the literal precedent for both
the per-crate manifest and the fixture placement.

| Pros | Cons |
|------|------|
| Extraction to ocx mono-repo = `git mv` + re-point one path dep (shapes already identical) | One-time conversion cost: ~16 `CARGO_MANIFEST_DIR` fixture call sites, `.licenserc.toml`, taskfile source globs, release tooling |
| Convention code exists exactly once from day one | Slightly deeper paths for daily work |
| Workspace lints/deps deduplicate the "copied exactly from ocx" dependency block | |
| `[patch.crates-io]` stays at workspace root — semantics unchanged | |

### Option 2: Root package stays; add `crates/ocx_python` as second member

**Description:** Keep the binary crate at the repo root, add
`members = ["crates/ocx_python"]` to the existing `[workspace]` table.

| Pros | Cons |
|------|------|
| Minimal diff; no fixture/tooling churn now | Asymmetric layout diverges from the ocx mono-repo shape — extraction later still forces the full conversion, plus unwinding the asymmetry |
| | Root package + member mix makes workspace-level lints/deps sharing awkward (root `[lints.rust]` is package-scoped by design) |
| | Two manifest styles to maintain |

### Option 3: `ocx_python` module inside the binary crate (extract later)

**Description:** Implement as `src/python/` module with a clean internal
boundary; physically extract when ocx-dist starts.

| Pros | Cons |
|------|------|
| Zero structural cost today (the default ponytail answer) | User-stated requirement: split from day one to prevent foreseeable migration cost |
| | Boundary erosion risk: nothing stops `use crate::pipeline::…` imports from creeping in; only a crate boundary is compiler-enforced |
| | Extraction later = the same workspace conversion anyway, plus a module→crate refactor |

## Decision Outcome

**Chosen Option:** Option 1 — full workspace conversion.

**Rationale:** The second consumer is a stated roadmap fact, the conventions
must have exactly one implementation, and the vendored
`external/ocx/crates/ocx_mirror/` snapshot proves the target shape works for
this exact code. Option 3's deferral value is voided by the explicit
requirement; Option 2 pays most of Option 1's future cost anyway while
adding asymmetry. The conversion friction is fully enumerated (see
Implementation Plan) — it is a bounded, one-commit mechanical cost.

### Consequences

**Positive:**
- Compiler-enforced translation/I-O boundary from day one.
- Extraction = move crate directory + switch `ocx_lib` path dep; no API
  rework, no fixture rework (fixtures live under the crate).
- Workspace `[workspace.dependencies]` replaces the hand-synced "copied
  exactly from ocx" comment block with one canonical table.

**Negative:**
- One-time mechanical conversion commit (must be its own `refactor:` commit,
  Two Hats — no behavior change mixed in).
- `task release:prepare` needs a versioning decision (below).

**Risks:**
- *Silent CI green on stale globs*: `.licenserc.toml` and
  `taskfiles/rust.taskfile.yml` `sources:` globs match zero files post-move
  without erroring — must be updated in the same commit and verified by
  touching a file in each crate.
- *uv crate churn*: git-pinned `uv-*` parser crates break API on bumps —
  same mitigation as the ocx submodule: pin to rev, documented bump
  procedure, never a floating range.
- *Platform-axis encoding is a one-way-door on the registry*: v1 encodes
  libc/ABI as mirror variant tag prefixes (existing `VariantSpec` +
  per-variant cascade); the upstream `+libc.gnu` platform-grammar MR moves
  the axis into platform keys/index entries (v2). Migration = republish.
  L1 wheel-tag→facts mapping is frozen in `ocx_python`; only the L2
  encoding versions. Coordinate the MR with the conventions ADR before
  either lands (design spec, "Platform & axis model").
- *Runtime writes vs read-only hardlink CAS*: CPython `__pycache__`
  writes (universal) and JIT caches (`numba`) target package content —
  mitigated by `PYTHONDONTWRITEBYTECODE=1` in composed env metadata (v1);
  pre-baked PEP 552 hash-based pyc is the v2 option. Residual cases
  documented as limitations.
- *CI platform-leg gaps*: container legs are rejected by the renderer
  today (capability deferred) → musllinux variant untestable and glibc
  floors not re-verified (wheel tag treated as upstream's compatibility
  promise; smoke tests only). Mitigation in scope: container-leg revival
  is a planned finalization-stage work item (design spec, "Implementation
  environment & rollout"); the limitation holds only until then.
- *Prepare-phase multi-asset friction*: `VersionInfo.assets` carries one
  URL per platform; a wheel set is N per platform — plan-schema extension
  or lock-as-single-asset fetch, decided at /swarm-plan (design spec,
  "Mirror integration").

## Technical Details

### Architecture (dependency direction)

```
crates/ocx_python  ──►  ocx_lib (path: external/ocx/crates/ocx_lib)
      ▲                  types only: package::info::Info, package::metadata,
      │                  publisher::LayerRef/ArchiveMediaType, oci::Platform,
      │                  layer-layout spec (arrives with submodule ≥ v0.4.1)
      │                  NEVER: publisher::Publisher (registry I/O)
crates/ocx_mirror  ──►  ocx_python + ocx_lib (incl. Publisher)
future ocx-dist    ──►  ocx_python + ocx_lib (its own I/O)
```

Rules:
1. `ocx_python` performs **no registry I/O** — no `Publisher`, no
   `ClientBuilder`. Filesystem I/O (reading wheels, writing repacked layers)
   is in scope; HTTP download is not (consumers own download, e.g. the
   mirror's `pipeline/download.rs` with resumption).
2. `ocx_python` owns a `thiserror`-derived `#[non_exhaustive]` error enum
   (lib crate per `quality-rust-errors.md`); `MirrorError` gains a wrapping
   variant with `#[source]`. `ocx_python` never imports `MirrorError` or
   `ocx_lib::cli::ExitCode`.
3. `uv-distribution-filename`, `uv-platform-tags`, `uv-pep508`, `uv-pep440`
   enter as git-pinned `[workspace.dependencies]` entries (rev-pinned, bump
   procedure documented in README next to the submodule procedure) —
   filename parse, tag compatibility, and marker evaluation respectively.
   On extraction they move to the ocx workspace table — noted so ocx-dist
   never pins different revs.
4. Versioning: both members inherit `[workspace.package] version`;
   `release:prepare` switches to `cargo set-version --workspace`.
   `ocx_python` gets an independent version only at extraction time.

### Prerequisite

Submodule bump to ocx ≥ v0.4.1 (per-layer prefix/strip + entrypoint args +
layer-layout types). Separate PR before any `ocx_python` work; follow the
README bump procedure including `[patch.crates-io]` + feature-list sync and
the `cargo tree -i oci-client` CI guard.

## Implementation Plan

Rollout constraints (dev.ocx.sh-only publishing, `ocx-contrib/mirror-pypi`
test repo, platform order, container-leg revival) are normative in the
design spec, "Implementation environment & rollout" — not duplicated here.

1. [ ] PR 1 — submodule bump to ≥ v0.4.1 (README procedure; verify layer
       layout + entrypoint args types are present).
2. [ ] PR 2 — workspace conversion (`refactor:`, no behavior change):
       - `git mv` src/, tests/ → `crates/ocx_mirror/{src,tests}`; root
         Cargo.toml → virtual manifest modeled on `external/ocx/Cargo.toml`;
         member manifest modeled on `external/ocx/crates/ocx_mirror/Cargo.toml`.
       - Same commit: `.licenserc.toml` globs → `crates/*/…`;
         `taskfiles/rust.taskfile.yml` `rust-sources` globs → `crates/*/…`;
         `release.yml` version grep → `cargo pkgid`/`cargo metadata`;
         `release:prepare` → `cargo set-version --workspace`.
       - Verify: `task verify` green; then touch one file per crate and
         confirm task cache invalidates (guards the silent-glob risk).
3. [ ] PR 3+ — `crates/ocx_python` skeleton + implementation per design
       spec (rollout sequencing per the spec section above).

## Validation

- [ ] `task verify` passes post-conversion; pytest harness untouched
      (binary resolution is env-var/`test/bin` based — verified).
- [ ] `cargo tree -i oci-client` still resolves to the fork (patch table at
      new workspace root).
- [ ] Extraction dry-run documented: diff `crates/ocx_python` manifest
      against ocx workspace expectations.

## Links

- [design_spec_ocx_python.md](./design_spec_ocx_python.md) — crate API + module map
- [research_python_wheel_oci.md](./research_python_wheel_oci.md) — feasibility research
- [adr_cli_namespace_restructure.md](./adr_cli_namespace_restructure.md) — CLI namespace this feature slots into
- Upstream ocx commits: `c26f362` (layer prefix), `048dcea` (entrypoint args)

---

## Changelog

| Date | Author | Change |
|------|--------|--------|
| 2026-07-04 | architect session | Initial draft |
