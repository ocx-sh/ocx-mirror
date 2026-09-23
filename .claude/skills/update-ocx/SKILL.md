---
name: update-ocx
description: Bump the external/ocx submodule and adopt what changed upstream. Use when asked to update, bump, or advance ocx, to move the mirror onto a new ocx release or onto upstream main, or to review what an ocx bump brings. Covers the mechanical pointer move, the drift gates (toolchain, copy-exactly dependency rows, patch table), a semantic review of the eight linked crates, consolidation of mirror code onto newly shared ocx API, and the upstream-issue cross-check.
user-invocable: true
disable-model-invocation: true
---

# update-ocx — bump `external/ocx` and adopt what changed

The mechanical procedure lives in **README.md § "Bumping ocx"** — it is the
source of truth for the commands and the four-place floor bump. This skill is
the review layer around it: what the bump *means*, what breaks quietly, and
what the mirror should stop hand-rolling.

## The two surfaces

A bump moves one of them. Never assume it moved both.

| Surface | Moves when | Governs |
|---------|-----------|---------|
| **Linked crates** — the eight `ocx_*` path deps | the `external/ocx` pointer moves | the mirror's own compiled behaviour |
| **Spawned CLI** — `ocx package push/announce/description` | the `ocx.sh/ocx/cli` pin in `ocx.lock` moves, and `OCX_CONTAINER_CLI_TAG` | what the child process does, and what generated CI bakes in |

Upstream `feat(...)!:` commits are almost always surface 2. They change nothing
for the mirror until the CLI pin moves. Say which surface each finding is on.

**And a third position the table hides: the mirror is also a *child*.** It runs
under `ocx exec` / `ocx run`, so a change to what the *parent* ocx puts in a
child's environment reaches the mirror without either surface moving — the
operator's ocx did. `ocx_config::env::Env::apply_ocx_config` is the function to
diff for this. Example from the 0.6.2→main range: the parent now forwards
`OCX_HOME`, `OCX_DEFAULT_REGISTRY` and `OCX_INSECURE_REGISTRIES` across
`--clean`, so `ocx_config::env::insecure_registries()` can return a non-empty
list in a clean-env run where it used to return none — and a leg that dialled
HTTPS may now dial plain HTTP. Directionally a fix; still a behaviour change
nobody asked for, on a security-relevant switch.

## Phase 0 — preflight

```sh
cd /path/to/ocx-mirror
git status --short                     # must be clean
git worktree list                      # sweep per CLAUDE.md worktree hygiene
git switch -c chore/bump-ocx-submodule
OLD=$(git ls-tree HEAD external/ocx | awk '{print $3}')
```

A stale `chore/bump-ocx-submodule` from an earlier bump may already exist.
Check its ocx pointer against `main`'s before deleting — older pointer and no
added files means superseded.

## Phase 1 — the range

```sh
git -C "$(git rev-parse --show-toplevel)/external/ocx" fetch origin --tags
NEW=$(git -C "$(git rev-parse --show-toplevel)/external/ocx" rev-parse origin/main)   # or a release tag: git -C "$(git rev-parse --show-toplevel)/external/ocx" rev-parse v0.6.3
git -C "$(git rev-parse --show-toplevel)/external/ocx" log --oneline $OLD..$NEW
git -C "$(git rev-parse --show-toplevel)/external/ocx" diff --stat $OLD..$NEW | tail -5
```

Then narrow to what the mirror actually links — everything else is noise:

```sh
for c in ocx_config ocx_console ocx_exit ocx_index ocx_oci ocx_package ocx_sign ocx_util; do
  echo "=== $c ==="; git -C "$(git rev-parse --show-toplevel)/external/ocx" diff --stat $OLD..$NEW -- crates/$c | tail -20
done
```

Public-API delta, both directions (the second command is the one that finds
silent breakage — a `pub` demoted to `pub(crate)`):

```sh
git -C "$(git rev-parse --show-toplevel)/external/ocx" diff -U0 $OLD..$NEW -- crates/ocx_{config,console,exit,index,oci,package,sign,util} \
  | grep -E '^\+\s*pub (fn|struct|enum|const|trait|type|mod)' | sort -u
git -C "$(git rev-parse --show-toplevel)/external/ocx" diff -U0 $OLD..$NEW -- crates/ocx_{config,console,exit,index,oci,package,sign,util} \
  | grep -E '^-\s*pub ' | sort -u
```

