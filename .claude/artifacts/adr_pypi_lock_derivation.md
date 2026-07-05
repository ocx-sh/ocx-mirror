# ADR: PEP 751 lock derivation for `source.type: pypi` mirrors

<!--
Architecture Decision Record
Filename: artifacts/adr_pypi_lock_derivation.md
Owner: Architect (/architect)
Handoff to: /swarm-execute (via plan artifact)
Related Skills: architect, swarm-plan

Format: Based on MADR (Markdown Any Decision Records) - https://adr.github.io/madr/
One decision: WHERE/HOW/HOW-OFTEN a source.type: pypi mirror derives the PEP
751 lock a published PyPI release doesn't ship. Companion to
adr_ocx_python_crate.md (workspace/boundary) and adr_ocx_python_conventions.md
(the conventions the derived lock then feeds into).
-->

## Metadata

**Status:** Accepted
**Date:** 2026-07-05
**Deciders:** pylock-mirror swarm
**GitHub Issue:** N/A (`plan_python_mirror_v2` W3 deliverable)
**Related Design Spec:** [design_spec_ocx_python.md](./design_spec_ocx_python.md)
**Stack Alignment:**
- [x] Decision fits existing stack (Rust 2024 + Tokio, see CLAUDE.md) and conventions in `.claude/rules/subsystem-mirror.md`
**Domain Tags:** source | pipeline | spec
**Supersedes:** N/A
**Superseded By:** N/A

## Context

`source.type: pypi` (`plan_python_mirror_v2` decision A) mirrors a published PyPI
application at a pinned version without requiring the maintainer to hand-maintain
a `pylock.toml` — unlike `source.type: pylock`, which reads a maintainer-committed
lock directly (`source/pylock.rs`). A published PyPI release ships **no lock at
all** (verified: `adr_ocx_python_crate.md`, `research_python_wheel_oci.md` F9), so
something inside the mirror must produce one before `ocx_python::select_wheels`
has anything to select against.

The mirror already owns a two-phase pipeline (PLAN discovers work, PREPARE/PUSH
build and publish it) shared across every source type. Where lock derivation runs
in that pipeline, how many times it runs, and which interpreter it resolves
against are one-way-door decisions: get any of them wrong and either the same
version derives a different wheel set on different legs (drift), or the
maintainer's `python.interpreter_package` choice — the mirror's core
"impose nothing" contract (Convention 1, `adr_ocx_python_conventions.md`) — gets
silently overridden by a mirror-owned interpreter.

## Decision Drivers

- **Cross-leg determinism** — every platform/variant leg for one version must
  select against the *same* resolved lock; two independent `uv` runs for the
  same version are two chances to drift (upstream index state can move between
  them).
- **Cost** — `uv pip compile` is a real subprocess invocation (network +
  resolver); running it once per version beats running it once per
  (version × platform × variant) leg.
- **Maintainer sovereignty** — the mirror must not silently substitute its own
  interpreter, wheel filter, or naming choice for the maintainer's;
  `python.interpreter_package`, `variants[].wheel_priority`, and repo naming
  stay entirely maintainer configuration.
- **Auditability** — a derived lock is generated content, not committed source;
  it must be inspectable after the run (CI failure triage, "why did this version
  pick that wheel") without being retained forever.
- **Eliminate the manual burden** — the entire point of `source.type: pypi` is
  that the maintainer never hand-maintains a `pylock.toml`; any design that
  reintroduces manual lock upkeep defeats the feature.

## Considered Options

### Option 1: Derive once per version in the PLAN phase, shipped to PREPARE (chosen)

`pipeline plan` derives the PEP 751 lock for each newly discovered
`(package, version)` exactly once, using `uv pip compile --format pylock.toml`
against the Python the spec's `python:` block selects (interpreter-less
`--python-version` by default; the materialized `python.interpreter_package`
for `universal: false` — see Technical Details), and ships the result to every
PREPARE/PUSH leg alongside `plan.json`.

