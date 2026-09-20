# Research: Cargo Source-Kind — Competitive Landscape for Building Source-Only Crates into Binaries

<!--
Technology Landscape Research
Filename: artifacts/research_cargo_competitive_landscape.md
Owner: Researcher (worker-researcher)
Handoff to: Architect (/architect), Swarm Plan (/swarm-plan)
Related Skills: architect, swarm-plan

Purpose: Persist tech landscape findings to inform ADRs, plans, design decisions.
Artifacts decay — check dates before trusting findings.
-->

## Metadata

**Date:** 2026-09-17
**Domain:** packaging
**Triggered by:** ADR cargo source kind (hex-arch-cargo-20260917)
**Expires:** 2027-03-17

**Builds on:** [research_mirror_per_os_smoke.md](./research_mirror_per_os_smoke.md) (2026-05-12)
— that artifact covers per-OS CI execution strategy (QEMU/Wine/Rosetta, GHA runner
matrix cost tiers, L1 version-smoke testing) for **mirroring already-built binaries**.
This artifact does not re-litigate execution strategy; it extends the same runner-cost
tiering to the **build** step a `cargo` source kind adds, and focuses on the parts that
are new: what to build, how to name variants, how to version rebuilds, and whether to
build at all vs. consume someone else's farm.

## Direct Answer

