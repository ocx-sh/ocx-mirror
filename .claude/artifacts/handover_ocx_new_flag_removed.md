# Handover — remove `--new` from every ocx invocation

**Status:** ready to execute. Self-contained; no questions back to the author needed
except the sequencing one at the bottom.

## Why

ocx-sh/ocx [#366](https://github.com/ocx-sh/ocx/issues/366) removed the
`--new` / `-n` flag from `ocx package push` and `ocx config push`.
Commit subject: `fix(publisher)!: cascade into a target repository that does not exist yet`.
Landed on the `goat` branch of `ocx` (not yet released at the time of writing).

`--new` made a `--cascade` push tolerate a **failing** tag listing — any failure,
auth and 5xx included — by cascading against an empty tag list. ocx now decides
this itself, narrowly, at one seam (`Client::list_tags_or_empty_addressed`): a tag
listing that answers `RepositoryNotFound` (registry 404 / `NAME_UNKNOWN`) **is**
the empty list; every other failure still aborts the push.

So the reasoning in `src/pipeline/ocx_cli/push.rs:64-69` is now upstream behaviour,
and the flag it justifies was removed outright — no accept-and-ignore shim (ocx is
pre-1.0 and breaks without deprecation windows).

**Consequence: every mirror push exits 64 (unknown argument) the moment it runs
against an ocx carrying this change.** The mirror passes `--new` unconditionally
(`push.rs:86` — "it is a no-op once the repository exists, so the mirror always
passes it"), so this is not a corner case, it is every push.

Second-order gain worth stating in the commit body: the mirror has been running
**fail-open** permanently. `--new` swallowed auth failures and 5xx too, so a blip
made a cascade compute against an empty tag list and re-point `latest` / `X` /
`X.Y` **backwards**. Dropping the flag closes that.

## What to change

Line numbers verified against `docs/owners-fixture-shape` @ `cd5aceb`.
Re-derive with `git grep -n -- '--new' -- src/` before editing.

| File:line | Change |
|---|---|
| `src/pipeline/ocx_cli/push.rs:64-69` | delete the `--new` paragraph from the doc comment |
| `src/pipeline/ocx_cli/push.rs:86` | `["--new", "-p", platform, "-i", target_ref]` → `["-p", platform, "-i", target_ref]` |
| `src/pipeline/python_push.rs:142` | drop `--new` from the doc comment (`… push --new` → `… push`) |
| `src/pipeline/python_push.rs:174-179` | drop the `args.push("--new".to_string());` beside `--cascade`, and the comment sentence explaining why the two travel together |
| `src/command/package/pipeline/push/tests/argv.rs:38` | drop `"--new",` from the expected argv |
| `src/command/package/pipeline/patch/tests/argv.rs:28` | drop `"--new",` from the expected argv |
| `src/command/package/pipeline/push/tests/cascade_backfill.rs:106,158` | drop `"--new",` from both expected argvs |
| `src/pipeline/python_push.rs:464` | delete the `assert!(args.contains(&"--new"…))` |
| `src/pipeline/python_push.rs:478` | delete the `assert!(!no_cascade.contains(&"--new"…))` — now trivially true, and a green that cannot go red is not a check |

Three production lines plus test updates. No behaviour depends on the flag: it was
a no-op once the repository existed, and its first-publish job is now ocx's.

While you are in `push.rs`, check whether the `cascade` doc comment still reads
correctly once the `--new` paragraph is gone — it currently flows into it.

## Verify

```sh
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo nextest run --workspace --locked
git grep -n -- '--new' -- src/     # must return nothing
```

Prove the argv tests discriminate: re-add `"--new"` to one expected argv and
confirm that test reds, then revert. A four-test suite that all pass before and
after the production edit would mean none of them was asserting on this flag.

## Sequencing (owner decision — do not guess)

The mirror breaks on exit 64 the moment ocx ships this. Either this change lands
with / before the ocx release, or the fleet stops pushing. Landing it *early* is
safe against an old ocx only if no brand-new mirror repository is pushed in the
window — against an ocx that still wants `--new`, dropping it reinstates the
first-publish 404 abort for a not-yet-published repository.

Since ocx is vendored here as a submodule, the natural coupling is: **bump the
`ocx` submodule to the commit carrying #366 in the same PR that drops the flag.**
Confirm with the owner before bumping — the submodule pin is a fleet-visible
decision, not a refactor.

Push authorization is revoked in this workspace: commit locally on a branch, do
not push, do not open the PR.

## Related

Same break reaches [ocx-sh/ocx-sdk-python](https://github.com/ocx-sh/ocx-sdk-python),
whose `Ocx.push(new=…)` keyword forwards the flag
(`src/ocx_sdk/_client.py:1903,1928,1951` + one unit test + one help fixture).
Its handover lives at `.claude/artifacts/handover_ocx_new_flag_removed.md` in
that repo. Whether the SDK removes the public keyword or accepts-and-ignores it
for one release is a separate product call — ocx's "break now, don't deprecate"
rule is ocx's, and the SDK is versioned separately.
