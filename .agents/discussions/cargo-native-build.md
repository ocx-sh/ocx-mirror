# Discussion: Mirror cargo crates by compiling them natively

State: handed-off → architect · Updated: 2026-11-08
Ratified: 2026-11-08 → architect
Confidence: ratified by Michael Herwig (invoked /hex-architect after the restate); research vintages 2026-09-17 (cargo tooling, build supply chain, competitive landscape), 2026-11-02 (CI portability), 2026-11-08 (slim LLVM)

## Intent

Owner wants packages that upstream does not ship as usable binaries: binary-less crates.io crates, compiled natively per platform; must work on GitLab (incl. self-managed) and in corporate setups (proxy, CA, registry mirrors, extra native deps). Widened 2026-11-08: also *custom builds of tools upstream does ship* — e.g. one slim LLVM prefix (a few tools, ~30–60 MB vs the ~1.6 GB official release), and post-processing published binaries (bundle/reference DLLs, patch RPATH) so they are more self-contained. Ecosystems beyond cargo are in view: pypi already has build-ish steps, npm may follow.

Out of scope for this hand-off: implementing tier 2 (post-processing: file subset, strip, RPATH/DLL) and tier 3 (operator recipes, e.g. slim LLVM) — the cargo ADR only names their seams; each gets its own follow-up ADR. Cross-compilation. A mirror-rendered GitLab pipeline (GitLab renders from `plan.json`). Modelling proxies, CAs, or registry mirrors in the spec.

## Requirements

