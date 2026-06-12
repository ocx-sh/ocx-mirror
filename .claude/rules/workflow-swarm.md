---
paths:
  - ".claude/agents/**"
  - ".claude/skills/swarm-*/**"
---

# Swarm Worker Guidelines

Rules for efficient multi-agent swarm execution.

## Context Efficiency

1. **Workers inherit session context** - CLAUDE.md and rules loaded, workers use focused tool sets
2. **Narrow scope** - Each worker one task
3. **Minimal tools** - Only tools needed
4. **Right-sized models** - Haiku exploration, Sonnet implementation, Opus architecture

## Universal Worker Protocol (Critical Steps for Every Build/Test/Review Worker)

1. **Read relevant quality rules FIRST, before any writes.** Path-scoped, auto-load by file type: `.claude/rules/quality-core.md` (universal, always loaded), plus language leaf (`quality-rust.md`, `quality-python.md`) matching files edited. Subsystem context lives in `.claude/rules/subsystem-mirror.md` (module map, pipeline phases, error model). Post-completion self-review no substitute.
2. **Grep for existing utilities before writing new code.** ocx-mirror has shared pipeline helpers (`src/pipeline.rs`), spec config types (`src/spec/`), and `ocx_lib` as a path dep (`external/ocx/crates/ocx_lib`). Check with Grep before inventing.
3. **If existing utility doesn't fit, extend it — don't work around it.** Workarounds = #1 source of over-engineered iteration loops in prior sessions.
4. **Report deferred findings instead of oscillating.** Fix needs human judgment or causes regression on re-attempt → stop, report deferred.
5. **Never auto-commit.** All commits Michael's explicit decision. Workers report `git status` only.
6. **Flag product-level insights.** Research/architecture/implementation uncovers shift in ocx-mirror's positioning, capabilities, or target use cases → flag in completion report (positioning lives in README.md + CLAUDE.md; human decides updates).

## Worker Types

| Worker | Model | Tools | Use |
|--------|-------|-------|-----|
| `worker-architecture-explorer` | sonnet | Read, Glob, Grep | Architecture discovery |
| `worker-explorer` | haiku | Read, Glob, Grep | Fast codebase search |
| `worker-builder` | sonnet (opus override for complex implementation) | Read, Write, Edit, Bash, Glob, Grep | Stubbing/implementation/refactoring (see model rationale below) |
| `worker-tester` | sonnet | Read, Write, Edit, Bash, Glob, Grep | Specification tests and validation |
| `worker-reviewer` | sonnet (default) | Read, Glob, Grep, Bash | Code review/security/spec-compliance (diff-scoped; model scales per tier via `--reviewer` overlay) |
| `worker-researcher` | sonnet | Read, Glob, Grep, WebFetch, WebSearch | External research |
| `worker-architect` | opus | Read, Write, Edit, Glob, Grep | Complex design decisions |
| `worker-doc-reviewer` | sonnet | Read, Glob, Grep, Bash | Documentation consistency review (mkdocs site under `docs/`) |

## Worker Focus Modes

Orchestrators specialize workers via focus mode in prompt.

**worker-builder focus modes:**
- `stubbing`: Public API surface only — types, traits, signatures with `unimplemented!()`/`NotImplementedError`. Gate: `cargo check` passes. Sonnet default.
- `implementation` (default): Fill stub bodies so spec tests pass. Sonnet default; orchestrator passes `model: opus` for architecturally complex / cross-module work.
- `testing`: Write tests, cover happy path + edge cases, ensure deterministic. Sonnet default.
- `refactoring`: Extract patterns, simplify conditionals, apply SOLID/DRY. Follow Two Hats Rule (see quality-core.md). Sonnet default.

**Model selection rationale:** Opus leads Sonnet on multi-step agentic chains and novel reasoning at higher cost; the gap narrows to near-parity on single-pass review. Policy: Opus for one-way-door architecture and max-tier complex implementation; Sonnet for standard review / testing / implementation; Haiku only for read-only exploration and narrow single-pass tasks. Per-tier overrides in each skill's `overlays.md`.

**worker-tester focus modes:**
- `specification`: Write tests from design record BEFORE implementation. Tests encode expected behavior as executable spec. Must fail against stubs.
- `validation` (default): Write tests to validate existing implementation, improve coverage

**worker-reviewer focus modes:**
- `quality` (default): Code review checklist — naming, style, tests, patterns
- `security`: OWASP Top 10 scan, hardcoded secrets, input validation, checksum/verify handling, webhook secret hygiene. Reference CWE IDs.
- `performance`: N+1 patterns, blocking I/O, allocations, concurrency limits, caching. See `quality-core.md`
- `spec-compliance`: Phase-aware design record consistency review. Orchestrator specifies phase: `post-stub` (stubs ↔ design), `post-specification` (tests ↔ design), `post-implementation` (full traceability). Knows early phases have no implementation yet.

**worker-doc-reviewer**: No focus modes — always runs full trigger matrix audit (CLI reference, mirror.yml reference, env vars, getting started, changelog).

## Swarm Patterns

