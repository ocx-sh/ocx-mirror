# ADR: `source.type: cargo` — a builder inside the mirror

<!--
Architecture Decision Record
Filename: artifacts/adr_cargo_source_kind.md
Owner: Architect (hex-architect, run hex-arch-cargo-20260917; revised 2026-11-08)
Handoff to: /hex-plan → /hex-execute
Format: MADR. Companion design: system_design_cargo_source_kind.md
-->

> **Revision 2026-11-08 (still Proposed, never implemented).** Inputs: dossier
> `.agents/discussions/cargo-native-build.md` (ratified 2026-11-08) and
> `research_ci_neutral_build_job_contracts.md`.
>
> **Pass 1, the architect revision.**
> 1. Stale references are fixed. Paths point at the post-split layout (`8be562e`,
>    `crates/crate_map.toml`), and the `plan.json` bump is v4 → v5 (`PLAN_SCHEMA_VERSION`,
>    `src/command/package/pipeline/plan.rs:67`). Claims were rechecked against code: prepare's
>    window runs through `ocx package create` (`45ac783`), `src/http.rs` is gone,
>    `ocx_mirror_spec::bin_scan` holds only the `BinScanMode` enum, and exit 83 is retired
>    (`ced1025`).
> 2. Renderer neutrality is a first-class contract (S15). The fetch-leg vs build-leg axis is named.
> 3. The build runs `cargo install --path` over a `.crate` the mirror itself fetched and
>    digest-verified. The draft's unconditional `--index` is dropped, because it defeats a corporate
>    `[source.crates-io] replace-with`.
> 4. Every build job is one command: `prepare` provisions Rust through `rustup` (S13) and writes
>    the push-ready hand-off layout (`--bundles-dir`, S17).
> 5. Corporate cargo setup is inherited operator environment (S14), and the credential classes are
>    named (S20).
> 6. The transform tiers are named, not designed (S19).
> 7. The Option B interim is withdrawn (owner: it duplicates discovery), and `cargo.toolchain` is
>    renamed `cargo.channel`.
>
> **Pass 2, the Round 1 review panel** (spec, adversarial quality, security, SOTA research, codex).
> 1. **Decision 2 is re-decided after a rescore: A′, a `versions[].build` marker.** The rescore
>    removed the double-counted duplication rows and the unverified GitLab-limit row. It also added
>    a kind-neutral marker (A′) and a derived DAG (B′). `prepare[]` units (C) were derivable from
>    `versions[]` plus `legs`, so they are withdrawn.
> 2. **Decision 3 is re-decided: (iv), fetch then build offline.** The read credential reaches only
>    `cargo fetch`. The build child gets `--offline` and an environment with the token removed. The
>    vendor split (v) is scored and kept as the zero-secret follow-up.
> 3. **Decision 1 totals are corrected** to A 77, B 81, C 62, D 73.
> 4. **The config-isolation claim was false.** `cargo install --path` reads config starting at
>    `$PATH/.cargo/config.toml` and walking ancestors. So `unpack_crate` skips the package-root
>    `.cargo/` and `rust-toolchain*`, and `prepare` refuses (78) a work dir with an ancestor cargo
>    project (S11).
> 5. **Single-host isolation is a stated operator requirement** (S21). The cron reference keeps
>    going after a failed unit.
> 6. **Prepare is bounded.** The `.crate` fetch is shared with plan and capped at 50 MiB, with
>    host-gated credentials and an SSRF floor. Unpacking keeps regular files and directories only.
> 7. **Push is hardened.**
>    - Installed bins are compared with the planned bins.
>    - `build.json` carries a `request_sha256` that push recomputes from the plan.
>    - Index annotations use uniform-or-`mixed` semantics, because ocx merges them with `extend`.
>    - A missing bundles dir under `--plan` counts as zero bundles.
> 8. **Build kinds never resume.** `test.if` and the test loop's first-failure exit are fixed
>    first, for every kind.
> 9. **Smaller changes.**
>    - `legs.<p>.containers[].toolchain` is added to the plan.
>    - G1 is restated, with `pipeline test` deferred to the owner.
>    - `prepare` and `push` refuse unsupported `requires` values.
>    - The `cargo_toml` crate replaces hand-rolled bin inference.
>    - Same-OS cross builds are documented.
>    - YAGNI trims: `Option<CargoBuild>`, a slimmer `build.json`, no `manifest.json` merge, no
>      `cargo.env`.
>    - A missing `rustup` exits 78.

## Metadata

**Status:** Proposed
**Date:** 2026-09-17 (revised 2026-11-08)
**Deciders:** Michael Herwig
**GitHub Issue:** N/A
**Related Design Spec:** [`system_design_cargo_source_kind.md`](./system_design_cargo_source_kind.md)
**Stack Alignment:**
- [x] Decision fits existing stack (Rust 2024 + Tokio) and `.claude/rules/subsystem-mirror.md` conventions
- [x] Deviations named explicitly in *Boundary and convention violations*
**Domain Tags:** pipeline | spec | source | ci | security
**Supersedes:** N/A. This ADR extends `adr_ocx_mirror.md` › Future Adapters, which assumed cargo crates ship binaries upstream; this ADR covers crates that ship none.
**Superseded By:** N/A

### Classification (hex-architect Phase 3, confirmed)

| Axis | Value |
|---|---|
| Tier | xhigh |
| Blast radius | Cross-area changes plus external contracts. Surfaces: the `mirror.yml` spec; the `pipeline plan/prepare/push` CLI; **`plan.json` v5, a documented third-party contract**; `run-summary.json` reason strings. Crates: `ocx_mirror_spec`, `ocx_mirror_source`, `ocx_mirror_pipeline`, `ocx_mirror_http`, `ocx_mirror_error`. Root: `command/package/pipeline/{plan,prepare,push}` and `generate/{ci,templates}`. Docs. |
| Reversibility | **One-way:** <ul><li>the `cargo:` spec block</li><li>`plan.json`'s `versions[].build` with `requires` semantics, `versions[].cargo`, and `legs.<p>.containers[].toolchain` (renderers outside this repo freeze on these)</li><li>`build.json`'s shape and the `--bundles-dir` layout (both documented in `plan-json.md` as the hand-off contract)</li><li>`prepare --platform` and `--bundles-dir`</li><li>`push --plan` and `--plan-sha256`</li><li>the `sh.ocx.mirror.*` annotation keys and their `mixed` value on published indexes</li><li>the product claim shift</li></ul> **Two-way:** the GitHub render (regenerated by `verify-generated`), the work-dir internals, and `manifest.json` (no production reader; § C4). |
| Novelty | The first source kind that executes a toolchain. The first prepare job with a per-OS runner. The first `plan.json` field a consumer must refuse on unknown values. |
| Security touch | Upstream build-time code (`build.rs`, proc-macros) executes on a runner. That runner may hold `source-read` credentials in its job environment. |

## Context

crates.io is a source registry: it ships `.crate` tarballs and no binaries. Every consumer-facing Rust CLI binary today is built by someone else's CI: cargo-dist in the crate's own repo, Homebrew, conda-forge, nixpkgs, or the cargo-quickinstall farm. No existing source kind can mirror a crate with no upstream binary release. `github_release` and `url_index` need published files (`ocx_mirror_source::resolver`), and `pylock`/`pypi` need wheels.

`source.type: cargo` works in four steps:
1. Discover versions from a sparse index.
2. Fetch the `.crate` and verify it against the index's digest.
3. Fetch dependencies with the operator's cargo configuration.
4. Build offline with `cargo install --path … --locked --offline --target <triple>`, natively on a runner of each platform.

The resulting `<root>/bin/*` is packaged as that platform's OCX layer. `push` stays the single holder of registry-write credentials.

**Product claim shift (explicit).** Every other kind publishes bytes an upstream produced, and vouches only for transport integrity. A cargo mirror publishes bytes *this pipeline produced* from upstream source. The contract is "immutable once pushed, plus pipeline-claimed provenance": crate, version, `.crate` digest, features, channel, the actual rustc, and the ocx-mirror revision. The output is **not** byte-reproducible (research_cargo_build_supply_chain § Finding 7). Docs and the catalog say "built by", never "mirrored from".

**Renderer neutrality (owner, 2026-11-08).** GitLab instances differ too much for the mirror to render a GitLab pipeline. Another renderer must therefore render the *entire* pipeline, build legs included, from mirror output alone. That renderer might be a GitLab dynamic child pipeline, Jenkins, Buildkite or a cron script.
- `plan.json` is already declared that contract (`docs/reference/plan-json.md`), but it carries only test legs (`legs.<platform>`).
- Prepare is one job per version, on a hard-coded `ubuntu-latest` (`templates/workflow.yml:63-70`).
- The GitHub renderer does not read `legs`. It renders at authoring time, and `leg_parity.rs` pins the parity.
- Cargo is the first kind that needs one prepare per (version × platform), with an environment requirement.

**Settled inputs (not options).** These come from the owner on 2026-09-16/17 and still hold:
- builder-not-mirror;
- the crates.io sparse index is version truth;
- the expected binaries come from the normalized `Cargo.toml`, read *before* building;
- `.crates2.json` is a receipt only;
- features ride `variants:` (phase 2);
- per-platform `prepare`;
- `push` consumes `bundle-{V}-{slug}.tar.xz` unchanged;
- pushed versions are immutable;
- acceptance is one binary-less crate on all six targets via the rendered workflow, **and** via a hand-written non-GitHub job.

"Build legs hold zero secrets" is narrowed by S20 and OQ1.

From the owner on 2026-11-08:
- renderer neutrality;
- the spec states environment needs and the renderer maps them;
- native build per target, with no cross-compilation in v1;
- the tier seams are named only;
- one discovery system: the separate builder repo is dropped.

## Decision Drivers

- **G1, one pasteable command per job.** This is a gate. Every job this change *adds or alters* is one `ocx-mirror` invocation, runnable on GitLab, Jenkins or cron with no GitHub sugar. The test leg is unchanged rendered bash. That is a named gap (violation 11) and an owner decision (§ Deferred to owner); `pipeline test` is the cure. G1 is stated this way because the test leg already violated the AGENTS.md principle before this change.
- **G2, renderer neutrality.** This is a gate: `plan.json` plus `plan-json.md` suffice to render build → test → push.
- **G3, one discovery system.** This is a gate: `plan` decides *when*, whatever builds.
- **G4, owner inputs.** This is a gate: cargo-quickinstall is not an upstream.
- **Prepare-window invariant** (`subsystem-mirror.md` › Phase 1). A build tree enters the window exactly where an extracted archive does. That point is the chmod → `authoring-metadata.json` → `ocx package create` → `reject_empty_scan` sequence (`crates/ocx_mirror_pipeline/src/orchestrator.rs:893-937`).
- **Push is the sole cascade writer and the sole registry-write holder**, wherever the operator gives push its own job or user (S21).
- **Fail closed before spending build minutes.** Lib-only crates, bins filtered to zero and a missing `Cargo.lock` are refused at plan time (65).
- **Cost.** macOS minutes cost 10× and Windows 2× Linux (research_cargo_tooling § Finding 8).
- **YAGNI** (`quality-core.md`). Keep the frozen surface smallest: anything that lands in `plan.json` is a one-way door.

## Industry Context & Research

**Research artifacts:**
- [`research_cargo_competitive_landscape.md`](./research_cargo_competitive_landscape.md)
- [`research_cargo_build_supply_chain.md`](./research_cargo_build_supply_chain.md)
- [`research_cargo_tooling.md`](./research_cargo_tooling.md)
- [`research_cargo_native_build_ci_portability.md`](./research_cargo_native_build_ci_portability.md) (2026-11-02)
- [`research_ci_neutral_build_job_contracts.md`](./research_ci_neutral_build_job_contracts.md) (2026-11-08)
- [`research_slim_llvm_package_size.md`](./research_slim_llvm_package_size.md) (2026-11-08, tier-3 motivation)
- execution baseline: [`research_mirror_per_os_smoke.md`](./research_mirror_per_os_smoke.md)

**Trending approaches:**
- cargo-dist's six-triple `--locked` matrix is the structural analogue. Its `dist-manifest.json` is forge-neutral for artifacts, but its `ci` object is GitHub-only. That object is the anti-pattern to avoid.
- Generator-emits-jobs is the established dynamic shape: GitLab `include: artifact` child pipelines, `buildkite-agent pipeline upload`, nix-eval-jobs JSON lines.
- A secretless build plus a credentialed, digest-verifying push is the SLSA-converged answer to unsandboxed `build.rs`.
- Homebrew builds Rust formulae with `cargo install --locked --root … --path .`. Building from a fetched, verified source tree is the packager norm. `cargo fetch` followed by an `--offline` build is the standard vendoring split (Debian, nixpkgs `cargoDeps`).

