# Research: `cargo install`-based mirror source (`source.type: cargo`)

<!--
Technology Landscape Research
Filename: artifacts/research_cargo_tooling.md
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

## Direct Answer

`cargo install <crate>@<ver> --locked --root <dir>` is a viable, boring adapter
primitive: it needs no new mirror-owned index parser for the *build* leg (cargo
itself resolves crates.io), produces a predictable `<root>/bin/*` + a
machine-readable `.crates2.json` receipt, and runs natively on all six GA
runner targets ocx-mirror needs except two ARM legs that require self-hosted or
cross-compiled fallback. The one piece worth hand-parsing directly (bypassing
`cargo install`) is the **sparse index JSON-lines format**, needed to resolve
`<crate>@<ver>` → download URL/checksum *before* invoking cargo, for spec
validation and dry-run/plan phases — that part has no dependency-shaped
shortcut, `tame-index` is the right library if a crate is used at all.

## Technology Landscape

### Established (proven, widely accepted)

| Tool/Pattern | Status | Notes |
|---|---|---|
| `cargo install --locked --root <dir>` | Standard, stable since Rust 1.0-era (`--root` day one); `--locked` semantics stable since 1.37 | The only supported way to build-and-place a crate's binaries; no alternative tool competes here |
| crates.io sparse index (`index.crates.io`) | Default registry protocol since Rust 1.70 (June 2023) | Plain HTTPS, no auth, no rate-limit documented for reads; static.crates.io CDN serves `.crate` tarballs |
| `Swatinem/rust-cache` | De facto standard GHA cache action, active 2026 | Keys cache on `rustc` version + `Cargo.lock` hash; use for repeated per-target rebuilds |

### Emerging (early but promising)

| Tool/Pattern | Signal | Worth Watching Because |
|---|---|---|
| `tame-index` (EmbarkStudios) | 31★, pushed 2026-04-27, 5 open issues — small but focused fork of `crates-index` built specifically for sparse-index + async reqwest fetch | Purpose-built for exactly this adapter's "resolve version → cksum/deps before build" need; avoids hand-rolling index-key prefix rules (1/2/3/ab/cd) |
| `cargo-zigbuild` | 2,652★, pushed 2026-09-14 (days old) | Most active of the cross-build fallbacks; covers musl and Linux→macOS(-ish) cross, but documented Darwin-framework linking gaps |

### Declining / lower-confidence

| Tool/Pattern | Signal | Avoid Because |
|---|---|---|
| `crates-index` (frewsxcv, original) | 75★, pushed 2026-09-03, 11 open issues, but `tame-index` explicitly forked it "with many improvements" | Upstream itself points users to the fork for new work |
| Hand-parsing `.crates2.json` as a stable contract | Format is `#[non_exhaustive]`-shaped internally (`other: BTreeMap<String, Value>` catch-all for forward-compat), never documented in the Cargo Book | Treat as a receipt to *read after the fact for verification*, not as an API to build logic against — no stability guarantee, only source-level evidence (see Key Finding 3) |

## Design Patterns Worth Considering

- **Resolve-then-invoke split** — do version/checksum resolution against the sparse index yourself (for spec validation, dry-run, and pinning `Cargo.lock`-less crates before committing to a build), then hand the resolved `<crate>@<version>` to `cargo install --locked` as a black box. Mirrors how `cargo-binstall` and `cargo-dist` structure their installers. This avoids ever needing to parse Cargo's own dependency resolver.
- **Receipt-as-verification, not receipt-as-contract** — read `.crates2.json` / `cargo install --list` *after* a successful install purely to assert "yes, these bin names got installed", never to drive control flow, given the format's non-exhaustive/undocumented status.
- **Runner-native for Tier-1 hosted targets, explicit fallback lane for the rest** — GitHub-hosted runners now cover 4 of 6 target/OS combos natively (Linux x64/arm64, Windows x64, macOS arm64/x64); only `windows-11-arm` and the musl pair need either `windows-11-arm` (GA, but check GHA runner pricing/label — see Key Finding 6) or `cross`/`cargo-zigbuild` fallback.