See `.claude/rules/workflow-feature.md` for canonical contract-first TDD protocol (Stub → Verify → Specify → Implement → Review-Fix Loop). `/swarm-execute` skill has full detailed protocol incl. review-fix loop spec.

## Review-Fix Loop

Canonical protocol used by `/swarm-execute`, `/swarm-review`, bug-fix workflow Phase 6, refactor workflow Phase 5. Byte-identical copies ship in `workflow-bugfix.md` and `workflow-refactor.md` so protocol auto-loads from all worker-relevant path scopes.

Diff-scoped, bounded iterative review. Tier-scaled: 1 round at `low`, up to 3 rounds at `high`/`max`.

**Round 1** — run every perspective on diff. Perspectives most likely find blockers run first (e.g. spec-compliance, correctness, behavior-preservation); if surface actionable findings, fix before remaining perspectives in same round.

Classify each finding:

- **Actionable** — fix automatically, re-run affected perspectives next round.
- **Deferred** — needs human judgment; surface in commit summary with context.

**Subsequent rounds** — re-run only perspectives with actionable findings prior round. Loop exits when no actionable findings remain or tier's round cap hit. Oscillating findings (same issue surfaced two rounds) auto-defer.

**Cross-model adversarial pass** (optional, tier-scaled): after Claude loop converges, run single Codex adversarial review against diff as final gate. One-shot, no looping — two-family stylistic thrash = failure mode. Skipped gracefully if Codex unavailable.

**Gate to exit**: no actionable findings remain, verification passes on final state, deferred findings documented for handoff.

## Tier & Overlay Vocabulary (for /swarm-plan, /swarm-execute, /swarm-review)

All three swarm skills (`/swarm-plan`, `/swarm-execute`, `/swarm-review`) take optional tier arg (`low | auto | high | max`, default `auto`) to scale pipeline to scope of feature/diff. Same pipeline shape every tier — only worker count, model choice, review breadth, Codex coverage change. Contract-first TDD (Stub → Specify → Implement → Review) preserved every tier.

### /swarm-plan tiers

| Tier | Intent | Defaults |
|---|---|---|
| `low` | Two-Way Door: flag/option change, doc edit, single-module tweak ≤3 files | 1 explorer, research skipped, inline design, 1 reviewer single pass, Codex off |
| `auto` (default) | Classifier picks low/high/max from signals | — |
| `high` | One-Way Door Medium: new subcommand, new spec field, new pipeline stage, 1–2 module areas | `worker-architecture-explorer` + 2–4 explorers, 1 researcher, inline/sonnet architect, parallel Claude review panel (2 rounds), Codex off (auto-on for One-Way Door signals) |
| `max` | One-Way Door High: breaking spec format, cross-module refactor, protocol change, generated-workflow surface change | Same as high + mandatory opus architect, mandatory 3-axis research, mandatory Codex plan-artifact review as final gate |

### /swarm-execute tiers

Execute reads classification from plan artifact header when present (primary signal); falls back to free-text signals otherwise. Loop rounds, builder model, review breadth scale per tier.

| Tier | Intent | Defaults |
|---|---|---|
| `low` | Two-Way Door from plan=low: 1-round loop, minimal Stage 2 (quality only), no arch verify, no Codex | sonnet stub+impl, tester (unit only), 1 reviewer Stage 1 + 1 reviewer Stage 2 |
| `auto` (default) | Classifier reads plan header `Tier:` verbatim; falls back to free-text signals | — |
| `high` | Medium plan: 3-round loop, full Stage 2 (quality / security / perf / docs), Codex off (auto-on for One-Way Door plan signals) | sonnet stub+impl (opus override for cross-module), arch-verify reviewer, unit + acceptance tests |
| `max` | Large plan: 3-round loop, adversarial Stage 2 (+ architect + SOTA + cli-ux), mandatory Codex code-diff gate | opus stub+impl (mandatory), reviewer + architect arch-verify, edge-case test coverage |

### /swarm-review tiers

Review classifies from **diff against configured baseline** (`--base=<ref>`, default `main`). Baseline = pipeline input, not overlay axis — tight baseline → small diffs (tier=low), wide baseline → large diffs (tier=high/max). Breadth, RCA, Codex scale per tier.

| Tier | Intent | Defaults |
|---|---|---|
| `low` | ≤3 files, ≤100 lines, 1 module area, no structural markers | 1 reviewer (spec-compliance + quality), no RCA, no Codex |
| `auto` (default) | Classifier reads diff metrics + paths + PR labels | — |
| `high` | ≤15 files, ≤500 lines, 1–2 module areas, no One-Way Door High signals | Stage 1 (spec-compliance + test-coverage) + Stage 2 full (quality / security / perf / docs), RCA for Block/High, Codex off (auto-on for One-Way Door signals) |
| `max` | >15 files, or cross-module, or breaking/protocol/security signals, or generated-workflow surface | Adversarial breadth (+ architect + SOTA + CLI-UX), RCA for all >Suggest, mandatory Codex code-diff gate |

### Overlays (stackable, single-axis adjustments on top of chosen tier)

**Plan overlays:**