| Pros | Cons |
|------|------|
| One `uv` run per version — every platform/variant leg selects from the identical resolved lock, no cross-leg drift | PLAN phase is now non-trivially slower for a `pypi` source (a network resolve per newly discovered version) |
| Preserves the existing PLAN → (asset URLs resolved) → PREPARE contract (issue #160: one crawl per run) | Requires the interpreter package to be pullable at PLAN time — only when `universal: false`; the default derives interpreter-less (see Technical Details) |
| Derived lock is an inspectable, retained artifact, not a black box | |

### Option 2: Derive per-leg, inside PREPARE (rejected)

Each `(version, platform, variant)` PREPARE task shells its own
`uv pip compile` independently.

| Pros | Cons |
|------|------|
| No PLAN-phase change; PREPARE stays fully self-contained per leg | N× `uv` invocations per version (N = platforms × variants) — N times the resolver cost |
| | Two legs resolving the same version at slightly different times can see different upstream index state → **cross-leg drift**, the exact correctness bug this feature must avoid |
| | Nothing to persist for audit — a failure investigation can't inspect "the lock the run actually used" after the fact |

### Option 3: Maintainer-committed lock (rejected)

Require the maintainer to run `uv export`/`uv pip compile` themselves and commit
the resulting `pylock.toml`, same as `source.type: pylock` today.

| Pros | Cons |
|------|------|
| Zero mirror-side derivation code | This is exactly the manual maintenance burden `source.type: pypi` exists to eliminate — reduces to `source.type: pylock`, not a new capability |
| | Maintainer must re-derive and re-commit on every upstream release to keep tracking new versions |

## Decision Outcome

**Chosen Option:** Option 1. `pipeline plan` derives each newly discovered
version's lock exactly once, in-process, before any PREPARE leg starts
(`crates/ocx_mirror/src/pipeline/lock_derive.rs`,
`command/package/pipeline/plan.rs::build_pypi_plan_entries`).

### Consequences

**Positive:**

- One resolver run per version, shared identically by every leg — cross-leg
  drift is structurally impossible, not just avoided by care.
- The maintainer's `python.interpreter_package` is the *only* interpreter the
  derivation ever sees — no mirror-owned interpreter, ever (and the default
  universal mode sees none at all, `UvPython::Version`).
- The derived lock is a retained, inspectable artifact (`derived-locks`, 90-day
  retention) — a CI wheel-selection failure is diagnosable after the run, not
  just from live logs.

**Negative:**

- PLAN-phase runtime for a `pypi` source now includes a real `uv` resolve per
  newly discovered version (plus an `ocx package pull` for the interpreter when
  `universal: false`) — previously PLAN was pure metadata/API work for every
  source type.
- A derivation failure blocks every leg of that version (single point of
  failure) — accepted, because a leg silently proceeding on a *different* lock
  would be the strictly worse outcome (drift).

**Risks:**

- `uv`'s own resolution semantics can shift between mirror runs (upstream index
  state changes) even with one derivation per version — mitigated by
  `--exclude-newer` (future work, below), not yet wired.

## Technical Details

### Python selector: `UvPython::{Version, Interpreter}` — universal derives interpreter-less

Which Python `uv` resolves for is a two-mode selector
(`lock_derive.rs::UvPython`), resolved **once per plan/prepare run** by
`plan.rs::resolve_uv_python` and shared by every candidate version:

| Mode | Spec trigger | `uv` flag | Interpreter on disk |
|---|---|---|---|
| `Version(X.Y)` (**default**) | `python.lock.universal: true`, or `lock:` omitted | `--python-version X.Y` (from `python.version`) | **None** — zero `ocx package pull` in the plan phase |
| `Interpreter(path)` | `python.lock.universal: false` | `--python <path>` | The maintainer's exact `python.interpreter_package`, materialized via `ocx --format json package pull` (`materialize_interpreter` probes the pulled root for a `bin/python3` executable — a digest/tag reference alone is nothing `uv` can invoke) |

