# Tier 2 — Dev-channel e2e (GitHub, real infrastructure)

You loaded this file to validate an unreleased ocx-mirror against a real
downstream mirror repository via the dev registry — without cutting a release.

Contents: [Flow](#flow) · [Step by step](#step-by-step) ·
[What to assert](#what-to-assert) · [Facts & gotchas](#facts--gotchas) ·
[Precedent](#precedent)

## Flow

```
ocx-mirror branch ──release.yml (dev)──▶ ghcr.io/ocx-sh/dev/mirror:<ver>-pr<N>_<ts>
                                              │ pull
downstream repo branch: ocx.toml pin ◀──┘
        └─▶ ocx lock → commit → dispatch generated workflow → assert
```

No OCX release is published; the dev package is separate from the release
package. Only the dev dispatch of `release.yml` writes to it, with the
workflow's own token. `<ver>` is the `Cargo.toml` workspace version, not a
computed next version.

The track follows the dispatched ref:

| Dispatched from | Tag | Package |
|-----------------|-----|---------|
| branch with an open pull request | `<ver>-pr<N>_<ts>`, rolling `<ver>-pr<N>` | `ghcr.io/ocx-sh/dev/mirror` |
| branch without a pull request | `<ver>-dev_<ts>`, rolling `<ver>-dev` | `ghcr.io/ocx-sh/dev/mirror` |
| `main` | `<ver>-canary_<ts>`, rolling `<ver>-canary` | `ocx.sh/ocx/mirror` (the release package) |

**Never dispatch this tier on `main`.** A dev dispatch there publishes a
canary into the release package and announces it to the index. Use a branch.

## Step by step

1. **Publish the dev build** (needs the branch pushed to GitHub):

   ```sh
   gh workflow run release.yml --repo ocx-sh/ocx-mirror --ref <branch> -f mode=dev
   gh run watch --repo ocx-sh/ocx-mirror   # ~build matrix + serial publish
   ```

   With an open pull request for the branch, the `comment` job posts one
   sticky pull request comment carrying `Rolling:` and `Exact:` addresses
   (`ghcr.io/ocx-sh/dev/mirror:<ver>-pr<N>_<UTC ts>`, e.g.
   `0.6.5-pr12_20260804050813`). Copy the exact address.

   Without a pull request there is no comment. List the tags and take the
   newest `<ver>-dev_<UTC ts>`:

   ```sh
   ocx --remote --format json index list ghcr.io/ocx-sh/dev/mirror
   ```

   Every branch without a pull request shares the rolling `<ver>-dev` tag,
   so pin the exact `…_<UTC ts>` tag.

2. **Pin it downstream** — one line in the mirror repo's `ocx.toml`, on a branch:

   ```toml
   [tools]
   mirror = "ghcr.io/ocx-sh/dev/mirror:0.6.5-pr12_20260804050813"  # was ocx.sh/ocx/mirror:0.5
   ```

   Then `ocx lock` (or `direnv reload`) to regenerate `ocx.lock`, commit
   **both files together** (`chore: re-pin mirror (<reason>)`).

3. **Regenerate CI with the dev binary** if the templates changed (this is
   itself part of the test):

   ```sh
   ocx-mirror package pipeline generate ci --spec <pkg>/mirror.yml
   ```

   Commit the regenerated workflows. A repo without generated CI yet
   (e.g. `mirror-pypa`) needs this step regardless.

4. **Dispatch** the generated `mirror-<pkg>.yml` workflow on the branch
   (`workflow_dispatch`).

## What to assert

- **Cheapest smoke**: a bootstrap-style job — `ocx version` +
  `ocx-mirror --help` exits 0 (proves the dev pin resolved from
  the dev package; there is no `--version` flag). Template:
  `mirror-pypi:.github/workflows/bootstrap.yml`.
- **Pipeline smoke**: the `discover` job succeeds and its `plan.json`
  output is sane (`has_new`, `versions[]`).
- **Full e2e**: the run publishes to the repo's real target and the
  registry tag list shows version + cascade tags.

## Facts & gotchas

- **Two pins, don't conflate**: `ocx.toml [tools] mirror = …` selects the
  `ocx-mirror` binary (the thing under test). The generated workflow's
  `setup-ocx` `version:` input pins the **ocx CLI** — baked from
  `OCX_CONTAINER_CLI_TAG` in `generate/ci/matrix.rs`, not what you edit
  for a dev test.
- `ocx pull`/`setup-ocx` exits 78 without a committed `ocx.lock` matching
  `ocx.toml` — always commit the lock with the pin.
- Pulling the dev package needs no downstream secret for
  bootstrap. Pushes to the repo's real target still need its own secrets.
- The dev dispatch of `release.yml` is serialized per ref (concurrency group
  `release-dev-<ref>`, `cancel-in-progress: false`), so timestamps stay
  monotonic within one ref. Two branches run concurrently and can race on
  the rolling `<ver>-dev` tag; the canary (main) and pull request tracks have
  their own rolling tags and the same per-ref queue.
- Pull request and `-dev` tags are never announced — the index only observes
  the release package. `prune.yml` removes a pull request build once the
  pull request is closed or the build is 30 days old, and a `-dev` build once
  it is 30 days old unless it is the newest of its version.

## Precedent

- Runbook narrative: `.claude/state/plans/plan_pylock_mirror.md` (§W3) and
  `.claude/state/plans/report_python_mirror_v2.md`.
- Live precedent repo: `~/dev/mirror-pypi` — still pinned to a dev build,
  history full of routine re-pin commits; its `bootstrap.yml` is the
  minimal assertion workflow.
- Policy statement: `.claude/artifacts/design_spec_ocx_python.md` ("Fast
  iteration loop", one pinned toolchain, no second install path).