**Key insights driving the decision:**
1. A `required-features` gate silently drops a `[[bin]]`. Cargo errors only on zero bins, and only after compiling the dependency tree (rust-lang/cargo#8970, #10289). So the manifest is read first.
2. `Cargo.lock` ships in every `.crate` since Cargo 1.84, and in bin crates since 1.37. So `--locked` is a hard floor.
3. Every proxy, CA and registry-mirror knob cargo has is env or `$CARGO_HOME/config.toml` (research_cargo_native_build_ci_portability). A registry `cargo install <crate>` ignores project-local config. **`cargo install --path P` does not**: discovery starts at `P/.cargo/config.toml` and walks every ancestor ([cargo-install › configuration discovery](https://doc.rust-lang.org/cargo/commands/cargo-install.html#configuration-discovery)). Parity with a registry install therefore needs the package-root `.cargo/` removed and a work dir with no ancestor cargo project (S11).
4. **`--index` and `replace-with` conflict.** An explicit `--index` names a registry source that `[source.crates-io] replace-with` does not redirect. Behind a firewall the draft's unconditional `--index` would dial crates.io and fail. Building `--path` leaves cargo's default registry, and therefore the operator's replacement, in charge of dependencies.
5. Self-managed GitLab has no hosted macOS or Windows runners: operators register their own. Native-per-target therefore excludes platforms an operator has no runner for. That does not make cross-compilation the default, because `ocx package test` needs a native runner anyway.
6. CI-neutral contracts converge on a flat job list with explicit edges. They fail loudly on unknown kinds or credential classes (research_ci_neutral_build_job_contracts). This ADR adopts the fail-loud valve. It does not adopt a job list: the jobs are derivable from `versions[]` and `legs`, so a list would freeze a second copy of them (Decision 2).
7. Hosted runners carry more than a clean consumer machine: the VC++ redistributable, Homebrew dylibs, glibc 2.39. A native build can go green on its own test leg and fail on a user's machine. cargo-dist defaults `msvc-crt-static = true` for this reason (S18).

## Considered Options

### Decision 1 — where the builder lives

| Option | Summary |
|---|---|
| **A** In-repo `Source::Cargo` plus per-platform `prepare` (chosen) | `plan` reads the index and pre-reads the `.crate`. `prepare --platform P` builds on the leg's runner and feeds the tree into the existing window. `push --plan` checks completeness and build records. |
| B Sibling cargo-dist repo publishing GitHub Releases | No mirror change; the build is a GitHub workflow elsewhere. |
| C `local` source, with the rendered template running the build | Smallest Rust diff; the build logic lives in rendered bash. |
| D cargo-quickinstall as an upstream | No build minutes. |

Gates come first. Rows below the gates score each option 1–5. Gate rows are shown, not scored.

| Criterion (w) | A | B | C | D |
|---|---|---|---|---|
| G1 one command per added job | pass | **fail** (the build is a GitHub workflow) | **fail** (the build is template bash) | pass |
| G2 renderer neutrality | pass | **fail** | **fail** (the recipe is absent from mirror output) | pass |
| G3 one discovery system | pass | **fail** (second spec, second discovery; owner 2026-11-08) | pass | pass |
| G4 owner inputs | pass | pass | pass | **fail** |
| Security posture (5) | 4 | 4 | 3 | 1 |
| Invariant reuse (4) | 5 | 5 | 3 | 5 |
| Operator UX: fail-closed, per-platform red (4) | 4 | 2 | 2 | 2 |
| Implementation cost (3) | 2 | 4 | 4 | 5 |
| Blast radius on existing kinds (3) | 3 | 5 | 3 | 5 |
| Build-minute cost (2) | 3 | 3 | 3 | 5 |
| **Weighted (/105)** | **77** | 81 | 62 | 73 |

**Margin statement.** Only B outscores A, at 81 to 77. B wins on implementation cost and blast radius, and it fails G1–G3. D (73) and C (62) score below A and also fail gates. A is the only option that passes every gate.

### Decision 2 — how `plan.json` describes build work (S15)

| Option | Shape |
|---|---|
| A Per-entry kind block plus a kind rule (2026-09-17 draft) | `versions[].cargo.targets {platform → triple}`. `plan-json.md` gains "an entry with `cargo` fans prepare out per platform on `legs.<p>.runner`". |
| **A′ Kind-neutral build marker (chosen)** | `versions[].build: {timeout_seconds, requires?}`. A documented rule says that an entry with `build` gets one prepare per element of its `platforms`, on `legs[p].runner`, with `--platform p`. Every other entry keeps v4's one-prepare-per-entry rule. Tier-3 recipes set the same marker. |
| B Flat `jobs[]` DAG (research recommendation) | `jobs: [{id, kind, phase, needs[], requires{leg,…}, inputs[], outputs[], credentials}]`, with tests per (V, P, C). The renderer emits one job per element and refuses an unknown `kind` or `credentials` value. |
| B′ Derived `jobs[]` with the shipped grouping | Like B, but emitted by the mirror from `versions[]` and `legs`, grouped as the GitHub render groups (prepare per unit, test per (P, C), one push). |
| C `prepare[]` units (pass-1 choice) | Top-level `prepare: [{version, platforms[], leg?, timeout_seconds?, requires?}]` for every kind. Tests and push keep the `versions[] × legs` derivation. |

Scores are 1–5. Duplication is counted once, in *Frozen surface*. The GitLab job-limit row was dropped from scoring because its numbers are unverified (research gap); it is noted below.

| Criterion (w) | A | A′ | B | B′ | C | Notes |
|---|---|---|---|---|---|---|
| Renderer completeness from file + doc (5) | 3 | 4 | 5 | 5 | 4 | A′/C leave fan-out, test and push to documented rules; B/B′ spell every job out |
| v4 rendering rules kept (4) | 4 | 5 | 2 | 2 | 3 | A′ adds one conditional rule; C replaces "one per entry"; B/B′ make the rules a parallel description |
| New build kinds and tiers need no renderer change (4) | 1 | 4 | 4 | 4 | 4 | A needs a rule per kind; a cross build needs a new `requires` value under every option |
| The shipped renderer consumes it at runtime (3) | 3 | 4 | 3 | 4 | 4 | A′: one `jq` projection over `versions[]` + `legs` |
| Semantic extensions fail loud (3) | 1 | 4 | 4 | 4 | 4 | valve: `build.requires` / `kind` / `requires` |
| Frozen surface (3) | 4 | 5 | 1 | 2 | 3 | A′: one optional object; C restates version and platforms per unit; B restates the test fan-out |
| Implementation cost (2) | 4 | 5 | 2 | 3 | 4 | |
| **Weighted (/120)** | 67 | **105** | 77 | 85 | 89 | |

**Honest trade-off.**
- **Why C lost.** C was carrying `version` and `platforms` that `versions[]` already holds. Only `leg`, `timeout_seconds` and `requires` were not derivable, and A′ keeps exactly those: `leg` is implied as `p`, and the other two sit under a marker.
- **Where B′ is right.** B′ is the complete contract once the pipeline grows a phase that the derivation rules cannot express, such as signing legs, per-platform push or a vendor/fetch job (Decision 3 (v)). A′ migrates to B′ additively, because the jobs derive from the same data.
- **The unscored GitLab note.** B lists tests per (V, P, C), so GitLab's per-job `needs` limit (50 by default, unverified) bites B first.

This is the biggest one-way door in the ADR, so it is carried as OQ2.

**What an old (v4) renderer does with a v5 cargo plan.** It ignores `build` and renders one prepare per entry with no `--platform`. `prepare` refuses that invocation for any entry carrying `build` (exit 64, with a message naming `versions[].build`, C4). The run fails at its first leg rather than building the wrong thing. For every existing kind, the v4 and v5 rules yield an identical job set.

### Decision 3 — credentials on a build leg (S20)

Scores are 1–5.

| Option | Corporate remotes usable (5) | Read-token exposure to upstream code (5) | Operator burden (3) | Implementation cost (2) | Weighted (/75) |
|---|---|---|---|---|---|
| (i) Zero-secret build legs, anonymous cargo remotes only | 1 | 5 | 4 | 5 | 52 |
| (ii) `source-read` in the job env, visible to every child | 5 | 2 | 5 | 5 | 60 |
| (iii) The mirror injects host-keyed creds into cargo (`OCX_AUTH_*` → `CARGO_REGISTRIES_*`, the `crates/ocx_mirror_pipeline/src/lock_derive.rs:304-313` precedent) | 5 | 1 | 5 | 3 | 51 |
| **(iv) Fetch, then build offline.** `cargo fetch --locked` runs with the operator's read credential, and cargo verifies every dependency checksum against `Cargo.lock` before any upstream code runs. Then `cargo install --path … --locked --offline` runs with registry tokens and providers scrubbed from its env. | 5 | 3 | 5 | 4 | **63** |
| (v) Vendor split. A separate fetch job per version runs `cargo vendor --locked` with the credential. Build legs receive the vendor tree as an artifact and build `--offline` with zero secrets. | 5 | 5 | 3 | 1 | 61 |

**Chosen: (iv), pending OQ1.** Each alternative fails as follows:
- **(iii)** hands a credential to the code under build. `lock_derive` runs `uv` at plan time on trusted input, and nothing there executes upstream code.
- **(i)** excludes the operators the corporate requirement exists for.
- **(ii)** puts the token in the environment every malicious crate dumps first.
- **(iv)** removes the token from the build child's env, which is the commodity-malware path. It does **not** remove it from same-user code, because the job env still holds it. Upstream code can read `/proc/<ppid>/environ`, a `credentials.toml`, a credential-provider socket, or, on GitHub-hosted runners with passwordless sudo, the runner's memory.
- **(v)** closes that residual. It costs a new job type in `plan.json` and every renderer, plus a per-version vendor artifact (size unmeasured). It is the named follow-up if OQ1 rejects (iv).

`--offline` does not stop `build.rs` opening sockets: cargo has no network isolation. Linux `unshare -n` around the install child is a later seam.

**What a leaked read token costs.** Read access to whatever the token can read on the operator's Artifactory or Nexus. That usually means private internal crates, meaning proprietary source, and often other repositories the same token is scoped to. Docs tell operators to issue a token scoped read-only to the cargo remote, and to supply it as an env variable rather than a `credentials.toml`, so that the scrub applies.

### Decision 4 — what a build leg declares about its environment (S16)

The owner settled this on 2026-11-08 ("spec states needs, renderer maps"). The table records the verdict and is not scored.

| Option | Verdict |
|---|---|
| **Runner label set only (`platforms.<p>.runner`, reused).** `prepare` provisions the Rust channel via `rustup`. Native build deps (C compiler, linker, `openssl` headers, `protoc`, `cmake`, `musl-tools`) come from the runner image. | **Chosen.** The mirror models no runner pool, image or forge. It checks the one requirement it can check: `rustup` on `PATH`. |
| Build container image per platform (`containers:`-style) | Follow-up, behind `build.requires: ["container"]`. `containers:` today means *test* images (libc proof); overloading it would let a test-matrix edit change the build. |
| ocx-provisioned toolchains (the spec names ocx packages for rustup, cmake, protoc) | Rejected for v1. It depends on packages that do not exist for all six targets, and it puts build-environment modelling into the spec. |

## Decision Outcome

**Chosen:** Decision 1 A, Decision 2 A′ (`versions[].build`), Decision 3 (iv), Decision 4 runner-only.

**Rationale.**
- A is the only builder location that passes all four gates.
- A′ adds the one missing fact, that an entry builds per platform, as kind-neutral data, and tier-3 recipes reuse it unchanged. Every existing rule and fact in `plan.json` stays single-sourced, and renderers get a fail-loud valve.
- The `--path` build over a mirror-verified `.crate` makes "planned = built" a property of the bytes. The fetch/offline split keeps the operator's cargo config in charge of dependencies, which is the whole corporate story. It also keeps the read credential away from the build child.

### Sub-decisions

| # | Decision | Chosen | Rejected | Why |
|---|---|---|---|---|
| S1 | Code location | <ul><li>`ocx_mirror_source::cargo`: sparse-index client, `fetch_crate`, `scan_crate`/`unpack_crate`, feature closure, and the grammar newtypes `CrateName`/`BinName`/`CargoFeature`/`RustChannel`; local `thiserror` enum, no spec types (E1)</li><li>`ocx_mirror_spec::cargo_config` (`CargoConfig`)</li><li>`ocx_mirror_spec::platform_keys::rust_target_triple`</li><li>`ocx_mirror_pipeline::cargo_build` (subprocesses, request digest)</li><li>root `plan/cargo.rs` (the entry function)</li><li>`ocx_mirror_error::cargo::classify_error`</li></ul> | a `crates/ocx_cargo` sibling | No second consumer (crate-placement › Promotion trigger). Every edge is allowed by `crates/crate_map.toml:10-18`: spec→source, pipeline→source/spec, root→all. |
| S2 | `MirrorTask` seam | `build: Option<CargoBuild>` (`crates/ocx_mirror_pipeline/src/mirror_task.rs:23-52`). When `Some`, the I/O step is `cargo::fetch_crate` (the plan's own fetch), then the existing `verify` with the digest required, then `cargo::unpack_crate` into `source/`. The build writes `content/`, and the window is unchanged. Refactor commit: the field is added as `None` at every construction site, output-identical (Two Hats). | the draft's `Acquire` enum replacing `download_url`/`asset_name`/`asset_type`/`verify_config` (dropped: verify and the window are reused, so replacing four fields broke every construction site for nothing); a single-variant `BuildStep` enum (YAGNI: tier 3 turns the `Option` into an enum when its arm exists); the generic `download` + archive extractor (uncapped, `download.rs:25-26`, and blind to entry types and package-root config) | the `.crate` is a downloaded, digest-verified archive; only its fetch and unpack need cargo-specific bounds |
| S3 | Plan carrier | The `.crate` goes in `versions[].assets[]` (one per platform, `digest = sha256:<cksum>`). Build parameters go in `versions[].cargo` (`skip_serializing_if`, the `pylock` idiom at `plan.rs:168-169`), with the marker in `versions[].build` (S15). `schema_version` goes **4 → 5**. `prepare --plan` is optional for cargo: the plan-less path computes the entry with the same function `plan` uses. `push --plan` is **required** for build kinds (64). | `--plan` mandatory at prepare (the draft) | One implementation, two call sites. Push needs the planned set because per-platform legs fail partially, and a missing leg is otherwise invisible (`push.rs:155-165` derives platforms from the files present). |
| S4 | `prepare --platform` | Repeatable, for archive and build kinds: keep only tasks whose full platform key is listed. A value that is not a `spec.platforms` key → 64. A listed key the entry lacks → warning; zero tasks left → exit 0, nothing written. **Env kinds → 64**, because one invocation writes one `env-manifest.json` per version and a filtered run would overwrite it. **An entry with `build` and no `--platform` → 64.** | all kinds; cargo only | The smallest honest filter. The 64 is what makes a v4 renderer fail loud. |
| S5 | Feature-set variants | **Phase 2.** v1 has top-level `cargo.features`/`no_default_features`/`bins` only, and `variants:` is refused for cargo. The phase-2 shape: `VariantSpec.assets` becomes an `Option`, `features/no_default_features/bins` are added, and validation in `validate_variants` (`crates/ocx_mirror_spec/src/lib.rs:641`) becomes source-aware. The three `EffectiveVariant.assets` callers (`plan.rs:402-403`, `sync.rs:78-79`, `prepare.rs:836-837`) unwrap under that validation guarantee. | a separate `cargo.variants:` list | It shares the name, default and tag-prefix rules. The tag grammar `<name>-<version>` is decided now. |
| S6 | Test phase | **Unchanged in shape.** Build legs upload `bundle-{V}-{slug}`. The existing `test` job (`workflow.yml:96-161`) runs `ocx package test`, and push gates by JUnit. Two pre-existing defects are fixed for every kind as prerequisite `fix:` commits: `test.if` gains `!cancelled()` (`workflow.yml:98`), and the test body records every version's JUnit and fails after the loop instead of exiting at the first failed version (`generate/ci/matrix.rs:530-531`). | smoke test inline on the build leg | zero change to a contract three modules share |
| S7 | Rebuild policy | **Phase 3**, unchanged: `cargo.rebuild: N` → annotation `sh.ocx.mirror.cargo.rebuild`, refused by `deny_unknown_fields` until then | rebuild number in the tag | the `+{timestamp}`/`_{timestamp}` stamp already is the artifact identity |
| S8 | Provenance | Index annotations (C6): `sh.ocx.mirror.rev` and `sh.ocx.mirror.cargo.{crate,version,cksum,features,no-default-features,channel,rustc}`. Each key holds the value every platform of the resulting index shares, or the literal `mixed`. These are pipeline claims, not attestations; `cksum` proves transport integrity of what the index named, not that the index was honest. `sh.ocx.mirror.` is reserved: a spec `annotations:` key under it is refused (65) for **every** kind. SLSA referrer + `cargo-auditable`: phase 4 | per-platform index annotations; omit-on-disagreement (draft: ocx merges index annotations with `extend`, `external/ocx/crates/ocx_oci/src/client.rs:625-629`, so an omitted key keeps a stale claim) | `mixed` is the only removal semantics `extend` allows; the durable per-platform seat is OQ3 |
| S9 | Other commands | `package check` works: it calls the same entry function, refusals included. `package sync` and `pipeline patch` refuse cargo (64). The drift scan skips cargo (the env precedent, `plan.rs:356-360`). | host-only build in `sync` | the pipeline is the product path; drift needs the pre-read for published versions |
| S10 | `--locked` | Always, with no knob. A `.crate` without `Cargo.lock` → 65, remedy "raise `versions.min`". | `locked: false` | the lock pins the transitive set that executes on the leg |
| S11 | Planned = built, registry parity | The mirror fetches the `.crate` from `assets[].url` and verifies `assets[].digest` before unpacking. `cargo install --path=<source>/<crate>-<vers>` builds exactly those bytes. There is no `--index` on argv: dependencies come from the packaged `Cargo.lock`, through cargo's default registry and the operator's `replace-with`. **Parity with `cargo install <crate>`:** <ul><li>`unpack_crate` skips the package-root `.cargo/` and `rust-toolchain`/`rust-toolchain.toml`, with a warning naming them. This reproduces registry semantics, where package-local config is never read.</li><li>Refusing such crates instead would reject packages every user installs fine.</li><li>`prepare` refuses (78) when an ancestor of the source dir holds a `Cargo.toml`, `.cargo/config.toml` or `.cargo/config`, other than `$CARGO_HOME`'s own. An ancestor workspace breaks the build, and an ancestor config silently changes it.</li></ul> | `--index <source.index>` plus a post-build hash of `$CARGO_HOME/registry/cache/*` (draft: dials crates.io past `replace-with`) | The verify runs *before* any upstream code. Registry parity is enforced, not assumed. |
| S12 | Child environment | Every child (`rustup`, `rustc -V`, `cargo fetch`, `cargo install`) gets: <ul><li>cwd = the task dir (never the source tree)</li><li>`CARGO_TARGET_DIR=<task>/target` and `CARGO_TERM_COLOR=never`</li><li>the process env **minus** the scrub list (C5)</li></ul> `cargo fetch` keeps cargo's registry credentials. `cargo install` loses them and runs `--offline` with `CARGO_NET_OFFLINE=true`. | allow-list; `env_clear` | Windows cargo needs its environment. The scrub is hygiene, not a boundary: same-user code can read `/proc/<ppid>/environ`. The boundary is what the job is given (S20, S21). |
| S13 | Toolchain provisioning | `prepare` runs `rustup toolchain install <channel> --profile minimal --target <triple> --no-self-update` itself, and `rustup` on `PATH` is the build leg's one checked requirement (missing → 78). <ul><li>**Trust model:** rustup fetches the channel manifest from `RUSTUP_DIST_SERVER` (default `static.rust-lang.org`) over TLS and checks every component against the manifest's sha256. Signatures are not enforced, so the dist server the operator configures is the trust root.</li><li>**Side effect:** the install persists in `RUSTUP_HOME` (default `~/.rustup`), shared by every rustup user of the host. Parallel installs of one toolchain on a shared host are not relied on (S21).</li></ul> | a rendered toolchain step | keeps the build job one command on every CI; idempotent on a cron box; `RUSTUP_DIST_SERVER` (inherited) covers corporate rustup mirrors |
| S14 | Corporate cargo setup | Inherited operator environment, never spec data: <ul><li>`$CARGO_HOME/config.toml`: `[source.crates-io] replace-with`, `[http] proxy/cainfo`</li><li>env: `CARGO_HTTP_PROXY`, `HTTPS_PROXY`, `CARGO_HTTP_CAINFO`, `CARGO_REGISTRIES_<NAME>_TOKEN`, `RUSTUP_DIST_SERVER`</li></ul> The mirror's own fetches (index, `.crate`) use its existing story: `HTTPS_PROXY`, `OCX_EXTRA_CA_CERTS`, `OCX_AUTH_<slug>_*` and netrc (host-gated, C2), `source.index`, and `source.url_rewrite` / `OCX_MIRROR_URL_REWRITE`. **`OCX_EXTRA_CA_CERTS` is not translated into `CARGO_HTTP_CAINFO`.** The former *adds* roots; the latter *replaces* cargo's trust store, so a translation would break every host the corporate CA does not sign. Docs page: `docs/corporate-cargo.md` (how-to). | a spec `cargo:` proxy/mirror block; CA pass-through | "runners, proxies, auth are not ours to model" (AGENTS.md) |
| S15 | `plan.json` build work | Decision 2 A′: `versions[].build` plus the `requires` must-understand list; contract in C3 | A, B, B′, C | matrix above; OQ2 |
| S16 | Build environment | Decision 4: runner label set only | build image, ocx toolchains | matrix above |
| S17 | Hand-off layout | `prepare --bundles-dir DIR` (all kinds) writes exactly what test and `push --bundles-dir` read (C4). It is one-way, because `plan-json.md` documents it. | the rendered bash flatten (`matrix.rs:611-646`) | a prepare job becomes one command; existing renders keep the flatten until a separate adoption change |
| S18 | Native per target, same-OS cross | <ul><li>The triple comes from the platform key (C8), and `--target` is always passed. There is no host check.</li><li>A different `platforms.<p>.runner` of the same OS therefore builds with zero new code, as long as that runner can also run the result. `darwin/amd64` on an arm64 macOS runner is the answer to `macos-15-intel` retiring 2027-08; Rosetta on hosted images is unverified. `+libc.musl` on a same-arch glibc host also works.</li><li>`windows/arm64` still needs an arm64 runner, because build and test share `runner`.</li><li>`*-pc-windows-msvc` builds use a static CRT (cargo-dist default), appended append-aware (C5).</li><li>Darwin builds set `MACOSX_DEPLOYMENT_TARGET` unless the operator did (C5).</li></ul> | cross-OS (zigbuild/xwin) in v1 | Owner decision. Cross-OS is a cost follow-up behind `build.requires: ["cross"]`. |
| S19 | Tier names and seams | Tier 1 **source kind**: the ecosystem package manager is the recipe (cargo; pypi already). Tier 2 **post-process**: data-declared tree operations for any source. Tier 3 **recipe**: an operator-owned build script. There are two leg classes: **fetch legs** (entries without `build`) and **build legs** (entries with `build`). See § Transform tiers. | "transform" (owner disliked), `patch` (taken by `pipeline patch`), `dist` (taken by `ocx-mirror dist`) | "post-process" says *when* it runs; "recipe" says *who owns* it |
| S20 | Credential classes | `none` ⊂ `source-read` ⊂ `registry-write`, documented per job type in `plan-json.md`. Discover, fetch prepare, build prepare and test → at most `source-read`. Push and patch → `registry-write`. Within a build prepare, only `cargo fetch` sees `source-read` (Decision 3 (iv)). | per-entry `credentials` data | Classes are fixed per job type today. A future per-entry class arrives as a `requires` value. |
| S21 | Single-host renders | Where build, test and push share a machine (cron, one Jenkins agent), the class split holds **only if the operator builds it**. The `docs/getting-started.md` non-GitHub section states it as a requirement: <ul><li>prepare and test run without push credentials, as a separate OS user or in a disposable container;</li><li>that user has no write path to the push user's `HOME`, credential store, `plan.json`, or any executable push runs (`ocx`, `ocx-mirror`);</li><li>each unit gets a fresh `CARGO_HOME`/`RUSTUP_HOME` (seeded from the operator's config) or a fresh container;</li><li>`plan_sha256` is computed on the push side before any unit runs.</li></ul> | mirror-enforced sandboxing | the execution environment is the operator's (threat model); the mirror cannot build users or containers, only state what its guarantees rest on |
| S22 | Build-kind resume | **None.** A build task deletes its previous outputs and rebuilds on every invocation. | the archive resume key plus `build-request.json` (pass 1) | A resume key cannot cover operator env (`RUSTFLAGS`, cargo config), and an interrupted rebuild could leave a stale `build.json` adoptable. YAGNI: legs are ephemeral on every forge. |

### Transform tiers and leg classes (S19 — seams only)

```
prepare_task window (one (version, platform) task; orchestrator.rs:747)
  ├─ fetch kinds : download (I/O permit) → verify → extract → content/          [github_release, url_index]
  └─ build kinds : cargo::fetch_crate → verify (digest required) → cargo::unpack_crate → source/
                   → cargo_build (rustup → rustc -V → cargo fetch → cargo install --offline) → content/
                   → bins check → receipts deleted                            [tier 1: cargo · tier 3: recipe (own ADR)]
  ┄ tier 2 POST-PROCESS seam ┄ (content/ → content/; none exists today; own ADR)
  → chmod declared (:893-897) → authoring-metadata.json (:899) → ocx package create (:902-920)
  → reject_empty_scan (:925-926) → sidecar rename (:927) → drop trees (:937)
env kinds (pylock/pypi) keep their parallel path (ocx_mirror_pipeline::python_prepare): tier 1 already
```

| Axis | Fetch leg | Build leg |
|---|---|---|
| `plan.json` | `versions[]` entry without `build` | entry with `build` |
| Runs on | any host with `ocx-mirror` + `ocx` | a runner carrying all of `legs[p].runner`, plus `rustup` and native build tools |
| Granularity | one per version | one per (version × platform) |
| Who | github_release, url_index, pypi, pylock; any tier-2 post-process | cargo (tier 1), recipes (tier 3) |

- **Tier 3 contract (named; designed by its own ADR).** A recipe is a spec-declared generator-style argv, in the `ocx_mirror_source::generator` shape (timeout, `kill_on_drop`). It receives `OCX_MIRROR_VERSION`, `OCX_MIRROR_PLATFORM`, `OCX_MIRROR_SOURCE_DIR` and `OCX_MIRROR_OUTPUT_DIR`. A non-zero exit or an empty output tree fails closed. Its plan entries carry `build` exactly like cargo, so `plan.json` does not change. `MirrorTask.build` becomes an enum in that ADR. The motivating case is a slim LLVM prefix: measure `bin/clang`, `bin/lld` and `lib/clang/NN` before choosing tier 2 or tier 3 (research_slim_llvm_package_size).
- **Tier 2 (named; own ADR).** Post-process covers file subset, strip, and RPATH/install_name/DLL bundling. It runs on every leg class between content and chmod, so the window still sees the final tree. Precedents: `strip_components_config.rs`, create's bin scan.

### Quantified Impact (v1)

| Metric | Archive kind | Cargo kind | Notes |
|---|---|---|---|
| Prepare wall time per version | ~1 min, one job | the slowest of six legs: 2–15 min, plus 0.5–1.5 min of `rustup` install, plus `cargo fetch` | legs run in parallel; `versions.new_per_run` bounds fan-out; no resume (S22) |
| Runner cost per version (private repo, 2026 rates) | ≈ $0.02 | ≈ $0.8 (build $0.65 + test $0.16) | macOS dominates; public repos free |
| 20-version backfill | ≈ $0.4 | ≈ $16 | docs recommend `versions.min` + `new_per_run: 2` |
| Plan-time cost | tags + one crawl | plus `config.json`, one index file and one `.crate` (0.1–2 MB typical, ≤ 50 MiB cap) per **new** version | |
| Existing golden renders | — | byte-identical **after** the two prerequisite `fix:` commits (S6), which regenerate every golden in their own commit; one new golden `tests/golden/mirror-cargo.txt` | the renderer branches on `Source::Cargo` only |
| Direct dependencies | — | `tar`, `flate2` copied exactly from ocx `[workspace.dependencies]` (`external/ocx/Cargo.toml:213-214`); `semver` mirror-owned (no ocx row; `1.0.28` already in `Cargo.lock`); `cargo_toml` mirror-owned and new to the lock (footprint to confirm with `cargo tree` at implementation); `toml` already a row | § Existing crates considered |

## Technical Details — component contracts (v1)

Every rule below is phrased so a test can be written against it without reading the code. The design spec § 6 maps each rule to a test seam.

### C1 `mirror.yml` surface

```yaml
name: cargo-nextest
target: { registry: ocx.sh, repository: cargo-nextest }
source:
  type: cargo
  crate: cargo-nextest            # optional, default: name
  index: https://index.crates.io  # optional; sparse index base
  url_rewrite: { from: …, to: … } # optional, existing shape: rewrites the .crate download host only
cargo:                            # optional for cargo; refused (65) for every other kind
  channel: "1.89.0"               # default "stable"
  features: [default-no-update]   # default []
  no_default_features: false      # default false
  bins: [cargo-nextest]           # optional subset; absent = every expected bin
  timeout_seconds: 3600           # default 3600
platforms:                        # unchanged schema; required for cargo
  linux/amd64:   { runner: ubuntu-24.04 }
  darwin/arm64:  { runner: macos-15 }
  windows/amd64: { runner: windows-2025, shell: pwsh }
tests: [{ name: version, command: cargo-nextest --version }]
```

| Field | Rule (violation → `SpecInvalid` 65 unless noted) |
|---|---|
| `source.crate` | Matches `^[A-Za-z][A-Za-z0-9_-]{0,63}$`. Not a Windows device name: `con`, `prn`, `aux`, `nul`, `com1`–`com9`, `lpt1`–`lpt9`, case-insensitive. The grammar is owned by `ocx_mirror_source::cargo::CrateName`. |
| `source.index` | Parses as a URL with scheme `https`, and no userinfo, query or fragment (the `Source::Pypi` arm precedent, `crates/ocx_mirror_spec/src/source.rs:441`). A `sparse+` prefix or a git index URL is refused. |
| `source.url_rewrite` | Accepted (existing `UrlRewrite::check` rules). |
| `cargo.channel` | Matches `^(stable\|beta\|nightly(-\d{4}-\d{2}-\d{2})?\|\d+\.\d+(\.\d+)?)$`; no host-qualified toolchain names (`RustChannel`). |
| `cargo.features[]` | Matches `^[A-Za-z0-9_][A-Za-z0-9_+.-]{0,63}(/[A-Za-z0-9_][A-Za-z0-9_+.-]{0,63})?$` (`CargoFeature`). Existence is checked at plan (C2). |
| `cargo.bins[]` | `BinName`: matches `^[A-Za-z0-9_][A-Za-z0-9_-]{0,63}$`, and is not a Windows device name (the `CrateName` list). Non-empty when present; no duplicates. |
| `cargo.timeout_seconds` | `60..=21600`. |
| `cargo` on any other kind | Refused; the message names `cargo:` and the source type. |
| Fields refused for cargo | `assets`, `asset_type`, `variants`, `wheels`, `wheel_scope`, `python`, `verify`, `bin_scan`. Each message names the field and the reason. |
| `platforms` for cargo | Required; every key maps through C8. |
| `tests` for cargo | Every platform's effective list (`platforms.<k>.tests`, else top-level) is non-empty **at load**, not only under `generate ci`. A built artifact nobody has run gets no green from an empty JUnit (`push/verdict.rs:101` passes a zero-testcase suite). |
| `annotations` (every kind) | A key starting `sh.ocx.mirror.` is refused. |
| `metadata`, `libc_lint`, `build_timestamp`, `versions`, `cascade`, `concurrency`, `sign`, `announce`, `catalog`, `notify` | As for archive kinds. |

Every v1 field is a literal. A field that ever has to be computed widens to `ValueSource`, the one computed-value shape. It never gains a `_from` sibling or a flag. There is no `cargo.env` in v1, because no v1 requirement needs one.

Fixtures: one file per rule under `tests/fixtures/invalid/cargo_*.yml`, following the `# expect:` convention of `tests/spec_validation.rs`.

### C2 `ocx_mirror_source::cargo` — discovery, fetch, scan, unpack

```rust
pub struct CrateName(String);    // constructed only through the C1 grammars; Display is the only way out.
pub struct BinName(String);      // This module owns every C1 cargo grammar, and spec validation calls
pub struct CargoFeature(String); // into it (the crate-placement convention).
pub struct RustChannel(String);
pub async fn list_versions(client: &reqwest::Client, index: &Url, name: &CrateName)
    -> Result<Vec<VersionInfo>, CargoSourceError>;
pub async fn fetch_crate(client: &reqwest::Client, url: &Url, trusted_hosts: &TrustedHosts)
    -> Result<bytes::Bytes, CargoSourceError>;            // plan pre-read AND prepare: one function
pub fn scan_crate(crate_bytes: &[u8], name: &CrateName, version: &semver::Version)
    -> Result<CrateManifest, CargoSourceError>;            // sync; callers wrap in spawn_blocking
pub fn unpack_crate(crate_bytes: &[u8], name: &CrateName, version: &semver::Version, dest: &Path)
    -> Result<UnpackReport, CargoSourceError>;             // same reader and bounds as scan_crate
pub fn expected_bins(manifest: &CrateManifest, index_features: &BTreeMap<String, Vec<String>>,
    features: &[CargoFeature], no_default_features: bool) -> Result<Vec<BinName>, CargoSourceError>;
```

**Clients and hosts.** Trusted hosts are the `source.index` host and, when set, the `url_rewrite.to` host. These are operator-authored, and private addresses are allowed for them.
- `config.json` and the index file go through the SSRF-floored client (`ocx_mirror_http`, S1 move) with `redirect::Policy::none()`. A redirect → 69, naming the `Location` host. These requests carry the index host's credentials (`ocx_mirror_http::auth`).
- The `.crate` goes through the same floored client, with redirects re-validated:
  - `https` only, at most 5 hops;
  - every hop resolves through the `GuardedResolver`, and a non-trusted hop host must be public;
  - credentials are attached **only** when the request host is a trusted host. A rendered `dl` on any other host is fetched anonymously, with no `OCX_AUTH_*` and no netrc rung, and no credential follows a redirect off the host it was attached for.
- Every body is read through `ocx_mirror_http::read_capped`: 8 MiB for `config.json` and the index file, 50 MiB for the `.crate`. Nothing is written unless the read completed.
- Every request has a 30 s timeout.

**Discovery.**
1. `GET {index}/config.json` comes first. It must parse, and `dl` must be present (else 65). `auth-required` is honoured by the host-gated auth.
2. `GET {index}/{prefix}/{lowercase name}`, with the prefix rule `1/`, `2/`, `3/<c>/`, `<ab>/<cd>/`. The client is `ocx_mirror_http::client()`, following the `pypi::list_versions` precedent (`crates/ocx_mirror_source/src/pypi.rs:109`).
3. Lines are parsed **fail-closed**. Every line must be one JSON object (`serde_json` into a local struct, ignoring unknown fields); an unparseable line aborts the listing with 65. The line's `name` must equal the crate's case-insensitively (else 65). `vers` must be a `semver::Version` (else 65). `cksum` must be 64 lowercase hex characters (else 65). A `vers` that appears twice aborts with 65. `features2` is merged into `features`. Yanked lines are dropped.
4. The `dl` template is rendered per version by substituting `{crate}`, `{version}`, `{prefix}`, `{lowerprefix}` and `{sha256-checksum}`. With no marker, `/{crate}/{version}/download` is appended. The result must be `https` with no userinfo, else 65.
5. Each `VersionInfo` carries one asset, `<crate>-<vers>.crate`, pointing at that URL, with digest `sha256:<cksum>` and `is_prerelease = !vers.pre.is_empty()`. The `cksum` comes from the index. It proves transport integrity against the index's claim, and nothing about the index's honesty.
6. Classification:
   - the crate's index file 404 → `NotFound` → 65;
   - `config.json` 404 → 65 ("not a sparse index");
   - connect failure, 5xx, 401/403, timeout, or a body over its cap → 69.

**Scan** (plan and `check`, per **new** version, in-process; never `cargo metadata`, § Existing crates considered) and **unpack** (prepare) share one reader and every bound:
- The gzip stream decompresses to at most 1 GiB, and the tar holds at most 100 000 entries.
- Every entry path starts `<crate>-<vers>/`, with no `..` component and no absolute or prefix path.
- Entry types are **regular files and directories only**. A symlink, hardlink, character or block device, FIFO, PAX global header or GNU sparse entry → 65.
- There is exactly one `<crate>-<vers>/Cargo.toml`, of at most 1 MiB, and it parses as TOML (else 65).
- `unpack_crate` does not write the package-root `.cargo/` or `rust-toolchain`/`rust-toolchain.toml` (S11). It reports them so `prepare` can warn by name.
- Every violation → 65.

**Manifest.** `cargo_toml::Manifest`, completed with `complete_from_abstract_filesystem` over the scanned entry list, yields the `[[bin]]` targets, implicit and explicit, with their `required-features`, plus `package.rust-version` and the presence of `<crate>-<vers>/Cargo.lock`. Each bin name must parse as a `BinName` (else 65).

**`expected_bins`.** The enabled set is the closure over the index `features` map, starting from `default` (unless `no_default_features`) ∪ `features`. `dep:x` enables no feature. `x/y` counts as enabled when requested or reached. A bin is expected iff its `required-features` ⊆ enabled.

**Refusals** (all 65, before any build):
- a requested feature absent from the map that is not of `x/y` form;
- zero bins;
- an empty expected set;
- a `cargo.bins` entry outside the expected set;
- `Cargo.lock` absent;
- `rust-version` above `channel`, when `channel` is a version.

### C3 `plan.json` v5 (the renderer contract)

```json
{ "schema_version": 5, "has_new": true, "has_drift": false, "target": "ocx.sh/cargo-nextest",
  "versions": [{ "version": "0.9.100_20261108T120000Z", "source_version": "0.9.100", "kind": "new",
    "variant": null, "platforms": ["darwin/arm64", "linux/amd64"],
    "assets": [
      { "platform": "darwin/arm64", "asset_name": "cargo-nextest-0.9.100.crate",
        "url": "https://static.crates.io/crates/cargo-nextest/0.9.100/download", "digest": "sha256:<64 hex>" },
      { "platform": "linux/amd64",  "asset_name": "cargo-nextest-0.9.100.crate",
        "url": "https://static.crates.io/crates/cargo-nextest/0.9.100/download", "digest": "sha256:<64 hex>" } ],
    "build": { "timeout_seconds": 3600 },
    "cargo": { "crate": "cargo-nextest", "version": "0.9.100", "index": "https://index.crates.io",
      "channel": "1.89.0", "features": [], "no_default_features": false, "bins": ["cargo-nextest"] } }],
  "legs": { "darwin/arm64": { "runner": ["macos-15"], "platform_slug": "darwin_arm64",
              "containers": [{ "id": "_native_", "shell": "bash", "toolchain": { "project": "ocx.toml", "groups": ["default"] } }],
              "…": "unchanged" },
            "linux/amd64":  { "runner": ["ubuntu-24.04"], "platform_slug": "linux_amd64", "…": "unchanged" } },
  "ocx_mirror_rev": null, "versions_resolved": { "min_inclusive": true, "max_inclusive": false } }
```

| Field | Since | Contract |
|---|---|---|
| `versions[].build` | 5 | Optional object. Present iff the entry's prepare is a **build leg per platform**. It is never present on a `metadata-drift` entry. |
| `versions[].build.timeout_seconds` | 5 | The bound the mirror enforces on each leg's `cargo install`. Give the job at least this, plus toolchain install and setup (GitLab's default job timeout is 60 min). |
| `versions[].build.requires` | 5 | Optional, omitted when empty; v5 never emits it. **Must-understand:** a renderer that meets a value it does not implement MUST fail the render. It must not drop the entry or run it as if the list were empty. This is the one exception to "ignore unknown fields". |
| `versions[].cargo` | 5 | Present iff `source.type: cargo`. Fields: `crate`; `version` (the crate version, equal to `source_version`); `index` (resolved, default included); `channel`; `features` (sorted, deduped); `no_default_features`; `bins` (the expected set, sorted). Informational for renderers. `prepare` and `push` re-validate every field with the C1/C2 grammars, because the plan crossed an artifact store. |
| `versions[].assets[]` (cargo) | 2 | One per `platforms` element, all naming the same `.crate` URL and `digest`. |
| `legs.<p>.containers[].toolchain` | 5 | Optional `{project, groups}`, present only on the `_native_` entry of a platform that declares `platforms.<p>.toolchain`. It is the ocx project the native test leg loads. `groups` is the effective list, `["default"]` when none is declared. It mirrors `matrix.rs:187` and is pinned by the leg-parity test. |

**Rendering rules.** These extend `plan-json.md` § Rendering. The v4 rules stand for every entry without `build`.
1. **Prepare.**
   - For an entry **without** `build`: one job on the renderer's default host, running
     `ocx-mirror package pipeline prepare --plan plan.json --version <V> --bundles-dir bundles`.
     `locks/` travels beside `plan.json` for `pypi`, as today.
   - For an entry **with** `build`: one job per `p ∈ platforms`, on a runner carrying every label of `legs[p].runner`, running the same command plus `--platform <p>`.
2. **Test (V, P, C).** It needs V's prepare job (fetch) or (V, P)'s (build). The native leg loads `containers[].toolchain` when present. Fan-out and JUnit naming are unchanged. A test job records every version's JUnit and fails after the loop.
3. **Push.** It runs after every prepare and test job, **including failed ones**, with `--plan plan.json`. It adds `--plan-sha256 <hex>` when the renderer carries the digest outside the artifact store. There is one push job.
4. **Credentials (S20).** Prepare and test jobs get at most `source-read`; only push and patch get `registry-write`. On a single host, S21 applies.
5. An unknown `build.requires` value fails the render.
6. Drift entries → `pipeline patch --metadata-only`, unchanged.
7. **Host requirements.** Every job needs `ocx-mirror` and `ocx`. A build-leg job also needs `rustup` on `PATH` and the target's native build tools.

**Evolution.**
- v5 is additive, and `$id` is `https://ocx.sh/schemas/plan/v5.json`.
- A field added under `build` is ignorable unless it changes *where or how* the job runs. Such a change must add a `requires` value. The reserved vocabulary is `container`, `cross`, `vendor` and recipe-specific capabilities, all follow-ups.
- `prepare` and `push` themselves refuse a `requires` value they do not implement, with `PlanError` 65 before any download or registry call. v5 implements none.
- A v4 renderer is covered by § Decision 2.
- `prepare --plan` keeps reading v4 documents for every existing kind. A cargo spec fed a plan without `versions[].cargo` or `build` → `PlanError` 65 ("regenerate: plan.json schema_version ≥ 5").
- **Path safety on read.** Before using a value in a path, `prepare` and `push` check that every `versions[].version` matches `^[0-9A-Za-z][0-9A-Za-z._+-]{0,127}$`, and that every platform is a `spec.platforms` key. Else `PlanError` 65.

### C4 `prepare` CLI

```
ocx-mirror package pipeline prepare --version V [--plan plan.json] [--platform KEY]... [--bundles-dir DIR] [--work-dir D] [--spec S]
```
- **`--platform` (S4).** Applies to archive and build kinds; env kinds → 64.
  - An entry with `build`, or a cargo spec run without a plan, with no `--platform` → 64. The message names `versions[].build` ("builds natively per platform: pass --platform, one prepare per platform").
  - A value that is not a `spec.platforms` key → 64.
- **`requires` and path safety (C3).** Checked before any download.
- **Without `--plan` (cargo).** Discovery and pre-read for `V` run through the plan's own entry function, so refusals are identical to `plan`'s.
- **`--bundles-dir DIR` (all kinds).** After a successful prepare, `prepare` copies each prepared task into the layout that test and `push --bundles-dir` read:
  - archive and build kinds: `DIR/bundle-{V}-{slug}.tar.xz` and `DIR/bundle-{V}-{slug}-metadata.json`;
  - build kinds additionally: `DIR/bundle-{V}-{slug}-build.json`;
  - env kinds: the version subtree as `DIR/{V}/…`.

  `{V}` is the plan's `versions[].version` verbatim, and `{slug}` is `platform_key_slug(key)` (`crates/ocx_mirror_spec/src/platform_keys.rs:118`). Same-named files are overwritten, and nothing else in `DIR` is touched. A failed prepare writes nothing to `DIR`.
- **Work dir.**
  - The layout is unchanged: `{work_dir}/{V_SLUG}/{slug}/{bundle.tar.xz, metadata.json, authoring-metadata.json}`, plus `build.json` for build kinds.
  - `manifest.json` keeps today's semantics: it lists the bundles this invocation wrote, so with `--platform` it lists only those. It has no production reader; only tests read it.
  - Two concurrent `prepare` processes on one work dir stay unsupported, as today.
- **Build kinds: no resume (S22).** A build task deletes its previous `bundle.tar.xz`, `metadata.json`, `authoring-metadata.json`, `build.json`, `source/`, `target/` and `content/` before fetching. After the bundle, `source/`, `target/` and `content/` are removed, as `content/` already is (`:937`).
- **Work-dir check (S11).** Before the first child is spawned, refuse with `BuildEnvironment` 78 when any ancestor of `<task>/source/<crate>-<vers>` holds `Cargo.toml`, `.cargo/config.toml` or `.cargo/config`, other than `$CARGO_HOME/config{,.toml}`. The message names the path and the remedy: a `--work-dir` outside any cargo project, such as `$RUNNER_TEMP`.

### C5 `ocx_mirror_pipeline::cargo_build` — the build step

This runs inside `prepare_task`'s CPU permit (`orchestrator.rs:859-940`), after `unpack_crate`.

1. **Toolchain.** If `rustup` is not on `PATH` → `BuildEnvironment` (78), naming the docs page. Then run `rustup toolchain install <channel> --profile minimal --target <triple> --no-self-update`, bounded at 900 s.
2. **Compiler fact.** `rustc +<channel> -V` gives the `rustc` build fact. It must start with `rustc ` and contain only printable ASCII, at most 200 chars, else 1.
3. **Fetch.** `cargo +<channel> fetch --locked --manifest-path=<task>/source/<crate>-<vers>/Cargo.toml`, with the fetch env, bounded at 900 s. Cargo verifies every dependency against `Cargo.lock` here, before any upstream code runs.
4. **Build.**

   ```
   cargo +<channel> install --path=<task>/source/<crate>-<vers> --locked --offline --root=<task>/content
         --target=<triple> [--no-default-features] [--features=<a,b>] [--bin=<x>]... [--config=<crt-static, msvc only>]
   ```

   - Tokens: only `channel`, `features`, `bins` and `triple` come from data, each a validated newtype. Paths are absolute and mirror-made, and every value rides in `--flag=value` form.
   - Environment: the install env. Execution uses `kill_on_drop`, with timeout `build.timeout_seconds`.
5. **Bins check.** The file names in `content/bin/`, with `.exe` stripped for `*-windows-*` triples, must equal the plan's `bins` exactly. Otherwise → 1, naming the missing and unexpected names. This is needed because `--bin-scan verify` accepts a declared name that is absent from disk (`external/ocx/crates/ocx_package/src/bin_scan.rs:258-291`).
6. **Receipts.** Delete `content/.crates.toml` and `content/.crates2.json`. They carry the builder's absolute paths and must never be published.
7. **Window, unchanged.**
   - Authoring metadata: without `metadata:`, the mirror synthesizes one interface-visible `PATH` entry `${installPath}/bin`, with `binaries` = the plan's `bins`. This replaces the `orchestrator.rs:766-768` bail for build kinds. With `metadata:`, the operator's file is used, with an absent `binaries` filled from the plan's `bins`.
   - Create runs with `--bin-scan verify` and `libc_lint` per spec.
8. **`build.json`.** `prepare_version` writes it last, once the bundle digest is known (`orchestrator.rs:402`):

   ```json
   {"version": "<tag>", "platform": "<full key>", "sha256": "<64 hex>", "request_sha256": "<64 hex>", "rustc": "<line>"}
   ```

   `request_sha256` is the sha256 of the canonical build request: compact JSON with fixed field order `crate, version, digest, channel, features, no_default_features, bins, target`, sorted arrays, and no trailing newline. It is computed by `cargo_build::request_digest`, the one function both prepare and push call.

**Child environments.**

| | rustup, rustc | cargo fetch | cargo install |
|---|---|---|---|
| cwd | task dir | task dir | task dir |
| `CARGO_TARGET_DIR`, `CARGO_TERM_COLOR=never` | — | set | set |
| scrub list (below) | applied | applied, **except** `CARGO_REGISTRIES_<NAME>_TOKEN` and `CARGO_REGISTRIES_<NAME>_CREDENTIAL_PROVIDER` | applied, no exception |
| `CARGO_NET_OFFLINE=true` | — | — | set |
| msvc static CRT | — | — | append-aware (below) |
| `MACOSX_DEPLOYMENT_TARGET` | — | — | darwin: the operator's value, else `11.0` (aarch64) or `10.12` (x86_64), matching rustc's own target minimum so C dependencies built by `cc` agree |

**Static CRT, append-aware.** Cargo's rustflags sources are mutually exclusive: `CARGO_ENCODED_RUSTFLAGS` beats `RUSTFLAGS`, which beats `target.<triple>`/`target.<cfg>` rustflags, which beat `build.rustflags`. So:
- When the operator env sets `CARGO_ENCODED_RUSTFLAGS`, append `␟-C␟target-feature=+crt-static` to it.
- Else when it sets `RUSTFLAGS`, append ` -C target-feature=+crt-static` to it.
- Else pass `--config=target.<triple>.rustflags=["-C","target-feature=+crt-static"]`. Cargo joins that array with the operator's `target.<triple>`/`target.<cfg>` rustflags, and it supersedes an operator `build.rustflags`. `docs/corporate-cargo.md` says to move MSVC flags to `target.<triple>.rustflags`.

**Scrub list.** Hygiene, not a boundary. It is removed from every child's env, with the table's exceptions:
- exact names: `GH_TOKEN`, `GITHUB_TOKEN`, `ACTIONS_RUNTIME_TOKEN`, `ACTIONS_ID_TOKEN_REQUEST_TOKEN`, `ACTIONS_ID_TOKEN_REQUEST_URL`, `GITHUB_ENV`, `GITHUB_PATH`, `GITHUB_OUTPUT`, `GITHUB_STATE`, `CI_JOB_TOKEN`, `CI_JOB_JWT`, `CI_JOB_JWT_V2`, `CI_REGISTRY_PASSWORD`, `CI_DEPLOY_PASSWORD`, `CI_REPOSITORY_URL` (it embeds the job token), `OCX_ANNOUNCE_TOKEN`, `OCX_MIRROR_DISCORD_HOOK`, `CARGO_REGISTRY_TOKEN` (a crates.io *publish* token, never needed to read), `SIGSTORE_ID_TOKEN`;
- prefixes: `OCX_AUTH_*`, `COSIGN_*`;
- the pattern: any name ending `_TOKEN`, `_PASSWORD` or `_SECRET`;
- every variable named by the spec's `sign:` `env://NAME` refs.

**Log relay.** Every child's stdout **and** stderr is streamed line by line:
- split on `\n`;
- remove `\r`, every other C0 control, DEL and C1 (U+0080–U+009F), with invalid UTF-8 replaced lossily;
- prefix with `<tool>| ` (`rustup| `, `rustc| `, `cargo| `).

The last 40 lines are kept for the error, prefixed and stripped the same way. No relayed line can start a CI workflow command such as `::add-mask::` (CWE-117).

**Failures** → `ExecutionFailed` (1) for that task, with the tail: spawn error, toolchain install failure, fetch failure, timeout, non-zero `cargo`, or a bins mismatch. Not 65, because a compile failure is upstream's and per-platform, and `platforms.<p>.exclude` is the operator's tool. `rustup` absent and the work-dir check → 78.

### C6 `push` delta

```
ocx-mirror package pipeline push --bundles-dir … --junit-dir … --write-summary … [--plan plan.json [--plan-sha256 HEX]]
```
- **`--plan`** is required for build kinds (64 without it) and optional for others. When given:
  - A **missing or empty bundles dir is zero bundles**, not an error (`push/bundles.rs:78-83` today errors before the completeness check). Every planned pair is then `missing_bundle`, `run-summary.json` is written, and the exit follows the verdict.
  - Build kinds publish **only** `(V, P)` with `P ∈ versions[V].platforms` of a non-drift entry. A bundle outside that set is ignored with a warning naming it. Other kinds enumerate `versions[] ∪ bundles`, as today.
  - Every planned `(V, P)` with no bundle → `PlatformFailure { reason: "missing_bundle" }`, and V's cascade is withheld (the `push.rs:233` condition). A red build leg reads `Partial`, `any_red` is true, and the next run backfills via `backfill-partial` + `cascade_backfilled_entries`.
  - `requires` refusal, path safety, and re-validation of every `versions[].cargo` field with the C1/C2 grammars all run before any registry call.
- **`--plan-sha256 HEX`.** The plan file's sha256 must equal HEX before anything is pushed, else `PlanError` 65.
  - **What it blocks:** a build or test leg, which is upstream code holding the forge's artifact token, rewriting `plan.json` in the shared store to change what push publishes. That could mean adding a version or tag, dropping a red platform from `platforms` so a partial version cascades, or altering the `versions[].cargo` values push writes as annotations.
  - **What it does not do:** it authenticates the plan only. Bundle bytes are bound by nothing stronger than the build record (§ Security).
- **Build records** (build kinds, per `(V, P)` with a bundle). `bundle-{V}-{slug}-build.json` is parsed with `deny_unknown_fields`.
  - Absent or unparseable → `build_record_missing`.
  - Any of the following → `build_record_mismatch`: `version` ≠ V; `platform` ≠ P; `request_sha256` ≠ `request_digest` computed from the plan for (V, P); `sha256` ≠ sha256(bundle); or a `rustc` outside the C5 grammar.

  Either one withholds V's cascade. The check binds bundle, record and plan against corruption, staleness and omission. It is **not** a tamper barrier, because all three travel in the same store.
- **Annotations** (cargo). The run map is built once per run (`crates/ocx_mirror_spec/src/annotations.rs:49`, called at `push.rs:149`). A per-version overlay is merged over it before `invoke_push` (`push.rs:804`), and the same overlay goes on every platform push of V.
  - **Keys:**
    - `sh.ocx.mirror.rev` = this binary's version + short sha (`build_info`);
    - `…cargo.crate` and `…cargo.version`;
    - `…cargo.cksum` = the asset `digest` (`sha256:<hex>`);
    - `…cargo.features` = comma-joined, sorted, or `""` when there are none;
    - `…cargo.no-default-features` = `true` or `false`;
    - `…cargo.channel`;
    - `…cargo.rustc`.
  - **Semantics.** Each value is computed over the **resulting index**: the platforms this run pushes, plus, for a `backfill-partial` entry, the platforms already published. Retained platforms are represented by the existing index annotation, read through `ocx_mirror_pipeline::target_registry`; a read failure → `TargetError` 69, the issue #157 fail-safe. If every value agrees, the key carries that value; otherwise it carries the literal `mixed`. Because ocx merges index annotations with `extend` (`client.rs:625-629`), writing `mixed` is the removal semantics: an omitted key would keep a stale claim.
- **`run-summary.json`.** `platforms_failed[].reason` gains `build_record_missing` and `build_record_mismatch`. `missing_bundle` is now reachable for plan-declared pairs.

### C7 Error mapping

| Variant | Exit | When |
|---|---|---|
| `CargoError(String)` **new** (`ocx_mirror_error::cargo::classify_error`, the `pypi` precedent) | 65 | every C2 refusal (including an unparseable line, a duplicate `vers`, `config.json` without `dl`, a TOML failure, a forbidden entry type); the rendered `dl` is not https or carries userinfo; the `.crate` is 404/410 at plan **or prepare**; the `.crate` exceeds 50 MiB |
| `BuildEnvironment(String)` **new** | 78 | `rustup` not on `PATH`; the C4 work-dir check. This is a host configuration error, so a retry cannot fix it (`quality-rust-exit_codes.md`, the `SignMaterialMissing` → 78 precedent) |
| `SourceError` | 69 | index, `config.json` or `.crate` host unreachable (plan **or prepare**), 5xx, 401/403, timeout, a refused redirect, a body over the index cap |
| `ExecutionFailed` | 1 | every other C5 failure; a `.crate` digest mismatch at prepare (the existing verify path) |
| `SpecUsageError` | 64 | `package sync` / `pipeline patch` on cargo; prepare on an entry with `build` and no `--platform`; a `--platform` value not in `spec.platforms`; `--platform` on an env kind; cargo `push` without `--plan` |
| `PlanError` | 65 | a cargo prepare/push with a plan lacking `versions[].cargo` or `build`; a `--plan-sha256` mismatch; an unimplemented `requires` value; a path-unsafe version or an unknown platform |
| `TargetError` | 69 | the existing-index annotation read for a `backfill-partial` entry failed (C6) |
| `SpecInvalid` | 65 | every C1 rule |

The 65/69 split follows `PypiError` vs `SourceError` (`subsystem-mirror.md` › Error Model): "absent or malformed" is data, and "unknown" is availability. Missing `uv` maps to 1 today. That predates the exit-code rule, and the divergence is flagged (violation 16).

### C8 Platform key → target triple (`ocx_mirror_spec::platform_keys::rust_target_triple`, beside `platform_key_slug`)

| Key | Triple | Runner (docs default) |
|---|---|---|
| `linux/amd64`, `linux/amd64+libc.glibc` | `x86_64-unknown-linux-gnu` | `ubuntu-24.04` |
| `linux/amd64+libc.musl` | `x86_64-unknown-linux-musl` | `ubuntu-24.04` (+ `musl-tools` only for C deps) |
| `linux/arm64`, `+libc.glibc` / `+libc.musl` | `aarch64-unknown-linux-gnu` / `-musl` | `ubuntu-24.04-arm` |
| `darwin/amd64` | `x86_64-apple-darwin` | `macos-15-intel`, or `macos-15` (arm64, Rosetta for the test leg; S18) |
| `darwin/arm64` | `aarch64-apple-darwin` | `macos-15` |
| `windows/amd64` | `x86_64-pc-windows-msvc` | `windows-2025` |
| `windows/arm64` | `aarch64-pc-windows-msvc` | `windows-11-arm` |

Any other key, or a libc feature on darwin or windows → 65 at validate. Linux without a libc feature means gnu (the cargo-dist default). `libc_lint` (inside `ocx package create`) verifies the built binary against `os.features`, unchanged.

### C9 GitHub renderer delta (`generate/ci.rs`, `ci/matrix.rs`, `templates/workflow.yml`)

**Prerequisites for every kind** (S6), as separate `fix:` commits that regenerate the goldens:
- `test.if: ${{ !cancelled() && … }}`;
- the test body records every version's JUnit and exits non-zero after the loop.

**For `Source::Cargo` only:**
1. `discover` adds two outputs:
   - `units` = `jq` over `plan.json`: every `(entry with build, p ∈ platforms)` → `{version, platform: p, platform_slug: .legs[p].platform_slug, runner: .legs[p].runner}`. The step **errors** if any entry carries `build.requires` (rule 5), or if `has_new` is true and `units` is empty. No maps are baked at render time.
   - `plan_sha256`.
2. The `prepare` job:
   - `strategy.matrix.include: ${{ fromJson(needs.discover.outputs.units) }}`, `runs-on: ${{ matrix.runner }}`, `fail-fast: false`;
   - `permissions: { contents: read }`; `actions/checkout` with `persist-credentials: false`; matrix values reach the shell only through `env:`; `shell: bash` (Git Bash on Windows);
   - steps: `setup-ocx` (`groups: default`), then one step running `ocx-mirror package pipeline prepare --plan plan.json --version "$VERSION" --platform "$PLATFORM" --work-dir "$RUNNER_TEMP/ocx-mirror" --bundles-dir bundles`, then an upload of `bundle-${VERSION}-${PLATFORM_SLUG}`. There is no toolchain step and no flatten script.
3. Workflow-level `permissions: { contents: read }`; push keeps its job-level grant.
4. Push downloads `plan` and passes `--plan plan.json --plan-sha256 ${{ needs.discover.outputs.plan_sha256 }}` through `env:`.

Notify is byte-identical. There is a new `tests/fixtures/mirror-cargo.yml` + `tests/golden/mirror-cargo.txt`. The build leg loads the repo's `default` ocx group, so that group must resolve on every build platform (docs + the golden's comment).

### C10 Docs

- `docs/reference/mirror-yml.md`: `source.type: cargo`, `cargo:`, the C8 table with the same-OS notes, the claim shift, a cost note, and glibc-floor and static-CRT notes. For crates with no C dependencies, it recommends `+libc.musl` keys.
- `docs/reference/plan-json.md`: the v5 shape, `versions[].build`, `versions[].cargo`, `containers[].toolchain`, `requires`, the hand-off layout (`--bundles-dir`, `build.json`), the credential classes table, the new rendering rules, and a changelog row.
- `docs/reference/cli.md`: `prepare --platform/--bundles-dir`, `push --plan/--plan-sha256`, exit codes (78 is new for the mirror's build path), and the new reasons.
- **`docs/corporate-cargo.md`** (new how-to). It covers:
  - the S14 knobs table;
  - `source.index`/`url_rewrite` for an internal index, and `OCX_AUTH_*` for the plan leg;
  - supplying the cargo read token via env (scope: read-only, cargo remote only);
  - why `OCX_EXTRA_CA_CERTS` is not passed to cargo;
  - MSVC rustflags placement;
  - the GitLab `CI_JOB_TOKEN` residual.
- `docs/getting-started.md`: the non-GitHub job, and the S21 single-host isolation requirements with the design § 5.2 reference script.
- `CATALOG.md` wording.
- `security-threat-model.md`: the ruling paragraph.
- `subsystem-mirror.md`: module-map rows.

## Existing crates considered (`quality-core.md` › Don't Own Non-Domain Code)

| Need | Considered | Verdict |
|---|---|---|
| Index-line parsing | `crates-index` (sparse support) | **Not used.** Its git feature pulls the gix family, which AGENTS.md records as a cost the mirror already refused for ocx. Its sparse API is built around a local cache dir. The parse it would save is a six-field `serde_json` struct, and `serde_json` is already a dependency. Footprint without `git` is unverified. |
| Manifest parsing and bin inference | `cargo_toml` (`Manifest::complete_from_abstract_filesystem`); `cargo metadata --no-deps` with cwd outside the tree; a hand-rolled walk | **`cargo_toml`.** It handles implicit and explicit targets, `autobins` and `required-features` over an in-memory entry list. `cargo metadata` has the highest fidelity, but it needs cargo on the plan host (a new host requirement for every non-GitHub renderer) and an unpack at plan time. The hand-rolled walk was the pass-1 design, and it is non-domain code. |
| Tar/gzip | `tar`, `flate2` (ocx rows) | Used. The mirror owns only the policy: bounds, entry types, prefix, skipped paths. |
| Version grammar | `semver` | Used (mirror-owned row). |
| Feature closure | none (cargo's resolver is internal) | Owned. It is domain logic: which bins a feature set builds. |
| `dl` template rendering | `crates-index` | Owned: five markers, cargo-documented, and table-tested against cargo's examples. |

## NFR coverage

| NFR | Coverage |
|---|---|
| Scalability | Build-leg jobs number Σ (new versions × platforms), capped by `versions.new_per_run`. The GitHub matrix allows at most 256. On GitLab, render one child pipeline: cross-pipeline `needs:pipeline:job` may be Premium+, unverified. Keep units × test jobs within the `needs` limit (50 by default, unverified, a research gap) by grouping tests per (P, C) as GitHub does. There is no build cache in v1; `Swatinem/rust-cache` keyed on crate + channel + triple is the documented upgrade. |
| Availability | An index or `dl` host down at plan → 69 before any build minute. A leg failing mid-build reds that leg only: push records `missing_bundle`, and the next run backfills. A registry down at push → the existing 75 (retried) / 69 / 82 ladder. |
| Latency | Plan: +3 requests and at most one `.crate` per new version. Build: 2–15 min per leg in parallel, plus 0.5–1.5 min of `rustup` install, plus `cargo fetch`. |
| Security | § Security: ruling, attackers, controls, residuals. |
| Cost | ≈ $0.8 per version private, $0 public. Surfaced in docs together with `versions.min` and `new_per_run`. |
| Operability | One red platform produces one red leg, a `run-summary.json` row (`missing_bundle` / `build_record_*`) and a Discord row. `platforms.<p>.exclude` silences a known-bad `(V, P)`. Every relayed build line is tool-prefixed. A rerun rebuilds (S22). |

### Security — ruling and controls

**Ruling (2026-09-17, extending the 2026-08-14 ruling in `security-threat-model.md`).** Executing upstream `build.rs`/proc-macro code on a build leg is inside the threat model, as an accepted risk. The compensating control is the job split: build legs never hold registry-write, signing, announce or webhook credentials, and `push` alone does.
- This holds wherever the renderer runs push as its own job (GitHub, GitLab, per-stage Jenkins agents).
- On a single host it holds only if the operator builds the split (S21).

**OQ1 asks to extend the ruling:** a build leg may hold `source-read` credentials in its job environment, hidden from the build child (Decision 3 (iv)).

| Attacker (in scope) | Path | Control |
|---|---|---|
| Malicious crate author or transitive dependency | Arbitrary code in `build.rs`/proc-macro on the build leg | <ul><li>No registry-write credential on the leg (S20). Rendered: no `secrets.*` in `prepare`, `permissions: contents: read`, `persist-credentials: false`.</li><li>The read credential reaches `cargo fetch` only.</li><li>The S12 scrub; `--locked`; `--offline`; cwd outside the source tree; receipts stripped.</li></ul> **Residual:** the code can read whatever the leg holds (env, `/proc/<ppid>/environ`, `credentials.toml`, runner memory), bounded to `source-read` by OQ1. **Forge residual:** GitLab injects `CI_JOB_TOKEN` into every job, and by default it can push to the project's own container registry. The scrub hides it from the children only. A spec whose `target` is that registry must have the operator restrict the job token, or build in a project without registry write (design § 5.1; refuse-to-run is § Deferred to owner). |
| Same, on a single host | Reading push credentials, rewriting `plan.json`, or trojaning an executable push later runs (`ocx`, `ocx-mirror`), or a shared `CARGO_HOME`/`RUSTUP_HOME` that later builds use | S21 operator requirements; the design § 5.2 reference script runs units as a separate user with a fresh `HOME`; `plan_sha256` is taken before any unit |
| Same, via the CI log | A crafted line that starts a workflow command (`::add-mask::`, `::stop-commands::`) when relayed | Every relayed line, stdout and stderr, is tool-prefixed and stripped of `\r` and C0/C1 controls (C5) |
| Same, via package-local config | A packaged `.cargo/config.toml` setting `rustc-wrapper`, `linker`, `[env]` or `[source]` to override `replace-with`; a `rust-toolchain.toml` | `unpack_crate` skips them (S11); `+<channel>` overrides any toolchain file; the ancestor check (C4) |
| Index operator / network / compromised index | A crafted `name`/`vers`/`cksum`/`[[bin]]`/`dl`, or a crafted `.crate` (bomb, path escape, link entry, device entry, duplicate `Cargo.toml`) | <ul><li>C1/C2 grammars before storage, and again at prepare and push.</li><li>Fail-closed parsing; caps on both fetches; type and path checks on both reads.</li><li>The digest is verified **before** any unpack or build (S11). A compromised index can serve a matching `cksum` for malicious bytes, which is the attacker row above.</li></ul> |
| Same, SSRF | A `dl` template, or a redirect from the index or `dl` host, pointing at `169.254.169.254` or an internal host | <ul><li>`dl` must render to `https` without userinfo.</li><li>Every fetch runs on the SSRF floor: the `ocx_oci::ssrf::guard_destination` pre-flight plus the `GuardedResolver`. Index fetches allow no redirects; `.crate` redirects are re-validated per hop.</li><li>Trusted (private-allowed) hosts are only the `source.index` and `url_rewrite.to` hosts.</li><li>The floor's constructor moves from `crates/ocx_mirror_pipeline/src/registry_sync/catalog.rs:176-183` into `ocx_mirror_http`, giving two callers (a Two-Hats move).</li><li>Applying it to `url_index`/`github_release` downloads is a named follow-up, because the class is pre-existing there.</li></ul> |
| Same, credential exfiltration | A `dl` on a third host harvesting `OCX_AUTH_*` or a netrc `default` credential | Credentials are attached only for trusted hosts. They are never sent to a rendered `dl` host outside that set, and they never follow a cross-host redirect (C2). |
| A sibling leg of the same run (research_cargo_build_supply_chain § Finding 4) | Overwriting another leg's `bundle-*`, `build.json`, the `plan` artifact or `junit-*` | **Residual, accepted.** `build_record_*` binds bundle, record and plan for consistency only. `--plan-sha256`, taken from discover's job output, blocks plan rewrites (C6). The class is pre-existing: test legs run upstream binaries before push reads the same store. An authenticated bundle-digest channel before attestations is § Deferred to owner. SLSA (phase 4) attests what push received. |

Dependency confusion across registries is not introduced. Dependencies resolve from the packaged `Cargo.lock`'s sources through cargo's own source replacement, with the lock's checksums enforced at `cargo fetch`. Nothing in `mirror.yml` names an environment variable to *read*.

## Boundary and convention violations introduced (named, not smoothed)

1. **First subprocess that runs upstream code.** `url_index.generator` runs operator code, and `uv`/`ocx` are trusted tools. Covered by the 2026-09-17 ruling.
2. **Build legs may hold `source-read` credentials in their job environment while running upstream code.** They are hidden from the build child only. This widens the ruling and needs an owner decision (OQ1).
3. **`plan.json` v5 adds a conditional rendering rule:** entries with `build` fan out per platform. The v4 rule stands for every other entry.
4. **`build.requires` is must-understand**, the first `plan.json` field whose unknown *values* a consumer must refuse. It is a narrow exception to "ignore unknown fields" (`plan-json.md:248`).
5. **First prepare job on a non-`ubuntu-latest` runner.** The build leg requires the repo's `default` ocx group to resolve on darwin and windows.
6. **`push --plan` is mandatory for build kinds only, and for them push publishes only planned `(V, P)`.** This is a CLI and semantic asymmetry.
7. **`sh.ocx.mirror.` becomes a reserved annotation namespace for every kind.** That is a new load-time refusal for existing specs. Annotations become per-version for cargo, and `mixed` is a published value.
8. **`bin_scan:` becomes source-conditional:** refused for cargo and forced to `verify`, with the extra bins check (C5).
9. **Crate-boundary move.** The SSRF-floored client builder moves down from `ocx_mirror_pipeline::registry_sync::catalog` into `ocx_mirror_http`.
10. **Dependencies.** `tar` and `flate2` are copy-exact. `semver` and `cargo_toml` are mirror-owned rows with no ocx counterpart (the `octocrab` precedent), and they need a CLAUDE.md Dependency-model note.
11. **G1 holds for the jobs this change adds or alters, not for the test leg.** The cargo pipeline's test leg is rendered bash on every non-GitHub CI (`matrix.rs:483-533`). `pipeline test` in v1 is an owner decision (§ Deferred to owner).
12. **Pre-existing gaps:**
    - Fixed as prerequisite `fix:` commits for every kind: the test loop's first-failure exit (`matrix.rs:530-531`); `test.if` without a status function (`workflow.yml:98`); and `pipeline plan` accepting an empty effective test list. On that last one, `plan-json.md:146` says `legs.<p>.tests` is "Never empty" while `build_legs` uses `unwrap_or_default()` (`plan/legs.rs:71`), and push passes a zero-testcase JUnit (`push/verdict.rs:101`).
    - Fixed in v5: `platforms.<p>.toolchain` reached the GitHub matrix (`matrix.rs:187`) but not `PlanLeg`/`PlanContainer` (`plan/legs.rs:18-53`).
13. **Product claim shift** in docs and catalog: "built by ocx-mirror from crates.io source", never "mirrored".
14. **Phase 2 (named now):** `VariantSpec.assets` goes from required to `Option`, moving enforcement from the type to `validate`.
15. **Single-host renders:** the build/push credential split is the operator's to build (S21). The mirror states the requirement and cannot enforce it.
16. **Exit-code divergence:** missing `rustup` → 78, while missing `uv` → 1 (pre-existing). Aligning `uv` is a separate `fix:`.
17. **`prepare --platform` is refused for env kinds**, an asymmetry that keeps `env-manifest.json` whole.

## Migration / rollout

- **Existing specs.** Nothing breaks, except a spec already using an `sh.ocx.mirror.*` annotation key (65; no known case). An old binary refuses `cargo:` and `type: cargo` via `deny_unknown_fields` and the tag (65).
- **`prepare` CLI.**
  - `--platform` (archive + build) and `--bundles-dir` (all kinds) are additive.
  - `manifest.json` semantics are unchanged.
  - The archive resume and work-dir layout are unchanged; build kinds never resume.
- **Bundle artifact layout.** Names are the same; build kinds add `bundle-{V}-{slug}-build.json`. `enumerate_bundles` matches `.tar.xz` only (`push/bundles.rs:108-110`), so the extra file is inert for every other reader.
- **`plan.json` consumers.** v5 is additive. Third-party renderers of existing kinds need no change, but should read `containers[].toolchain`. A cargo renderer must read `versions[].build`, and a v4 renderer fails loud (64). `ocx-mirror schema plan` emits v5.
- **GitHub renderer.** Two prerequisite `fix:` commits change the test job of every golden, landing first. After them, cargo-only branches keep every existing golden byte-identical. Downstream mirror repos see the two fixes on their next `generate ci`, and nothing else until a spec switches to `type: cargo`.
- **Bazel surface.**
  - `crates/ocx_mirror_source/BUILD.bazel` gains `tar`, `flate2`, `semver` and `cargo_toml` deps (and any crate that names them likewise); `task bazel:build:drift` keeps the edges equal to Cargo's.
  - `Cargo.bazel.lock.json` is regenerated by `task bazel:bootstrap`.
  - `crates/TEST_TARGET_MAP.toml` counts rise; the floor is rise-only.
  - The cargo acceptance module needs `rustup` and a Rust toolchain on the test host. That is an undeclared host input under the remote-cached `//test:acceptance`. Declare it, or keep the module out of the cached target with a recorded reason.
- **Docs.** C10.
- **Staged rollout.**
  1. Unit tests: index parser, fetch policy, scan/unpack bounds, feature closure, triple table, argv/env builder, scrub list, log relay, request digest, validation fixtures, `versions[].build` derivation, push checks, golden.
  2. Acceptance harness `test/`, as `test/tests/test_mirror_cargo.py` in the `test_mirror_pypi.py` shape. It uses a fixture crate with one dependency, served from a local sparse index plus a `dl` dir. This is a new fixture: `test/src/static_index.py` writes ocx-index trees, not a cargo index. It runs real `cargo` + `rustup` on linux/amd64 and covers:
     - all four commands;
     - **the design § 5.2 reference script, run verbatim**, which is the hand-written non-GitHub job of the acceptance bar;
     - a `$CARGO_HOME/config.toml` with `[source.crates-io] replace-with` pointing at a local registry, plus a crate shipping its own `.cargo/config.toml` that replaces the source. The build must succeed through the operator's replacement.
  3. `/e2e-test` tier 3: one binary-less crate (no upstream release, no C deps, a Linux build of 5 min or less) to `dev.ocx.sh` on all six targets, via the rendered workflow **and** via a GitLab child pipeline generated from `plan.json` alone (design § 5.1).
  4. Later phases.

## Consequences

**Positive.**
- The first mirrorable path for binary-less crates.
- A forge-neutral build marker that tier-3 recipes reuse.
- Per-platform prepare and a one-command hand-off.
- Corporate cargo works through the operator's own config, and the read token is kept from the build child.
- Pipeline provenance on the index.
- `package check` as a free dry run.
- Two pre-existing test-job defects fixed for every kind.

**Negative.**
- The pipeline executes upstream code on build legs, possibly beside `source-read` credentials.
- macOS minutes.
- `plan.json` grows a must-understand field that third parties must implement.
- Push gains a build-kind-only flag requirement and publish rule.
- The build leg loads the project toolchain on every OS.
- No build-kind resume, so a rerun rebuilds.

**Risks.**
- **glibc floor.** A gnu build links the runner's glibc (2.39 on `ubuntu-24.04`). *Mitigation:* docs prefer `+libc.musl` for crates without C dependencies, and the oldest supported runner image otherwise. An older-glibc container test leg reds a too-new binary. Follow-ups: a max-`GLIBC_x.y` symbol check (a lint in create, or an annotation), a container build (`requires: ["container"]`), and zigbuild's glibc pin.
- **Host-only dylibs.** A darwin build can link Homebrew libraries the test runner has and users lack. *Mitigation:* docs recommend vendored features (e.g. `openssl/vendored`). A foreign-dylib lint and tier-2 install_name rewriting are follow-ups.
- **Toolchain drift across legs** when `channel: stable` and a release lands mid-run. *Mitigation:* `rustc` is recorded per leg, and the index says `mixed` when legs disagree (OQ3).
- **`x86_64-apple-darwin` is Tier 2 with host tools, and `macos-15-intel` retires 2027-08.** `platforms.darwin/amd64` is opt-in. A same-OS build on an arm64 runner is the documented path (S18), but Rosetta on hosted images is unverified.
- **`rustup` on hosted arm images** (`ubuntu-24.04-arm`, `windows-11-arm`) is unverified. A missing install is a clean 78; the renderer fix is a setup step on those runners.

## Open questions

- [NEEDS CLARIFICATION: OQ1 — extend the 2026-09-17 ruling so a build leg running upstream `build.rs` may hold the operator's read-only cargo credential in its job environment (Decision 3 (iv): only `cargo fetch` sees it; the build child runs `--offline` with it scrubbed, but same-user code can still reach the parent environ, credential files and runner memory), or require zero-secret build legs now via the vendor split (v), which adds a fetch job type to `plan.json` and every renderer?] Recommended: extend for v1 with (iv). It keeps auth-required Artifactory/Nexus cargo remotes usable and removes the token from the commodity-malware path, and the class is read-only and source-scoped. Docs require an env-supplied, read-only, cargo-scoped token. (v) is the named follow-up behind `build.requires: ["vendor"]`.
- [NEEDS CLARIFICATION: OQ2 — ship `plan.json` v5 with the kind-neutral `versions[].build` marker (Decision 2 A′, 105/120), the `prepare[]` units of pass 1 (C, 89), or the mirror-derived `jobs[]` DAG (B′, 85)?] Recommended: A′. It is the smallest frozen surface that makes build legs renderable, it keeps every v4 rule and fact single-sourced, and B′ can be derived from the same data later when a phase the rules cannot express appears.
- [NEEDS CLARIFICATION: OQ3 — toolchain truth: default `channel: stable` with the actual `rustc -V` recorded (`build.json`, `run-summary.json`, and the index annotation as a value or `mixed`), or require a pinned `x.y.z` channel; and does the durable per-platform `rustc` seat justify an ocx change (manifest-level annotations on `ocx package push`, via `/ocx-upstream-pr`)?] Recommended: default `stable`, record per leg, and annotate the value or `mixed` (v1). Docs recommend pinning. File the ocx change as a follow-up, not a v1 blocker.

**Resolved since the draft (no longer open).**
- Cross-leg tampering is accepted, and `--plan-sha256` blocks plan rewrites (pre-existing class).
- `windows/arm64` stays in the acceptance bar: the native runner is GA, and `exclude` handles `ring`-class failures.
- Build configurability: a fixed `cargo install` with `features`/`no_default_features`/`bins`. There is no `cargo.env` in v1. A free-form script is tier 3.
- Proxy/CA/registry mirrors: inherited (S14).
- Native build deps: the runner image (S16).
- The tier-2 name is "post-process" (S19).
- Slim LLVM: measure, then write the tier-3 ADR.

## Deferred to owner (decisions, not open markers)

- Split `plan.json` v5 (`versions[].build`, `requires`, `containers[].toolchain`) and the credential classes into their own ADR, so a non-cargo reader finds the renderer contract without the cargo detail.
- `pipeline test` in v1 (violation 11). The architect's judgement is **not in v1**. A one-command test leg must port the container prelude, the setup-image build, the per-shell wrappers, JUnit emission with the job-URL property, and the env-kind target resolution (the test-body and container-prelude renderers in `generate/ci/matrix.rs`). That is a cross-kind contract of its own, and its JUnit naming is already documented in `plan-json.md`.
- Refuse to run a build when `CI_JOB_TOKEN` is set and `target` is the job's own `CI_REGISTRY`, instead of documenting the residual.
- An authenticated bundle-digest channel before attestations: build legs report digests through the forge's job outputs, and push verifies them, instead of the accepted residual.

## Implementation Plan (phase-level; `/hex-plan` decomposes)

**v1**
0. [ ] Prerequisite `fix:` commits (every kind, goldens regenerated):
   - the test loop records every version's JUnit, then fails;
   - `test.if` with `!cancelled()`;
   - `pipeline plan` refuses an empty effective test list.
1. [ ] Two-Hats refactors, output-identical: `MirrorTask.build: Option<CargoBuild>` (`None` everywhere); the SSRF-floored builder moves into `ocx_mirror_http`.
2. [ ] Spec: `Source::Cargo`, `CargoConfig` (`crates/ocx_mirror_spec/src/cargo_config.rs`), C1 validation + fixtures, `rust_target_triple`, the `sh.ocx.mirror.` reservation. Deps `tar`, `flate2`, `semver`, `cargo_toml`, with their `BUILD.bazel` edges.
3. [ ] `ocx_mirror_source::cargo`: discovery, `fetch_crate`, `scan_crate`/`unpack_crate`, feature closure, newtypes. `ocx_mirror_error::cargo::classify_error`; `CargoError`, `BuildEnvironment`.
4. [ ] Plan v5: the `plan/cargo.rs` entry function (shared with prepare and check), `.crate` assets, `versions[].cargo`, `versions[].build`, `containers[].toolchain` + parity test, `PLAN_SCHEMA_VERSION = 5`, schema `$id`.
5. [ ] Prepare:
   - `--platform` (archive + build), `--bundles-dir` (all kinds);
   - `requires` and path checks; the work-dir check;
   - `ocx_mirror_pipeline::cargo_build` (rustup, fetch, offline install, env, scrub, static CRT, deployment target, log relay, bins check, request digest);
   - synthesized metadata, `build.json`, no resume.
6. [ ] Push: `--plan` completeness and the planned-only publish rule, zero bundles, `--plan-sha256`, build-record checks, the `mixed` annotation overlay, reasons. `sync`/`patch` refusals; `check` support.
7. [ ] Renderer: cargo discover outputs, prepare matrix, permissions, `--work-dir "$RUNNER_TEMP/…"`, push plan download; golden.
8. [ ] Docs (C10), the ruling paragraph, module-map rows. Acceptance harness (incl. the § 5.2 script and `replace-with`); `TEST_TARGET_MAP.toml` rows; `/e2e-test` tier 3 (rendered + GitLab child pipeline).

**Phase 2:** feature-set variants (S5). **Phase 3:** `cargo.rebuild` (S7). **Phase 4:** SLSA referrer on `push --sign` + `cargo-auditable`.

**Named follow-ups (not designed here):**
- tier-2 ADR; tier-3 ADR;
- cross-OS builds (`requires: ["cross"]`); container builds (`requires: ["container"]`); the vendor split (`requires: ["vendor"]`, Decision 3 (v)); `unshare -n` around the install child;
- `pipeline test`;
- `--bundles-dir` + `push --plan` in the shared template;
- the SSRF floor on all foreign downloads;
- the per-platform `rustc` seat (ocx);
- the max-`GLIBC` check;
- drift/patch for cargo;
- an index-override env var if a second operator needs byte-identical specs;
- aligning missing-`uv` to 78.

## Validation

- [ ] Unit:
  - every C1/C2/C7 refusal has a test, and the triple table is total over the six keys ± libc;
  - no argv token can be built from an unvalidated string (property test: the grammars reject `-x`, `/`, whitespace, `;`, NUL);
  - the scrub list removes each named variable and the pattern; fetch keeps `CARGO_REGISTRIES_X_TOKEN`, and install drops it;
  - the msvc rustflags rule appends to whichever source wins;
  - `\r` and C0/C1 controls never survive the relay;
  - `request_digest` is stable under feature order;
  - `versions[].build` derivation: absent for archive and env kinds, present on every non-drift cargo entry.
- [ ] Golden:
  - after the prerequisites, existing renders are byte-identical;
  - `mirror-cargo.txt` shows `runs-on: ${{ matrix.runner }}` on `prepare`, `persist-credentials: false`, `!cancelled()` on `test`, and `--work-dir "$RUNNER_TEMP/…"`;
  - `grep -c 'secrets\.'` = 0 between `prepare:` and `test:`;
  - there is no flatten script and no toolchain step in `prepare`.
- [ ] Acceptance:
  - the four commands on a local index;
  - a lib-only crate fails at plan with 65;
  - cargo `prepare` without `--platform` exits 64;
  - a red leg yields `Partial` + `missing_bundle`, and all legs red yields every pair `missing_bundle` with exit by verdict;
  - a tampered bundle or a foreign `build.json` yields `build_record_mismatch`;
  - `--bundles-dir` output is accepted by `push` unchanged;
  - the § 5.2 script runs green and reports a failed unit without skipping push;
  - `replace-with` survives a crate-shipped `.cargo/config.toml`.
- [ ] Mixed-version contract tests:
  - a v4 archive plan into v5 `prepare`;
  - a v5 cargo plan rendered by v4 rules (no `--platform`) → 64;
  - a plan with `build.requires: ["x"]` → `prepare` and `push` exit 65 before any fetch or registry call;
  - unknown fields anywhere are ignored.
- [ ] Single-host checklist (S21) verified on the reference script: units run as another user, push credentials are absent from their env, and `plan.json` is read-only to them.
- [ ] The security review reads `security-threat-model.md` and this ADR's ruling paragraph first.
- [ ] `task verify:scoped` locally; CI `task verify`.

## Links

- [`adr_ocx_mirror.md`](./adr_ocx_mirror.md) — base pipeline, build-timestamp tag decision
- [`adr_mirror_source_generators.md`](./adr_mirror_source_generators.md) — first operator-declared subprocess (the tier-3 argv shape)
- [`adr_ocx_python_crate.md`](./adr_ocx_python_crate.md) — sibling-crate precedent (and why it does not apply)
- [`adr_pypi_lock_derivation.md`](./adr_pypi_lock_derivation.md) — plan-time derivation shipped to prepare; credential injection into `uv` (Decision 3 (iii))
- [`adr_mirror_signing.md`](./adr_mirror_signing.md) — the `push --sign` leg phase 4 builds on
- [`adr_bazel_crate_split.md`](./adr_bazel_crate_split.md) — crate map and error boundary (E1/E2)
- [`security-threat-model.md`](../rules/security-threat-model.md), [`subsystem-mirror.md`](../rules/subsystem-mirror.md), [`crate-placement.md`](../rules/crate-placement.md)
- Cargo: [registry index](https://doc.rust-lang.org/cargo/reference/registry-index.html), [cargo-install (configuration discovery)](https://doc.rust-lang.org/cargo/commands/cargo-install.html#configuration-discovery), [cargo-fetch](https://doc.rust-lang.org/cargo/commands/cargo-fetch.html), [source replacement](https://doc.rust-lang.org/cargo/reference/source-replacement.html), [config (rustflags precedence)](https://doc.rust-lang.org/cargo/reference/config.html#buildrustflags), [rust-lang/cargo#8970](https://github.com/rust-lang/cargo/issues/8970); [`cargo_toml`](https://docs.rs/cargo_toml); [GitLab downstream pipelines](https://docs.gitlab.com/ci/pipelines/downstream_pipelines/); [SLSA v1.1 requirements](https://slsa.dev/spec/v1.1/requirements); [cargo-auditable](https://github.com/rust-secure-code/cargo-auditable)

---

## Changelog

| Date | Author | Change |
|------|--------|--------|
| 2026-09-17 | Architect (hex-arch-cargo-20260917) | Initial draft |
| 2026-09-17 | Architect (hex-arch-cargo-20260917) | hex-architect Round-1 panel fixes applied (4 Block, 7 High, 17 Warn) |
| 2026-09-17 | Orchestrator (hex-arch-cargo-20260917) | Re-validation residuals applied (3 Warn, 3 Suggest) |
| 2026-11-08 | Architect (hex-architect revision) | Post-split paths; plan.json v5; `--path` build over a mirror-verified `.crate`; rustup provisioning + `--bundles-dir`; corporate setup inherited + credential classes; tier seams; `cargo.channel`; static CRT; Option B interim withdrawn; OQs reconciled to three |
| 2026-11-08 | Architect (hex-architect Round 1 fixes) | D1 totals corrected; D2 re-decided to A′ `versions[].build` (C withdrawn); D3 re-decided to (iv) fetch/offline, (v) scored; config-discovery parity (S11); single-host isolation (S21); bounded fetch/unpack, entry types, host-gated creds, redirect policy; bins check; `request_sha256` binding; `mixed` annotation semantics; no build resume (S22); prerequisite test-job fixes; `containers[].toolchain`; G1 restated; scrub/relay/rustflags/deployment target; `cargo_toml`; same-OS cross; YAGNI trims; `BuildEnvironment` 78; `requires` refused by prepare/push; Bazel rollout; Deferred-to-owner list |
