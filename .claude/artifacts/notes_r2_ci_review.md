# R2 CI/docs review — verdicts 4–8, fleet ordering, and one extra finding

Scope: everything except `src/`, `crates/`, `tests/`, `Cargo.toml`.
Baseline `main`; working tree uncommitted **on `main`** (not `feat/ocx-0.6-adoption`).
All ocx behaviour below is measured against real binaries, not recalled.

---

## 4. `ocx.toml` / `ocx.lock` — PASS

### `lock_version` did not move

```
main   ocx.lock:2   lock_version = 3     generated_by = "ocx 0.5.6"
HEAD   ocx.lock:2   lock_version = 3     generated_by = "ocx 0.6.0"
```

`declaration_hash_version` is also unchanged (1). Only three metadata lines moved:
`declaration_hash`, `generated_by`, `generated_at`. This matters for the fleet
(see §Rollout): a 0.5.8 binary can still read a 0.6.0-written lock.

### The four version claims, checked against what upstream actually published

Upstream via `gh api repos/<r>/releases/latest`:

| Tool | ocx.lock resolves to | Upstream latest | Published | Verdict |
|---|---|---|---|---|
| go-task | **3.53.1** | `v3.53.1` | 2026-08-18 | current |
| git-cliff | **2.13.1** | `v2.13.1` | 2026-04-26 | current |
| lychee | **0.24.2** | `lychee-v0.24.2` | 2026-05-01 | current |
| uv | **0.12.5** | `0.12.7` | 2026-08-27 | **2 patches behind — see below** |

Each lock version was confirmed **digest-for-digest**, not by name. Method:
`ocx -r --json package inspect ocx.sh/<repo>:<version>` and match every
`candidates[].digest` against the `[tool.platforms]` table. All platforms agreed;
`linux/amd64` shown as the witness:

```
go-task   3.53.1  linux/amd64 = sha256:df420d1c99d7e80ce6366104af4e6b57135e95940763e235c907525c438effee  == ocx.lock
git-cliff 2.13.1  linux/amd64 = sha256:01c961c1bbaefdfb3aea7b3951e1350240ba6a6d2dd508bc3a822f8a00eaf9ef  == ocx.lock
uv        0.12.5  linux/amd64 = sha256:b6ad3246b8d4709c5deac30b4d2a1674fc62f8686e52f7fde5cd63a79c7010b5  == ocx.lock
lychee    0.24.2  linux/amd64 = sha256:fb090cfaa91ba0fcfbe963185add1c72a1a18a4cfcbe560d84bd003ab9ddbb81  == ocx.lock
ocx       0.6.0   linux/amd64 = sha256:e9113c87037ed72c9c1f989ff1ec938bf71e3aef11144a76a350105f2b56c166  == ocx.lock
```

**On uv 0.12.5 vs upstream 0.12.7 — not a defect, and not fixable by `ocx lock`.**
The binding is `uv = "ocx.sh/astral-sh/uv:0"`, a rolling major tag, so `ocx lock`
resolves whatever the **ocx.sh mirror** currently carries, not what astral-sh
published. Measured:

```
ocx.sh/astral-sh/uv:0.12.6  -> NOT PRESENT on the mirror
ocx.sh/astral-sh/uv:0.12.7  -> NOT PRESENT on the mirror
ocx.sh/astral-sh/uv:0       -> sha256:04975adbdb9d08cb870de1d1e2e2e26f41d973b0de0429239d97d77246c557ce
ocx.sh/astral-sh/uv:0.12.5  -> sha256:04975adbdb9d08cb870de1d1e2e2e26f41d973b0de0429239d97d77246c557ce   (identical)
```

So `:0` **is** 0.12.5 today. The lock is the correct resolution of the declared
binding; the gap is upstream-mirror lag in `ocx.sh/astral-sh/uv`, out of this
diff's control. Worth a separate ticket against that mirror, not this branch.

### No digest is a downgrade

Old digests resolved back to their version tags the same way:

