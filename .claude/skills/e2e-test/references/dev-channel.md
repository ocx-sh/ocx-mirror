# Tier 2 — Dev-channel e2e (GitHub, real infrastructure)

You loaded this file to validate an unreleased ocx-mirror against a real
downstream mirror repository via the dev registry — without cutting a release.

Contents: [Flow](#flow) · [Step by step](#step-by-step) ·
[What to assert](#what-to-assert) · [Facts & gotchas](#facts--gotchas) ·
[Precedent](#precedent)

## Flow

```
ocx-mirror branch ──Deploy Dev──▶ dev.ocx.sh/ocx/mirror:<ver>-dev_<ts>
                                        │ anonymous pull
downstream repo branch: ocx.toml pin ◀──┘
        └─▶ ocx lock → commit → dispatch generated workflow → assert
```

No OCX release is published; `dev.ocx.sh` is a separate registry with
anonymous read. Only the Deploy Dev workflow needs credentials (repo
environment `dev.ocx.sh`, already configured).

## Step by step

1. **Publish the dev build** (needs the branch pushed to GitHub):

   ```sh
   gh workflow run "Deploy Dev" --repo ocx-sh/ocx-mirror --ref <branch>
   gh run watch --repo ocx-sh/ocx-mirror   # ~build matrix + serial publish
   ```

   The `compute-version` job output yields the tag, shape
   `<next-semver>-dev_<UTC ts>` (e.g. `0.6.0-dev_20260804050813`). Full
   coordinate: `dev.ocx.sh/ocx/mirror:<tag>`.

2. **Pin it downstream** — one line in the mirror repo's `ocx.toml`, on a branch:

   ```toml
   [tools]
   mirror = "dev.ocx.sh/ocx/mirror:0.6.0-dev_20260804050813"  # was ocx.sh/ocx/mirror:0.5
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
  dev.ocx.sh; there is no `--version` flag). Template:
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
- `dev.ocx.sh` pulls are anonymous; no downstream secrets needed for
  bootstrap. Pushes to the repo's real target still need its own secrets.
- Deploy Dev is `workflow_dispatch`-only, serialized
  (`cancel-in-progress: false`) so timestamps stay monotonic.
- Dev tags are never announced (`announce: false`) — the index only
  observes the prod registry.

## Precedent

- Runbook narrative: `.claude/state/plans/plan_pylock_mirror.md` (§W3) and
  `.claude/state/plans/report_python_mirror_v2.md`.
- Live precedent repo: `~/dev/mirror-pypi` — still pinned to a dev build,
  history full of routine re-pin commits; its `bootstrap.yml` is the
  minimal assertion workflow.
- Policy statement: `.claude/artifacts/design_spec_ocx_python.md` ("Fast
  iteration loop", one pinned toolchain, no second install path).
