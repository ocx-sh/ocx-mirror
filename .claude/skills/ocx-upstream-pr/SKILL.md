---
name: ocx-upstream-pr
description: Author a change to upstream ocx (ocx-sh/ocx) inside the mirror's external/ocx submodule and land it as one GitHub-verified commit, then move the mirror pointer onto it. Use when a change belongs in ocx rather than the mirror — a crate promotion per crate-placement.md, an upstream fix or new pub API the mirror needs — and has to reach ocx through a PR before the pointer can move. Covers the submodule branch setup the guard hook requires, ocx's own verify gate, the GraphQL createCommitOnBranch signed-commit recipe, PR checks and Verify Deep, the landing rules (squash only if the repo setting, the main ruleset and an approving review all allow it; never rebase-merge; otherwise an owner action) and the hand-over to /update-ocx.
user-invocable: true
---

# ocx-upstream-pr — land a change in ocx from the submodule

Authority: `.claude/artifacts/adr_bazel_crate_split.md` § C9 (b) (steps), § C6
(landing) and § Amendments. This skill is the executable form; if they disagree,
the ADR wins.

The end state is fixed: the PR head is **exactly one GitHub-verified commit whose
parent is the current ocx `main` tip**. ocx's `main` ruleset requires signed
commits, a PR with an approving review, and no force-push — nothing else lands.
Agent sessions here cannot sign locally, so the verified commit is made
server-side by GraphQL `createCommitOnBranch`; local commits are scratch and never
pushed.

## The guard hook

`.claude/hooks/pre_tool_use_guard.py` (PreToolUse) blocks slips around the
submodule. Its stderr names the rule and the compliant spelling:

| Rule | Blocks | Meaning / fix |
|------|--------|---------------|
| G1 | `git commit\|merge\|pull\|push` in the submodule while its HEAD is `main` or detached | you skipped step 1 — create the feature branch first |
| G2 | any git in the submodule without an absolute `-C` (`cd external/ocx && git …`, `-C external/ocx`, a cwd inside it) | spell it `git -C "$(git rev-parse --show-toplevel)/external/ocx" …` from the mirror root |
| G3 | `git submodule update … --recursive` not run inside the submodule | from the mirror root it reverts the submodule checkout; run it via the `-C` spelling |
| G4 | Write/Edit into the submodule while HEAD is `main` or detached | same as G1 — branch first |
| G5 | writes, and any non-read git, in the sibling checkout `../ocx` next to the mirror's main checkout | that is the owner's working copy; this repo only reads it. Author in the submodule |

So: **every** git command touching the submodule below is spelled
`git -C "$(git rev-parse --show-toplevel)/external/ocx" …`, run with the bash cwd
at the mirror root. No shell variable for the path — the hook recognises the
literal prefix only. A block is a real mistake: fix the command, never work
around the hook.

## State lives in files

Shell variables do not survive between Bash tool calls. An empty `$BRANCH` would
dispatch Verify Deep on `main` and hang the `headSha` wait. All run state lives
in the scratch dir `$(git rev-parse --show-toplevel)/.tmp/ocx-upstream-pr/`
(`.tmp/` is gitignored): `branch`, `headline`, `commit-body.txt`, `pr-body.md`,
`new`, `pr`, `ptr`, `ocx-verify.log`. Every block below starts with the
**prologue**, then reads what it needs and asserts it non-empty:

```sh
cd /abs/path/to/ocx-mirror                       # the literal -C prefix resolves from here
S="$(git rev-parse --show-toplevel)/.tmp/ocx-upstream-pr"
BRANCH=$(cat "$S/branch"); : "${BRANCH:?}"       # likewise NEW, PR, PTR from their files
```

A failed `: "${X:?}"` aborts the block — refill the file from its step, never
type the value inline from memory.

## Step 0 — preflight