| Tool | main | HEAD | Direction |
|---|---|---|---|
| go-task | `e47cb228…` = **3.52.0** | `df420d1c…` = **3.53.1** | upgrade |
| ocx | `41562b5c…` = **0.5.8** | `e9113c87…` = **0.6.0** | upgrade |
| uv | `56cef303…` = **0.12.1** | `b6ad3246…` = **0.12.5** | upgrade |
| git-cliff | `01c961c1…` | `01c961c1…` | **unchanged** |
| lychee | `fb090cfa…` | `fb090cfa…` | **unchanged** |

The 0.5.8 attribution for the old ocx digest is independently proven: pulling
`ocx.sh/ocx/cli:0.5.8` materialised under `…/sha256/41/562b5c0f7056f91388ad3a7c2cb972/`.

**Correction to the task brief.** It said `ocx lock` "re-resolved all five bindings".
Re-resolution ran on all five, but only **three digests moved**. git-cliff and
lychee were already at the latest upstream release, so their blocks are
byte-identical to `main`. 23 insertions / 23 deletions = 3 metadata + 6 go-task
+ 6 ocx + 8 uv. Nothing was silently rewritten.

Unrelated pre-existing observation: the lychee block has **5** platforms, not 6 —
no `windows/arm64`. Identical on `main`, so not a regression here.

---

## 5. `test/**` — PASS

`test/src/helpers.py:166-170` is the only change, and it is correct:

```python
"description",
"push",
f"{registry}/{repository}",
```

Swept every ocx invocation in `test/src/` and `test/tests/`, including string
literals and fixture text: no `describe`, no `package info`, no `--announce-file`,
no `--tags-from-file`, no `--new`. The other `ocx package …` mentions in
`test/tests/` are prose in docstrings (`test_mirror_pylock.py:150`,
`test_mirror_pypi.py:15,180,614,621`, `test_mirror_pipeline.py:308`), all naming
verbs unchanged in 0.6 (`push`, `pull`, `test`).

---

## 6. Docs — FAIL (F1, F2 already reported and accepted) + one more, below

Nothing to add beyond F1/F2 except **F3**, which I had not yet reported:

### F3 — Low, pre-existing (not introduced by this diff)

`docs/reference/environment.md:91` documents a **three**-rung child-binary
resolution order:

> resolved in order: `OCX_BINARY_PIN`, an `ocx` co-located with the `ocx-mirror`
> executable, then `ocx` on `PATH`.

The implementation has **two** rungs. `src/pipeline/ocx_cli.rs:23-36`:

```rust
/// 1. `OCX_BINARY_PIN` env var (set by ocx itself when running under `ocx exec`).
/// 2. `"ocx"` on `PATH`.
pub(crate) fn resolve_ocx_binary() -> Result<PathBuf, String> {
    if let Ok(pin) = std::env::var("OCX_BINARY_PIN") && !pin.is_empty() {
        return Ok(PathBuf::from(pin));
    }
    Ok(PathBuf::from("ocx"))
}
```

There is no co-located lookup. Attribution: **pre-existing** — the diff to that
file is a one-line doc-comment edit (`ocx run` → `ocx exec`) and `main` has the
same two-rung body. Flagging it because it is a docs-scope defect and because it
is load-bearing for the rollout answer below: in generated CI the mirror is
invoked directly (no `ocx exec` wrapper), so `OCX_BINARY_PIN` is unset and the
child ocx comes from **PATH**, i.e. the repo's project toolchain — not from
anything co-located and not from the setup-ocx bootstrap.

Same fix pass as F1; it is one sentence.

---

## 7. `.licenserc.toml` — PASS. Necessary, not gate-weakening.

The edit was forced, not chosen. hawkeye 7.0.0 against the **old** config:

```
error: cannot load config
  caused by: ConfigInvalid => TOML parse error at line 6, column 1
  | excludes = ["external/**"]
  | ^^^^^^^^
  unknown field `excludes`, expected one of `header`, `files`, `props`, `git`, `styles`, `rules`
```

It refuses to load — no degradation, no partial check. The new config runs clean:

```
$ hawkeye check
235 files, 0 changes, 0 conflicts, 0 unsupported
```

**The include set did not narrow.** 235 is exactly the four globs, unchanged as
strings and only relocated under `[files]`:

