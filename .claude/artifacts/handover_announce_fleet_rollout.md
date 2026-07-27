# Handover: migrating a mirror onto GHCR and into the OCX index

**Date:** 2026-07-27
**Status:** written after the `bazelbuild/bazelisk` pilot published to GHCR and
resolved from the index. One thing is still broken and it will hit every repo
that follows: the announce from CI cannot write to the shared fork. See §5.
**Audience:** a maintainer with no context on the announce initiative, migrating
one of the ~41 remaining `ocx-contrib/mirror-*` repositories off `ocx.sh` onto
`ghcr.io` and onto the index.

Everything below is either a rule with the reason it exists, or a step. The
rules are the expensive part — every one of them cost real debugging time on the
pilot, and none of them is discoverable from the code alone.

Cross-references, for the reader who wants the decision record rather than the
procedure. Both live in the **ocx** repository, not this one:

- `.claude/artifacts/design_spec_announce_initiative.md` — §8 carries the
  E-P1…E-P4 rulings quoted below; §7 the dev-loop mechanics. Committed; you will
  find it in any ocx checkout.
- `meta-plan_announce.md` — track structure and the `G-*` gates. Lives in
  `.claude/state/plans/`, which is **gitignored and per-checkout**: it exists
  only on the machine that ran the initiative. If you cannot find it, everything
  in this document stands on its own — it is context, not a dependency.

Local reference for the spec surface: [`docs/reference/mirror-yml.md`](../../docs/reference/mirror-yml.md).

---

## 0. What "migrated" means

Four separate states, in this order. Each is independently observable, and a
later one silently does nothing if an earlier one is missing:

1. The mirror **publishes to GHCR** (`target.registry: ghcr.io`) instead of `ocx.sh`.
2. The GHCR package is **public**.
3. The index has a **root** for the logical name (`p/<vendor>/<pkg>.json` on `ocx-sh/index`).
4. The root's **`tags` map** carries observed tags, refreshed by `announce` on every run.

State 3 and state 4 are not the same state. See rule R6.

---

## 1. The rules

### R1 — Physical paths use slash segments, and the repo is named for the vendor

Logical index name `ocx.sh/<vendor>/<pkg>` maps to physical
`ghcr.io/ocx-contrib/<vendor>/<pkg>`. Never hyphen-flatten:
`ocx-contrib/bazelbuild/bazelisk`, not `ocx-contrib/bazelbuild-bazelisk`.
The publishing repository is named for the **vendor** — `mirror-bazelbuild`, not
`mirror-bazelisk`.

*Why:* the ruling is E-P4 (design spec §8). GHCR has no nested-namespace
feature — a multi-segment path is just a path, and the middle segment does
**not** need a repository of its own. Package-to-repository linkage comes from
the `org.opencontainers.image.source` annotation, which the pipeline writes
automatically. (Verified against `ghcr.io/homebrew/core/wget`: the segment
`core` matches no repository, yet the package links fine.) Naming the repo for
the vendor is what lets one repo ship several of that vendor's packages later
without a rename.

**Hard prerequisite:** do not publish a mirror before `ocx package push
--annotation` is available to it. Published without annotations, the package has
no repository linkage and no path shape to fall back on.

### R2 — GHCR's first publish is private, and only a human can flip it

A newly created GHCR package is private, including one created by a workflow in
a public repository. A *linked* package inherits the repository's **access
permissions** — who may read and write it — but not the repository's
**visibility**. Flipping it to public is a UI action on the package settings
page, and the token doing it needs `write:packages`.

*Why it costs time:* everything downstream looks broken and nothing says
"private". Anonymous resolution fails, the index root resolves to a package
nobody can pull, and the failure surfaces as an authorization error rather than
a visibility one. Budget one manual step per repository and do it immediately
after the first successful push.

### R3 — Anonymous GHCR never answers 404, so `discover` must log in

Observed responses to a read of `ghcr.io/v2/<path>/tags/list`:

