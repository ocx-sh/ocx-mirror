# Conventional Commits v1.0.0 — Cheat Sheet

Reference for `/commit` skill. Full spec: https://www.conventionalcommits.org/en/v1.0.0/

## Structure

```
<type>[optional scope]: <description>

[optional body]

[optional footer(s)]
```

- Blank line between subject and body.
- Blank line between body and footers.

## Types (ocx-mirror usage)

| Type | Meaning | Changelog? |
|---|---|---|
| `feat` | New feature or capability | Yes (MINOR) |
| `fix` | Bug fix | Yes (PATCH) |
| `perf` | Perf improvement, behavior unchanged | Yes |
| `refactor` | Structure change, behavior unchanged | Yes |
| `docs` | Docs only | Yes |
| `test` | Tests only | Yes |
| `build` | Build system, deps, Cargo.toml, submodule bumps | Yes |
| `ci` | CI config (workflows, actions) | Yes |
| `chore` | **AI/tooling files, `.claude/`, CLAUDE.md, skills, rules, taskfiles** | **No** |
| `style` | Formatting only (prefer skip — `cargo fmt` handles) | No |

Repo rule: `chore:` for anything under `.claude/` or AI-config files so stay out of user changelog.

## Scope

Optional noun for area touched. Examples for this repo:

- `feat(pipeline): carry resolved assets in plan.json`
- `fix(spec): reject hardcoded webhook URLs at parse time`
- `refactor(push): extract cascade tag computation`
- `chore(claude): tighten swarm-review classify signals`

Add scope only when narrows change. Skip for cross-cutting work.

## Description

- Imperative mood: `add`, `fix`, `remove` — not `added`, `fixes`, `removing`.
- Lowercase first letter.
- No trailing period.
- ≤72 chars full subject line.

Bad: `Added a new feature to the pipeline.`
Good: `feat(pipeline): skip non-applicable platform pairs in plan`

## Body (optional)

Explain **why**, not what — diff show what. Include body only when reason non-obvious (hidden constraint, subtle invariant, workaround for specific bug, context future reader miss).

Wrap ~80 chars. Plain prose, no markdown bullet soup unless listing discrete items.

## Footers (optional)

Format: `Token: value` or `Token #reference`. Tokens use hyphens, not spaces.

Common footers:

- `BREAKING CHANGE: <description>` — mandatory for breaking changes (even if `!` in subject). Only footer where spaces allowed in token.
- `Refs: #123` — reference issue without closing.
- `Closes: #123` — close issue when commit lands on default branch.

**Never use `Co-Authored-By`** in this repo.

## Breaking Changes

Two signals, used together:

1. `!` before colon: `feat(spec)!: remove deprecated asset_type shorthand`
2. Footer: `BREAKING CHANGE: asset_type must now be an explicit object; see migration notes.`

Both appear. `!` fast scan; footer give detail.

## Worked Examples

### Simple feature

```
feat(cli): add --fail-fast flag to sync command
```

### Bug fix with context

```
fix(mirror): stop prepare legs re-crawling the source

pipeline prepare rebuilt its task list from a fresh source crawl even
when a plan.json with resolved assets was available, multiplying API
calls per run (N+1 crawls) and risking rate limits on large mirrors.
```

### Breaking change

```
refactor(spec)!: require explicit platforms matrix

BREAKING CHANGE: mirror.yml without a `platforms:` section now errors
at parse time. Previously a default linux/amd64-only matrix was
assumed, which silently dropped other platforms.
```

### AI config change (chore, no changelog entry)

```
chore(claude): add /commit skill with PR-prompt memory
```

## Common Mistakes

- **Title case description** — `feat: Add foo` should be `feat: add foo`
- **Past tense** — `fix: fixed the bug` should be `fix: fix the bug`
- **Explain what not why** — diff show what; body for why
- **Bullet body for single-line change** — prose fine; bullets noise
- **Scope duplicate type** — `feat(feature):` add nothing
- **Multiple concerns one commit** — split; one concern per commit