```
src/**/*.rs            224
tests/**/*.rs            2
crates/*/src/**/*.rs     9
crates/*/tests/**/*.rs   0
                    ------
                       235
```

No license header stopped being checked.

---

## 8. CI-only failure modes — PASS

- **Pinned action SHAs.** This diff changes none. Every `uses:` across all six
  workflows is a 40-hex pin with a version comment; `setup-ocx` is
  `de8e3366f812941423985eaccae28663ef192e8b # v1.3.0` in both files it appears in.
- **`permissions:` blocks.** Untouched and still sufficient. `oci-publish.yml`
  deliberately declares none (a called workflow can only narrow its caller);
  `release.yml:80-84` grants `packages: write` for the GHCR push. No step gained
  a new capability requirement — `description push` and `announce` hit the same
  registry and the same `OCX_ANNOUNCE_TOKEN` the old spellings did.
- **Toolchain still provides everything.** `ocx.sh/ocx/cli:0.6.0` is published
  (resolved live), so `setup-ocx` can satisfy `version: "0.6.0"`.
- **Bootstrap-vs-lock skew.** `verify.yml:151` warns the bootstrap must stay
  `>=` the ocx that wrote `ocx.lock` or `ocx pull` exits 78. Lock says
  `generated_by = "ocx 0.6.0"`, pin says `0.6.0`. Level. And moot anyway,
  because `lock_version` held at 3.
- **All setup-ocx inputs agree.** `oci-publish.yml:86`, `verify.yml:157`, and
  every rendered golden: `version: "0.6.0"`, 0 remaining `"0.5.8"`.
- **Renovate coverage is intact.** The renderer derives the generated pin from
  one tracked constant — `matrix.rs:302` `OCX_CONTAINER_CLI_TAG = "v0.6.0"` with
  the `# renovate:` anchor, and `ocx_cli_version()` at :309 just strips the `v`.
  So the container-leg download and the generated `setup-ocx` pin cannot drift.
  Pre-existing gap, unchanged by this diff: the two `version:` inputs in *this*
  repo's own workflows are `with:` inputs, which no Renovate manager sees. They
  are hand-maintained, as their comments say.
- **Container test leg is not a break vector.** It only ever runs
  `ocx package test --platform … --identifier … <bundle>`, and `package test`
  exists with those flags in both 0.5.8 and 0.6.0.

---

## Fleet rollout ordering — yes, there is a window, and both single-sided orders hit it

### The measurement that decides it

Every 0.5-era spelling is **hard removed** in 0.6.0 — no deprecation alias:

```
ocx 0.6.0  package describe            -> exit 64
ocx 0.6.0  announce --tags-from-file   -> exit 64  ("tip: a similar argument exists: '--tags-file'")
ocx 0.6.0  push --announce-file        -> exit 64
ocx 0.6.0  push --new                  -> exit 64
```

and symmetrically:

```
ocx 0.5.8  package description push    -> exit 64  ("unrecognized subcommand 'description'")
ocx 0.5.8  announce --tags-file        -> exit 64  ("tip: a similar argument exists: '--tags-from-file'")
```

Only `ocx run` and `ocx package info` survive as soft aliases (exit 0 + `WARN …
removed in 0.7`). The four verbs the mirror actually spawns are not among them.

**So there is no ocx version that speaks both dialects.** The compatibility
window is empty by construction, not by oversight.

### The break matrix

A contrib repo's `ocx.toml` carries two bindings that both matter:
`ocx-mirror = ocx.sh/ocx/mirror:<X>` and `ocx = ocx.sh/ocx/cli:<Y>`.

| `ocx-mirror` | `ocx` | `describe.yml` | `push.yml` announce |
|---|---|---|---|
| 0.5-era | 0.5.8 | OK | OK |
| **0.6-adopting** | 0.5.8 | **exit 64** (`description` unknown) | **exit 64** (`--tags-file` unknown) |
| 0.5-era | **0.6.0** | **exit 64** (`describe` removed) | **exit 64** (`--tags-from-file` removed) |
| 0.6-adopting | 0.6.0 | OK | OK |

Both off-diagonal cells are broken. **Bumping either binding alone breaks the
repo**, in both directions.