| Caller | Package state | Response |
|---|---|---|
| anonymous | absent | `403 DENIED` |
| anonymous | present but private | `401 UNAUTHORIZED` |
| authenticated | absent | `404 NAME_UNKNOWN` |

*Why it matters:* the pipeline's `discover` job decides "which `(version,
platform)` pairs are missing" from the target's tag list, and it treats an
authoritative *repository not found* as "first publish, nothing published yet".
Anonymous, that authoritative answer never arrives — the registry will not
confirm non-existence to a caller it cannot authorize. So the generated workflow
does a `docker login ghcr.io` in `discover` before planning; `ocx` picks the
credential up through its native-credential fallback. Remove that login and a
first publish can never start.

The `403` / `401` split is what lets a fleet-wide audit distinguish "not
published yet" from "published but still private" without credentials — useful
for R2. Treat it as an observation about today's GHCR, not a contract:
`ocx`'s own error surface collapses both into a `DENIED` authentication failure.

### R4 — Bumping the `cli` pin re-resolves the lock to a version the bootstrap rejects

`ocx.toml` in a mirror repo pins the toolchain; `ocx.lock` records the resolved
digests. Re-resolving with a *newer* `ocx` than the one CI bootstraps writes
`lock_version = 3`, and the `ocx` that `ocx-sh/setup-ocx` installs rejects it —
the run dies at **exit 78** before step one, with nothing about lock versions in
the visible failure.

*Rule:* re-resolve the lock with the same `ocx` version `setup-ocx` installs
(the pilot's lock records `generated_by = "ocx 0.4.3"`; check the header of the
repo's own `ocx.lock`). Never hand-edit `ocx.lock` — use `ocx update`.

Related, from the dev loop: pin the **floating** dev tag
(`dev.ocx.sh/ocx/cli:0.5.0-dev`), never a build-timestamped one
(`…-dev_20260727…`). If `ocx update` keeps resolving the old digest, the deploy
failed to advance the floating tag — fix the deploy, do not pin a timestamp.

### R5 — The announce bot has no write on `ocx-sh/index`; the namespace must be claimed first

`announce:` in `mirror.yml` names a `fork:` (`ocx-contrib/index`). The bot pushes
its branch to that **fork** and opens a cross-repository pull request against
`ocx-sh/index`. It holds no write on the index itself, by design.

Two consequences:

- The token in `OCX_ANNOUNCE_TOKEN` needs push on the **fork**, not on the
  index. When it does not have it, GitHub masks the unauthorized *write* as
  not-found — the announce dies on
  `forge returned HTTP status 404 for https://api.github.com/repos/ocx-contrib/index/git/refs`.
  There is no missing path; the path is fine and the write was refused.

  **Do not debug this from the collaborator list.** Observed on
  `ocx-contrib/mirror-bazelbuild` run
  [30254441005](https://github.com/ocx-contrib/mirror-bazelbuild/actions/runs/30254441005):
  `ocx-bot` holds `push` on `ocx-contrib/index` as a collaborator, and the
  announce still 404'd. The token is scoped narrower than the account it
  belongs to. Check, in order: the PAT's own scopes (a classic PAT needs full
  `repo`; `public_repo` alone is not enough for the org policy path), then
  whether the org restricts classic tokens, then — for a fine-grained PAT —
  whether `ocx-contrib/index` is in its repository selection with
  `Contents: read and write` **and** `Pull requests: read and write`.
  A PAT can never exceed the permission of the GitHub App backing the machine
  account, so an App-scoped read-only grant caps the PAT regardless of what the
  PAT itself requests.
- `announce` **refuses an unclaimed namespace**. The root must exist before the
  first announce, and the bot cannot create it. The claim is a separate,
  human-lane pull request against `ocx-sh/index`.

### R6 — A merged claim with `tags: {}` does not resolve

The index root and its `tags` map are separate states. A merged claim gives you
a root — name, `repository`, `owners`, `upstream` — with an empty `tags` map,
and `ocx package inspect ocx.sh/<vendor>/<pkg>` still fails. Only the first
successful `announce` writes tags into it.

