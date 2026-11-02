---
paths:
  - "CHANGELOG.md"
  - "cliff.toml"
---

# Git & Commit Workflow

Shared branch-and-commit hygiene for ocx-mirror. Used by `/commit` skill (working phase) and `/finalize` skill (rebasing phase). Catalog-only: referenced on demand, not auto-loaded via path glob — nothing in repo "a git file".

## Branching Model

Plain feature branches off `main` (`feat/...`, `fix/...`, `chore/...`). No worktree model.

- **Never commit on `main`.** If on `main`, stop and switch to a feature branch first.
- **Never push.** Push triggers CI, real cost. Human decides when push. No skill, agent, automation push on own.
- **Never `Co-Authored-By`** in commit messages. Repo convention.

## Two-Phase Model

Branch commit history go through two phases. Each phase: different goal, different skill, different rules.

| Phase | Skill | Goal | Rule |
|---|---|---|---|
| **Working** (default on feature branches) | `/commit` | Save progress while iterating. Bundle freely. Amend rolling Checkpoints. | One concern per commit **relaxed**. Honest bundle message better than fake narrative. |
| **Rebasing** (explicit, before landing) | `/finalize` | Produce exact commits that appear in changelog | Strict Conventional Commits v1.0.0. One concern per commit. Reword/squash/split as needed. |

Default posture on feature branches: **working phase**. Do not badger user about splitting during working phase — they clean up with `/finalize` before landing.

## Checkpoint Convention

Commit with subject exactly `Checkpoint` (no type, no body) means "rolling WIP". Amended every time new work lands on top. Never goes to `main`. `/finalize` refuses to land branch that still contains one.

`task checkpoint` creates or amends rolling Checkpoint automatically.

## Conventional Commits (Quick Rules)

Full cheat sheet: [`commit_reference.md`](../skills/commit/commit_reference.md) (types, scopes, footers, breaking changes, worked examples).

- Format: `<type>[optional scope]: <description>`
- Types: `feat`, `fix`, `refactor`, `perf`, `test`, `docs`, `build`, `ci`, `chore`
- **`chore:`** for AI/tooling files (`.claude/`, `CLAUDE.md`, skills, rules, taskfiles) — keeps out of user-facing changelog
- Imperative mood, lowercase description, no trailing period, subject ≤72 chars
- Body explains **why**, not what. Only when non-obvious.
- Breaking changes: `!` before colon **and** `BREAKING CHANGE:` footer

## Land-Ready Definition

Branch ready to fast-forward onto `main` when **all** hold:

- [ ] Rebased on top of current `main` (no merge commits in `main..HEAD`)
- [ ] Every commit in `main..HEAD` has Conventional Commits subject
- [ ] No `Checkpoint` commits remain
- [ ] No "bundle" commits mixing unrelated concerns (working-phase bundles must split or squash)
- [ ] Each commit one concern
- [ ] The checks the series touched pass (`task verify:scoped`); CI runs the full gate

`/finalize` checks each and proposes rebase plan for anything that fails.

## Quality Gate

The baseline's prek hooks gate the commit message (conventional subject); verification is on you.

**Verification levels — no full `task verify` locally.** It takes 20–30 min, and CI runs the whole pipeline (`verify-basic`) on every push; the deep tier (`verify-deep`) is nightly or the `run-deep` label — never dispatch it per PR. Pick the level the change needs, the agent decides:

| Change | Level before committing |
|---|---|
| Docs, comments, AI config, plan artifacts | none |
| Code in one or a few crates | `task rust:verify` (Rust loop gate) or `task verify:scoped` (Bazel rdeps of what changed vs `origin/main`, claude hooks, baseline contract) |
| `verify:scoped` escalates (taskfile, BUILD/bzl, `scripts/**`, workflows) | run what the edit touches (`bazel:build:drift`, `scripts:self-test`, `workflows:lint`, …) |
| Finalize | what the series touched; CI is the full gate |
| `release:` | `task release:prepare` runs the full `task verify` |

Never a full `task verify` otherwise, not even "for the signal" — push (the human does) and read CI.

## Phase Boundaries — When to Use Which Skill

| Situation | Use |
|---|---|
| Saving progress mid-task | `/commit` (working phase) |
| "Commit this as a proper conventional commit" | `/commit` (drafts message, stages, commits) |
| "Checkpoint this" / "save WIP" | `/commit` (creates/amends rolling Checkpoint) |
| Branch has messy history, prepare to land on main | `/finalize` |
| "Squash this branch into one commit for the changelog" | `/finalize` (squash-all mode) |
| Reword a stranded Checkpoint deep in history | `/finalize` |

## Submodule (`external/ocx`)

`external/ocx` is a **vendored read-only submodule** — never commit inside it, except under `/ocx-upstream-pr` on a submodule feature branch. Bumping ocx = bumping the submodule pointer (procedure in README.md); the pointer change commits in this repo as `build:`.

## References

- [`commit_reference.md`](../skills/commit/commit_reference.md) — Conventional Commits v1.0.0 cheat sheet
- [workflow-feature.md](./workflow-feature.md) — where commits fit in broader feature flow
- `CLAUDE.md` — Workflow section
