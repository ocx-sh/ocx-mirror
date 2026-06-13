# Suggested Commands

Run `task --list` for the full set. Key tasks:

| Command | Purpose |
|---------|---------|
| `task` | fast check (default) |
| `task rust:verify` | Rust loop gate: format check + clippy + license + build + unit tests |
| `task verify` | Full pre-merge gate (rust:verify + acceptance tests) |
| `task rust:test:unit` | unit tests only |
| `task rust:format:apply` / `:check` | apply / check `cargo fmt` |
| `task rust:clippy:fix` / `:check` | clippy |
| `task rust:license:format` / `:check` | add / check license headers (required on every source file) |
| `task test` (= `test:default`) | acceptance: builds binary + starts registry |
| `task test:parallel` / `test:quick` | parallel acceptance (`:quick` = no rebuild) |
| `task run -- <args>` | build & run the binary |
| `task docs:serve` / `docs:build` | mkdocs site |
| `task release:prepare` | compute version, changelog, verify (human commits+tags+pushes) |

Single acceptance test:
```sh
cd test && uv run pytest tests/test_mirror.py::<name> -v --no-build
```

System: Linux. Standard GNU coreutils/git. `rtk` proxy rewrites many shell commands transparently (token-optimized; see `~/.claude/RTK.md`).