*Why it matters:* "the claim PR merged" reads like done and is not. The pilot
needed two merged pull requests before the name resolved
([#80](https://github.com/ocx-sh/index/pull/80) claim,
[#81](https://github.com/ocx-sh/index/pull/81) 11 curated tags).

### R7 — Seed `owners[]` with the bot's numeric id at claim time

The claim PR is where the bot gets standing. The root's `owners[]` must contain
the machine account by **numeric** id — `ocx-bot`, `309019509` — alongside the
human owner. The `upstream` block (`org`, `repository_url`, `disclaimer`) is
governance-mandatory for third-party mirrors and also belongs in the claim.

*Why:* forgetting either means a green announce run whose pull request the index
CI cannot classify into the machine lane, so it waits for a human click that
nobody is expecting.

### R8 — A Renovate `customManager` that matches nothing looks exactly like one that works

Your generated workflows carry SHA-pinned actions you cannot bump yourself —
they are generated files behind a drift guard. Those pins are maintained
upstream in `ocx-mirror` by a `customManager`, because they live outside
`.github/` where the built-in manager would find them.

That manager pointed at `src/command/pipeline/generate/templates/` — a path
missing the `package/` level, and so a path that has not existed since that
level was introduced. It matched nothing, opened no pull requests, and reported
no error. Every action pin baked into every generated workflow went unbumped for
as long as it was wrong. Fixed 2026-07-27; if your generated workflows carry
suspiciously old action pins, this is why.

*The general rule:* a `customManager` fails **silently**. A wrong
`managerFilePatterns` glob, a regex that no longer matches after a rename, a
capture group renamed upstream — all produce the same observable as a manager
with nothing to do, which is nothing at all. Renovate cannot tell you it found
no files, because finding no files is legitimate.

So do not review one by reading it. Run the glob and the regex against the
working tree and assert a match — a dozen lines of Python is enough:

```python
import json, re, pathlib
cfg = json.load(open("renovate.json"))
for m in cfg["customManagers"]:
    pat = m["matchStrings"][0].replace("(?<", "(?P<")   # JS → Python named groups
    hits = [p for p in pathlib.Path(".").rglob("*")
            if re.search(m["managerFilePatterns"][0].strip("/"), p.as_posix())
            and p.is_file() and re.search(pat, p.read_text())]
    assert hits, f"customManager matches nothing: {m['description']}"
```

Same reflex applies to the `paths:` globs on rule files and to any other
config whose failure mode is "quietly does less".

---

## 2. Per-repository checklist

Order matters — steps 4 and 6 are the ones that block if run early.

1. **Rename the repo to the vendor** if it is still named for the tool
   (`mirror-bazelisk` → `mirror-bazelbuild`). GitHub redirects the old name, so
   this is safe to do first (R1).

2. **Point `target:` at GHCR** in `mirror.yml`:

   ```yaml
   target:
     registry: ghcr.io
     repository: ocx-contrib/<vendor>/<pkg>
   ```

   Slash segments; no repository needed for the `<vendor>` segment (R1).

3. **Decide the libc story** and set `assets:` / `platforms:` keys accordingly.
   Procedure in §3.

4. **Claim the namespace** on `ocx-sh/index` — a human-lane pull request adding
   `p/<vendor>/<pkg>.json` with `name`, `repository: oci://ghcr.io/…`,
   `owners[]` (human **and** `ocx-bot` / `309019509`), `upstream {org,
   repository_url, disclaimer}`, `status: "active"`. This must merge **before**
   the first announce (R5, R7). No new claim is needed if the repo already has
   an `ocx.sh` root — in that case only seed `owners[]` and `upstream` onto the
   existing root.

5. **Add the `announce:` block**:

   ```yaml
   announce:
     package: <vendor>/<pkg>     # logical name, spelled out — not derived from target
     fork: ocx-contrib/index     # index_repo defaults to ocx-sh/index
   ```

   The logical name and the physical path are related by convention only; the
   code cannot derive one from the other, which is why it is spelled out.

6. **Confirm `OCX_ANNOUNCE_TOKEN` reaches this repository.** It is an
   `ocx-contrib` org secret; check its repository-access list covers the new
   repo. Without it the run publishes normally and records the announce as
   `skipped_no_credential` — green, and the index silently never learns about
   the release. With it present but expired, the push job **fails**, by design.

7. **Add `containers:`** to the Linux platforms if the package needs a multi-libc
   test matrix. Pattern in §4.

8. **Regenerate CI** and commit the result:

   ```sh
   ocx run -- ocx-mirror package pipeline generate ci
   ```

   `.github/workflows/{mirror,describe,verify-generated}.yml` are generated
   output — never hand-edit them. `verify-generated.yml` reds the next run if
   they drift from what the pinned renderer produces, so the renderer version in
   `ocx.lock` and the committed workflows must move together (R4).

9. **Run it**, then **flip the GHCR package to public** as soon as the first push
   succeeds (R2).

10. **Verify** from a scratch `OCX_HOME` with no credentials:

    ```sh
    ocx package inspect ocx.sh/<vendor>/<pkg>
    ```

    Failing here after a green run almost always means R2 (still private) or R6
    (root merged, tags empty).

---

## 3. The libc / `os.features` decision procedure (E-P2)

Run this per package, once:

1. **Does upstream ship separate musl and glibc builds?**
   - **Yes** → mirror both. They are *variants of one package*, distinguished by
     `+libc.musl` / `+libc.glibc` platform keys, and they resolve by platform.
     Never by tag suffix — no `-musl` tags, ever.
   - **No, one universal binary** → publish one variant.

2. **For the single-binary case, is it actually static?** A single static Go or
   Rust-musl binary is universal, and its correct `os.features` is **empty** —
   declaring `libc.glibc` on something that needs no libc is a false narrowing.
   A dynamically-linked single binary is not universal and must declare what it
   links.

3. **Whatever you declared, make the container matrix prove it** (§4). An empty
   `os.features` is a claim that the artifact loads under *any* userland; the
   only evidence for it is the artifact running under both a musl and a glibc
   loader. bazelisk is a single static Go binary, so its `os.features` is empty
   and its alpine leg is what makes that honest.

**Do not** declare `+libc.*` on a platform key and then test it in an image of
the other family. A musl-static artifact runs fine under glibc, so the leg goes
green having verified nothing. The renderer rejects that pairing at spec-load
time rather than letting it publish a claim nothing checked.

---

## 4. The `containers:` test matrix pattern

Not a new mechanism and not a script — it is the existing mirror test pipeline.
A platform with `containers:` runs each `ocx package test` inside `docker run
<image>` against a libc-matched static `ocx` release, so the mirrored artifact is
loaded by that image's own loader.

```yaml
platforms:
  linux/amd64:
    runner: ubuntu-latest
    containers:
      - { image: "ubuntu:24.04", shell: bash }
      - { image: "alpine:3.20",  shell: sh }
      - { image: "fedora:40",    shell: bash }
  linux/arm64:
    runner: ubuntu-24.04-arm
    containers:
      - { image: "ubuntu:24.04", shell: bash }
      - { image: "alpine:3.20",  shell: sh }
      - { image: "fedora:40",    shell: bash }
```

Constraints worth knowing before you write it:

- **Linux only.** `containers:` on a `darwin/*` or `windows/*` platform is
  rejected at spec load — those runners have no Linux container engine. Mixing
  is fine: container legs on Linux, native legs elsewhere.
- **No qemu.** An arm64 platform with `containers:` needs an arm64 `runner:`
  (`ubuntu-24.04-arm`). Mismatch reds the leg up front with a message naming the
  fix, instead of a bare exec-format error minutes in.
- **Nothing else to declare.** `containers:` needs no companion block. The
  `ocx` the legs download is a renderer constant, so the whole fleet tests
  against one binary and it advances when your `ocx.lock` does.
- **Shell defaults** are inferred from the image name: alpine → `sh`;
  ubuntu/debian/fedora/rocky/opensuse → `bash`. Any other image needs an
  explicit `shell:`. Setting it explicitly anyway documents the intent.
- **The gate is AND across containers.** `(version, platform)` is green only when
  every container leg for that platform is green, and `push` is gated on it. One
  red image blocks the publish for that tile — which is the point.

---

## 5. Open questions for the fleet

None of these blocked the pilot; all of them are plausible at ~41 repositories.

- **Pull-request volume against `ocx-sh/index`.** Each repo needs one claim plus
  one announce PR to start, then an announce PR per release. Forty repos
  migrating in the same window is a burst the index CI has never seen.
- **Rate limits on the shared fork.** `ocx-contrib/index` is a single fork with a
  branch per package. At fleet scale, concurrent pushes to one fork from ~40
  workflows is the first thing to measure.
- **Ordering.** Simultaneous first-claims from many repos collide on index
  review capacity. Sequence the claims; the announces can then run unattended.
- **Should the org secret widen to all `ocx-contrib/mirror-*`** before or after
  the human-lane claims are seeded? Widening first means a repo can announce
  into a namespace nobody claimed — which `announce` refuses, so the failure is
  loud rather than wrong. Widening after means one more manual step per repo.
- **The fork path is blocked today — this is the one thing to fix before the
  fleet.** Both of the pilot's merged index pull requests were opened from
  branches in `ocx-sh/index` itself under a human identity, not from the
  `ocx-contrib/index` fork under the bot. The announce PR
  ([#81](https://github.com/ocx-sh/index/pull/81)) *was* merged by
  `app/github-actions` with no human click, so the machine **merge** lane is
  proven. The cross-repository leg — bot pushes to the shared fork, opens the PR
  from there — is not, and it is what every fleet repo will use.

  As of 2026-07-27 it fails: run
  [30254441005](https://github.com/ocx-contrib/mirror-bazelbuild/actions/runs/30254441005)
  published `1.25.0` across all five platforms and then reported
  `index announce for bazelbuild/bazelisk failed: … HTTP status 404 for
  https://api.github.com/repos/ocx-contrib/index/git/refs — the registry is
  ahead of the index`. That is R5, and the push job is red because of it (by
  design: a registry ahead of the index is a failure, not a warning). The index
  root is therefore missing the `1.25.0` / `1.25` tags. Fix
  `OCX_ANNOUNCE_TOKEN` per R5 before migrating repo two — every fleet repo will
  hit this on its first announce.

  **Recovery is a re-announce, not a re-publish.** The images are in GHCR and
  correct; only the index is behind. Once the token works, re-run the mirror
  workflow: `discover` will find nothing new (`1.25.0` is already published, so
  no version is scheduled and no bundle is rebuilt), and the announce refreshes
  the root's `tags` map from the registry's actual state. Do not delete tags or
  force a republish to "trigger" it — that would orphan digests under
  `build_timestamp: none` for no gain.

---

## 6. Things that look like litter and are not

- **`ocx-contrib/mirror-libc-probe` is archived, not deleted, on purpose.** It is
  a throwaway mirror that published nothing, but its three runs are the only live
  proof of two renderer behaviours the pilot cannot show: a test that *must* fail
  under musl and pass under glibc reddening only the alpine legs
  ([30252063070](https://github.com/ocx-contrib/mirror-libc-probe/actions/runs/30252063070)),
  and the arch guard firing with its intended message when an arm64 container leg
  is pinned to an x86_64 runner
  ([30252180542](https://github.com/ocx-contrib/mirror-libc-probe/actions/runs/30252180542)).
  Deleting the repository 404s those URLs and orphans the evidence. Archived, it
  runs nothing and costs nothing. Leave it.