crates.io ships no binaries by design (it is a source registry), so every ecosystem
that wants a `<tool>` binary either (a) builds it itself on a runner matrix at release
time (cargo-dist, Homebrew bottles, conda-forge, nixpkgs/Hydra, Debian/Fedora, Arch) or
(b) consumes someone else's opportunistic build farm (cargo-quickinstall, consumed by
cargo-binstall and by mise's cargo backend). No ecosystem surveyed treats "build the
crate at mirror time" as a *mirroring* operation — everywhere it happens, it is
modeled as a distinct pipeline stage with its own trust and versioning story, because
building introduces inputs (toolchain version, feature set, lockfile resolution,
transitive dep versions) that a pure asset copy never has. `ocx-mirror`'s proposed
`source.type: cargo` should copy cargo-dist's build discipline (six-triple runner
matrix, `--locked`, per-binary feature sets via `precise-builds`) but not its
distribution model (it ships from the crate owner's own repo, not a third party), and
should treat quickinstall as a *pattern to imitate* (opportunistic tail coverage) not
as an upstream to depend on (no SLA, minisign-only trust, "unofficial" by its own
README).

## Technology Landscape

### Trending (gaining momentum)

| Tool/Pattern | Adoption Signal | Key Benefit | Relevance to ocx-mirror |
|---|---|---|---|
| cargo-binstall | 2,875 GitHub stars, pushed 2026-09-15, `[package.metadata.binstall]` now a de facto standard maintainers add proactively | Multi-source resolution (repo release → quickinstall → source) lets a crate work with zero extra CI once the maintainer adds ~5 lines of metadata | The resolution *order* (self-hosted release first, third-party build farm second, compile last) is the right default order for ocx-mirror to encode in a `cargo` kind's fallback ladder |
| cargo-dist (`dist`) | 2,115 stars, pushed 2026-09-13, 329 open issues (high engagement) | Maintainer-side, in-repo, `--locked` release builder with a fixed 5–7 triple default matrix and generated installers | Closest structural analog to what ocx-mirror is proposing to build — same "build once per release, six/seven triples, GitHub Actions" shape |
| Windows ARM64 GHA runners (`windows-11-arm`) | GA per GitHub Changelog 2024-09-03; Arm's own newsroom post promoting native (non-cross-compiled) builds | Removes the `cargo-xwin`/`ring` cross-compile failure class for `aarch64-pc-windows-msvc` | If ocx-mirror adds a 7th target, native Windows-arm64 runners avoid the C-dependency cross-compile trap several projects hit (e.g. `ring`) |

### Established (proven, widely accepted)

| Tool/Pattern | Status | Notes |
|---|---|---|
| cargo-quickinstall | Mature, 314 stars, still active (pushed 2026-09-16) but explicitly self-described as "a bit like Homebrew Bottles... just a glorified bash script" | Opportunistic community build farm; binaries hosted as **GitHub Releases**, signed with **minisign** (`minisign.pub` in-repo); "non-default features are not supported" — no variant story at all |
| Homebrew bottles + `brew test-bot` | Standard since ~2013 | `rebuild` field is the versioning-without-a-version-bump primitive; bottle DSL keys binaries by OS-tag (`arm64_tahoe:`, `sequoia:`, …), not by architecture triple |
| conda-forge feedstock + `conda_build_config.yaml` | Standard | `build number` + `conda_build_config.yaml` variants (e.g. `microarch_level`) is the most direct analog to "same version, different flags, needs a rebuild that sorts higher" |
| nixpkgs `rustPlatform.buildRustPackage` + `cargoHash`/`cargoLock` | Standard, actively debated (issue #89563 open since 2021: fixed-output-derivation abuse) | `cargoLock = { lockFile = ./Cargo.lock; }` (no separate hash to maintain) is now preferred over `cargoHash`/`cargoSha256`, precisely because a hash-of-vendored-deps drifts out of sync with `Cargo.lock` |
| Arch `cargo build --frozen --release --all-features` | Standard, ArchWiki-documented | `--frozen` (offline + locked) is the reproducibility floor every packaging guideline converges on |
| Fedora `rust2rpm` / one-RPM-per-crate + per-feature Provides | Standard | Fedora and Debian both **unbundle**: every dependency becomes its own distro package; feature flags become their own sub-package/Provides tag. This is the opposite pole from vendoring and is not viable for a CI-speed mirror |

### Emerging (early but promising)

| Tool/Pattern | Signal | Worth Watching Because |
|---|---|---|
| aqua registry `cargo` package type | Documented package-type constant in `aquaproj/aqua-registry` (378 stars, pushed 2026-09-16) | Confirms even a "download binaries" registry ships an explicit `cargo install`-at-install-time escape hatch for crates with no binary — same shape ocx-mirror is contemplating as a fallback for crates a mirror can't yet build for a given platform |
| mise cargo backend delegating to cargo-binstall with exit-code-94 fallback | `mise.jdx.dev/dev-tools/backends/cargo.html`, jdx/mise at 33,994 stars (by far the largest project in this survey) | The **exit code 94 = "no prebuilt artifact"** convention from cargo-binstall is becoming a machine-checkable contract other tools key off — ocx-mirror's own builder should emit a distinguishable "nothing to build for this platform" signal the same way, rather than a generic non-zero |
| `precise-builds` in cargo-dist | Documented `dist` config key | The direct answer to "how do you build per-workspace-member feature variants without rebuilding the whole workspace" — worth stealing verbatim if ocx-mirror ever mirrors workspace crates with divergent feature sets |

### Declining (losing mindshare)

| Tool/Pattern | Signal | Avoid Because |
|---|---|---|
| `cargoSha256`/`cargoHash` (bare vendor-hash) in nixpkgs | Open issue since 2021 flags FOD abuse; docs now steer toward `cargoLock` | A hash computed over vendored deps is a second source of truth that silently goes stale relative to `Cargo.lock` — exactly the "two places, one goes stale" DRY violation `quality-core.md` warns about. Don't invent an equivalent for a `cargo` mirror kind; hash the `Cargo.lock`/lockfile content directly (see cargoLock model) if a build-input hash is needed at all |
| Non-MSVC Windows binary testing via Wine | Covered exhaustively in [research_mirror_per_os_smoke.md](./research_mirror_per_os_smoke.md) — vcruntime failures persistent through 2025 | Not new to this artifact, but reconfirmed: a `cargo` kind building `*-pc-windows-msvc` targets still needs a real `windows-latest`/`windows-11-arm` runner to smoke-test, Wine is a non-starter |

## Design Patterns Worth Considering

- **Fallback ladder, ordered by trust and cost** — cargo-binstall's `repo release → quickinstall → compile` order is a general pattern: prefer the thing the maintainer/owner shipped, then a third-party farm, then pay full build cost. `ocx-mirror`'s `cargo` kind is itself positioned to *be* the middle or first rung for crates it mirrors — a repo operator who runs it stops needing quickinstall/binstall's fallback chain for that crate at all. [github.com/cargo-bins/cargo-binstall](https://github.com/cargo-bins/cargo-binstall)
- **Exit-code-as-contract for "nothing to build here"** — cargo-binstall's exit code 94, consumed by mise to trigger its own `cargo install` fallback, is a clean machine-readable signal. A homegrown `ocx-mirror cargo` builder should adopt an equivalent distinguishable failure mode (e.g., for `required-features` gating a `[[bin]]` out on a given feature set) rather than a bare non-zero exit that looks like a build break. [mise.jdx.dev/dev-tools/backends/cargo.html](https://mise.jdx.dev/dev-tools/backends/cargo.html)
- **Rebuild-without-version-bump primitive** — Homebrew's `rebuild` field and conda-forge's `build number` both solve "same upstream version, different artifact" (toolchain bump, transitive security fix) without touching the semver the user sees. ocx-mirror's OCX tag scheme needs an equivalent monotonic rebuild counter *per platform layer* if a `cargo` kind ever needs to re-cut a build after a Rust toolchain CVE without bumping the mirrored crate's version. [docs.brew.sh/Bottles](https://docs.brew.sh/Bottles), [conda-forge conda_build_config.yaml](https://github.com/conda-forge/conda-forge-pinning-feedstock/blob/main/recipe/conda_build_config.yaml)
- **`--locked`/`--frozen` as the non-negotiable reproducibility floor** — every packaging guideline surveyed (Arch, cargo-dist implicitly via `dist`'s own lockfile handling, Debian/Fedora unbundling) converges on: never let `cargo install`/`cargo build` re-resolve dependencies at build time. A missing or stale `Cargo.lock` should be a hard build failure, not a silent re-resolve — this is also cargo's own upstream direction (`cargo install --locked` failing closed on a stale lock). [wiki.archlinux.org/title/Rust_package_guidelines](https://wiki.archlinux.org/title/Rust_package_guidelines), [doc.rust-lang.org/cargo/commands/cargo-install.html](https://doc.rust-lang.org/cargo/commands/cargo-install.html)
- **`precise-builds` for per-binary feature variance** — cargo-dist's answer to "one crate version, several `[[bin]]`s with different `required-features`, one workspace" is to build each package independently instead of once for the whole workspace when feature sets diverge. Directly reusable naming/config pattern for a mirror spec's `features:` field. [axodotdev.github.io/cargo-dist/book/reference/config.html](https://axodotdev.github.io/cargo-dist/book/reference/config.html)

## Key Findings

1. **crates.io publishes source only; every consumer-facing binary is built somewhere else, by someone else's CI, under someone else's trust model** — there is no "binary registry" to mirror from for cargo the way GitHub Releases already is for most other `ocx-mirror` sources; a `cargo` source kind is a **builder**, not a mirror, and should be named/reasoned about as such internally. [crates.io](https://crates.io) publishes no binaries by design.
2. **cargo-quickinstall is the only opportunistic community build farm in this space, and it has no variant, provenance-beyond-minisign, or feature-flag story**: "Non-default features are not supported," binaries are plain GitHub Release assets signed with a single long-lived minisign key with no key-rotation or transparency-log story documented. [github.com/cargo-bins/cargo-quickinstall](https://github.com/cargo-bins/cargo-quickinstall)
3. **cargo-binstall's fallback order — repo release, then quickinstall, then source compile — is the closest thing this space has to a canonical resolution policy**, and it is what most Rust CLI consumers (directly, or transitively via mise) experience today. [github.com/cargo-bins/cargo-binstall](https://github.com/cargo-bins/cargo-binstall)
4. **mise's cargo backend treats `cargo-binstall`'s exit code 94 ("no prebuilt artifact") as a first-class signal** to fall back to `cargo install`, and explicitly *skips* cargo-binstall entirely the moment a user sets non-default `features` or `default-features = false` — i.e. the existing ecosystem tooling already gives up on prebuilt binaries the instant a feature-variant is requested. [mise.jdx.dev/dev-tools/backends/cargo.html](https://mise.jdx.dev/dev-tools/backends/cargo.html)
5. **cargo-dist's default runner matrix is six triples**: `x86_64-apple-darwin`, `aarch64-apple-darwin`, `x86_64-pc-windows-msvc`, `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, plus musl variants (`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`) available as opt-in — i.e. glibc is the Linux default, musl is opt-in, not the reverse. [axodotdev.github.io/cargo-dist/book/reference/config.html](https://axodotdev.github.io/cargo-dist/book/reference/config.html)
6. **Windows ARM64 (`aarch64-pc-windows-msvc`) is GA-runner-capable (GitHub Actions arm64/Windows runners GA September 2024) but adoption among Rust CLI tools remains blocked by C-dependency cross-compile failures** (e.g. `ring` failing under `cargo-xwin`) when cross-compiled from x86_64 — native Windows-arm64 runners sidestep this, but few projects have flipped the target on yet. [github.blog/changelog/2024-09-03…](https://github.blog/changelog/2024-09-03-github-actions-arm64-linux-and-windows-runners-are-now-generally-available/), [newsroom.arm.com — Windows Arm64 runners](https://newsroom.arm.com/blog/windows-arm64-runners-git-hub-actions)
7. **`required-features` silently drop a `[[bin]]` target from `cargo install`/`cargo build` rather than erroring** — "Binaries are skipped if they have required-features that are missing" — and worse, cargo currently *builds the whole dependency tree before* reporting "no binaries produced," wasting full compile time on a crate that will yield nothing (tracked, unresolved as of survey date). A `cargo` mirror kind must special-case this: inspect `Cargo.toml`/`cargo metadata` for `required-features` before invoking a full build, not after. [github.com/rust-lang/cargo/issues/8970](https://github.com/rust-lang/cargo/issues/8970)
8. **Missing/stale `Cargo.lock` is a hard-fail under `--locked`, by design** — "If the lock file is missing, or it needs to be updated, Cargo will exit with an error" under `--locked`/`--frozen`. Every packaging guideline surveyed (Arch, effectively cargo-dist and nixpkgs `cargoLock`) treats this as correct, fail-closed behavior rather than something to work around by relaxing to an unlocked build. [doc.rust-lang.org/cargo/commands/cargo-install.html](https://doc.rust-lang.org/cargo/commands/cargo-install.html)
9. **Rebuild-without-version-bump is a distinct axis conda-forge and Homebrew both model explicitly and nixpkgs models implicitly via content-addressed hashes** — Homebrew's `rebuild:` field and conda-forge's `build number` both let the *same* upstream version re-publish after a toolchain/dependency-only change; nixpkgs instead gets this for free because `cargoLock`'s hash is a function of `Cargo.lock` content, so a dependency bump changes the hash automatically. ocx-mirror's OCX tags are immutable per push already (project convention) — a `cargo` kind needs an explicit rebuild counter *distinct from* the crate's own version, the same way Homebrew and conda-forge do, because "we rebuilt because our own toolchain moved" is not a fact `Cargo.toml`'s version field can express. [docs.brew.sh/Bottles](https://docs.brew.sh/Bottles), [conda-forge conda_build_config.yaml](https://github.com/conda-forge/conda-forge-pinning-feedstock/blob/main/recipe/conda_build_config.yaml)
10. **Fedora and Debian's Rust packaging model — unbundle every dependency into its own distro package, one binary sub-package per feature — is structurally incompatible with a fast, per-mirror CI build** and should be explicitly rejected as a model, not silently ignored: it optimizes for distro-wide dependency dedup and security-patch fan-out, at the cost of a build graph no single-mirror CI job could reasonably reproduce. [fedoraproject.org/wiki/Changes/Packaging_Rust_applications_and_libraries](https://fedoraproject.org/wiki/Changes/Packaging_Rust_applications_and_libraries), [wiki.debian.org/Teams/RustPackaging/Policy](https://wiki.debian.org/Teams/RustPackaging/Policy)

## Recommendation

Build a `source.type: cargo` kind modeled on **cargo-dist's shape, not cargo-quickinstall's**: `ocx-mirror` already controls per-mirror CI (per `subsystem-mirror.md`'s pipeline phases), so it can run `cargo install <crate>@<ver> --locked --root <dir> [--features ..]` natively across the same six/seven-triple matrix cargo-dist defaults to (`{x86_64,aarch64}-{apple-darwin,unknown-linux-gnu,pc-windows-msvc}`, musl as an explicit opt-in, `aarch64-pc-windows-msvc` behind a flag pending broader ecosystem cross-compile fixes). Concretely:

- **Fail closed, before spending build time**: run `cargo metadata` first to detect `required-features`-gated or lib-only crates, and refuse (not silently skip) a `[[bin]]` target the requested feature set can't produce — don't let cargo discover this after a full compile ([rust-lang/cargo#8970](https://github.com/rust-lang/cargo/issues/8970)).
- **`--locked` unconditionally; missing `Cargo.lock` is a spec error, not a build-time re-resolve.** Never fall back to an unlocked build the way `cargo install` does by default when no lock is present.
- **Feature-set variants get their own tag component**, not a separate crate entry — steal cargo-dist's `precise-builds`/`features` shape rather than inventing one, since mise, cargo-dist, and Arch's `--all-features` default all converge on "features change the artifact identity."
- **Add an explicit rebuild counter separate from the crate's own version**, mirroring Homebrew's `rebuild:`/conda-forge's `build number` — needed the first time a Rust toolchain CVE forces a re-cut with no upstream crate change. Do not try to fold this into a content hash of vendored deps (nixpkgs' `cargoHash` approach is explicitly flagged as an anti-pattern in its own tracker); hash `Cargo.lock` content directly if a build-input fingerprint is wanted at all.
- **Do not consume cargo-quickinstall as an upstream.** It has no SLA, no variant support, single-key minisign trust with no rotation/transparency story, and describes itself as "a glorified bash script" — acceptable for a hobby fallback, not for a mirror `ocx-mirror` operators build supply chains on. Treat it only as prior art for *what* to build (its "most-requested" popularity heuristic is a reasonable seed list for which crates deserve a `cargo` mirror spec at all) — the same "build vs. mirror an existing binary" decision cargo-binstall's own priority order already encodes: only reach for `cargo` as a source kind when no upstream binary release and no better-trusted farm exists for that crate.
- **Do not chase Debian/Fedora-style unbundling.** It solves a different problem (distro-wide dependency dedup) at a build-graph cost incompatible with per-mirror CI turnaround.

## Sources

| Source | Type | Date | Relevance |
|---|---|---|---|
| [cargo-bins/cargo-quickinstall README](https://github.com/cargo-bins/cargo-quickinstall/blob/main/README.md) | Repo/Docs | Checked 2026-09-17 (repo pushed 2026-09-16) | Build-farm decision heuristic, hosting, minisign trust, no-variant limitation |
| [cargo-bins/cargo-binstall README](https://github.com/cargo-bins/cargo-binstall) | Repo/Docs | Checked 2026-09-17 (repo pushed 2026-09-15) | Fallback resolution order, `[package.metadata.binstall]` schema |
| [axodotdev/cargo-dist config reference](https://axodotdev.github.io/cargo-dist/book/reference/config.html) | Docs | Checked 2026-09-17 (repo pushed 2026-09-13) | Default target matrix, `precise-builds`, installer types |
| [docs.brew.sh/Bottles](https://docs.brew.sh/Bottles) | Docs | Checked 2026-09-17 | `rebuild:` field semantics, OS-tag keyed bottle DSL |
| [docs.brew.sh/BrewTestBot-For-Maintainers](https://docs.brew.sh/BrewTestBot-For-Maintainers) | Docs | Checked 2026-09-17 | `brew test-bot`, `--HEAD` build validation |
| [conda-forge conda_build_config.yaml (pinning feedstock)](https://github.com/conda-forge/conda-forge-pinning-feedstock/blob/main/recipe/conda_build_config.yaml) | Repo | Checked 2026-09-17 | Build variants / microarch levels as the feature-set analogue |
| [conda-build variants docs](https://docs.conda.io/projects/conda-build/en/stable/resources/variants.html) | Docs | Checked 2026-09-17 | Build-number-vs-variant mechanics |
| [nixpkgs rust.section.md](https://github.com/NixOS/nixpkgs/blob/master/doc/languages-frameworks/rust.section.md) | Docs | Checked 2026-09-17 | `cargoHash` vs `cargoLock`, fixed-output derivation model |
| [NixOS/nixpkgs#89563 — buildRustPackage FOD abuse](https://github.com/NixOS/nixpkgs/issues/89563) | Issue (open since 2021) | **Flagged: >18 months old**, still open/unresolved 2026-09-17 | `cargoHash` anti-pattern documented by the project itself |
| [ArchWiki — Rust package guidelines](https://wiki.archlinux.org/title/Rust_package_guidelines) | Docs | Checked 2026-09-17 | `--frozen --release --all-features` reproducibility baseline |
| [Fedora — Packaging Rust applications and libraries](https://fedoraproject.org/wiki/Changes/Packaging_Rust_applications_and_libraries) | Docs | **Flagged: undated wiki page, verify currency** | Unbundling / per-feature RPM Provides |
| [Debian — Teams/RustPackaging/Policy](https://wiki.debian.org/Teams/RustPackaging/Policy) | Docs | **Flagged: undated wiki page, verify currency** | Per-feature binary package generation |
| [mise cargo backend docs](https://mise.jdx.dev/dev-tools/backends/cargo.html) | Docs | Checked 2026-09-17 (repo pushed 2026-09-16, 33,994 stars) | binstall delegation, exit-code-94 fallback, features force source build |
| [aqua-registry / aquaproj Go package docs (pkg.go.dev)](https://pkg.go.dev/github.com/aquaproj/aqua/v2/pkg/config/registry) | Docs (generated) | Checked 2026-09-17 | Confirms `cargo` as a first-class aqua package type alongside `github_release`/`go_install` |
| [houseabsolute/ubi](https://github.com/houseabsolute/ubi) | Repo | Checked 2026-09-17 (595 stars, pushed 2026-09-12) | Confirms ubi is download-only (GitHub/GitLab release assets), not a builder — out of scope as a `cargo` kind analog |
| [rust-lang/cargo#8970](https://github.com/rust-lang/cargo/issues/8970) | Issue (open) | Checked 2026-09-17 | `required-features` skip semantics + wasted build-before-error behavior |
| [doc.rust-lang.org/cargo/commands/cargo-install.html](https://doc.rust-lang.org/cargo/commands/cargo-install.html) | Official docs | Checked 2026-09-17 | `--locked`/`--frozen` semantics, missing-lockfile hard-fail |
| [GitHub Changelog — arm64 Linux/Windows runners GA](https://github.blog/changelog/2024-09-03-github-actions-arm64-linux-and-windows-runners-are-now-generally-available/) | Official changelog | **>18 months old (2024-09-03)** — still current per Arm's own newer promotional post | Native Windows-arm64 runner availability |
| [Arm Newsroom — Windows Arm64 GHA runners](https://newsroom.arm.com/blog/windows-arm64-runners-git-hub-actions) | Vendor blog | Checked 2026-09-17 | Confirms native (non-cross-compiled) Windows-arm64 build viability |
| [rustc platform support — aarch64-unknown-linux-musl](https://doc.rust-lang.org/rustc/platform-support/aarch64-unknown-linux-musl.html) | Official docs | Checked 2026-09-17 | Tier-2 musl target confirmation |
| `gh api repos/<owner>/<repo>` for cargo-quickinstall, cargo-binstall, cargo-dist, ubi, aqua-registry, mise | Live API pull | 2026-09-17 | Stars + `pushed_at` recency used to rank Trending/Established/Emerging above |
