# ADR: `source.type: cargo` — a builder inside the mirror

<!--
Architecture Decision Record
Filename: artifacts/adr_cargo_source_kind.md
Owner: Architect (hex-architect, run hex-arch-cargo-20260917)
Handoff to: /hex-plan → /hex-execute
Format: MADR. Companion design: system_design_cargo_source_kind.md
-->

## Metadata

**Status:** Proposed
**Date:** 2026-09-17
**Deciders:** Michael Herwig
**GitHub Issue:** N/A
**Related Design Spec:** [`system_design_cargo_source_kind.md`](./system_design_cargo_source_kind.md)
**Stack Alignment:**
- [x] Decision fits existing stack (Rust 2024 + Tokio) and `.claude/rules/subsystem-mirror.md` conventions
- [x] Deviations named explicitly in *Boundary and convention violations*
**Domain Tags:** pipeline | spec | source | ci | security
**Supersedes:** N/A — extends `adr_ocx_mirror.md` › Future Adapters ("Cargo crates (binary crates with pre-built releases)" — that line assumed upstream binaries exist; this ADR covers crates that ship none)
**Superseded By:** N/A

### Classification (hex-architect Phase 3, confirmed)

| Axis | Value |
|---|---|
| Tier | xhigh |
| Blast radius | cross-area + external contract: `mirror.yml` spec surface, `pipeline plan/prepare/push` CLI; areas `spec/`, `source/`, `command/package/pipeline/{plan,prepare,push}`, `pipeline/{mirror_task,orchestrator,package}`, `generate/templates` + `generate/ci`, docs |
| Reversibility | **one-way**: the `cargo:` spec block, the `sh.ocx.mirror.*` annotation keys on published indexes, and the product claim shift. **Two-way**: `plan.json` v4 and the per-leg build facts — both run-scoped (`retention-days: 1`, `workflow.yml:60,92,156`). **Genuinely one-way and previously unnamed**: the actual `rustc -V` that built an immutable index survives only in those run-scoped artifacts — resolved by a durable annotation (S8, OQ1) |
| Novelty | first source kind that executes a toolchain; first per-OS `runs-on` on a **prepare** job (the test job already fans out per OS: `workflow.yml:102`) |
| Security touch | upstream build-time code execution (`build.rs`, proc-macros) on a runner |

## Context

crates.io is a source registry: it ships `.crate` tarballs and no binaries. Every consumer-facing Rust CLI binary today is built by someone else's CI (cargo-dist in the crate's own repo, Homebrew, conda-forge, nixpkgs, or the opportunistic cargo-quickinstall farm). A crate with no upstream binary release is therefore unmirrorable by every existing source kind — `github_release`/`url_index` need published files (`src/resolver.rs`), `pylock`/`pypi` need wheels.

`source.type: cargo` runs `cargo install <crate>@<version> --locked --index <index> --root <dir> --target <triple> [--features …] [--no-default-features] [--bin=…]` natively on a runner for each platform and packages `<root>/bin/*` as that platform's OCX layer. Versions come from the crates.io sparse index; six GitHub-hosted labels cover the six OCX targets; `push` stays the single credential-holding aggregator. Feature-set variants are phase 2.

**Product claim shift (explicit).** Every other kind publishes bytes an upstream produced and vouches only for transport integrity. A cargo mirror publishes bytes *this pipeline produced* from upstream source. The contract is "immutable once pushed + provenance-attested" (crate, version, `.crate` cksum, features, toolchain, actual rustc, ocx-mirror rev), **not** byte-reproducible (research_cargo_build_supply_chain § Finding 7). Docs and the catalog description must say "built by" not "mirrored from".

Settled by owner discussion 2026-09-16/17 (inputs, not options): builder-not-mirror; crates.io sparse index is version truth; expected binaries come from the crate's normalized `Cargo.toml` `[[bin]]` + `required-features` read *before* building; `.crates2.json` is a receipt only; features ride `variants:` (phase 2); build legs hold zero secrets; per-platform `prepare`; `push` consumes `bundle-{V}-{slug}.tar.xz` unchanged; pushed versions immutable; acceptance = one binary-less crate on all six targets via the rendered workflow *and* a hand-written non-GitHub job.

## Decision Drivers

- **One pasteable command per capability** (CLAUDE.md product principle) — a **gate**, not a criterion: `plan → prepare --platform P (per runner) → push` must work on GitLab/Jenkins/cron with no GitHub sugar.
- **Owner inputs** — a second gate: cargo-quickinstall is not an upstream.
- **Prepare-window invariant** (`subsystem-mirror.md` › Phase 1): extract → bin_scan → chmod → libc_lint → sidecar stays one non-resumable window; a build tree enters it exactly where an extracted archive does (`orchestrator.rs:711-737`).
- **Push is the sole cascade writer and sole credential holder**; build legs gain none.
- **Fail closed before spending build minutes**: lib-only crates, feature-filtered-to-zero bins, missing `Cargo.lock` are plan-time (65) refusals, never a 10-minute macOS build ending in "no binaries".
- **Cost**: macOS minutes are 10×, Windows 2× Linux (research_cargo_tooling § Finding 8).
- **YAGNI** (`quality-core.md`): no abstraction for one implementation; no sibling crate without a stated second consumer; v1 ships the smallest surface that meets the acceptance bar.

## Industry Context & Research

**Research artifacts:** [`research_cargo_competitive_landscape.md`](./research_cargo_competitive_landscape.md), [`research_cargo_build_supply_chain.md`](./research_cargo_build_supply_chain.md), [`research_cargo_tooling.md`](./research_cargo_tooling.md); execution baseline [`research_mirror_per_os_smoke.md`](./research_mirror_per_os_smoke.md).

**Trending approaches:** cargo-dist's in-repo six-triple `--locked` build matrix is the structural analogue; cargo-binstall's `repo release → quickinstall → compile` ladder is the canonical resolution order (a cargo mirror *is* the rung above "compile" for the crates it carries); SLSA L2 provenance via a secretless build job + isolated push job is the ecosystem's converged answer to unsandboxed `build.rs`; crates.io Trusted Publishing shows long-lived publish tokens are the recurring failure mode; `cargo-auditable` embeds a `.dep-v0` SBOM in the binary that `cargo-audit`/Trivy/Grype read — near-zero cost, complements SLSA (phase 4).