The interpreter-less default is not just a cost win (no registry pull in PLAN):
uv's interpreter inspection **cannot classify a fully-static python build** —
it fails with "Could not detect a glibc or a musl libc" (found live in the W4
pilot against the corpus's static interpreter, commit cc50eb4). Universal
resolution with `--python-version` never inspects an interpreter, so it is the
only derivation mode compatible with a fully-static
`python.interpreter_package`. `universal: false` keeps the exact-interpreter
resolution for maintainers who need it — and is documented as **structurally
incompatible with fully-static interpreters** for the same inspection reason.

Maintainer sovereignty holds in both modes: when derivation touches an
interpreter at all (`universal: false`), it is the exact package the maintainer
configured in the spec's `python:` block; the mirror never provisions an
interpreter of its own for resolution.

### `uv pip compile --format pylock.toml`, once per version

`derive_pylock` shells
`uv pip compile <package>==<version> --format pylock.toml` with the `UvPython`
selector flag above (`--python-version X.Y` by default, `--python <path>` for
`universal: false`; plus the remaining `LockOptions`: `extras`, `exclude`,
`timeout_seconds` — `python.lock` in the spec), writes the raw output, then:

1. **Relaxes `requires-python`** (uv#15995 workaround, below).
2. **Stamps a provenance header** — a TOML comment block (`package`, `version`,
   `generated_at`) prepended to the lock body; TOML comments are valid at the
   start of any document, so this composes cleanly without touching the lock's
   own content.
3. **Fail-closed re-parses** the final bytes through `ocx_python::parse_pylock`
   before trusting them — a derived lock this crate cannot parse back is never
   handed to PREPARE as if it were trustworthy.

### uv#15995 workaround: patch-floor relaxation

`uv pip compile` emits an overly strict `requires-python = ">=X.Y.Z"` floor
(patch-pinned) that rejects an otherwise-compatible interpreter downstream —
[uv#15995](https://github.com/astral-sh/uv/issues/15995). `relax_requires_python`
rewrites only `>=X.Y.Z` clauses down to `>=X.Y` via a targeted regex
(`>=(\d+\.\d+)\.\d+` → `>=$1`); every other operator (`==`, `<`, ranges) is left
untouched. `relax_requires_python_in_lock` applies this to the single top-level
`requires-python` line via a line-based rewrite — not a full TOML
parse/reserialize — so uv's own formatting and comments in the rest of the file
survive byte-for-byte.

**Removal criterion**: this workaround is removed the release uv ships a fix for
#15995 — a one-line diff at `relax_requires_python`'s call site, not a design
dependency.

### Persistence: three tiers

| Tier | Location | Retention | Purpose |
|---|---|---|---|
| PLAN artifact | `locks/` alongside `plan.json` in the `plan` GHA artifact | 1 day | PREPARE/PUSH legs consume the already-derived lock — no leg re-derives, matching the existing "one crawl per run" contract (issue #160) |
| Audit artifact | `derived-locks` GHA artifact, `path: locks/` | 90 days | Outlives the 1-day plan artifact so a CI failure investigation can inspect "the lock the run actually used" after the plan artifact has expired (`if-no-files-found: ignore` — a no-new-work run leaves it empty, not an error) |
| In-lock provenance | TOML comment header prepended to the lock body | Lives as long as the lock does | `package`/`version`/`generated_at` — identifies *which run* produced *this exact* lock even if it is later found detached from both GHA artifacts (e.g. copied out for local debugging) |

**Not yet done — documented future work**: OCI referrer attachment (publishing
the derived lock as a referrer/attestation on the pushed package manifest) would
give the lock a permanent, registry-native home instead of relying on GHA
artifact retention. Deferred — the three-tier scheme above is sufficient for the
current audit need and adds no new registry-side contract.

### Corpus interpreter model (V1c-verified)

The reference corpus (`pycowsay`, `yt-dlp`, `black`) uses astral
python-build-standalone's fully-static interpreter flavor
(`cpython-<ver>+<tag>-x86_64-unknown-linux-musl-noopt+static-full.tar.zst`) as
`python.interpreter_package`, paired with `variants[].wheel_priority: ["any"]`.
Verified directly (`.claude/state/plans/notes_v1v2v3_python_mirror_v2.md`, V1c —
the primary evidence record for this ADR; gitignored under `.claude/state/` so it
is cited by path rather than linked):

- **Genuinely static**: `readelf -d` shows no dynamic section, no `PT_INTERP`
  program header — confirmed against python-build-standalone's actual
  `+static-full` asset (V1a/V1b had tested the *wrong* asset flavor,
  `-install_only`, which is dynamically linked against musl's own libc/loader
  and cannot even start on a glibc host; V1c supersedes that with the corrected
  asset).
- **Runs unmodified on both `debian:12` (glibc) and `alpine:3.20` (musl)** —
  stdlib C-extensions (`ssl`, `sqlite3`, `zlib`, `ctypes`) are compiled *into*
  the interpreter binary itself, not `dlopen`'d, so they work on both legs
  identically.
- **Structurally cannot `dlopen` a shared-object C-extension wheel**
  (`Py_ENABLE_SHARED=0`, confirmed via `sysconfig`; a musllinux C-extension
  import fails with `ImportError: Dynamic loading not supported`) — this is not
  a bug to work around, it is the reason `wheel_priority: ["any"]` is
  **mandatory** for this interpreter choice: without it, tag-priority alone
  would pick a compiled musllinux/manylinux wheel over a pure one, and that
  wheel would fail to import at runtime on this interpreter.

Consequence: `wheel_priority: ["any"]` + the fully-static interpreter together
produce **one bare-named package that works unmodified on both debian and
alpine** — no libc-suffixed variant name, no separate builds. C-extension
applications (`streamlit`, `google-cloud-aiplatform`) instead pin a dynamic gnu
interpreter by maintainer choice, and are debian-only by that same choice (V2:
their transitive dependency trees are manylinux-only for a meaningful fraction
of packages — 8/42 for `streamlit`, 29/140 for `aiplatform` — so an alpine leg
would need per-app wheel-availability decisions this ADR does not make on the
maintainer's behalf).

### Principle: the mirror imposes nothing

Quoting the plan's locked decision H directly, because it is the governing
constraint every technical choice above is checked against:

> the mirror imposes NOTHING — interpreter choice, wheel selection
> (`wheel_priority` per platform/variant), and published naming are entirely
> maintainer configuration; no libc/variant naming semantics ever.

Concretely: `python.interpreter_package` is always the maintainer's free choice
(glibc or musl, static or dynamic); a maintainer may ship a gnu-only interpreter
and publish a bare name that simply requires glibc — that is their choice, not a
mirror-encoded constraint. The mirror never infers or injects libc/variant
semantics into a published name.

### Future work

- **`--exclude-newer <cutoff>`** — not yet wired. Pins `uv`'s resolution to
  upstream index state as of a fixed timestamp, making a re-derivation of the
  same version reproducible even after the upstream index has moved. A natural
  pairing with the provenance header's `generated_at` field.
- **OCI referrer attachment** — see Persistence, above.

## Validation

- [x] Unit tests cover the derivation mechanics: `relax_requires_python_*`,
  `derive_pylock_relaxes_stamps_and_reparses` (`lock_derive.rs`),
  `build_pypi_plan_entries` locks-dir behavior (`plan.rs`).
- [x] Interpreter/corpus claims verified against real artifacts, not asserted:
  `notes_v1v2v3_python_mirror_v2.md` V1c (static-build verification on both
  debian:12 and alpine:3.20 containers) and V2 (per-app musllinux/pure wheel
  coverage from the committed reference locks).
- [ ] Acceptance corpus exercises `source.type: pypi` end-to-end against a stub
  `uv`/fake PyPI index (W3, tracked in `plan_python_mirror_v2.md`).

## Links

- [design_spec_ocx_python.md](./design_spec_ocx_python.md) — component design
  spec (lock-derivation section, platform mapping)
- [adr_ocx_python_crate.md](./adr_ocx_python_crate.md) — the workspace/boundary
  decision; establishes "published wheels ship no lock"
- [adr_ocx_python_conventions.md](./adr_ocx_python_conventions.md) — the
  conventions this derivation feeds into (Convention 4's entrypoint-selection
  revision references the same corpus/verification work)
- [research_python_wheel_oci.md](./research_python_wheel_oci.md) — background
  research (F9: no lock on published wheels)
- `.claude/state/plans/notes_v1v2v3_python_mirror_v2.md` — primary evidence
  record (V1a/V1b/V1c/V2/V3 runtime verifications); gitignored, cited by path
- `.claude/state/plans/plan_python_mirror_v2.md` — locked decisions A–H, wave
  status

---

## Changelog

| Date | Author | Change |
|------|--------|--------|
| 2026-07-05 | pylock-mirror swarm (w3-adr) | Initial draft — PEP 751 lock derivation decision, corpus interpreter model, persistence tiers |
| 2026-07-05 | pylock-mirror swarm (w3-adr) | `UvPython` selector split (cc50eb4) — universal (default) derives interpreter-less via `--python-version` (uv inspection cannot classify fully-static builds: "Could not detect a glibc or a musl libc", live W4); `ocx package pull` materialization only for `universal: false`, structurally incompatible with fully-static interpreters |
