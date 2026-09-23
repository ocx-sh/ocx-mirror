State: handed-off → architect · Updated: 2026-09-22
Ratified: 2026-09-22 → architect
Confidence: ratified by Michael Herwig (owner, explicit "yes, drain"); research vintage 2026-09-22 (ocx recon, Bazel prior art, submodule-PR workflow, kernel/app split)
Participants: Michael Herwig (owner), hex-discuss; one session, 2026-09-22

## Intent

Owner, in their words: give ocx-mirror a fast Bazel build with fast, cached test execution,
carrying over every lesson from ocx's own Bazel migration (test-report regression, slower
builds). Split ocx-mirror into smaller crates and consolidate: most logic belongs in ocx
libraries, except what is truly mirror-specific, so future `ocx-dist` and `ocx-mcp` apps reuse
the "kernel" that lives in ocx — above all the Python lock interface, which an `ocx-dist` could
use to distribute Python apps. Add a coherent AI configuration that authors ocx-side changes and
PRs from the `external/ocx` submodule checkout and knows the ocx libraries and where code belongs.
After the library rework, the new ocx-mirror must pass the **old** acceptance suite.

Why now: ocx just finished its Bazel adoption (`../ocx` `02ff88f0`, `8a8546f8`, `18dca451`) and
has its test-tier follow-up in flight (`../ocx/.claude/artifacts/plan_test_speed_tiers.md`), so
the lessons are fresh; `ocx-dist` is the next planned consumer of the Python packaging code.

Deliverable of this discussion: a `/goal` prompt in the shape of
`../arcana/.tmp/examples/loops/*.md` (below, § Goal prompt) that runs the work autonomously.