- Every capability stays one pasteable command per CI job (`AGENTS.md` principle); GitHub workflow rendering is a convenience only.
- New upstream versions must be detected and rebuilt without a second discovery system — whatever builds a package, the mirror's existing discovery (`plan`) decides *when*.
- Corporate cargo/proxy/CA setup is the operator's environment, not spec data (pending owner confirmation, see Open questions).
- **Renderer neutrality (owner, 2026-11-08):** GitLab instances differ too much for the mirror to render one GitLab pipeline; another renderer (GitLab's own, Jenkins, an operator script) must be able to render the *whole* pipeline — build legs included — from mirror output alone. `plan.json` (`docs/reference/plan-json.md`) is already declared that contract, but today it only carries *test* legs (`legs.<platform>`); prepare is one job per version and the GitHub renderer does not read `legs` (`.claude/rules/subsystem-mirror.md`, `plan/legs.rs` row). Build legs must be fully described there: command, platform, environment requirements, artifacts in/out, ordering build → test → push.
- Build legs may need per-platform environment overrides (runner, image, toolchain, env).

## Decisions

- Outcome: revise `.claude/artifacts/adr_cargo_source_kind.md` via `/hex-architect` (owner, 2026-11-08) — not a fresh ADR.
- Line under discussion (proposed 2026-11-08, not yet confirmed): one pipeline (discover → acquire → transform → package → push), three transform tiers, split by **who owns the recipe**, test = "can the spec say it as data, with mirror code that executes it identically for every package of that kind?":
  1. **Source kind** (mirror): the ecosystem's package manager *is* the recipe — `cargo install --locked`, pypi wheels→venv (exists), npm later. Spec carries parameters only.
  2. **Tree transforms** (mirror, any source kind): generic data-declared post-processing — file subset, strip, RPATH rewrite / DLL bundling. Applies equally to a `github_release` LLVM tarball and a cargo build. None exist today (nearest precedent: `strip_components`, `bin_scan`).
  3. **Recipe** (operator-owned code, not mirror code): per-package build script (cmake flags, stage builds, patches). The mirror still discovers versions and plans legs; per leg `prepare` runs the recipe under a fixed contract (in: version, platform, fetched source; out: a tree), then tier 2, then the unchanged bundle → push path.
- Revised 2026-11-08 after owner pushback (cargo is real compilation, pypi/npm are not): the three tiers sort by *who owns the recipe*; a second, orthogonal axis sorts by **where it must run** — and that axis is what the renderer contract sees:
  - **Fetch legs** — github_release, pypi, npm, tier-2 tree ops: run on any host, one `prepare` per version (today's `plan.json` shape).
  - **Build legs** — cargo (tier 1) and recipes (tier 3): need a target-capable build environment, one `prepare` per (version × platform), with environment requirements. New for `plan.json` and every renderer; cargo is the first source kind that needs them, so the cargo ADR revision defines them.
- ADR revision scope (owner, 2026-11-08): cargo + named seams for tier 2 and tier 3; both get their own follow-up ADRs.
- Tier 3 contract shape (owner, 2026-11-08): spec-declared command + env contract (generator-style argv; mirror passes version, platform, source dir, output dir; fails closed on an empty/invalid tree).
- Tier naming: left to the ADR (owner unsure about "transform"; `patch` is taken by `pipeline patch`).
- **Environment overrides (owner, 2026-11-08): spec states needs, renderer maps.** This is already the existing design, not a new mechanism: `platforms.<p>.runner` is a label set that `plan.json` carries as `legs.<p>.runner` — "GitHub maps it to `runs-on`, GitLab to `tags`" (`docs/reference/plan-json.md` › `legs.<platform key>`). A GitLab operator writes their own labels in their spec or remaps in their own renderer; the mirror models no forge. The cargo ADR's build leg reuses the same per-platform `runner` (ADR S4/S6, `prepare --platform P`).
- **Native build per target; no cross-compile in v1** (derived 2026-11-08 from the existing design). Every `legs.<p>` already needs a runner of that platform to run `ocx package test` natively (`tests` is never empty), so a target with no native runner cannot be tested either; cross-compiling would save runner cost, not availability. Operators without a macOS/Windows runner use `platforms.<p>.exclude` / omit the platform. Cross (zigbuild/xwin) stays a named follow-up for cost.
- Superseded: "bespoke builds live in a separate builder repo publishing releases" — dropped because it duplicates discovery (owner's point, 2026-11-08).
- `dist` is not a usable name for tier 3: `ocx-mirror dist` already mirrors ocx's bootstrap layer (`crates/ocx_mirror_spec/src/dist.rs`).

## Research

- Recon 2026-11-02: no `Source::Cargo` exists (`crates/ocx_mirror_spec/src/source.rs:24`). `plan.json` `legs` (runner label sets) is already the forge-neutral job list (`docs/reference/plan-json.md`); push already consumes bundles built by separate prepare legs (`push/bundles.rs`). No build precedent anywhere: pypi skips sdists. Build-time code execution has no clause in `security-threat-model.md`. No file-subset or RPATH transform exists.
- Prior art 2026-11-02 → `.claude/artifacts/research_cargo_native_build_ci_portability.md`. Self-managed GitLab has no hosted macOS/Windows runners (operator registers own); gitlab.com's are beta. Cargo's proxy/CA/mirror knobs are all env or `$CARGO_HOME/config.toml` (`cargo install` ignores project config). zigbuild + xwin let one Linux host reach Linux, macOS (needs SDK) and Windows-MSVC — cargo-dist's fallback.
- Checked 2026-11-08: LLVM `llvmorg-23.1.5` ships prebuilt Linux X64/ARM64, macOS ARM64, Windows x64/arm64 MSVC on GitHub Releases; only macOS x86_64 missing.
- Slim LLVM 2026-11-08 → `.claude/artifacts/research_slim_llvm_package_size.md`. No evidence a file subset reaches 30–60 MB for clang+lld; only single tools are that small (clang-format wheel ~2 MB, clang-tidy 28–46 MB). Official release builds all projects with thin LTO and no dylib → tools likely static. Per-binary sizes unmeasured — measure before deciding tier 2 vs tier 3.

## Related

- `.claude/artifacts/adr_cargo_source_kind.md` — Proposed 2026-09-17, option A (build inside `prepare --platform P`); rejected option C (`local` source, build in rendered bash) as untestable and unable to fail closed before runner spend. Stale: pre-crate-split paths; says plan.json schema 3→4 (already v4); assumes GitHub-hosted native runners for all six targets.
- `.claude/artifacts/system_design_cargo_source_kind.md`, `research_cargo_{tooling,build_supply_chain,competitive_landscape}.md` — companions.
- `.agents/worktrees/hermetic-llvm` (branch `build/hermetic-llvm`, unpushed) — Bazel llvm toolchain cross-builds darwin from Linux; evidence the cross-compile path works in this repo.

## Open questions

- [NEEDS CLARIFICATION: Cargo build configurability — fixed `cargo install` + spec fields (features, `--bin`, env), or a free-form script?]
  Recommended: fixed command + fields — keeps plan-time fail-closed; a script is tier 3.
- [NEEDS CLARIFICATION: Cargo/proxy/CA setup — inherit operator env + `$CARGO_HOME`, or a spec `cargo:` block?]
  Recommended: inherit — matches the "runners, proxies, auth are not ours to model" principle.
- [NEEDS CLARIFICATION: Native build deps (openssl, protoc, cmake) — runner image / `containers:`, or ocx packages declared in the spec?]
  Recommended: runner image for v1.
- [NEEDS CLARIFICATION: Tier name for generic post-processing ("transform" disliked).]
  Recommended: "post-process" — says when it runs; `patch` is taken.
- [NEEDS CLARIFICATION: Slim LLVM — does a file subset of the official 23.x tarball reach 30–60 MB, or is it a tier-3 rebuild?]
  Recommended: measure `bin/clang`, `bin/lld`, `lib/clang/NN` sizes before the tier-3 ADR — not this one.

## Verification

The revised ADR (`.claude/artifacts/adr_cargo_source_kind.md`) is accepted when:

- Its paths match the post-split crate layout (`crates/crate_map.toml`; source in `ocx_mirror_source`, pipeline in `ocx_mirror_pipeline`), and the `plan.json` schema bump it names starts from the shipped v4.
- `plan.json` alone is enough to render the full cargo pipeline: build legs (command, platform, `runner` label set, artifacts in/out, build → test → push order) are specified in `docs/reference/plan-json.md` terms, and the ADR's acceptance bar keeps the hand-written non-GitHub job (a GitLab job rendered from `plan.json` only).
- It names the tier-2 (post-process) and tier-3 (recipe: command + env contract) seams in `prepare` without implementing them, and the fetch-leg vs build-leg axis.
- It states corporate cargo setup as inherited operator environment (`$CARGO_HOME/config.toml`, `CARGO_HTTP_*`, `HTTPS_PROXY`) with a docs page, and carries the four open questions above as its docket.
- Implementation later: the ADR's own staged rollout (unit → acceptance harness with a local sparse index → six-target run) plus one GitLab job driven by `plan.json`.
