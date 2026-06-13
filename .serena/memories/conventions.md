# Conventions

## Code style / quality
- Follow `.claude/rules/quality-core.md` (SOLID/DRY/KISS/YAGNI) + `quality-rust.md` (ownership, async/Tokio, error handling, edition-2024 patterns). Grep for existing helpers before writing new (`pipeline.rs`, `spec/`, `ocx_lib`).
- Every source file carries a **license header** — `task rust:license:check` enforces; `rust:license:format` adds.
- Errors: one `MirrorError` enum, each variant maps to a sysexits exit code (`error.rs::kind_exit_code`; table in `mem:core` docs). Propagate with context, don't swallow.

## Serena tool discipline (this project's agents)
- Code files: discovery via `get_symbols_overview`/`find_symbol`/`find_referencing_symbols`; edits via `replace_symbol_body`/`insert_*_symbol`/`replace_content`. Built-in Read for discovery and Edit on code are FORBIDDEN by the active context. Grep/Glob allowed for discovery only.
- Serena line numbers are **0-based**.

## Git / workflow
- [Conventional Commits](https://www.conventionalcommits.org/) (`feat`,`fix`,`refactor`,`ci`,`chore`). **No `Co-Authored-By` trailers.**
- Work on branches, **never `main`**. **Never push** — human decides (CI cost real).
- `task checkpoint` amends a rolling "Checkpoint" commit during work. Dev cycle: `/commit` (working) → `/finalize` (clean conventional commits, fast-forward onto main).
- Every task routes through `.claude/rules/workflow-intent.md` (classify feature/bugfix/refactor, check GitHub issues/PRs first).
- Planning flow: ADR → Design Spec → Plan → Implementation. Durable artifacts in `.claude/artifacts/`; executable plans + `## Status` blocks in `.claude/state/plans/` (`mem:` n/a — see `meta-plan-status.md`).

## `.claude/` surface
- Mostly **ported verbatim from upstream ocx**, kept upstream-compatible. Mirror-native files: `subsystem-mirror.md`, `meta-plan-status.md`, `meta-ai-config.md`, `artifacts/**`, `CLAUDE.md`. `meta-ai-config.md` governs the port/re-sync protocol + adaptation list.