```sh
cd /abs/path/to/ocx-mirror
git branch --show-current                        # must not be main
git -C "$(git rev-parse --show-toplevel)/external/ocx" status --porcelain   # must be empty
gh pr list --repo ocx-sh/ocx --state open --json number,title,headRefName  # no duplicate PR
S="$(git rev-parse --show-toplevel)/.tmp/ocx-upstream-pr"
mkdir -p "$S" && rm -f "$S"/{new,pr,ptr}
printf '%s\n' 'feat/<topic>' > "$S/branch"
printf '%s\n' 'feat(<scope>): <summary>' > "$S/headline"   # conventional commit headline
```

Dirty submodule → stop and report; it is someone's unlanded work. Branch name
`<type>/<topic>` in ocx's conventional-commit vocabulary.

## Step 1 — branch from a fresh ocx main

```sh
cd /abs/path/to/ocx-mirror
S="$(git rev-parse --show-toplevel)/.tmp/ocx-upstream-pr"
BRANCH=$(cat "$S/branch"); : "${BRANCH:?}"
git -C "$(git rev-parse --show-toplevel)/external/ocx" -c credential.helper= -c credential.helper='!gh auth git-credential' \
  fetch https://github.com/ocx-sh/ocx.git '+refs/heads/main:refs/remotes/origin/main' --tags
git -C "$(git rev-parse --show-toplevel)/external/ocx" switch -c "$BRANCH" origin/main
git -C "$(git rev-parse --show-toplevel)/external/ocx" submodule update --init --recursive
```

The switch comes **before** the nested update and before any edit (G1/G4). From
here to step 6 the mirror's `git status` shows `external/ocx` modified — do not
stage it.

## Step 2 — author and gate inside the submodule

Edit files under `external/ocx/` with Write/Edit. Commit locally as often as
useful — unsigned, never pushed:

```sh
cd /abs/path/to/ocx-mirror
git -C "$(git rev-parse --show-toplevel)/external/ocx" add -A
git -C "$(git rev-parse --show-toplevel)/external/ocx" -c commit.gpgsign=false commit --no-verify -m 'wip'
```

`--no-verify` skips ocx's `commit-msg` hook (`scripts/commit_gate.py`), which
demands a conventional subject and a verify mark from a completed local
`task verify` — these local commits are scratch and never pushed, so the hook
has nothing to certify; the subject and verify-mark discipline is enforced
where it counts, on the single server-made commit's headline (step 0) and gate
(step 2/3), and again by ocx CI on the PR.

Gate with **ocx's** `task verify`, not the mirror's, in a subshell so the cwd
does not leak:

```sh
cd /abs/path/to/ocx-mirror
S="$(git rev-parse --show-toplevel)/.tmp/ocx-upstream-pr"
(cd "$(git rev-parse --show-toplevel)/external/ocx" && ocx exec -- task verify > "$S/ocx-verify.log" 2>&1); echo "exit=$?"
```

`exit=0` or it is not green. Never pipe the gate into `tail`/`head`. Plain
`task verify` is fine when direnv is fresh; `ocx exec --` avoids the stale-direnv
exit 65. Also run the mirror's `ocx exec -- task verify` when the change is
something the mirror consumes — the mirror builds against the submodule as it
stands.

## Step 3 — the single verified commit

Precondition: step 2 green, submodule clean, merge-base with `origin/main` equal
to the remote main tip (the script refuses otherwise). If main moved:
re-fetch (step 1 fetch line), then
`git -C "$(git rev-parse --show-toplevel)/external/ocx" -c commit.gpgsign=false rebase origin/main`,
re-gate.

The commit is built on a **stage ref** (`<branch>-stage`) set to the tip, then
the PR branch is force-moved onto it in one update. Setting an open PR's branch
to the base tip directly makes the PR empty and GitHub closes it.

Write the body to `$S/commit-body.txt` first (Write tool, or `printf`), then:

