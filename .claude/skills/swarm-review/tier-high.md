# Tier: high — /swarm-review

Default tier for medium diffs (1–2 module areas, ≤15 files, ≤500
lines changed, no One-Way Door High signals). Baseline existing
callers get when pass no explicit tier. Stage 1 run spec-compliance +
test-coverage in parallel; Stage 2 run full quality / security /
performance / docs perspective set against changed files. Cross-model
Codex pass auto-on for One-Way Door Medium signals, off otherwise.

Load this file via `Read` from `SKILL.md` after config announced.

## Phase 1: Discover

Read diff against resolved baseline. Parse file list,
map paths to module areas (`.claude/rules/subsystem-mirror.md` module
map). Read language quality rules (`quality-rust.md`,
`quality-python.md`) matching diff languages.

**Gate**: Diff fetched, module areas + language rules loaded.

## Phase 2: Stage 1 — Correctness (parallel, 2 workers)

> **Reviewer model**: every `worker-reviewer` launch in this tier use resolved `--reviewer` overlay value (tier=high default `sonnet`). See `overlays.md` reviewer axis.

Launch **in single message with multiple Agent tool calls** so run concurrently:

- **1** `worker-reviewer` (focus: `spec-compliance`, phase:
  `post-implementation`) — review full Stub → Specify → Implement
  trajectory against design record, focus on **Implement**-phase
  output. Anchors: `MirrorError` exit-code mapping (`src/error.rs`),
  two-phase pipeline ordering (prepare concurrent / push sequential,
  oldest first), fail-safe target-registry reads, spec validation at
  parse time (no hardcoded webhook URLs).
- **1** `worker-reviewer` (focus: `quality`, lens: test-coverage) —
  check **Specify** phase produced adequate tests: new code
  has tests, bug fixes have regression tests, edge cases covered.

If Stage 1 has actionable findings, surface prominently — polishing
code that not meet spec or lacks tests waste downstream effort.
Review findings in Stage 2 still produced (parallel), not gated,
but Stage 1 actionable findings dominate verdict.

**Gate**: Both reviewers complete; findings classified.

## Phase 3: Stage 2 — Quality / Security / Performance / Docs (parallel)

Launch **in single message with multiple Agent tool calls** so run concurrently (only applicable perspectives fire):

- `worker-reviewer` (focus: `quality`) — naming, SOLID, duplication,
  language-specific quality rule violations
- `worker-reviewer` (focus: `security`) — **fires when** diff touches
  checksum/verify handling, webhook/notify code, input handling, or
  archive extraction; otherwise skipped
- `worker-reviewer` (focus: `performance`) — **fires when** diff
  touches hot paths, async code, or concurrency limits; otherwise skipped
- `worker-doc-reviewer` — **fires when** documentation trigger
  matrix matches changed files (CLI commands, spec fields, env vars,
  getting started, changelog); model per resolved `--doc-reviewer`
  overlay (`sonnet` default; `haiku` when narrow-scope doc trigger
  fires — see `overlays.md` doc-reviewer axis)

Each reviewer classifies findings:
- **Actionable** (Block/Warn) — reported with remediation
- **Deferred** — needs human judgment; reason stated
- **Suggest** — optional; goes direct to deferred summary

Max concurrent: 4 Stage 2 reviewers + 2 Stage 1 reviewers (already
done) ≤ 6 parallel workers. Under 8 concurrent worker ceiling.

**Gate**: All applicable Stage 2 perspectives complete.

## Phase 4: Root-cause analysis (rca=on, Block/High findings)

For every finding classified Block or High, apply Five Whys:

```
**Issue**: [problem]
**Why 1**: [first-level cause]
**Why 2**: [deeper cause]
**Why 3**: [deeper cause]
**Why 4**: [deeper cause]
**Why 5**: [root cause]
**Systemic Fix**: [what prevents recurrence across the codebase]
```

Stop early if cause chain terminates before five levels — quality
matter more than depth. Issue may share systemic root with
other findings; note cluster.

Warn-tier findings skip RCA at this tier (max-tier applies RCA to
those too).

**Gate**: RCA complete for all Block/High findings.

## Phase 5: Cross-model — Codex (when `--codex` fires)

If `--codex` on (user flag or classifier-inferred from One-Way Door
Medium signals), invoke `codex-adversary` with scope `code-diff --base
<base>` against branch diff. One-shot, no looping.

Triage per `overlays.md`:

- **Actionable** → reported in Cross-Model section of output.
  Review read-only — no builder fix pass. If caller wants
  findings applied, hand off to `/swarm-execute`.
- **Deferred** → added to Deferred Findings with reason
- **Stated-convention** → dropped, count mentioned
- **Trivia** → dropped, count mentioned

Unavailable path: log `Cross-model gate skipped: <reason>` and continue.

**Gate**: Codex triage complete (or skip logged).

## Phase 6: Verdict & Output

Produce review report using shared skeleton from `SKILL.md`:

```markdown
## Code Review: [target]
### Summary
- Verdict: [Approve / Needs Work / Request Changes]
- Tier: high
- Baseline: <base>
- Diff: N files, +L / -L lines, S module areas
### Stage 1 — Correctness
#### Spec-compliance
#### Test Coverage
### Stage 2 — Quality / Security / Performance / Docs
#### Quality
#### Security             # if fired
#### Performance          # if fired
#### Documentation        # if fired
### Cross-Model Adversarial (Codex)   # if --codex fired
### Root-Cause Analysis
### Deferred Findings
```

**Verdict rules**:
- **Request Changes** if any Block-tier finding unresolved, or
  security vulns exist, or breaking changes lack migration,
  or tests absent for new code
- **Needs Work** if Warn-tier findings exist but no Block-tier
- **Approve** otherwise

**Gate**: Report printed. No commits.

## Handoff

Standard handoff from `SKILL.md`. Classification line:

```
- Scope: Medium (One-Way Door Medium where signals fire)
- Tier: high
- Baseline: <base>
- Overlays: breadth=full, rca=on, codex=[off|on]
```

If actionable findings exist:

    /swarm-execute .claude/state/plans/plan_[feature].md   # if a plan exists
    /swarm-execute "apply high-tier review findings"     # otherwise