**Out of scope:** building `ocx-dist` or `ocx-mcp`; promoting generic code other than the Python
packaging crate into ocx; changing mirror behaviour, CLI surface, spec grammar or output formats
(the rework is behaviour-preserving); Bazel for the Windows/macOS legs or release builds (those
stay cargo, as in ocx); remote execution; a remote writer for acceptance verdicts; touching the
sibling `../ocx` checkout (it hosts ocx's own running goal).

## Requirements

Provisional — IDs are assigned downstream.

- **AI configuration first (phase 0).** A mirror-native placement rule stating which code lives
  in the mirror (spec grammars, pipeline orchestration, CI-workflow rendering, `MirrorError`) vs
  which goes to an ocx crate and at which tier (internal / ecosystem / interface, per
  `external/ocx/.claude/artifacts/system_design_crate_workspace.md`), with the promotion trigger
  "named second caller". A skill (or `/update-ocx` extension) for the reverse flow: branch inside
  `external/ocx`, commit, push, open a PR against `ocx-sh/ocx`, wait for green, squash-merge, bump
  the mirror's pointer to the PR's `merge_commit_sha` — never a PR-head SHA. Guard hooks in
  `.claude/settings.json` (none exist today): assert branch ≠ `main` before a commit inside
  `external/ocx`, and absolute paths / `git -C` for every git command so cwd never leaks into the
  submodule. `submodule update --init --recursive` runs inside `external/ocx`, never from the
  root. Registered in `CLAUDE.md` and `.claude/rules/meta-ai-config.md` the same commit.
- **Crate split (phase 1).** ocx-mirror becomes a Cargo workspace of smaller crates under
  `crates/`. Candidate boundaries from recon: source discovery (`src/source/*`), HTTP + auth
  (`src/http.rs`, `src/auth.rs`), CI reporting (`src/junit.rs`, `src/run_summary.rs`), ocx
  subprocess boundary (`src/pipeline/{ocx_cli,push}.rs`), spec (`src/spec/`), pipelines, and the
  binary (`src/command/`, `src/main.rs`). Generic crates are shaped for later promotion (no
  dependency on mirror spec/error types). No cycles; follow the `pipeline/target_registry.rs`
  precedent against upward edges. Behaviour-preserving.
- **Promotion (phase 2).** `crates/ocx_python` plus `src/source/pylock.rs` move into ocx as a new
  crate usable by a satellite (ecosystem tier), via one PR against `ocx-sh/ocx` authored in the
  submodule. ocx's `task satellite:verify` allow-list and crate-tier docs are updated in that PR.
  The mirror drops its local copy and links the ocx crate. The run itself squash-merges the ocx PR
  once its CI and Verify Deep are green (squash keeps ocx's trunk signed).
- **Bazel (phase 3).** Bazel for the Linux build and test loop, following the `bazel-adopt` skill
  (`.claude/skills/bazel-adopt/`) and ocx's `MODULE.bazel` / `.bazelrc` / pinned `.bazelversion`.
  Hand-written BUILD files. Carried lessons, all mandatory: per-test JUnit rebuilt from `test.log`
  with a count floor (ocx `8a8546f8`), `--remote_download_regex=.*/test\.(xml|log)$` under minimal
  downloads, no provenance stamping in dev/test builds (release-only), `exclusive` tag instead of
  global `--local_test_jobs`, rdeps-aware scoped test selection, cache-hit assertions keyed on
  `executionInfo.strategy`, generated BUILD files for submodule crates rather than committed ones.
  Acceptance suite as cached `bazel test` targets on the local disk cache. Bazel is mandatory for
  the local/agent loop (`task` entry points drive it). The CI lane swap from nextest to Bazel
  happens **only** if a before/after measured on one commit beats a bar written down before
  measuring; otherwise CI stays on nextest and the measurement is recorded.
- **Shape:** one feature branch and one PR in ocx-mirror, one PR in ocx. Mirror PR ends
  merge-ready (green pipeline, finalized); the owner merges it.

## Decisions

- **Phase order: AI config → split → promote → Bazel.** AI config first so the run uses its own
  guard rails; split before Bazel so BUILD files are written once for the final crate shape and
  the frozen suite gates the split before the build system changes.
- **ocx-side changes are authored in the `external/ocx` submodule**, not the sibling `../ocx`.
- **Promotion scope: named second caller only** — the Python packaging crate now; other generic
  code stays mirror-internal, promotion-ready. Prior art (rattler extracted at three consumers; uv
  splits internally and never stabilises) supports waiting.
- **The run may squash-merge its own ocx PR** once green; the mirror PR stays the owner's merge.
- **Bazel gate: local loop mandatory, CI swap only on a pre-declared measured bar.** Strongest
  counter-argument, recorded once: ocx measured its CI lane swap NO-GO on wall-clock
  (`../ocx/.claude/artifacts/measurement_bazel_r2.md` § VERDICT) — the win is local.
- **Oracle: `test/` at tag `v0.6.2`, zero edits**, run against the new binary via
  `OCX_MIRROR_COMMAND` after phase 1, after phase 2 and at the end; plus the current suite and
  `/e2e-test` tier 2 (local-registry run against real ocx-contrib specs).
- **One mirror PR + one ocx PR**, matching the goal examples.

## Research

- `.claude/artifacts/research_bazel_crate_split_lessons.md` — ocx Bazel lessons, ocx crate tiers,
  ocx-mirror module map, acceptance-suite binary coupling, cross-module crate_universe prior art.
- Submodule-PR workflow and kernel/app-split prior art: summarised inline in § Decisions and
  § Requirements (squash `merge_commit_sha`; no existing hooks; rattler / uv / gitoxide tiers).

## Related

- `../ocx/.claude/artifacts/adr_bazel_build_adoption.md` (may sit under `archive/`),
  `plan_bazel_build_adoption.md`, `measurement_bazel_r2.md`, `decision_bazel_adoption.md`,
  `adr_test_speed_tiers.md`, `plan_test_speed_tiers.md`, `system_design_crate_workspace.md`.
- `.claude/skills/bazel-adopt/`, `.claude/skills/bazel-diagnose/`, `.claude/skills/update-ocx/`,
  `.claude/skills/e2e-test/`, `.claude/rules/subsystem-mirror.md`, `CLAUDE.md` § Dependency model.
- Goal prompt examples: `../arcana/.tmp/examples/loops/1.md` … `6.md`.

## Open questions

- [NEEDS CLARIFICATION: How does the mirror's Bazel graph consume ocx crates — `bazel_dep` on the
  ocx module via `local_path_override(path = "external/ocx")`, or mirror-owned BUILD files over
  the submodule sources inside one crate_universe hub?] Recommended: one hub resolved from the
  mirror's own `Cargo.lock` with mirror-owned BUILD files for the linked ocx crates — two hubs
  duplicate shared crates across the boundary (rules_rust#3732, rules_rs#255 unmerged), the same
  failure class as the old reqwest major split. `/hex-architect` decides with fresh evidence.
- [NEEDS CLARIFICATION: How is ocx's `[patch.crates-io]` into nested submodules
  (`external/ocx/external/*`) expressed under crate_universe?] Recommended: copy ocx's own
  solution from its `MODULE.bazel` — it already solved it for the same forks.
- [NEEDS CLARIFICATION: Name and tier of the promoted Python crate in ocx.] Recommended: keep
  `ocx_python`, ecosystem tier — it has behaviour, not only value types, so not interface tier.
- [NEEDS CLARIFICATION: Exact mirror crate boundaries and names.] Recommended: `/hex-architect`
  settles them from the recon map; prefix `ocx_mirror_*` for mirror-internal crates so a later
  promotion is a rename, not a redesign.
- [NEEDS CLARIFICATION: The measured bar for the CI lane swap.] Recommended: written into the ADR
  before any measurement; wall-clock of the Linux verify job, same commit, cold and warm.

## Verification

- `task verify` green on the final branch (run via `ocx run -- task verify` if direnv is stale).
- Oracle: `git worktree add` of tag `v0.6.2`, run its `test/` suite unmodified with
  `OCX_MIRROR_COMMAND=<new binary>` — green after phase 1, after phase 2, and at the end;
  `git diff v0.6.2 -- test/` on that worktree is empty.
- Current acceptance suite green, also as cached `bazel test` targets; a second no-change run is
  fully cached.
- `/e2e-test` tier 2 green against the ocx-contrib specs.
- Per-test JUnit published to the PR with a case count equal to the libtest summary total.
- ocx PR squash-merged with green CI and Verify Deep; the mirror's `external/ocx` pointer equals
  its `merge_commit_sha`; `cargo tree -i oci-client` still shows the fork source.
- ocx `task satellite:verify` green with the new crate on the allow-list.
- CI lane-swap decision recorded with its before/after numbers, go or no-go.
- Mirror PR: green pipeline, finalized via `/hex-finalize`, left for the owner to merge.

## Goal prompt

```text
/goal

You are the meta-orchestrator. Forward work to Opus 5 sub-orchestrators, keep main context small.
Fully autonomous: never prompt.

Implement .agents/discussions/bazel-crate-split.md in ocx-mirror, four phases in order:
  0. AI config: placement rule, submodule-PR skill, guard hooks.
  1. Crate split into a Cargo workspace (behaviour-preserving).
  2. Promote ocx_python (+ pylock) into ocx: one ocx-sh/ocx PR authored in external/ocx; bump
     the pointer to the merged SHA.
  3. Bazel for the Linux build/test loop, every ocx lesson carried; CI lane swap only if the
     pre-declared measured bar is beaten.

Run state lives in the ledger .agents/goal/bazel-crate-split.md, never in memory: phase status,
active loop, branches, PR URLs, merged SHAs, oracle results, decisions, divergences + reasons,
next action. Create it first; re-read on every wake-up and compaction; update around every spawn.

Branch, commit artifact + ledger, then umbrella /hex-architect high on the artifact (resolves its
Open questions), then a meta-plan. One inner loop per phase, run by an Opus 5 sub-orchestrator:
/hex-architect (phase-scoped) -> /hex-plan -> /hex-execute -> /hex-review (loop's work only) ->
/hex-execute (fixes). Phase 0 lands first so later loops run under its rule and hooks. A phase
that proves the umbrella ADR wrong amends it via its own /hex-architect pass, logged; the run
continues.

Phase gate: oracle green, checkpoint commit, ledger entry done. Next phase starts only from a
passed gate; after a crash, resume from the last one.

Oracle (after phases 1, 2 and at the end): worktree of tag v0.6.2, run its test/ unmodified with
OCX_MIRROR_COMMAND at the new binary; plus the current suite and /e2e-test tier 2. Red = regression
to fix, never a test to change.

Serious doubt: spawn a sub-orchestrator to research a decisive, recorded recommendation; defer to
a GitHub issue only in hard cases. Anything not deferred gets resolved.

RULES
- No security paranoia.
- Diverge from the artifact rarely, with strong reason (SOTA, ocx consistency). Cutting scope to
  save time is NOT accepted.
- State of the art; copy ocx's solved Bazel patterns before inventing.
- Never touch ../ocx; ocx changes only in external/ocx: branch first, git -C / absolute paths,
  submodule update --init --recursive inside external/ocx, never from the root.
- May push and open PRs on both repos. May squash-merge (never rebase-merge) own ocx-sh/ocx PR
  once CI + Verify Deep green; pointer = its merge_commit_sha. Mirror PR stays for the owner.
- No SSH agent: commit via GraphQL createCommitOnBranch or unsigned, per memory.
- target/ and /tmp wiped hourly: drafts and long-job logs in project .tmp; delete it after.
  Delete stale temp dirs and worktrees freely.
- Limit and monitor RAM (rust-analyzer, Bazel server, parallel cargo) or WSL aborts.
- Call acceptance/taskfile test tasks directly in the loop; skip the commit hook only in the
  loop, never at finalization.

Test well, edge cases included: subagents hunt edge cases in design and code; design tests before
implementing. Keep docs/ and CLAUDE.md in sync with the new layout and build.

After all phases: self-refinement (max 3 turns) of /hex-review + /hex-execute, then /hex-finalize.
Force-push to the feature branch allowed. Mirror PR ends merge-ready: green pipeline, every
artifact Verification item met.

No getting stuck: wake-ups re-check state every 5 min; pull any subagent idle without reporting.
```
