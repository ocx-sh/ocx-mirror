---
paths:
  - ".claude/state/**"
  - ".claude/templates/artifacts/**"
---

# Plan Status Protocol

Every plan in `.claude/state/plans/plan_*.md` carries a `## Status` block at the top — first 30 lines after H1 — so skills and the user can read current state at a glance without scanning the full plan. Extracted from ocx's `meta-ai-config.md`; standalone here.

## Schema

```markdown
## Status

- **Plan:** plan_<slug>
- **Active phase:** <N> — <phase title>
- **Step:** <skill or activity, e.g. /swarm-execute → implementation>
- **Last update:** <YYYY-MM-DD> (after <commit-sha-short>: <subject>)
```

Allowed `Step` values:
- `/swarm-plan → plan-approved`
- `/swarm-execute → <stage>` (Stub, Specify, Implement, Review-Fix Loop)
- `/swarm-review → round N`
- `awaiting /swarm-review`
- `awaiting /swarm-execute (review-fix loop)`
- `awaiting /finalize`
- `finalized` (terminal — `/finalize` writes this then deletes `current_plan.md`)

## Global pointer

`.claude/state/current_plan.md` (gitignored):

```markdown
# Current Plan Pointer

- **Plan:** .claude/state/plans/plan_<slug>.md
- **Branch:** <branch-name>
- **Updated:** <YYYY-MM-DD HH:MM UTC>
```

Skills read the pointer first, jump straight to the referenced plan's Status block. Absent pointer → fall back to plan-glob, then commit-subject heuristic with user prompt.

## Per-skill mutation table

| Skill | Reads | Writes |
|---|---|---|
| `/swarm-plan` | — | Init Status in new plan; write `current_plan.md` |
| `/swarm-execute` | Status | Flip `Step` on phase entry/advance; bump `Last update` |
| `/swarm-review` | Status | Flip `Step` on round entry; set `awaiting /finalize` or `awaiting /swarm-execute` on verdict |
| `/commit` | Status | Bump `Last update` only (no phase advance) |
| `/finalize` | Status | **Refuse if Step ≠ `finalized` and `Active phase` not last** (`--force` overrides); on success set `Step: finalized`, delete `current_plan.md` |

Phase advancement (`Active phase: N → N+1`) is the orchestrator/plan-author decision encoded as Step transition — never an automatic side-effect of commits.

## Templates seed the block

`.claude/templates/artifacts/plan.template.md` and `bugfix_plan.template.md` carry a Status block at top so every new plan gets one for free.

## Why both files

- `current_plan.md` is the **fast path** (read one small file, jump to referenced plan).
- Status block in plan file is the **truth** (survives `current_plan.md` deletion, captures plan-internal phase progression).
- Together: `current_plan.md` answers "which plan?", Status block answers "where in that plan?". Either alone is incomplete.

## Subplans (parent-stack)

A plan may spawn a subplan (e.g. a high-tier review opens its own `plan_review_*.md`, or a discovered cross-cutting refactor needs its own plan before the parent can resume). The Status schema supports nesting via an optional `**Parent plan:**` field:

```markdown
## Status

- **Plan:** plan_review_X
- **Parent plan:** plan_parent_slug (resume after Step: finalized)
- **Active phase:** 1 — Findings triage
- **Step:** /swarm-review → round 1
- **Last update:** 2026-06-12 (after 9c2b4c9: ...)
```

Protocol:

1. **Spawn**: when a skill creates a subplan, it (a) writes the new plan with `Parent plan:` set to the current `current_plan.md` target, (b) repoints `current_plan.md` to the new subplan. The parent's Status block is untouched (its `Step` already records what triggered the spawn).
2. **Run**: standard mutation rules apply to the subplan only.
3. **Return**: when the subplan reaches `Step: finalized`, `/finalize` checks `Parent plan:`. If present, instead of deleting `current_plan.md` it repoints `current_plan.md` back to the parent and bumps the parent's `Last update`. If absent, original behaviour (delete `current_plan.md`).
4. **Stack depth**: kept implicit via the chain of `Parent plan:` fields — no explicit stack file.

This keeps the common (single-plan) case zero-cost while making nested workflows recoverable.