### Why the setup-ocx pin is *not* the thing to sequence around

The generated push step calls `ocx-mirror` **directly**, never wrapped in
`ocx exec` — deliberately, per `templates/workflow.yml:208-213`. So
`OCX_BINARY_PIN` is unset, and by F3's two-rung resolver the child ocx is taken
from **PATH**, which setup-ocx populated from the repo's own project toolchain.
The `version:` input is only the bootstrap. That is why the ordering question
lands on the `ocx.toml` bindings, not on CI regeneration.

### Safe order

1. Publish the 0.6-adopting `ocx-mirror` release.
2. **Per repo, one commit:** bump *both* `ocx.toml` bindings — `ocx-mirror` to
   the new release and `ocx` to `ocx.sh/ocx/cli:0.6.0` — then `ocx lock`.
   One PR per repo. This is the only atomic step; do not split it into two waves.
3. **Regenerate CI as a follow-up wave** (`setup-ocx` `0.5.8` → `0.6.0`). Safe to
   defer, for three independently checked reasons:
   - `lock_version` held at 3, so a 0.5.8 bootstrap still `ocx pull`s a
     0.6.0-written `ocx.lock` without the exit-78 that `verify.yml:151` warns about;
   - the container test leg only uses `package test --platform --identifier`,
     valid in both versions;
   - the child ocx comes from PATH, not the bootstrap.

**One caveat that turns step 3 into a gate for some repos.** Deferral is only
sound where the push step is unmodified from the template. If any contrib repo
hand-edited it to wrap the call in `ocx exec --`, `OCX_BINARY_PIN` is set to the
*bootstrap* ocx (still 0.5.8), and the atomic binding bump in step 2 will **not**
save it — that repo breaks anyway. Grep the fleet for `ocx exec -- ocx-mirror` /
`ocx run -- ocx-mirror` in the push job before relying on the deferral; regenerate
those repos in step 2's PR.

**If one atomic PR per repo is impractical**, there is no safe split — the only
alternative is a code change in `ocx-mirror` to feature-detect the child ocx and
emit either dialect. That is a design decision, not a sequencing one, and I am
not recommending it: it re-adds the capability probe this diff just deleted.

---

## Diff-integrity detectors (whole diff, non-Rust)

- **Deleted assertions:** none.
- **Added `#[ignore]` / pytest skip / xfail:** none.
- **Stubs (`todo!`/`unimplemented!`):** none in scope.
- **Gate files edited:** `.licenserc.toml`, `oci-publish.yml`, `verify.yml`.
  All three necessary — the hawkeye schema migration is forced (§7) and the two
  workflow edits are the rename itself. **None makes a red gate green without
  fixing the underlying thing.**
- **Snapshot/golden churn:** the goldens changed by exactly two things —
  `version: "0.5.8"` → `"0.6.0"`, and three comment lines
  (`ocx run` → `ocx exec`, `--tags-from-file` → `--tags-file`). No assertion,
  no structure, no argv shape moved.
- **Lockfile churn:** `ocx.lock` intentional and fully verified in §4.
  `Cargo.lock` is out of my scope.
- **Scope creep:** none. Every changed file traces to the 0.6 rename.

---

## Two notes on method

1. **rtk silently reported a clean diff that was not clean.**
   `git diff main -- tests/golden/` through the rtk hook returned **zero**
   changed content lines. `/usr/bin/git --no-pager diff --no-color` on the same
   range returned **four**. If the other reviewer used the hooked path on
   `tests/golden/` or `Cargo.lock`, their result is not trustworthy. Matches the
   existing `rtk-filtered-output-unreliable` memory; use an absolute binary path
   for anything load-bearing.
2. **`echo "exit=$?"` after a pipe reports the pipe tail's status, not the
   command's.** Bit me once mid-review on the hawkeye run (reported 0 for a
   command that errored). Re-ran unpiped. Matches the
   `gate-exit-code-masked-by-pipe` memory.

## One process item

The working tree is uncommitted **on `main`**, not on `feat/ocx-0.6-adoption` as
the brief stated. `CLAUDE.md` says work on branches, never `main`. Branch before
committing.
