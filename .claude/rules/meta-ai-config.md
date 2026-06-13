---
paths:
  - .claude/**
---

# AI Config Maintenance (Mirror)

Governs the `.claude/` surface of **ocx-mirror**. Most of it is **ported verbatim from
upstream [ocx](https://github.com/ocx-sh/ocx)** and kept upstream-compatible so re-syncs
stay clean diffs. A small set of files are **mirror-native** and owned here. Load on any
`.claude/**` edit.

> No grimoire-package distribution yet (see CLAUDE.md). Ports are manual, diff-reviewed
> copies. This rule is the sync protocol until that lands.

## Surface Inventory

### Ported from ocx (keep upstream-compatible — do NOT diverge casually)

| Kind | Files |
|------|-------|
| Skills | `architect/`, `swarm-plan/`, `swarm-execute/`, `swarm-review/`, `commit/`, `finalize/` |
| Agents | all `worker-*.md` (architect, architecture-explorer, builder, doc-reviewer, explorer, researcher, reviewer, tester) |
| Rules | `quality-core`, `quality-python`, `quality-rust`, `quality-rust-errors`, `quality-rust-exit_codes`, `workflow-intent`, `workflow-feature`, `workflow-bugfix`, `workflow-refactor`, `workflow-git`, `workflow-swarm` |
| Templates | `templates/artifacts/*.template.md` |

These are byte-identical to ocx except for the adaptation list below. A noisy diff on
re-sync means drift — investigate before landing.

### Mirror-native (NOT ports — never overwrite from upstream)

| File | Why native |
|------|-----------|
| `rules/subsystem-mirror.md` | No ocx equivalent; mirror module map, pipeline, spec, error model |
| `rules/meta-plan-status.md` | Extracted from ocx's monolithic `meta-ai-config.md`, restructured standalone |
| `rules/meta-ai-config.md` | This file |
| `artifacts/**` | ocx-mirror ADRs, design specs, plans, research |
| `CLAUDE.md` | Project root instructions |

### Deliberately not ported

ocx ships extra skills (`deps`, `docs`, `next`, `meta-maintain-config`, …), many
`subsystem-*`/`arch-principles`/`product-*`/`quality-{bash,ts,vite,cli-help,security}`
rules, and enforcement machinery the mirror does **not** have: **no `.claude/rules.md`
catalog, no `.claude/tests` structural tests, no `.claude/hooks`**. Never reference these
in a ported file — strip such references on port. (`worker-doc-writer.md` is also not
ported — the mirror has no doc-authoring swarm role.)

## Adaptation List (the ONLY legitimate edits when porting)

1. **Naming** — "OCX/ocx mono-repo" → "ocx-mirror" where the text means *this* project
   (the OCX package format and the upstream repo keep their names).
2. **Paths** — `crates/ocx_lib`, `crates/ocx_cli` → `src/` (single crate, root manifest);
   the vendored submodule is `external/ocx/crates/ocx_lib`.
3. **Subsystem refs** — `arch-principles.md` / `subsystem-{cli,oci,…}.md` → `subsystem-mirror.md`.
4. **Swarm handoffs** — keep `/swarm-plan → /swarm-execute → /swarm-review → /finalize`;
   drop refs to un-ported skills and to `worker-doc-writer` (not ported). Every referenced
   worker must exist in `.claude/agents/`.
5. **Verify command** — `task verify` → `task rust:verify` for the Rust loop gate inside
   swarm tiers; `task verify` stays the full pre-merge gate.
6. **Drop upstream-only plumbing** — remove any reference to `.claude/rules.md`,
   `task claude:tests` / `test_ai_config.py`, or `.claude/hooks/` (none exist here).

Anything beyond this list is divergence. If a change is genuinely mirror-specific, put it
in a mirror-native file — never bolt it onto a port.

## Maintenance Protocol

- **Re-sync a port**: re-pull the upstream file, re-apply only the adaptation list, diff
  against the mirror copy, land as `chore(claude):`. Clean diff = healthy; noisy diff =
  drift to resolve.
- **Mirror edits stay upstream-compatible**: if you must change a ported file for the
  mirror, prefer pushing the change upstream first, or move it to a native file.
- **Register new ports in CLAUDE.md the same commit**: new skill/agent → "Skills &
  Workflow" list; new rule → the `.claude/rules/` auto-load enumeration in "Workflow".
  Unregistered ports drift silently (no test enforces this here).
- **Authoring conventions** (mirror small set): rules `<200` lines, `paths:` scope unless
  truly global; SKILL.md `<500` lines with progressive disclosure; skill `description` =
  what + when (≤1024 chars); action skills with side effects set
  `disable-model-invocation: true`; agents pick model by role (haiku explore, sonnet
  implement/review, opus architect) and minimal `tools`.

## Gate

After any `.claude/**` change: `task verify` (full gate), `cargo fmt` before commit.
Confirm cross-refs resolve and every referenced worker/skill/rule actually exists.