## Phase 2 — move the pointer

Follow README.md § "Bumping ocx". One trap it does not spell out: run the
nested-submodule update **inside** `external/ocx`, never from the repo root —
from the root it re-resolves the superproject gitlink and silently reverts the
checkout you just made.

```sh
git -C "$(git rev-parse --show-toplevel)/external/ocx" checkout $NEW && git -C "$(git rev-parse --show-toplevel)/external/ocx" submodule update --init --recursive
cargo check --all-targets     # refreshes Cargo.lock; must be clean
git add external/ocx Cargo.lock
```

A clean `cargo check` is the whole of the compile-compatibility answer. If it
fails, the removed-`pub` list from Phase 1 names the cause.

## Phase 3 — drift gates

Four checks, none of which `cargo check` catches.

1. **Toolchain channel** — `rust-toolchain.toml` `channel` must equal
   `external/ocx/rust-toolchain.toml`'s. (Targets legitimately differ: ocx
   cross-builds its Windows shim, the mirror does not.)
2. **Copy-exactly dependency rows** — for every dep shared with ocx, the
   version and feature list in the mirror's root `Cargo.toml` **and in
   `crates/ocx_python/Cargo.toml`** (its own rows, not inherited from the
   workspace until phase 2) must match ocx's `[workspace.dependencies]` byte
   for byte:
   ```sh
   git -C "$(git rev-parse --show-toplevel)/external/ocx" diff $OLD..$NEW -- Cargo.toml
   ```
   Empty diff = nothing to sync *for this bump*. Pre-existing drift is a
   separate finding — report it, do not fold it into the bump commit.
   Mirror-owned rows (`octocrab`, `rustls`, `sha1`, `md-5`, `tower-service`)
   have no ocx counterpart; see CLAUDE.md § "Dependency model" for the
   authoritative split.
3. **`[patch.crates-io]`** — must still point into `external/ocx/external/…`
   and still satisfy ocx's requirements. Dropping or stale-pinning it does
   **not** error; it silently resolves unpatched crates.io releases.
   ```sh
   cargo tree -i oci-client | head -3     # must name the fork path
   ```
4. **No internal-tier crate** — the bump must not add a path row into a crate
   outside the ecosystem/interface tier. ocx's `task satellite:verify` asserts
   this; a row added here fails that job upstream.

## Phase 4 — semantic review (delegate)

Split the eight crates into two groups and give each an `opus` subagent
(CLAUDE.md model routing — this is correctness- and security-adjacent):

- **config + index** — env-key surface, config/project resolution, index
  regeneration and object retention.
- **oci + util** — registry auth store, OCI client builder and transport, TLS
  roots, file locking, atomic writes.

Ask each for exactly four things, with `file:line` cites:

1. **Breaking changes** against the mirror's real usage. Get that list first:
   ```sh
   grep -rhoE 'ocx_(config|console|exit|index|oci|package|sign|util)::[A-Za-z_:]+' \
     src crates --include='*.rs' | sort | uniq -c | sort -rn
   ```
2. **New reusable pub API** — and what mirror code it would replace.
3. **Silent behaviour changes** — what changes for a caller that changes no
   code. This is where bumps actually hurt: retention rules, resolution order,
   default timeouts, what a walk now refuses to adopt.
4. **Bugs fixed** — one line each, plus: *can the mirror's own code carry the
   same defect?* An upstream fix inside a crate the mirror links is inherited
   for free; an upstream fix to a pattern the mirror re-implements is not.

Prompt hygiene for those agents: every git command against the submodule uses
`git -C "$(git rev-parse --show-toplevel)/external/ocx" …` — the guard hook
blocks a bare `cd` into the submodule followed by a `git` command — and
`/usr/bin/git`, since the `git` alias truncates long output.

## Phase 5 — consolidation

The point of the bump. For every new or hardened shared helper, find the
mirror's hand-rolled twin and route it through the crate instead. Standing
candidates, each a real duplication class:

| Upstream home | Mirror twin to check |
|---------------|----------------------|
| `ocx_config::env::keys::*` | `OCX_*` names as string literals — `ocx_mirror_pipeline::ocx_cli` forwards a hand-maintained whitelist to every child `ocx` |
| `ocx_util::fs::persist_temp_file` | any `fs::write` / `File::create` onto a path a concurrent reader may open |
| `ocx_index` retention / sweep | `ocx_mirror_pipeline::registry_sync::index_write` dispatch-object and CAS retention |
| `ocx_oci::auth` | anything reading or writing a docker `config.json` outside the crate |
| `ocx_util::fs` locking | ad-hoc lock files |