```sh
cd /abs/path/to/ocx-mirror
S="$(git rev-parse --show-toplevel)/.tmp/ocx-upstream-pr"
BRANCH=$(cat "$S/branch"); HEADLINE=$(cat "$S/headline"); : "${BRANCH:?}" "${HEADLINE:?}"
test -s "$S/commit-body.txt" || { echo "no commit body"; exit 1; }
python3 - "$(git rev-parse --show-toplevel)/external/ocx" "$BRANCH" "$HEADLINE" "$S/commit-body.txt" > "$S/new" <<'PY'
import base64, json, subprocess, sys
sub, branch, headline, body_file = sys.argv[1:]
repo, stage = "ocx-sh/ocx", branch + "-stage"

def run(*a, inp=None):
    return subprocess.run(a, check=True, capture_output=True, input=inp).stdout
def git(*a): return run("git", "-C", sub, *a)
def gh(*a): return run("gh", "api", *a)
def text(b): return b.decode("utf-8").strip()
def upsert(ref, sha):
    try:
        gh("-X", "PATCH", f"repos/{repo}/git/refs/heads/{ref}", "-f", f"sha={sha}", "-F", "force=true")
    except subprocess.CalledProcessError as e:
        said = (e.stdout + e.stderr).decode("utf-8", "replace")
        if "Reference does not exist" not in said and "HTTP 404" not in said:
            raise                         # only a missing ref is created; anything else is fatal
        gh(f"repos/{repo}/git/refs", "-f", f"ref=refs/heads/{ref}", "-f", f"sha={sha}")

def main():
    tip = text(gh(f"repos/{repo}/git/ref/heads/main", "--jq", ".object.sha"))
    local = text(git("rev-parse", "origin/main"))
    base = text(git("merge-base", "origin/main", "HEAD"))
    if not tip == local == base:
        sys.exit(f"stale base: remote main {tip}, origin/main {local}, merge-base {base}")
    if git("status", "--porcelain"):
        sys.exit("submodule has uncommitted changes")

    adds, dels = [], []
    raw = git("diff", "--raw", "-z", "--no-renames", "--no-abbrev", "origin/main...HEAD")
    fields = iter(x for x in raw.split(b"\0") if x)
    for meta in fields:                   # ":<old mode> <new mode> <old> <new> <status>" then path
        path = next(fields).decode("utf-8")
        _, mode, _, _, status = meta.decode("utf-8").split()
        if status == "D":
            dels.append({"path": path})
        elif mode != "100644":            # renames arrive as D + A thanks to --no-renames
            sys.exit(f"{path}: mode {mode} — createCommitOnBranch writes plain files only")
        else:
            adds.append({"path": path, "contents": base64.b64encode(git("show", f"HEAD:{path}")).decode("ascii")})
    if not adds and not dels:
        sys.exit("nothing to commit")
    with open(body_file, encoding="utf-8") as f:
        body = f.read()

    query = "mutation($i: CreateCommitOnBranchInput!) { createCommitOnBranch(input: $i) { commit { oid } } }"
    payload = {"query": query, "variables": {"i": {
        "branch": {"repositoryNameWithOwner": repo, "branchName": stage},
        "message": {"headline": headline, "body": body},
        "expectedHeadOid": tip,
        "fileChanges": {"additions": adds, "deletions": dels}}}}
    upsert(stage, tip)
    try:
        out = json.loads(run("gh", "api", "graphql", "--input", "-", inp=json.dumps(payload).encode("utf-8")))
        if out.get("errors") or not out.get("data"):
            sys.exit(f"createCommitOnBranch failed: {out.get('errors')}")
        oid = out["data"]["createCommitOnBranch"]["commit"]["oid"]
        upsert(branch, oid)
    finally:
        try:
            gh("-X", "DELETE", f"repos/{repo}/git/refs/heads/{stage}")
        except subprocess.CalledProcessError as e:  # harmless: the next run force-resets it
            print(f"warning: {stage} not deleted: {e.stderr.decode('utf-8', 'replace')}", file=sys.stderr)
    print(oid)

try:
    main()
except subprocess.CalledProcessError as e:
    sys.exit(f"failed: {' '.join(e.cmd)}\n{e.stderr.decode('utf-8', 'replace')}")
PY
echo "exit=$? NEW=$(cat "$S/new")"
```