| Flag | Axis | Effect |
|---|---|---|
| `--architect=inline\|sonnet\|opus` | Architect model in Design phase | inline = orchestrator drafts design; sonnet/opus = `worker-architect` with named model |
| `--research=skip\|1\|3` | Research worker count | skip / 1 axis / 3 axes parallel (tech / patterns / domain) |
| `--codex` / `--no-codex` | Plan-artifact Codex pass | Force on/off regardless of tier default |
| `--dry-run` / `--form` | Meta-plan preview UI | Gate orchestrator behind single approval interaction |

**Execute overlays:**

| Flag | Axis | Effect |
|---|---|---|
| `--builder=sonnet\|opus` | Builder model for Stub + Implement phases | sonnet default; opus for architecturally complex / cross-module; mandatory at tier=max |
| `--loop-rounds=1\|2\|3` | Max Review-Fix Loop iterations | 1 for low, 3 for high/max |
| `--review=minimal\|full\|adversarial` | Stage 2 perspective breadth | quality only / + security/perf/docs / + architect + SOTA + CLI-UX |
| `--codex` / `--no-codex` | Code-diff Codex pass after loop converges | Force on/off regardless of tier default |
| `--dry-run` / `--form` | Meta-plan preview UI | Same semantics as plan |

**Review overlays:**

| Flag | Axis | Effect |
|---|---|---|
| `--base=<git-ref>` | Diff baseline (pipeline input, not axis) | Default `main`; PR targets auto-resolve via `gh pr view --json baseRefName`; user flag wins |
| `--breadth=minimal\|full\|adversarial` | Stage 2 perspective breadth | quality only / + security/perf/docs / + architect + SOTA + CLI-UX |
| `--rca=on\|off` | Five Whys root-cause analysis depth | off at low; on for Block/High at high; on for >Suggest at max |
| `--codex` / `--no-codex` | Cross-model Codex code-diff pass | Force on/off regardless of tier default |
| `--dry-run` / `--form` | Meta-plan preview UI | Same semantics as plan/execute |

User-supplied flags always override classifier-inferred overlays (except tier=max's mandatory `--builder=opus` in `/swarm-execute`). Ambiguous classifications resolved at meta-plan gate (single approval point), never via mid-flow questions.

## Codex Plan Review (cross-model, plan-artifact scope)

Extends cross-model adversarial pass up lifecycle. Same entry point (`codex-adversary`), different scope:

| Scope | When fires | Target |
|---|---|---|
| `code-diff` (default) | `/swarm-execute` final gate after Claude review loop converges; `/swarm-review` cross-model pass after Claude panel converges | Git diff (`working-tree` / `branch` / `--base`) |
| `plan-artifact` | `/swarm-plan` Phase 6 after Claude panel converges | Plan / ADR markdown file (via `--target-file`) |

Both one-shot (no looping — prevents two-family stylistic thrash). Gating by tier:

- `low`: skipped (Two-Way Door — cost > value)
- `high`: off by default; auto-on when classifier detects One-Way Door signals (public API change, breaking change, novel algorithm); explicit via `--codex`
- `max`: mandatory final gate

Triage for plan-artifact scope mirrors code-diff pass: Actionable → orchestrator edits plan, re-runs one `worker-reviewer` (spec-compliance) pass; Deferred → handoff; Stated-convention / Trivia → dropped with count. Unavailable path (Codex companion plugin missing or non-zero exit): log `Cross-model plan review skipped: <reason>` and continue. Gate, not blocker.

## Plan Status Tracking

Every `.claude/state/plans/plan_*.md` carries a `## Status` block at top: `Plan` / `Active phase` / `Step` / `Last update`. Swarm skills mutate it on phase entry, round entry, verdict, commit. Global pointer `.claude/state/current_plan.md` (gitignored) names the active plan. `/finalize` refuses if any phase still active. Schema + per-skill mutation table → [`meta-plan-status.md`](./meta-plan-status.md).

## Coordination Protocol

1. **Orchestrator** decomposes task into clear assignments
2. **Workers** pick up assigned tasks, begin execution
3. **Workers** complete task following Worker Completion Requirements below
4. **Workers** report completion to orchestrator
5. **Orchestrator** integrates and verifies

### Worker Completion Requirements

When worker completes assigned task:

1. File issues for remaining work (or list them in the completion report)
2. Run quality gates via `task verify` (if code changed) — run `task --list` to discover available commands
3. **Commit all changes** on feature branch (orchestrator-directed; workers never decide commits alone)
4. Report completion to orchestrator

**Critical**: NEVER push to remote — human decides when to push (CI has real cost).

## Performance Tips

- Launch multiple explorers for broad searches
- Use worker-architect for decisions, worker-builder for execution
- Send all Agent calls in single message with multiple tool invocations → run concurrently (max 8 workers)
- Keep worker prompts under 500 tokens for fast startup

## Anti-Patterns

- NO loading full context into workers
- NO sharing state between workers
- NO workers spawning workers (single-level only)
- NO long-running workers (timeout at 5 min)
- NO opus for simple tasks (cost optimization)
- NO pushing to remote (human decides when to push — CI has real cost)