Rule: adopt a const or a helper freely; do **not** adopt an upstream function
whose contract needs ocx-resolved state the mirror never builds (an
`OcxConfigView`, a project walk result). Take the names, not the machinery.

## Phase 6 — upstream issue cross-check

```sh
git -C "$(git rev-parse --show-toplevel)/external/ocx" log --format='%s%n%b' $OLD..$NEW | grep -oE '#[0-9]+' | sort -u
gh issue view <N> --repo ocx-sh/ocx --json number,title,state,labels
gh issue list --repo ocx-sh/ocx-mirror --state open --limit 100 --json number,title
```

For each upstream fix, answer one question: **does the mirror have an issue
for the same defect, and can it now be closed?** Two outcomes worth reporting:

- upstream fix lands the mirror's open issue → close it, citing the ocx commit;
- upstream fix describes a defect the mirror plausibly shares but has no issue
  for → file one.

Every reference is a full link — `[ocx-sh/ocx#488](https://github.com/ocx-sh/ocx/issues/488)`.

## Phase 7 — floor decision

Does the bump raise the **spawned-CLI** floor (the mirror's argv needs a
subcommand or flag only the new ocx has)? If yes, README.md § "Bumping ocx"
lists the four places the pin moves together, plus the golden-fixture
regeneration. If no, leave `ocx.lock` alone — a linked-crate bump does not
require a CLI bump, and coupling them for no reason strands the mirror on an
unreleased ocx.

**Provisional pointers**: pointing `external/ocx` at an unreleased commit
breaks downstream CI checkout and must be re-pointed at a release tag before
any mirror release. A mirror release must never ship before the ocx release it
pins.

## Phase 8 — gate and land

```sh
cargo fmt
ocx run -- task verify        # plain `task verify` exits 65 on a stale direnv
```

Never pipe the gate into `tail`/`head` — the pipeline reports the last
command's status and a red gate reads as green. Let it print, or
`task verify > log 2>&1; echo $?`.

Commit the pointer and the lock together:

```
chore(deps): bump external/ocx to <short-sha> (<n> commits)
```

Consolidation edits and issue closures are **separate commits** — a bump commit
that also refactors cannot be reverted cleanly.

## Report to the owner

Conclusion first, then `## Actions`. The owner needs five lines, not the diff:

- range and size (`<old>..<new>`, n commits, m files);
- compile verdict and any breaking API the mirror had to absorb;
- silent behaviour changes that reach the mirror's output;
- consolidation opportunities found, as file:line;
- issues to close or file, as links.

Everything else is working notes.

## Traps this procedure exists to catch

- **A doc-comment edit reads like a behaviour change.** A diff line moving
  "30 s" to "120 s" in a doc comment may be a stale-doc fix, not a retune.
  Always confirm a constant's value at *both* revs before reporting it:
  `git -C "$(git rev-parse --show-toplevel)/external/ocx" grep -n '<CONST>' $OLD -- crates/` against the working tree.
- **Test-only churn dominates the line count.** ocx invests heavily in test
  infrastructure; a crate can show ±400 lines with zero production delta.
  Check whether the changed hunks are inside `#[cfg(test)]` before drawing
  conclusions from a diffstat.
- **`pub` → `pub(crate)` is the breakage `--stat` hides.** Upstream demotes an
  item the moment its last cross-crate caller goes away — and deletes it
  outright just as readily, which the `^-\s*pub` grep reports identically.
  `cargo check --all-targets` is the only reliable detector.
- **A `feat(...)!:` in the log is usually not your break.** It is a CLI
  contract change (surface 2). Read the two-surfaces table before escalating.
- **The bash CWD persists into the submodule.** Every git command targeting
  it is spelled `git -C "$(git rev-parse --show-toplevel)/external/ocx" …` in
  full, never a `cd` into it followed by a bare `git …` — the repo's
  `.claude/hooks/pre_tool_use_guard.py` PreToolUse guard blocks that `cd`
  form outright (rule G2), so a later `git` command cannot silently run
  against the submodule and land work on the wrong branch.
- **`task verify | tail` reports `tail`'s exit code.** A red gate reads green.