## Key Findings

1. **`cargo install` target selection**: default is all `[[bin]]` targets (`--bins`); a bin whose `required-features` aren't satisfied by the selected feature set is silently **skipped**, not an error — `cargo install --bins` errors only if *zero* bins end up selected ([rust-lang/cargo#10289](https://github.com/rust-lang/cargo/issues/10289)). A lib-only crate fails outright: "specified package has no binaries" ([rust-lang/cargo#3821](https://github.com/rust-lang/cargo/issues/3821), [#2308](https://github.com/rust-lang/cargo/issues/2308)) — treat this as the adapter's hard-fail signal for "not an installable crate."
2. **`--locked` vs `--frozen`**: `--locked` freezes the lockfile (`lock_update_allowed() = false`) but still allows network; `--frozen` additionally blocks network (`network_allowed() = false`, equivalent to `--offline` + `--locked` combined) — implementation confirmed in `context/mod.rs` via Context7 (`/rust-lang/cargo`, `src/context/mod.rs`). Neither flag sandboxes build-script network/FS access — build scripts run unconstrained regardless (relevant to `security-threat-model.md`: a malicious crate's `build.rs` is an in-scope supply-chain surface, not mitigated by these flags).
3. **`.crates2.json` schema** (`CrateListingV2`, from `cargo::ops::common_for_install_and_uninstall`, doc.rs + raw source fetch):
   ```rust
   struct CrateListingV2 {
       installs: BTreeMap<PackageId, InstallInfo>,
       other: BTreeMap<String, Value>,   // forward-compat catch-all
   }
   struct InstallInfo {
       version_req: Option<String>,
       bins: BTreeSet<String>,           // installed binary names (no .exe suffix confirmed in source; Windows behavior undocumented — verify empirically)
       features: BTreeSet<String>,
       all_features: bool,
       no_default_features: bool,
       profile: String,
       target: Option<String>,
       rustc: Option<String>,
       other: BTreeMap<String, Value>,
   }
   ```
   The `PackageId` map key is documented in-repo only as a `"name version (source-kind+url)"`-shaped string; no stable public schema doc exists in the Cargo Book — **not a format to parse for control flow**, only to eyeball/verify. `cargo install --list` (human-readable, stable CLI surface) is the safer machine-adjacent receipt if parsing is unavoidable: [doc.rust-lang.org/…/cargo-install.md](https://github.com/rust-lang/cargo/blob/master/doc/book/src/commands/cargo-install.md).
4. **`Cargo.lock` in packaged `.crate`**: always included since Rust/Cargo **1.37** for crates with a binary target ("`publish-lockfile` unstable flag removed"); **since Cargo 1.84** ([rust-lang/cargo#14815](https://github.com/rust-lang/cargo/pull/14815)) it's **always** included regardless of bin/lib — so `--exclude-lockfile` is the only way a `.crate` ships without one. Cargo Book: [reference/unstable.md#publish-lockfile](https://github.com/rust-lang/cargo/blob/master/doc/book/src/reference/unstable.md), [reference/publishing.md](https://doc.rust-lang.org/cargo/reference/publishing.html).
5. **Sparse index protocol**, confirmed via Context7 (`/rust-lang/cargo`, `reference/registry-index.md`) and [doc.rust-lang.org/cargo/reference/registry-index.html](https://doc.rust-lang.org/cargo/reference/registry-index.html): URL `https://index.crates.io/<prefix>/<name>` (1-char/2-char names: `1/<name>`, `2/<name>`; 3-char: `3/<first-char>/<name>`; 4+: `<first-2>/<next-2>/<name>`), newline-delimited JSON, one line per published version. Confirmed fields: `name`, `vers`, `deps[]` (`name`, `req`, `features`, `optional`, `default_features`, `target`, `kind`, `registry`, `package`), `cksum` (SHA-256 of the `.crate` tarball), `features`, `features2` (namespaced/weak-dep syntax, needs `"v": 2`), `yanked`, `links`, `v` (schema version), `rust_version` (optional), `pubtime` (optional, ISO-8601 subset). `config.json` at index root: `{"dl": "...", "api": "...", "auth-required": bool}` — crates.io's `dl` expands to `https://static.crates.io/crates/{crate}/{crate}-{version}.crate` pattern via the documented `{crate}`/`{version}`/`{prefix}`/`{sha256-checksum}` markers. No auth or documented rate limit for reads against `index.crates.io`. **Default protocol since Rust 1.70** (June 2023) — this is a mature, load-bearing surface, not novel.
6. **Runner matrix (fetched from `actions/runner-images` README, 2026-09-17 snapshot)**: all of `ubuntu-24.04`/`ubuntu-24.04-arm`/`ubuntu-22.04`/`ubuntu-22.04-arm` are **GA**. macOS: `macos-15`/`macos-15-xlarge` (arm64) and `macos-15-large`/`macos-15-intel` (x64) are **GA**; `macos-14`/`macos-14-large` are **deprecated**; `macos-26`(`-large`) is the new `-latest`. Windows: `windows-2025`/`windows-latest` (x64) **GA**; `windows-11-arm`/`windows-11-vs2026-arm` (arm64) **GA**. Source: [actions/runner-images README](https://github.com/actions/runner-images/blob/main/README.md) (live doc, self-dating). Cross-check via `gh api repos/actions/runner-images/releases` shows active `win25`/`win11-arm64` release trains as of 2026-09-15 — the labels are live, not stale.
7. **Rust target tier status** (rustc platform-support docs, fetched 2026-09-17): `aarch64-pc-windows-msvc`, `aarch64-apple-darwin`, `aarch64-unknown-linux-gnu` are **Tier 1 with host tools**; `x86_64-apple-darwin` has been **demoted to Tier 2 with host tools** (reflects Apple's Intel wind-down — flag this as a source worth re-verifying near the expiry date, since tier status is a policy call the Rust project revisits); `x86_64-unknown-linux-musl` / `aarch64-unknown-linux-musl` are **Tier 2 without host tools**. [doc.rust-lang.org/rustc/platform-support.html](https://doc.rust-lang.org/rustc/platform-support.html).
8. **GHA cost multipliers** (2026 pricing, WebSearch-aggregated from GitHub's own changelog + third-party trackers, cross-check the primary source before budgeting): Linux 2-core ≈ $0.006/min (1×), Windows ≈ $0.010/min (2×), macOS ≈ $0.062/min (10×). Free-tier minutes drain at these same multipliers. Primary source: [GitHub Actions pricing changelog, 2025-12-16](https://github.blog/changelog/2025-12-16-coming-soon-simpler-pricing-and-a-better-experience-for-github-actions/) — **verify against `github.com/pricing` directly before finalizing a cost model**, third-party aggregator numbers are secondary.
9. **Cross-compile fallbacks are healthy but not native-runner replacements**: `cargo-zigbuild` (2,652★, commit 2026-09-14) is the most active, covers musl targets well, but has a documented Darwin-framework-linking gap requiring `SDKROOT` workarounds for macOS cross-targets. `cross` (8,317★, pushed 2026-08-19, 260 open issues — much heavier issue backlog) is the Docker-based heavyweight option. `cargo-xwin` (633★, pushed 2026-09-07) handles Windows-MSVC-from-Linux specifically. None of these are needed if the ADR's runner matrix sticks to GitHub-hosted labels for all 6 targets (Key Finding 6 shows GA coverage exists for all 6 without cross-compilation) — **recommendation: don't add a cross-compile dependency at all**, run natively on-label everywhere, which the runner matrix now supports end to end.
10. **`--target-dir` isolation**: when unset, `cargo install` creates a `cargo-install`-prefixed tempdir automatically (confirmed in Cargo source via Context7, `src/ops/cargo_install.rs`) — the adapter does **not** need to manage a scratch build dir itself unless it wants build-cache reuse across versions, in which case set `CARGO_TARGET_DIR` explicitly (also configurable via `build.target-dir` config or `CARGO_BUILD_TARGET_DIR`).
11. **`cargo install --message-format json`**: only affects `--list`'s file-listing format per the CLI docs; the *build itself* emits the standard `cargo build`-style JSON stream (`compiler-artifact`, `build-script-executed`, `build-finished` reasons) when the flag is threaded through — **not a purpose-built "install receipt"**, same generic message stream as any cargo build. Confirms Key Finding 3's conclusion: there is no first-class structured install receipt; `.crates2.json` (read-only, best-effort) or `cargo install --list` (stable CLI, human-format) are the two real options.
12. **`Swatinem/rust-cache`**: keys on the active `rustc` version + `Cargo.lock` hash; place after toolchain setup. For a matrix mirroring many crate versions, prefer a `prefix-key`/`shared-key` scheme keyed on the *mirrored crate name*, not a single shared cache — otherwise cache thrashing across unrelated crates being mirrored in the same workflow. [Swatinem/rust-cache](https://github.com/Swatinem/rust-cache).

## Recommendation

Build the `cargo` source adapter as: **(a)** a thin resolve step against the crates.io sparse index (hand-rolled `reqwest` + `serde` GET of `https://index.crates.io/<prefix>/<name>`, parsing the documented JSON-lines schema in Key Finding 5 — this is ~30 lines, doesn't clear the bar for a new dependency per `CLAUDE.md`'s "no new dep for what a few lines do," and the repo already has both `reqwest`+`serde`; skip `tame-index` unless retry/caching/rate-limit-handling logic grows past what's trivial) to validate `<crate>@<version>` exists and fetch `cksum` for a pre-build integrity assertion; **(b)** shell out to `cargo install --locked --root <tmp> <crate>@<version> [--target <triple>] [--bin <name>]` as the actual build, treating a "no binaries"/`required-features`-empty-selection outcome as a hard spec error; **(c)** read back `<root>/bin/*` directly off disk as the packaging source of truth — use `cargo install --list` only as a human-debuggable cross-check, never parse `.crates2.json` in the hot path (Key Finding 3). Runner matrix: run **natively on GitHub-hosted labels for all six targets** — `ubuntu-24.04`/`ubuntu-24.04-arm` (Linux x64/arm64), `macos-15`/`macos-15-large` (macOS arm64/x64), `windows-2025` (Windows x64), `windows-11-arm` (Windows arm64) — all GA as of this snapshot (Key Finding 6); do **not** add `cross`/`cargo-zigbuild`/`cargo-xwin` as a dependency of the ADR's happy path, keep them noted as a documented fallback for non-GitHub CI operators per the "ship the command" principle in `CLAUDE.md`. Budget macOS/Windows minutes at their 10×/2× multipliers (Key Finding 8) when estimating monthly cost — macOS will dominate the bill for any multi-version-per-month cadence.

## Sources

| Source | Type | Date | Relevance |
|---|---|---|---|
| [Cargo Book — cargo-install.md](https://github.com/rust-lang/cargo/blob/master/doc/book/src/commands/cargo-install.md) | Docs | Current (master) | `cargo install --list`, target selection |
| [Cargo `install.rs` CLI source](https://github.com/rust-lang/cargo/blob/master/src/bin/cargo/commands/install.rs) | Source | Current (master) | `--root`, `--target-dir`, `--version` flag definitions |
| [Cargo `context/mod.rs`](https://github.com/rust-lang/cargo/blob/master/src/context/mod.rs) | Source | Current (master) | `--locked` vs `--frozen` vs `--offline` semantics |
| [rust-lang/cargo#10289](https://github.com/rust-lang/cargo/issues/10289) | Issue | Open | `--bins` + `required-features` skip-vs-error behavior |
| [rust-lang/cargo#3821](https://github.com/rust-lang/cargo/issues/3821) | Issue | Open | "specified package has no binaries" error text |
| [rust-lang/cargo#2308](https://github.com/rust-lang/cargo/issues/2308) | Issue | Open | Corroborates no-binaries error scenario |
| [reference/unstable.md — publish-lockfile](https://github.com/rust-lang/cargo/blob/master/doc/book/src/reference/unstable.md) | Docs | Current | Cargo.lock always included for bin crates since 1.37 |
| [rust-lang/cargo#14815](https://github.com/rust-lang/cargo/pull/14815) | PR | Merged, landed for Cargo 1.84 | Cargo.lock always included for *all* published crates |
| [doc.rust-lang.org/cargo/reference/registry-index.html](https://doc.rust-lang.org/cargo/reference/registry-index.html) | Docs | Current | Sparse index JSON schema, `config.json` fields |
| [doc.rust-lang.org — CrateListingV2](https://doc.rust-lang.org/stable/nightly-rustc/cargo/ops/common_for_install_and_uninstall/struct.CrateListingV2.html) | Rustdoc | Current | `.crates2.json` top-level struct |
| [rust-lang/cargo — common_for_install_and_uninstall.rs](https://github.com/rust-lang/cargo/blob/master/src/ops/common_for_install_and_uninstall.rs) | Source | Current (master) | `InstallInfo` struct fields (raw-fetched) |
| [actions/runner-images README](https://github.com/actions/runner-images/blob/main/README.md) | Docs (live) | Fetched 2026-09-17 | GA/preview/deprecated status of all hosted runner labels |
| [actions/runner-images releases (gh api)](https://github.com/actions/runner-images/releases) | Repo API | Fetched 2026-09-17 | Confirms active `win25`/`win11-arm64` release trains |
| [doc.rust-lang.org/rustc/platform-support.html](https://doc.rust-lang.org/rustc/platform-support.html) | Docs | Fetched 2026-09-17 | Target tier classification (flag: tier policy for `x86_64-apple-darwin` worth re-checking before ADR expiry) |
| [GitHub Actions pricing changelog](https://github.blog/changelog/2025-12-16-coming-soon-simpler-pricing-and-a-better-experience-for-github-actions/) | Changelog | 2025-12-16 (~9 months old) | Per-minute rates; **re-verify against github.com/pricing directly**, this artifact only cross-checked via WebSearch aggregation, not a primary fetch |
| [tame-index (crates.io)](https://crates.io/crates/tame-index) | Registry | Metadata dated 2026-01-07, repo pushed 2026-04-27 | Sparse-index client library candidate |
| [frewsxcv/rust-crates-index (gh api)](https://github.com/frewsxcv/rust-crates-index) | Repo API | Fetched 2026-09-17 | 75★, pushed 2026-09-03 — original, superseded by `tame-index` for new work |
| [rust-cross/cargo-zigbuild (gh api)](https://github.com/rust-cross/cargo-zigbuild) | Repo API | Fetched 2026-09-17 | 2,652★, pushed 2026-09-14 — most active cross fallback |
| [cross-rs/cross (gh api)](https://github.com/cross-rs/cross) | Repo API | Fetched 2026-09-17 | 8,317★, pushed 2026-08-19, 260 open issues — heavy but capable fallback |
| [rust-cross/cargo-xwin (gh api)](https://github.com/rust-cross/cargo-xwin) | Repo API | Fetched 2026-09-17 | 633★, pushed 2026-09-07 — Windows-MSVC-from-Linux fallback |
| [Swatinem/rust-cache](https://github.com/Swatinem/rust-cache) | Repo/Docs | Fetched 2026-09-17 | GHA cache action, keying strategy |
| `ocx-mirror/CLAUDE.md` — Dependency model | Project doc | Current | Constrains adapter to reuse existing `reqwest`+`serde` over adding `tame-index`/`crates-index` |