`exit=0` and a 40-hex `NEW`, or stop — a failed run leaves `$S/new` empty, so no
later block proceeds on a stale value. The payload goes through `--input -`
(stdin), never argv — base64 blobs overflow the argument limit. Then sync the
local branch, **resetting only after the fetch succeeded and the trees match**:

```sh
cd /abs/path/to/ocx-mirror
S="$(git rev-parse --show-toplevel)/.tmp/ocx-upstream-pr"
BRANCH=$(cat "$S/branch"); NEW=$(cat "$S/new"); : "${BRANCH:?}" "${NEW:?}"
git -C "$(git rev-parse --show-toplevel)/external/ocx" -c credential.helper= -c credential.helper='!gh auth git-credential' \
  fetch https://github.com/ocx-sh/ocx.git "$BRANCH" \
&& test "$(/usr/bin/git -C "$(git rev-parse --show-toplevel)/external/ocx" rev-parse FETCH_HEAD)" = "$NEW" \
&& test "$(/usr/bin/git -C "$(git rev-parse --show-toplevel)/external/ocx" rev-parse FETCH_HEAD^{tree})" \
      = "$(/usr/bin/git -C "$(git rev-parse --show-toplevel)/external/ocx" rev-parse HEAD^{tree})" \
&& git -C "$(git rev-parse --show-toplevel)/external/ocx" reset --hard FETCH_HEAD; echo "exit=$?"
/usr/bin/git -C "$(git rev-parse --show-toplevel)/external/ocx" rev-list --count origin/main..HEAD   # 1
gh api "repos/ocx-sh/ocx/commits/$NEW" --jq .commit.verification.verified                         # true
```

Tree mismatch means the server commit does not reproduce the local work — stop,
do not reset, investigate.

## Step 4 — PR, checks, Verify Deep

Write the PR body to `$S/pr-body.md`. Create the PR once; a fix-loop pass skips
the `create` line (the PR already follows the branch):

```sh
cd /abs/path/to/ocx-mirror
S="$(git rev-parse --show-toplevel)/.tmp/ocx-upstream-pr"
BRANCH=$(cat "$S/branch"); HEADLINE=$(cat "$S/headline"); : "${BRANCH:?}" "${HEADLINE:?}"
gh pr create --repo ocx-sh/ocx --base main --head "$BRANCH" --title "$HEADLINE" --body-file "$S/pr-body.md"
gh pr view "$BRANCH" --repo ocx-sh/ocx --json number --jq .number > "$S/pr"; echo "exit=$? PR=$(cat "$S/pr")"
```

```sh
cd /abs/path/to/ocx-mirror
S="$(git rev-parse --show-toplevel)/.tmp/ocx-upstream-pr"
BRANCH=$(cat "$S/branch"); NEW=$(cat "$S/new"); PR=$(cat "$S/pr"); : "${BRANCH:?}" "${NEW:?}" "${PR:?}"
gh pr checks "$PR" --repo ocx-sh/ocx --watch --fail-fast; echo "exit=$?"
gh workflow run verify-deep.yml --repo ocx-sh/ocx --ref "$BRANCH"
gh run list --repo ocx-sh/ocx --workflow verify-deep.yml --branch "$BRANCH" --event workflow_dispatch \
  --json databaseId,headSha,status,createdAt --jq "[.[] | select(.headSha==\"$NEW\")][0]"   # newest run for NEW; repeat until non-null
gh run watch <run-id> --repo ocx-sh/ocx --exit-status; echo "exit=$?"
```

Plain dispatch runs the full matrix including `satellite-verify` (the mirror
built against this tree). Both must be green **on `$NEW`**.

**Fix loop (C6.1):** edit, commit locally (unsigned) on top, re-gate (step 2),
then rerun step 3 — it recreates the single commit from a fresh tip and moves
the PR branch onto it. Then step 4 again (minus `create`). Never push a second
commit onto the PR branch.