**Key insights driving the decision:**
1. `required-features` silently drops a `[[bin]]`; cargo errors only on zero bins, and only *after* compiling the dependency tree (rust-lang/cargo#8970, #10289) → read the manifest first.
2. `Cargo.lock` ships in every `.crate` since Cargo 1.84 (bin crates since 1.37) → `--locked` is a hard floor; a lock-less crate is a refusal, not a re-resolve.
3. `actions/upload-artifact` is not a cross-job tamper barrier (research_cargo_build_supply_chain § Finding 4); the class is pre-existing here (the `test` job runs upstream binaries before `push` reads the same store) → state the residual, do not sell a digest check as closing it.
4. All six targets have GA GitHub-hosted labels (`ubuntu-24.04`, `ubuntu-24.04-arm`, `macos-15-intel`, `macos-15`, `windows-2025`, `windows-11-arm`; the last migrates to a VS2026 image from 2026-09-21) → no cross-compiler in the happy path. `actions/attest-build-provenance` is free on public repos only (Enterprise Cloud for private).
5. Homebrew `rebuild:` / conda `build number` exist because "same upstream version, new artifact" is a real axis. OCX already stamps every push `+{timestamp}` (`adr_ocx_mirror.md` › "Always push X.Y.Z+{timestamp}"), so the *tag* side of a rebuild counter already exists (phase 3).
6. rustup precedence: a `+<toolchain>` argv override outranks any `rust-toolchain.toml` inside the crate — the spec's pin wins, deterministically.

## Considered Options

### Option A: In-repo `Source::Cargo` + `MirrorTask` acquire/build split + `prepare --platform` (chosen)

`plan` reads the sparse index and the `.crate` manifest, emits a build-shaped plan entry; `prepare --platform P --plan plan.json` runs `cargo install` on the leg's runner and feeds `<root>` into the existing bundle window; `push --plan` adds a completeness check and a manifest digest check.

| Pros | Cons |
|------|------|
| One tool, one spec, one pipeline; every existing invariant (window, cascade, JUnit gating, `platforms.<p>.exclude`) reused verbatim | First subprocess the pipeline spawns that runs *upstream* code (generators run operator code; `uv`/`ocx` run trusted tools) |
| Per-platform `prepare` is a general improvement (a GitLab operator can split archive prepare across runners too) | `MirrorTask` refactor touches every construction site (`sync.rs`, `prepare.rs` ×2, tests); longest time to first build |
| Provenance lands on the same index annotations and `push --sign` leg every other kind uses | Renderer grows a second prepare-job shape |

### Option B: Sibling cargo-dist repo emits GitHub Releases; mirror consumes via `github_release` untouched

| Pros | Cons |
|------|------|
| Zero mirror change; the "mirror" claim stays literally true; `actions/attest-build-provenance` comes free with cargo-dist | The *build* is reachable only as a GitHub workflow in a second repo — fails the one-command gate; a second credential holder (release token) sits next to `build.rs` |
| Fastest time to first build — a **viable interim** while A ships | Versions/variants/exclusions duplicated across two specs; cross-repo naming is a one-way door of its own (`adr_ocx_python_crate.md` › Context) |

### Option C: Minimal mirror change — a `local` (`file://`) source; the template does the build

| Pros | Cons |
|------|------|
| Smallest Rust diff (a directory-shaped `Acquire`); `local` reusable for other hand-built trees | Build logic (manifest pre-read, `required-features`, triple table, failure classes) lives in **rendered bash** — untestable in `tests/golden/`, and a second dialect for non-GitHub operators |
| | Plan cannot fail closed (no pre-read in Rust) → macOS minutes burned on lib-only crates |

### Option D: Consume cargo-quickinstall as an upstream (rejected by owner input)

| Pros | Cons |
|------|------|
| No build minutes; consumable via `github_release` today | No SLA, "glorified bash script" by its own README, non-default features unsupported, single long-lived minisign key, no provenance predicate (research_cargo_competitive_landscape § Finding 2) |

### Trade-off matrix

Gates first: **G1** one pasteable command for any CI (B fails — the build step is a GitHub workflow in another repo); **G2** owner input "quickinstall is not an upstream" (D fails). Survivors are ranked by the weighted rows below; gate rows are shown, not scored. Scores 1–5.

| Criterion (w) | A in-repo | B sibling farm | C local + bash | D quickinstall |
|---|---|---|---|---|
| G1 one command, any CI | pass | **fail** | pass | pass |
| G2 owner inputs | pass | pass | pass | **fail** |
| Security posture (5) | 4 | 4 | 3 | 1 |
| Invariant reuse (4) | 5 | 5 | 3 | 5 |
| Operator UX: fail-closed, per-platform red (4) | 4 | 2 | 2 | 2 |
| Implementation cost (3) | 2 | 4 | 4 | 5 |
| Blast radius on existing kinds (3) | 2 | 5 | 3 | 5 |
| Build-minute cost (2) | 3 | 3 | 3 | 5 |
| Time to first build (2) | 2 | 5 | 4 | 5 |
| **Weighted (all rows)** | **78** | 91 | 70 | 83 |
| Reversibility | one-way (spec block, annotation keys, claim) | two-way for the mirror; one-way for the farm's naming | one-way (`local` is a permanent kind) | two-way |
| Main risk | build-leg code execution (accepted by ruling) | second credential holder running `build.rs` | untestable build logic in YAML | upstream disappears |

**Honest margin statement:** on the scored rows alone B (91) and D (83) beat A (78); both are excluded by gates, not by score. Among survivors A beats C 78 : 70, driven by fail-closed planning and invariant reuse against C's lower cost. The win is principle-driven — if the one-command gate were relaxed, B is the cheaper answer and remains a viable interim (see Decision Outcome).

## Decision Outcome

**Chosen Option:** A. **Viable interim:** B — a cargo-dist repo publishing GitHub Releases consumed through `github_release` needs zero mirror change and can carry a crate today while A ships; the owner decides whether to stand one up (handoff item).

**Rationale:** Only A satisfies both gates and the ruling at once: the build is a Rust-owned, testable step reachable as `ocx-mirror package pipeline prepare --platform P`, the build legs carry nothing, and every existing pipeline invariant is reused rather than re-implemented. C moves the build logic into YAML and out of the test suite.

### Sub-decisions

| # | Decision | Chosen | Rejected | Why |
|---|---|---|---|---|
| S1 | Cargo logic location | `src/source/cargo.rs` (index + `.crate` pre-read) and `src/pipeline/cargo_build.rs` (subprocess) inside the binary crate | `crates/ocx_cargo` sibling | `adr_ocx_python_crate.md` chose a crate because ocx-dist was a *stated* second consumer of one-way-door registry conventions. No second consumer of a cargo builder exists; the on-registry shape is the ordinary bundle layer. YAGNI; extraction later is a module→crate move with no wire contract |
| S2 | `MirrorTask` seam | Replace `download_url`/`asset_name`/`asset_type`/`verify_config` (`mirror_task.rs:28-29,41,44`) with one enum field `acquire: Acquire { Download { url, asset_name, asset_type, verify }, CargoBuild(CargoBuildSpec) }`; `prepare_task` matches on it for the acquisition phase only (`orchestrator.rs:657-706`), then joins the shared window at `:711`. The refactor commit changes **no output** (Two Hats) | Trait object (open set for two implementations, hides which semaphore an arm belongs to). Parallel task type + parallel `prepare_*_version` (the `WheelEnvTask`/`python_prepare.rs` precedent): duplicates JoinSet/manifest/resume and — worse — the window | Closed two-variant set; the window stays single-sourced |
| S3 | Plan carrier | `PlanVersionEntry.cargo: Option<PlanCargoEntry>` (`skip_serializing_if`, like `pylock` at `plan.rs:145-146`); `assets` stays empty; `schema_version` 3 → **4**. `--plan` is **required** for a cargo `prepare` and a cargo `push` (`SpecUsageError` 64 without it): the pre-read happens once, in `plan`, and the index is never consulted again | Overloading `PlanAssetEntry.url` with a `cargo://` URL: lies to `build_tasks_from_plan` (`prepare.rs:666`) and every URL-shaped consumer. Optional `--plan` with an index re-read fallback: a second pre-read path to keep correct | One crawl per run; a v3 plan fed to a cargo prepare fails with `PlanError` naming the schema |
| S4 | `prepare --platform` | New repeatable `--platform <key>` on `prepare`, **all kinds**: keeps only tasks whose full platform key is listed. A value that is not a key of `spec.platforms` → `SpecUsageError` 64 (bad argument value); a declared key absent from the plan entry → no-op with a warning. Absent flag = every platform (existing behaviour). Rendered for cargo only | Cargo-only flag; refusing it for other kinds | A filter is the smallest honest semantic and gives archive mirrors per-runner prepare for free |
| S5 | Feature-set variants | **Phase 2.** v1: top-level `cargo.features`/`no_default_features`/`bins` only; `variants:` is **rejected** for cargo exactly as for env sources (`spec.rs:470-517`); `VariantSpec` untouched. Phase-2 shape: `VariantSpec.assets` → `Option`, `features/no_default_features/bins` added, source-aware validation in `validate_variants` (`spec.rs:548-572`); the three `EffectiveVariant.assets` callers (`plan.rs:350`, `sync.rs:77`, `prepare.rs:746`) unwrap under the validation guarantee — the "illegal states unrepresentable" deviation named in `adr_mirror_source_generators.md` | Separate `cargo.variants:` list: forks name/default/tag-prefix/`latest`-reserved rules that exist to be shared | v1 meets the acceptance bar without variants; the one-way tag grammar `<name>-<version>` is decided now, implemented later |
| S6 | Test phase | **Unchanged in shape.** The build leg uploads `bundle-{V}-{slug}`; the existing `test` job (`workflow.yml:94-156`, `runs-on: ${{ matrix.runner }}`) downloads and runs `ocx package test` natively or in the declared containers; `push` gates by JUnit as today. One rendering fix for the cargo render: `test.if` gains `!cancelled()` (see C8) | Smoke test inline on the build leg: needs a second JUnit-emitting path and breaks the AND-across-containers contract (`push/verdict.rs`) for linux legs | Zero change to a contract three modules share; ~1 min × 6 legs |
| S7 | Rebuild policy | **Phase 3.** No new tag grammar: `cargo.rebuild: N` becomes an index annotation `sh.ocx.mirror.cargo.rebuild`; `plan` treats a published version whose annotation is `< N` as `New` again (fresh `+{timestamp}` stamp; cascade re-points rolling tags), windowed by `versions:`. Not in v1 `CargoConfig` — `deny_unknown_fields` refuses it until then | Rebuild number in the tag (`1.2.3-r2`): a second grammar; conflicts with the variant prefix. Immutable-forever: leaves a toolchain-CVE rebuild no path but a spec-name change | The timestamp stamp already *is* the distinct artifact identity; only the trigger is missing |
| S8 | Provenance | v1: index annotations `sh.ocx.mirror.cargo.{crate,version,cksum,features,no-default-features,toolchain,rustc}` + `sh.ocx.mirror.rev`, values from `plan.json` (`rustc` from the per-leg build facts: written when every pushed leg of the version agrees, else omitted with a warning until OQ1's per-platform seat exists). Mirror-owned keys under the `sh.ocx.mirror.` prefix (upstream precedent `sh.ocx.provenance`; ownership of the prefix is a handoff item). SLSA v1 referrer on `push --sign` + `cargo-auditable`: **phase 4** | Per-platform index annotations: `ocx package push --annotation` writes the *index*, last key wins | Honest about what the index carries today; the durable per-platform `rustc` needs a manifest-level seat (OQ1) |
| S9 | Sync/check | `package check` **works** for cargo (index list + tag scan, no build — the dry run before spending macOS minutes); `package sync` refuses cargo with `SpecUsageError` 64 | Host-only build in sync | The pipeline is the product path; check is free |
| S10 | `--locked` | Always; no spec knob. A `.crate` without `Cargo.lock` is a plan-time refusal (65) with the remedy "raise `versions.min`" | `locked: false` opt-out | Every packaging guideline treats a re-resolve as a bug; the lock pins the transitive set that will *execute* on the leg |
| S11 | Planned = built | argv carries `--index <source.index>` unconditionally; after the build the leg hashes the fetched `.crate` in `$CARGO_HOME/registry/cache/*/<crate>-<ver>.crate` and requires `== plan.cksum`, else `ExecutionFailed` | Trust cargo's default registry | Without `--index`, `plan`'s cksum describes bytes that were never built |
| S12 | Child environment | `cargo_build` scrubs the child env with a **deny-list** (`spec/prescan.rs::CREDENTIAL_DENY_LIST` keys + `GH_TOKEN`, `GITHUB_TOKEN`, `CARGO_REGISTRIES_*_TOKEN`) | Allow-list | Windows cargo needs its environment; the leg is trusted, the deny-list is for the cron box / GitLab runner where the operator may have exported the push secret |

### Quantified Impact (v1)

| Metric | Archive kind | Cargo kind | Notes |
|---|---|---|---|
| Prepare wall time per version | ~1 min, one job | max over six legs: 2–15 min (Windows/macOS ≈ 2× Linux) | legs run in parallel; `new_per_run` bounds fan-out |
| Runner cost per version (private repo, 2026 rates) | ≈ $0.02 | ≈ $0.65 build + $0.16 test ≈ **$0.8** | macOS dominates; public repos free |
| 20-version backfill | ≈ $0.4 | ≈ $16 | recommend `versions.min` + `new_per_run: 2` |
| Plan-time cost | tags + one API crawl | + one `.crate` download (0.1–2 MB) per **new** version | cksum-verified in memory |
| Existing golden renders | — | **byte-identical**; one new golden `tests/golden/mirror-cargo.txt` | renderer branches on `Source::Cargo` only; the step-1 refactor changes no output |
| Direct dependencies | — | `semver`, `tar`, `flate2`, `toml` — four lines copied exactly from ocx `[workspace.dependencies]`; already in `Cargo.lock` via `ocx_lib`, zero new lock entries | CLAUDE.md dependency model |

### Consequences

**Positive:** first mirrorable path for binary-less crates; per-platform `prepare` for all kinds; a provenance story stronger than quickinstall/binstall (research § Finding 8); `package check` as a free dry run.

**Negative:** the pipeline now executes upstream code on the prepare leg; macOS minutes; `MirrorTask` construction-site churn; `--plan` becomes mandatory for one kind (a CLI asymmetry between kinds).

**Risks:**
- Cross-leg artifact tampering within one run (research § Finding 4). Not closed by any in-store digest; bounded by the `plan_sha256` job-output check and by scope (same run). See Security and OQ2.
- Toolchain drift across legs when `toolchain: stable` and a Rust release lands mid-run. *Mitigation:* actual `rustc -V` recorded per leg; the index annotation is written only when legs agree; docs recommend a pin (OQ1).
- `x86_64-apple-darwin` demoted to Tier 2 with host tools; `macos-15-intel` retires 2027-08. *Mitigation:* `platforms.darwin/amd64` is opt-in like every key; the triple table is one function to edit.

## Technical Details — component contracts (v1)

### C1 `mirror.yml` surface

```yaml
name: cargo-nextest
target: { registry: ocx.sh, repository: cargo-nextest }
source:
  type: cargo
  crate: cargo-nextest            # optional, default: name. Grammar ^[A-Za-z0-9_-]{1,64}$, no leading '-'
  index: https://index.crates.io  # optional, default crates.io sparse index; https only, no userinfo, no query/fragment
cargo:                            # optional for cargo (every field defaults); rejected (65) for every other kind
  toolchain: "1.89.0"             # default "stable" — a rustup toolchain name; ^[A-Za-z0-9._-]{1,64}$
  features: [default-no-update]   # default []
  no_default_features: false
  bins: [cargo-nextest]           # optional subset of [[bin]]; absent = every bin the feature set satisfies
  timeout_seconds: 3600           # per cargo install
platforms:                        # unchanged schema; keys → target triple via C7
  linux/amd64:   { runner: ubuntu-24.04 }
  linux/amd64+libc.musl: { runner: ubuntu-24.04 }
  linux/arm64:   { runner: ubuntu-24.04-arm }
  darwin/amd64:  { runner: macos-15-intel }
  darwin/arm64:  { runner: macos-15 }
  windows/amd64: { runner: windows-2025, shell: pwsh }
  windows/arm64: { runner: windows-11-arm, shell: pwsh }
tests: [{ name: version, command: cargo-nextest --version }]
```

Validation: `Source::validate` gains a Cargo arm beside the Pypi arm (`spec/source.rs:245-262` — https-only, no userinfo, the precedent) for `index`; `MirrorSpec::validate` (`spec.rs:470-517`) gains a third arm keyed on a new `Source::is_build()` predicate beside `is_env()` (`spec/source.rs:120`). For cargo — `assets`, `asset_type`, `variants`, `wheels`, `python`, `verify`, `bin_scan` **rejected** (65, message names the field and why); `platforms:` required and every key must map through C7; `metadata:` optional. For every other kind — `cargo:` rejected. Fixtures: one file per rule under `tests/fixtures/invalid/cargo_*.yml`.

### C2 `Source::Cargo` and the index client (`src/source/cargo.rs`)

```rust
Source::Cargo { #[serde(default)] crate_name: Option<String>, #[serde(default)] index: Option<String> }
pub struct CrateName(String);   // constructed only through the grammar; `Display` is the sole way out
pub struct BinName(String);     // same grammar; the only type `--bin=` accepts
pub struct CrateVersion { vers: semver::Version, cksum: [u8; 32], yanked: bool,
                          rust_version: Option<String>, features: BTreeMap<String, Vec<String>> }
pub async fn list_versions(client, index, crate) -> Result<Vec<VersionInfo>, MirrorError>
```
Client: built exactly as `registry_sync/catalog.rs:185-215` — `redirect::Policy::none()`, `GuardedResolver` + `guard_destination` pre-flight, fetch ceiling — **not** the bare `http::client()` (`src/http.rs:115-130` is roots + connect timeout only). `GET {index}/{prefix}/{name}` (prefix rule `1/`, `2/`, `3/<c>/`, `<ab>/<cd>/`), newline-JSON, unknown fields ignored, `features2` merged; yanked lines dropped; `VersionInfo.assets` empty (env precedent, `source/pypi.rs`). Every `vers` parses as `semver::Version` and every name matches the grammar **before** storage. `cksum` must be 64 hex chars else 65. Classification: 404 from the index → `CargoError` (65); connect/5xx/timeout/malformed → `SourceError` (69).

Manifest pre-read (plan only, per new version, **in-process**: `tar` + `flate2` + `toml`, never `cargo metadata` — `discover` holds `GITHUB_TOKEN`, `workflow.yml:35`): read `config.json`'s `dl` (foreign: https, no userinfo, same host guard before dialling), `GET` the `.crate`, sha256 == `cksum` (mismatch → 65), extract `Cargo.toml` only, bounds on **decompressed** bytes (64 MiB), entry count (10 000), no path escape, exactly one `Cargo.toml` (duplicate → 65). Collect `[[bin]]` names (each validated as `BinName`, else 65) + `required-features`, `package.rust-version`, `Cargo.lock` presence. Expected bins = bins whose `required-features ⊆ enabled(features, default_features)` (transitive closure over the index `features` map). Refusals (all `CargoError` 65, before any build): zero `[[bin]]`; expected set empty; `bins:` names a bin outside the expected set; `Cargo.lock` absent; `rust-version` > the pinned `toolchain` (only when pinned to a version).

### C3 `plan.json` delta (schema_version 4)

```json
{ "schema_version": 4, "has_new": true, "has_drift": false, "target": "ocx.sh/cargo-nextest",
  "versions": [{ "version": "0.9.100_20260917T120000Z", "source_version": "0.9.100", "kind": "new",
    "variant": null, "platforms": ["linux/amd64", "darwin/arm64"], "assets": [],
    "cargo": { "crate": "cargo-nextest", "version": "0.9.100", "index": "https://index.crates.io",
      "cksum": "<64 hex>", "toolchain": "1.89.0", "features": [], "no_default_features": false,
      "bins": ["cargo-nextest"], "timeout_seconds": 3600,
      "targets": { "linux/amd64": "x86_64-unknown-linux-gnu", "darwin/arm64": "aarch64-apple-darwin" } } }] }
```
`targets` carries only this entry's `platforms` (full keys). Consumers: `prepare --plan` and `push --plan` (both must find `cargo`, else `PlanError` "regenerate with schema_version >= 4"); the rendered discover step's `jq` projection reads `version`/`platforms`/`kind` as today (`workflow.yml:51`) plus the new `legs` output (C8). `prepare` re-validates every plan field it puts in argv (`crate`, `version`, `cksum`, `bins`, `toolchain`, `index`, `targets`) with the C2 grammars — the plan crossed the artifact store.

### C4 `prepare` CLI and per-leg outputs

```
ocx-mirror package pipeline prepare --spec mirror.yml --version V --plan plan.json [--platform KEY]... [--work-dir D]
```
- `--platform` (all kinds): S4 semantics. Cargo without `--plan` → `SpecUsageError` 64.
- Cargo acquisition (`src/pipeline/cargo_build.rs`): argv = `cargo +<toolchain> install <crate>@<version> --locked --index <index> --root <task_dir>/content --target <triple> [--no-default-features] [--features a,b] [--bin=x]...` (every token a validated newtype or a plan value re-checked in C3); `CARGO_TARGET_DIR=<task_dir>/target`; child env = process env minus the S12 deny-list; `kill_on_drop`, timeout `timeout_seconds` — the `lock_derive.rs:318-352` shape. Runs under the CPU permit (`max_bundles`), never the download permit. Post-build, before the window: cksum bind (S11); `.crates2.json`/`.crates.toml` **deleted** from `content/`. Then `content/` enters the shared window unchanged: `bin_scan::resolve_binaries` in `Verify` mode against the synthesized/declared `binaries` claim = the pre-read expected bins (`orchestrator.rs:711`, `spec/bin_scan.rs:25-29`) — the **one** binaries check; chmod (`:722`), `libc_lint` (`:730`), sidecar (`:731`), `bundle` (`:737`). After the bundle, `cargo_build` removes `target/` and `content/` explicitly — the prepare path removes only `content/` (`orchestrator.rs:738`; `clean_task_dir` at `:906` runs on the `sync` path only).
- Metadata: `prepare_task` bails on `metadata_config == None` (`orchestrator.rs:630-632`) and `resolve_metadata` reads a file; a cargo task with no `metadata:` builds an in-memory `AuthoringMetadata::Bundle { version: 1, env: [PATH=${installPath}/bin], binaries: <expected> }` and bypasses `resolve_metadata` — the seam is a second constructor of `ExpectedMetadata`. With `metadata:` present the operator's file is read and `binaries` filled/verified.
- Outputs per task dir (`orchestrator.rs:577`): `bundle.tar.xz`, `metadata.json` (existing). `VersionManifest`'s `BundleEntry` (`orchestrator.rs:56-65`) gains `cargo: Option<BuildFacts { rustc: String /* `rustc +tc -V` */, target: String, runner: String /* label only, never hostname */, duration_s: u64 }>` with `#[serde(skip_serializing_if = "Option::is_none")]` (the `pylock` idiom, `plan.rs:145`) — `None` for every other kind, so `manifest.json` stays byte-identical there. The cargo prepare job's flatten copies the leg's `manifest.json` as `bundles/bundle-{V}-{slug}-manifest.json`; the shared archive/env flatten (`matrix.rs:600-635`) is untouched.
- Failure on the leg: spawn failure, timeout, non-zero `cargo` exit, cksum-bind mismatch → `ExecutionFailed` (1) with a ≤ 40-line stderr tail, ANSI/control bytes stripped (CWE-117). Not 65: the `lock_derive` 65 precedent covers operator-authored spec/lock content, whereas a compile failure is upstream's and per-platform — push reports `missing_bundle`, `platforms.<p>.exclude` is the operator's tool, and 65 would tell the operator to fix the spec. Each leg is one job with `fail-fast: false`.

### C5 `push` delta

```
ocx-mirror package pipeline push --spec … --bundles-dir … --junit-dir … --write-summary … [--plan plan.json [--plan-sha256 HEX]]
```
- `--plan` required for cargo (64 without); optional for other kinds. Completeness: every `(V, P)` the plan declares for a non-`metadata-drift` entry with no bundle → `PlatformFailure { reason: "missing_bundle" }`. Today such a pair is invisible (`push.rs:156-166` derives platforms from files present) and the version cascades short; with the flag a failed build leg reads as `Partial`, `any_red` is true, notify fires, the next run backfills via `backfill-partial` + `cascade_backfilled_entries`. Extending the completeness check to other kinds' rendered workflows is a **named follow-up** (regenerates every golden).
- `--plan-sha256`: when given, the plan file's digest must match, else `PlanError` 65. The rendered `discover` job — the one trusted job, finished before any upstream code runs — exports it as a job output; `push` passes it.
- Manifest check (cargo pairs): `sha256(bundle) == bundle-{V}-{slug}-manifest.json` entry else `PlatformFailure { reason: "manifest_mismatch" }`; entry or file absent → `manifest_missing`. Any failure withholds the version's cascade (`push.rs:234` unchanged).
- Annotations (cargo only): the S8 set via the existing `--annotation` tail (`pipeline/ocx_cli/push.rs:101`), values from `plan.json`; `rustc` from the manifests when all pushed legs agree. Seam: `annotations.rs:36-38` builds the map once per run; cargo values are per version, so `invoke_push` takes a per-version overlay merged over the run map.
- `run-summary.json`: `platforms_failed[].reason` gains `missing_bundle` (now reachable), `manifest_mismatch`, `manifest_missing`.

### C6 Error mapping

| Variant | Exit | When |
|---|---|---|
| `CargoError(String)` **new** | 65 DataError | 404 from the index; `.crate` cksum mismatch or non-hex cksum; lib-only crate; expected bins empty; `bins:` names an unknown bin; a `[[bin]]` name outside the grammar; `Cargo.lock` absent; `rust-version` above the pinned toolchain; malformed or duplicate `Cargo.toml` |
| `ExecutionFailed` | 1 | `cargo`/`rustup` not found, spawn error, timeout, non-zero build, cksum bind mismatch (per leg; never retried) |
| `SourceError` | 69 | index / `dl` host unreachable, 5xx, timeout, unparseable index body |
| `SpecUsageError` | 64 | `package sync` on a cargo spec; cargo `prepare`/`push` without `--plan`; `--platform` value not a `spec.platforms` key |
| `PlanError` | 65 | plan lacks `cargo` (schema < 4); `--plan-sha256` mismatch |

The 65/69 split follows `PypiError` vs `SourceError` (`subsystem-mirror.md` › Error Model): "absent/malformed" is data, "unknown" is availability.

### C7 Platform key → target triple (`spec/platform_keys.rs`, beside `platform_slug`)

| Key | Triple | Runner (docs default) |
|---|---|---|
| `linux/amd64`, `linux/amd64+libc.glibc` | `x86_64-unknown-linux-gnu` | `ubuntu-24.04` |
| `linux/amd64+libc.musl` | `x86_64-unknown-linux-musl` | `ubuntu-24.04` (+`musl-tools`, `rustup target add`) |
| `linux/arm64`, `+libc.glibc` / `+libc.musl` | `aarch64-unknown-linux-gnu` / `-musl` | `ubuntu-24.04-arm` |
| `darwin/amd64` | `x86_64-apple-darwin` | `macos-15-intel` |
| `darwin/arm64` | `aarch64-apple-darwin` | `macos-15` |
| `windows/amd64` | `x86_64-pc-windows-msvc` | `windows-2025` |
| `windows/arm64` | `aarch64-pc-windows-msvc` | `windows-11-arm` |

Anything else → `SpecInvalid` (65) at validate. Libc-less linux = gnu (cargo-dist default). `+libc.musl` publishes as `os.features` exactly as for archives and `libc_lint` verifies the built binary's `PT_INTERP` against it unchanged.

### C8 Renderer delta (`generate/ci.rs`, `ci/matrix.rs`, `templates/workflow.yml`)

For `Source::Cargo` only: (1) `discover` emits `legs` = flattened `[{version, platform, platform_slug, runner}]` built by `jq` from `plan.json` and renderer-baked `{platform → runner}` / `{platform → slug}` maps, plus `plan_sha256`; (2) the prepare job becomes `strategy.matrix.include: ${{ fromJson(needs.discover.outputs.legs) }}`, `runs-on: ${{ matrix.runner }}`, `permissions: { contents: read }`, `actions/checkout` with `persist-credentials: false`, matrix values passed through `env:` (never inline in `run:`), a toolchain step, then `prepare --version --plan --platform`, flatten (bundle, metadata, manifest), upload `bundle-{V}-{slug}`; (3) `permissions: contents: read` at workflow top and on `test`; (4) **`test.if: ${{ !cancelled() && needs.discover.outputs.has_new == 'true' }}`** — today's `if:` (`workflow.yml:96`) has no status function, so it is an implicit `success()` over `needs: [discover, prepare]`, and one red prepare leg would skip every test leg and starve push's JUnit gate for all platforms. Pre-existing for every kind (hidden by the per-version prepare matrix) — filed separately for the shared template; the cargo render carries the fix now because a 6× matrix makes it likely; (5) push downloads the `plan` artifact and passes `--plan plan.json --plan-sha256 ${{ needs.discover.outputs.plan_sha256 }}`. Notify untouched. Existing goldens byte-identical; new `tests/fixtures/mirror-cargo.yml` + `tests/golden/mirror-cargo.txt`.

## NFR coverage

| NFR | Coverage |
|---|---|
| Scalability | Fan-out is `versions × platforms` legs; `new_per_run` caps versions. No cache action in v1; `Swatinem/rust-cache` keyed on the crate name is the documented upgrade |
| Availability | Index/`dl` host down at plan → 69 before any build; down mid-build → that leg 1, others proceed, push records `missing_bundle`, next run backfills. Registry down at push → existing 75/69/83 ladder |
| Latency | Cold build 2–15 min per leg in parallel; plan adds one small download per new version |
| Security | Below — the ruling, named attackers, mandatory controls, stated residuals |
| Cost | ≈ $0.8/version private, $0 public; 20-version backfill ≈ $16 — surfaced in docs with `versions.min` / `new_per_run` |
| Operability | One platform red = one red matrix leg + a `missing_bundle` row in `run-summary.json` with `job_url` + a Discord row; `platforms.<p>.exclude` (`severity: broken`, `reason`) silences a known-bad `(V,P)`. Resume: the window is non-resumable as today; a leg re-run rebuilds from scratch; the `bundle_path.exists()` early return (`orchestrator.rs:636`) still applies to a persistent `work_dir` on a cron box |

### Security — ruling and controls

**Ruling (2026-09-17, extends the 2026-08-14 ruling in `security-threat-model.md`):** executing upstream `build.rs`/proc-macro code on a **secretless, ephemeral build leg** is inside the threat model as an accepted risk; the compensating control is the job split — build legs hold no registry, signing, announce or webhook credential; `push` alone does. `security-threat-model.md` gains a paragraph citing this ADR.

| Attacker | Path | Control |
|---|---|---|
| Malicious or compromised crate author / transitive dependency | arbitrary code in `build.rs`/proc-macro on the prepare leg | secretless leg: no `secrets.*` in the job, `permissions: contents: read`, `checkout persist-credentials: false` (a persisted token in `.git/config` is readable by `build.rs`); S12 env scrub for non-rendered runners; `--locked` pins the transitive set to what the author published; `.crates2.json`/`.crates.toml` stripped |
| Index / `dl` host / network (MITM, compromised index mirror) | crafted `name`/`vers`/`[[bin]]` reaching argv or a path; crafted `.crate`; an open redirect or an internal `dl` host | C2 grammars before storage and again in `prepare`; cksum verified on the pre-read and bound to the built `.crate` (S11); index client on the SSRF floor (`catalog.rs` shape: no redirects, guarded resolver, ceiling); `dl` guarded the same way; `index` https-only/no-userinfo at validate. **Egress note:** `cargo install`'s own fetch of `.crate`/deps bypasses `http::client()` entirely — it trusts cargo's TLS and the index cksums; the S11 bind is what ties it back to the plan |
| A sibling build leg in the same run (research § Finding 4) | overwrite another leg's `bundle-*`, the `plan` artifact, or `junit-*` before push reads them | **Residual, accepted (OQ2).** The manifest digest check binds bundle ↔ manifest (corruption/omission) and does **not** close this: the facts ride in the same store. The class is pre-existing for every kind — the `test` job runs upstream binaries before `push` reads the same store. JUnit is a functional gate, not a security control. One leg-authenticated channel is cheap and added: `discover`'s `plan_sha256` job output, verified by `push`. SLSA (phase 4) attests what `push` received |
| Upstream binary under test | reads `GH_TOKEN` in the test leg env (`workflow.yml:119-120`) | **pre-existing for every kind**, not widened; scoped by the job's read permissions. Noted, not addressed |

Nothing in `mirror.yml` names an environment variable (auth ladder rule, `auth.rs`); cargo output is logged with control bytes stripped, and the leg has nothing to leak.

## Boundary and convention violations introduced (named, not smoothed)

1. **First subprocess that runs upstream code.** `url_index.generator` runs operator code; `uv`/`ocx` are trusted tools. Covered by the ruling.
2. **First non-`ubuntu-latest` `runs-on` on a prepare job**; first job with `permissions:`/`persist-credentials` shaped for hostile code; `test.if` differs between the cargo render and the shared template until the standalone fix lands.
3. **`MirrorTask` refactor** touches every constructor (`sync.rs`, `prepare.rs:704-722`, tests) — a Two-Hats refactor commit *before* the feature commit, output-identical.
4. **`plan.json` schema bump to 4** — additive; docs move.
5. **`bin_scan:` becomes source-conditional** (rejected for cargo, forced `verify`), a second source kind overriding an operator knob.
6. **`--plan` is mandatory for one kind** (`prepare`, `push`) and optional for the others — a CLI asymmetry; `push` gains `--plan-sha256` and new `run-summary.json` reason strings.
7. **Annotations become per-version for one kind** (`annotations.rs:36-38` builds once per run) and `sh.ocx.mirror.*` is a new mirror-owned annotation namespace, cargo-only in v1.
8. **Four direct dependencies** (`semver`, `tar`, `flate2`, `toml`) added copy-exactly; no new lock entries.
9. **Product claim shift** in docs/catalog copy: "built by ocx-mirror from crates.io source", never "mirrored".
10. **Phase 2 (named now):** `VariantSpec.assets` required → `Option`, enforcement moving from the type to `validate`.

## Migration / rollout

Nothing existing breaks: `cargo:` is refused on old binaries' `deny_unknown_fields` (65, "unknown field"), old plans fail closed on a cargo prepare, existing goldens are byte-identical, `manifest.json` is unchanged for other kinds.

Docs: `docs/reference/mirror-yml.md` (new `source.type: cargo` + `cargo:` + C7 table + the claim shift + cost note + the `windows-11-arm` image note), `docs/reference/cli.md` (`prepare --platform`, `--plan` required for cargo, `push --plan`/`--plan-sha256`, plan v4, manifest sidecar, exit codes), `docs/getting-started.md` (the four-line GitLab job; the job never hands `prepare` the push secret), `CATALOG.md` wording for cargo packages.

Staged rollout: (1) unit — index parser, pre-read, feature closure, triple table, argv builder, validation fixtures, golden; (2) acceptance harness `test/` — a fixture crate served from a local sparse index + `dl` dir, `cargo` on PATH, all four commands on linux/amd64 (`test/tests/test_mirror_cargo.py`, the `test_mirror_pypi.py` shape); (3) one real crate with no upstream binaries to `dev.ocx.sh` across six targets via the rendered workflow, then the same via a hand-written GitLab job (`/e2e-test` tier 3); (4) later phases below.

## Open questions

- [NEEDS CLARIFICATION: OQ1 — toolchain truth: default `toolchain: stable` with the actual `rustc -V` recorded durably per platform in the OCX config annotation `sh.ocx.mirror.cargo.rustc`, or require an explicit pin?] Recommended: `stable` + durable actual-rustc annotation — an unpinned default matches `cargo install` users' expectation, and the annotation closes the H7 provenance loss; docs recommend pinning for reproducible re-cuts. Note: a per-platform durable seat needs `ocx package push` to write a manifest-level annotation (index annotations collide across platforms) — an ocx change on the critical path; until then v1 writes the index key only when legs agree.
- [NEEDS CLARIFICATION: OQ2 — cross-leg tampering residual (attacker = crate author's `build.rs` on leg N; reach = another leg's bundle / the plan artifact / junit in the shared store): accept permanently and say so, or restructure the rendered workflow to one static job per platform (≤ 8, versions sequential per job) so digests travel as leg-authenticated job outputs?] Recommended: accept for v1 with the `plan_sha256` job-output check — the class is pre-existing for every kind, the effect is bounded to bundles of the same run, and a static-job restructure costs the matrix's parallelism for a residual SLSA cannot close either (provenance signed by `push` attests what `push` received).
- [NEEDS CLARIFICATION: OQ3 — keep `windows/arm64` in the six-target acceptance bar?] Recommended: yes — native runner is GA (private repos too), `platforms.<p>.exclude` handles `ring`-class crate failures per version.

## Implementation Plan (phase-level; `/hex-plan` decomposes)

**v1**
1. [ ] Two-Hats refactor: `MirrorTask.acquire` enum — no output change, goldens and `manifest.json` byte-identical.
2. [ ] Spec: `Source::Cargo` + Cargo arm of `Source::validate`, `cargo:` block (`spec/cargo_config.rs`), `is_build()`, validation arm (variants rejected), invalid fixtures, `platform_keys::rust_target_triple`; four copy-exact deps.
3. [ ] `source/cargo.rs`: SSRF-floored index client, in-process `.crate` pre-read, feature closure, expected bins, `CrateName`/`BinName`; `CargoError`.
4. [ ] Plan: cargo entries, schema 4, `targets`; `prepare --platform` (all kinds) + `--plan` required for cargo; `pipeline/cargo_build.rs` (argv, `--index`, env scrub, cksum bind, cleanup); synthesized metadata; window join; `BuildFacts` in `manifest.json`.
5. [ ] Push: `--plan` required for cargo + completeness, `--plan-sha256`, manifest check, per-version annotation overlay; `sync` refusal, `check` support.
6. [ ] Renderer: cargo prepare matrix + toolchain steps, `legs`/`plan_sha256` outputs, `!cancelled()` test gate, workflow/test permissions, push plan download; golden.
7. [ ] Docs + `security-threat-model.md` ruling paragraph + `subsystem-mirror.md` module map rows; acceptance harness + dev.ocx.sh e2e (rendered + hand-written job).

**Phase 2** — feature-set variants (S5). **Phase 3** — `cargo.rebuild` (S7). **Phase 4** — SLSA provenance referrer on `push --sign` + `cargo-auditable` (S8); `push --plan` completeness for every kind.

## Validation

- [ ] Unit: every refusal in C2/C6 has a test; triple table total over the six keys ± libc; a property test that no argv token (`crate`, `version`, `bins`, `toolchain`, `index`, `target`) can be built from an unvalidated string.
- [ ] Golden: existing renders byte-identical; `mirror-cargo.txt` shows six `runs-on` values, `persist-credentials: false`, `!cancelled()` on `test`, and `grep -c 'secrets\.'` = 0 between `prepare:` and `test:`.
- [ ] Acceptance: four commands on a local index; a lib-only crate fails at plan with 65; a red leg yields `Partial` + `missing_bundle`; a tampered bundle yields `manifest_mismatch`; cargo `prepare` without `--plan` exits 64.
- [ ] Security review reads `security-threat-model.md` + this ADR's ruling paragraph first.
- [ ] `task verify` passes.

## Links

- [`adr_ocx_mirror.md`](./adr_ocx_mirror.md) — base pipeline, `+{timestamp}` tag decision
- [`adr_mirror_source_generators.md`](./adr_mirror_source_generators.md) — first operator-declared subprocess
- [`adr_ocx_python_crate.md`](./adr_ocx_python_crate.md) — sibling-crate precedent (and why it does not apply)
- [`adr_pypi_lock_derivation.md`](./adr_pypi_lock_derivation.md) — plan-time derivation shipped to prepare (S3 precedent)
- [`adr_mirror_signing.md`](./adr_mirror_signing.md) — the `push --sign` leg S8 defers provenance onto
- [`security-threat-model.md`](../rules/security-threat-model.md), [`subsystem-mirror.md`](../rules/subsystem-mirror.md)
- Cargo: [registry index](https://doc.rust-lang.org/cargo/reference/registry-index.html), [cargo-install](https://doc.rust-lang.org/cargo/commands/cargo-install.html), [rust-lang/cargo#8970](https://github.com/rust-lang/cargo/issues/8970), [#14815](https://github.com/rust-lang/cargo/pull/14815); [SLSA v1.2 levels](https://slsa.dev/spec/v1.2/levels); [cargo-auditable](https://github.com/rust-secure-code/cargo-auditable)

---

## Changelog

| Date | Author | Change |
|------|--------|--------|
| 2026-09-17 | Architect (hex-arch-cargo-20260917) | Initial draft |
| 2026-09-17 | Architect (hex-arch-cargo-20260917) | hex-architect Round-1 panel fixes applied (4 Block, 7 High, 17 Warn) |
| 2026-09-17 | Orchestrator (hex-arch-cargo-20260917) | Re-validation residuals applied (3 Warn, 3 Suggest): run-summary clause, `skip_serializing_if`, bare-hex `cksum` + re-validated list, wording |
