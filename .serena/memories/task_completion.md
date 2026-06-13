# Task Completion

When a coding task is "done":

1. `cargo fmt` (or `task rust:format:apply`) — **always before commit**.
2. During Review-Fix loops / iterative work: run **`task rust:verify`** (format + clippy + license headers + build + unit tests) — fast Rust-only gate.
3. Final pre-merge gate: **`task verify`** (rust:verify + acceptance tests). Required after implementation lands.
4. Acceptance-affecting changes: `task test` / `task test:parallel`; target one with `cd test && uv run pytest tests/test_mirror.py::<name> -v --no-build`.
5. `.claude/**` edits: `task verify` (full) + confirm cross-refs resolve and every referenced worker/skill/rule exists (`meta-ai-config.md` gate).
6. Commit on a feature branch with a Conventional Commit message; **never push** (human decides). Report `git status` only.