## Step 5 — landing

**First, re-check the base.** Main may have moved during checks and review:

```sh
cd /abs/path/to/ocx-mirror
S="$(git rev-parse --show-toplevel)/.tmp/ocx-upstream-pr"
NEW=$(cat "$S/new"); PR=$(cat "$S/pr"); : "${NEW:?}" "${PR:?}"
P=$(gh api "repos/ocx-sh/ocx/commits/$NEW" --jq '.parents[0].sha'); M=$(gh api repos/ocx-sh/ocx/commits/main --jq .sha)
echo "parent=$P main=$M"; test -n "$P" && test "$P" = "$M"; echo "exit=$?"
```

Non-zero → main moved: recreate (re-fetch, rebase, re-gate, steps 3–4 incl.
checks and Verify Deep on the new `NEW`) before landing **or** stopping. A
C6.3 owner action never names a head whose parent is not the main tip.

**Then read what landing is allowed** — all three, never the repo flag alone:

```sh
gh api repos/ocx-sh/ocx --jq '{squash: .allow_squash_merge, delete_on_merge: .delete_branch_on_merge}'
gh api repos/ocx-sh/ocx/rules/branches/main \
  --jq '[.[] | select(.type=="pull_request") | {ruleset_id, methods: .parameters.allowed_merge_methods, approvals: .parameters.required_approving_review_count}]'
gh pr view "$PR" --repo ocx-sh/ocx --json reviewDecision --jq .reviewDecision
```

`rules/branches/main` returns the effective rules of every active ruleset on
`main` with its `ruleset_id` — no hardcoded id (inspect one in full with
`gh api repos/ocx-sh/ocx/rulesets/<ruleset_id>`). As of 2026-09-22 the answer is
`squash: false`, methods `["rebase"]`, 1 approval, `REVIEW_REQUIRED` — and the
author cannot approve their own PR. So C6.3 is the expected path.

**C6.2 — only if** `squash` is `true` **and** every pull_request rule's
`methods` contains `squash` **and** `reviewDecision` is `APPROVED`:

```sh
cd /abs/path/to/ocx-mirror
S="$(git rev-parse --show-toplevel)/.tmp/ocx-upstream-pr"
PR=$(cat "$S/pr"); : "${PR:?}"
gh pr merge "$PR" --repo ocx-sh/ocx --squash
gh api "repos/ocx-sh/ocx/pulls/$PR" --jq .merge_commit_sha > "$S/ptr"; PTR=$(cat "$S/ptr"); : "${PTR:?}"
# re-run the step 1 fetch line, then:
/usr/bin/git -C "$(git rev-parse --show-toplevel)/external/ocx" merge-base --is-ancestor "$PTR" origin/main; echo "exit=$?"   # 0
gh api "repos/ocx-sh/ocx/commits/$PTR" --jq .commit.verification.verified                                                 # true
```

**C6.3 — otherwise:** stop the landing — no ruleset bypass is authorised. After
the prologue, `cp "$S/new" "$S/ptr"` (the pushed PR-head SHA, fetchable from its
branch) and continue. Record this owner action, filled in:

> Land [ocx-sh/ocx#<n>](https://github.com/ocx-sh/ocx/pull/<n>) — either
> (a) fast-forward the verified head as admin:
> `git fetch https://github.com/ocx-sh/ocx.git <branch> && git push https://github.com/ocx-sh/ocx.git <NEW>:refs/heads/main`,
> or (b) enable squash in **both** the repo setting and the `main` ruleset's
> allowed merge methods, approve the PR, and squash-merge it.
> **Do not press the PR page's merge button** — it offers only rebase-merge,
> which replays the commit unsigned.

That branch must not be deleted while the mirror pointer names it; if
`delete_branch_on_merge` is true, add to the action: *a squash landing deletes
the branch — re-point the mirror to `merge_commit_sha` in the same breath*.
Owner fast-forwards → `$NEW` is final. Owner squashes → one-line pointer
re-point to `merge_commit_sha`.

Hard rules: **never rebase-merge** — GitHub replays commits unsigned (seen on
[ocx-sh/ocx-mirror#87](https://github.com/ocx-sh/ocx-mirror/pull/87)). **A
never-pushed SHA is never a pointer.** Landing on the owner's behalf is limited
to the C6.2 squash.

**Release guard (C6.5) — not yet enforced (C8 adds it to `task
release:prepare`).** Until then, before any mirror release, check by hand and
refuse on empty output:

```sh
cd /abs/path/to/ocx-mirror
PTR=$(git ls-tree HEAD external/ocx | awk '{print $3}'); : "${PTR:?}"   # the committed pointer
git -C "$(git rev-parse --show-toplevel)/external/ocx" -c credential.helper= -c credential.helper='!gh auth git-credential' \
  fetch https://github.com/ocx-sh/ocx.git '+refs/heads/main:refs/remotes/origin/main' --tags \
&& git -C "$(git rev-parse --show-toplevel)/external/ocx" tag --contains "$PTR"    # must list a tag
```

## Step 6 — pointer bump

Hand over to **`/update-ocx`** with `NEW=$(cat "$S/ptr")`: it moves the pointer,
runs the drift gates and the adoption review, and makes the pointer commit.
`/update-ocx` has `disable-model-invocation: true` — an autonomous run cannot
invoke it directly. Fall back to the mechanical bump in README.md's "Bumping
ocx" procedure (pointer move, drift gates, `task verify`), and record
`/update-ocx`'s semantic review of the nine linked crates as an owner action
in the report below. Adjust its preflight for this hand-off:

- Its Phase-0 clean check will show `external/ocx` modified — the gitlink moved
  in steps 1–3. That one line is expected; anything else is not clean.
- Inside a goal or feature-branch run, stay on the current mirror branch — skip
  its `git switch -c chore/bump-ocx-submodule`.
- Skip its `NEW=$(… rev-parse origin/main)` line; the pointer is the `ptr` file,
  not main. In C6.2, check the submodule out at it first
  (`git -C "$(git rev-parse --show-toplevel)/external/ocx" switch --detach "$(cat "$S/ptr")"`,
  after the prologue).

## Report

Conclusion first: PR link, `PTR` and which of C6.2/C6.3 applied, verified =
true, Verify Deep run link. `## Actions` only if C6.3 left an owner action — the
filled-in text from step 5, self-contained.

## Pitfalls

- **Bash cwd persists into the submodule.** Once there,
  `$(git rev-parse --show-toplevel)` resolves to the submodule itself and every
  `-C` lands in `external/ocx/external/ocx`. Start each block with an absolute
  `cd` to the mirror root; gate in a subshell.
- **SSH remotes fail without an ssh-agent.** The submodule's `origin` is https
  already, but the mirror's own origin (`git@github.com`) and some worktrees use
  SSH. The explicit https URL + `gh auth git-credential` form above is always
  safe; never reset before that fetch succeeded.
- **rtk-filtered output misreports diffs and logs.** Use `/usr/bin/git` for any
  check whose answer decides something (tree equality, commit count, ancestry).
- **ocx `main` requires signatures + PR + approval + no force-push.** An
  unsigned or locally signed-then-rebased commit never lands; only the
  server-made commit (fast-forwarded by an admin) or a GitHub squash does.
- **`createCommitOnBranch` writes file contents only** — no symlink, no gitlink.
  The script refuses every non-`100644` new mode; a nested-submodule pointer
  bump in ocx cannot go this route — report it as an owner action. *Known
  limit (deferred):* it also refuses modifying an existing `100755` file,
  because whether the mutation preserves the mode on modify is unverified.
- **Never touch the sibling `../ocx` checkout** (G5). It is the owner's; reading
  it is fine, but never compare behaviour against it instead of upstream main —
  it may be on any branch.
