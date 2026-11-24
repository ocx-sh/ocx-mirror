# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 The OCX Authors
"""Structural guards of `release.yml`, `prune.yml`, `canary.yml` and `backfill.yml`, ported from ocx's `test_workflows.py`.

Each family is a check function returning violations. The test asserts the live
workflows have none; `CONTROLS` mutates a deep copy of the parsed YAML in memory
and asserts the same function then reds, so a check that stopped reading the
workflows cannot stay green.
"""

from __future__ import annotations

import copy
import json
import os
import re
import subprocess
import sys
from collections.abc import Callable, Iterator
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any

import pytest
import yaml

REPO_ROOT = Path(__file__).absolute().parent.parent
WORKFLOWS = REPO_ROOT / ".github" / "workflows"
BAZEL_TASKFILE = REPO_ROOT / "taskfiles" / "bazel.taskfile.yml"
RELEASE_OCX = REPO_ROOT / ".github" / "actions" / "release-ocx" / "action.yml"

Docs = dict[str, dict[str, Any]]


def _load(path: Path) -> dict[str, Any]:
    return yaml.safe_load(path.read_text(encoding="utf-8"))


def _documents() -> Docs:
    return {name: _load(WORKFLOWS / name) for name in ("release.yml", "backfill.yml", "prune.yml", "canary.yml")}


def _jobs(docs: Docs) -> Iterator[tuple[str, dict[str, Any]]]:
    for name, document in docs.items():
        for job_name, job in (document.get("jobs") or {}).items():
            yield f"{name}:{job_name}", job


def _job(docs: Docs, where: str) -> dict[str, Any]:
    name, job = where.split(":")
    return docs[name]["jobs"][job]


def _runs(job: dict[str, Any]) -> list[str]:
    return [str(s["run"]) for s in job.get("steps") or [] if "run" in s]


def _all_runs(docs: Docs) -> list[tuple[str, str]]:
    return [(where, run) for where, job in _jobs(docs) for run in _runs(job)]


def _needs(job: dict[str, Any]) -> list[str]:
    needs = job.get("needs") or []
    return [needs] if isinstance(needs, str) else list(needs)


def _code_runs(job: dict[str, Any]) -> list[str]:
    return [_code({"steps": [step]}) for step in job.get("steps") or [] if "run" in step]


def _code(job: dict[str, Any]) -> str:
    """The `run:` bodies of a job with comment-only lines dropped."""
    return "\n".join(
        line for run in _runs(job) for line in run.splitlines() if not line.lstrip().startswith("#")
    )


def _triggers(document: dict[str, Any]) -> Any:
    """PyYAML reads the bare key `on` as the boolean True."""
    return document.get("on", document.get(True))


def _step_index(steps: list[dict[str, Any]], needle: str) -> int | None:
    found = [i for i, step in enumerate(steps) if needle in "\n".join(l for l in str(step.get("run", "")).splitlines() if not l.lstrip().startswith("#"))]
    return found[0] if len(found) == 1 else None


def _step_script(docs: Docs, where: str, needle: str) -> str:
    """The comment-free `run:` body of the one step of `where` that contains `needle`."""
    steps = _job(docs, where)["steps"]
    index = _step_index(steps, needle)
    assert index is not None, f"{where} has no unique step containing {needle!r}"
    return "\n".join(l for l in str(steps[index]["run"]).splitlines() if not l.lstrip().startswith("#"))


def _eval_if(expression: object, mode: str, results: dict[str, str] | None = None, outputs: dict[str, str] | None = None) -> bool:
    """A job `if:` over `inputs.mode`, `needs.<job>.result` and `needs.plan.outputs.<name>`, the only contexts release.yml's use.

    Limit: `!cancelled()` is evaluated as always true, so no case here models a cancelled workflow; a cancelled run
    is covered only through a `needs.<job>.result == 'cancelled'` row, never through the status function itself.
    """
    body = re.fullmatch(r"\$\{\{(.*)\}\}", str(expression).strip(), re.DOTALL)
    text = " ".join((body.group(1) if body else str(expression)).split())
    text = text.replace("!cancelled()", "True").replace("&&", " and ").replace("||", " or ")
    text = re.sub(r"!(?!=)", " not ", text)
    text = re.sub(r"needs\.plan\.outputs\.(\w+)", r"OUT['\1']", text)
    text = re.sub(r"needs\.([\w-]+)\.result", r"RES['\1']", text).replace("inputs.mode", "MODE")
    scope = {"MODE": mode, "RES": results or {}, "OUT": {"track": "", "pull_request": "", **(outputs or {})}, "true": True, "false": False}
    return bool(eval(text, {"__builtins__": {}}, scope))  # the expression is a workflow file in this repo


# ---------------------------------------------------------------------------
# Checks: each returns its violations.
# ---------------------------------------------------------------------------

SIGNERS = {"release.yml:publish", "backfill.yml:backfill"}


def sign_findings(docs: Docs) -> list[str]:
    """Only `publish` and `backfill` sign: the certificate names the file that holds the signing job."""
    found = []
    for where, job in _jobs(docs):
        granted = (job.get("permissions", docs[where.split(":")[0]].get("permissions")) or {}).get("id-token")
        signing_step = any(
            re.search(r"--sign\b|package sign\b|--attest\b|package attest\b|--sbom\b|publish_mirror_package\.sh|backfill_release\.py", run)
            for run in _code_runs(job)
        )
        signs = granted == "write" or signing_step
        if signs and where not in SIGNERS:
            found.append(f"{where} signs but is not a signing job")
        if where in SIGNERS and not signing_step:
            found.append(f"{where} no longer runs a signing step")
        if where in SIGNERS and signs and granted != "write":
            found.append(f"{where} signs without id-token: write")
        if signs and "uses" in job:
            found.append(f"{where} signs through a called workflow")
    return found


# Names only the release package or the index; a dev build must not reach them.
_RELEASE_ONLY = re.compile(r"PACKAGE_RELEASE|PACKAGE_LOGICAL|INDEX_REPO|INDEX_PACKAGE|gh release|package announce")


def dev_isolation_findings(docs: Docs) -> list[str]:
    """A dev build changes nothing but the dev package; only the `canary` track reaches the release package."""
    found = []
    for where, job in _jobs({"release.yml": docs["release.yml"]}):
        condition = " ".join(str(job.get("if", "")).split())
        name = where.split(":")[1]
        if name in ("verify", "readiness", "github-release") and "inputs.mode != 'dev'" not in condition:
            found.append(f"{where} runs on a dev build")
        if name == "announce":
            # A skipped github-release passes the announce only on the canary track.
            skipped = re.findall(r"needs\.github-release\.result == 'skipped'", condition)
            gated = "needs.plan.outputs.track == 'canary' && needs.github-release.result == 'skipped'" in condition
            if len(skipped) != 1 or not gated or "needs.github-release.result == 'success'" not in condition:
                found.append(f"{where} announces a non-canary dev build")
        if name == "prune" and "inputs.mode == 'release'" not in condition:
            found.append(f"{where} is not release-only")
        if name == "comment" and "needs.plan.outputs.pull_request != ''" not in condition:
            found.append(f"{where} comments without a pull request")
        if name not in ("plan", "prune", "announce") and _RELEASE_ONLY.search("\n".join(_runs(job)) + str(job.get("env"))):
            if "inputs.mode != 'dev'" not in condition:
                found.append(f"{where} touches the release package or index without excluding dev")
    release = docs["release.yml"]["jobs"]
    if "prune-dev" in release:
        found.append("release.yml prunes dev builds; prune.yml owns that")
    dev = docs["prune.yml"]["jobs"]["dev"]
    if _RELEASE_ONLY.search("\n".join(_runs(dev))) or "PACKAGE_RELEASE" in str(dev):
        found.append("the prune.yml dev job names the release package")
    pruned = [s for s in dev["steps"] if "package prune" in str(s.get("run"))]
    if not pruned or any(s["env"]["PACKAGE"] != "${{ env.PACKAGE_DEV }}" for s in pruned):
        found.append("the prune.yml dev job does not prune PACKAGE_DEV")
    pruned_here = release["prune"]
    if "canary" in _code(pruned_here).lower() or "CANARY_KEEP" in str(pruned_here) or "CANARY_KEEP" in docs["release.yml"]["env"]:
        found.append("the release.yml prune job selects canary tags; prune.yml owns them")
    plan = "\n".join(_runs(release["plan"]))
    arm = (
        r'dev\)\s.*?package="\$PACKAGE_DEV".*?git_tag=""\s*'
        r'(?:#[^\n]*\n\s*)?if \[\[ "\$track" == canary \]\]; then\s+package="\$PACKAGE_RELEASE"\s+fi\s*;;'
    )
    if not re.search(arm, plan, re.DOTALL):
        found.append("the plan's dev arm does not pick PACKAGE_DEV with an empty git tag, or lets a non-canary reach the release package")
    if not re.search(r'"\$GITHUB_REF" == "refs/heads/main" \]\]; then\s+track=canary', plan):
        found.append("the canary track is not tied to refs/heads/main")
    if plan.count("PACKAGE_RELEASE") != 2:  # the release arm and the canary override
        found.append("the plan names the release package outside the release arm and the canary override")
    return found


def force_findings(docs: Docs) -> list[str]:
    """`--force` only in the `prune.yml` dev job, on the dev package."""
    found = []
    for where, job in _jobs(docs):
        for step in job.get("steps") or []:
            for line in str(step.get("run", "")).splitlines():
                if not re.search(r"(?<!\S)--force(?!\S)", line):
                    continue
                if where != "prune.yml:dev":
                    found.append(f"{where}: --force outside the prune.yml dev job")
                elif '"$PACKAGE"' not in line or step["env"]["PACKAGE"] != "${{ env.PACKAGE_DEV }}":
                    found.append(f"{where}: --force not pinned to the dev package")
    if not any("--force" in run for where, run in _all_runs(docs) if where == "prune.yml:dev"):
        found.append("the prune.yml dev job lost its --force")
    return found


def prerelease_findings(docs: Docs) -> list[str]:
    """A candidate is a pre-release, a release is the latest."""
    run = "\n".join(_runs(docs["release.yml"]["jobs"]["github-release"]))
    shape = r'if \[\[ "\$MODE" == candidate \]\]; then\s+args\+=\(--prerelease\)\s+else\s+args\+=\(--latest\)\s+fi'
    found = [] if re.search(shape, run) and run.count("--prerelease") == 1 else ["candidate is not --prerelease and release --latest"]
    found += [f"{where} sets a release flag" for where, r in _all_runs(docs) if where != "release.yml:github-release" and re.search(r"--prerelease|--latest", r)]
    return found


def tag_and_release_findings(docs: Docs) -> list[str]:
    """The tag and the release are created once, by `github-release`, after `publish`."""
    runs = _all_runs(docs)
    creators = [where for where, run in runs if "gh release create" in run]
    found = [] if creators == ["release.yml:github-release"] else [f"gh release create in {creators}"]
    found += [f"{where} creates a tag by hand" for where, run in runs if re.search(r"git tag\b|git push\b|git/refs", run)]
    release = docs["release.yml"]["jobs"]["github-release"]
    if "publish" not in _needs(release):
        found.append("github-release does not wait for publish")
    if not any('--target "$GITHUB_SHA"' in run for run in _runs(release)):
        found.append("the release is not created with --target at the dispatched commit")
    if "github-release" not in _needs(docs["release.yml"]["jobs"]["announce"]):
        found.append("release.yml:announce does not wait for github-release")
    return found


def readiness_findings(docs: Docs) -> list[str]:
    """`publish` waits for green `verify` and `readiness`; only a dev build may pass them skipped."""
    publish = docs["release.yml"]["jobs"]["publish"]
    condition = " ".join(str(publish.get("if", "")).split())
    found = [f"publish does not need {n}" for n in ("verify", "readiness") if n not in _needs(publish)]
    if not (
        "needs.verify.result == 'success'" in condition and "needs.readiness.result == 'success'" in condition
    ):
        found.append("publish does not require a green verify and readiness")
    allowed = r"\(inputs\.mode == 'dev' && needs\.verify\.result == 'skipped' && needs\.readiness\.result == 'skipped'\)"
    if condition.count("'skipped'") != 2 or not re.search(allowed, condition):
        found.append("a skipped gate passes publish outside dev mode")
    return found


def identity_findings(docs: Docs) -> list[str]:
    """Release and candidate verify against the literal main identity; no `${{ }}` inside a `run:`."""
    found = [f"{where}: ${{{{ }}}} inside run:" for where, run in _all_runs(docs) if "${{" in run]
    pinned = r"\$\{\{ github\.server_url \}\}/ocx-sh/ocx-mirror/\.github/workflows/release\.yml@refs/heads/main"
    if not re.fullmatch(pinned, str(docs["release.yml"]["env"].get("RELEASE_IDENTITY"))):
        found.append("RELEASE_IDENTITY is not the literal release.yml@refs/heads/main")
    verify = next(
        (s for s in docs["release.yml"]["jobs"]["publish"]["steps"] if "package verify" in str(s.get("run"))), None
    )
    run = str(verify["run"]) if verify else ""
    if not re.search(r'"\$PACKAGE" == "\$PACKAGE_DEV" \]\]; then\s+identity="\$\{GITHUB_SERVER_URL\}/\$\{GITHUB_WORKFLOW_REF\}"\s+else\s+identity="\$RELEASE_IDENTITY"\s+fi', run):
        found.append("the verify step does not pick RELEASE_IDENTITY for the release package")
    if run.count("--certificate-identity") != run.count('--certificate-identity "$identity"') or not run:
        found.append("a verify call names an identity other than $identity")
    return found


def status_function_findings(docs: Docs) -> list[str]:
    """A job that runs in dev mode behind a job that dev mode skips needs a status function, or it is skipped too."""
    jobs = docs["release.yml"]["jobs"]

    def ancestors(name: str) -> set[str]:
        direct = _needs(jobs[name])
        return set(direct).union(*(ancestors(n) for n in direct))

    found = []
    for name, job in jobs.items():
        condition = str(job.get("if", ""))
        excludes_dev = "inputs.mode != 'dev'" in condition or "inputs.mode == 'release'" in condition
        skippable = any("inputs.mode != 'dev'" in str(jobs[a].get("if", "")) for a in ancestors(name))
        if not excludes_dev and skippable and not ("!cancelled()" in condition and "needs." in condition):
            found.append(f"{name} runs in dev mode behind a skipped ancestor without a status function")
    return found


def concurrency_findings(docs: Docs) -> list[str]:
    release = docs["release.yml"]["concurrency"]
    found = []
    m = re.fullmatch(
        r"(\w+)-\$\{\{ inputs\.mode == 'dev' && format\('(dev-\{0\})', github\.ref\) \|\| '(\w+)' \}\}", str(release["group"])
    )
    if not m:
        found.append("the release group no longer selects by dev mode and queues dev runs per ref")
    else:
        prefix, dev, other = m.groups()
        if dev == other:
            found.append("a dev build shares the release group")
        # Release and candidate take the same arm, so they share a group by construction.
    if release.get("cancel-in-progress") is not False:
        found.append("release cancels in progress")
    backfill = docs["backfill.yml"]["concurrency"]
    if backfill.get("group") != "backfill" or backfill.get("cancel-in-progress") is not False:
        found.append("backfill group or cancel-in-progress changed")
    return found


def floating_tag_findings(docs: Docs) -> list[str]:
    """Floating tags move once, after every platform is pushed, signed, attested and verified."""
    steps = docs["release.yml"]["jobs"]["publish"]["steps"]

    def index(pred: Callable[[str], bool]) -> int:
        hits = [i for i, s in enumerate(steps) if pred(str(s.get("run", "")))]
        return hits[0] if len(hits) == 1 else -1

    push = index(lambda r: "publish_mirror_package.sh" in r and "--sign" in r)
    verify = index(lambda r: "package verify" in r and "--attestation" in r)
    move = index(lambda r: "cascade repair" in r)
    found = []
    if min(push, verify, move) < 0:
        found.append("publish lost its push, verify or cascade repair step (or has two)")
    elif not push < verify < move:
        found.append("the floating tags move before every platform is pushed, signed and verified")
    else:
        verify_run, move_run = _code({"steps": [steps[verify]]}), _code({"steps": [steps[move]]})
        if '"$PLATFORMS"' not in verify_run or "--attestation --type cyclonedx" not in verify_run:
            found.append("the verify step does not check the signature and the cyclonedx attestation of every platform")
        if '"${PACKAGE}:${TAG}"' not in move_run or '--tags-file "$tags_file"' not in move_run:
            found.append("the cascade step is not scoped to the pushed tag or does not extend the published tags file")
        upload = next((i for i, s in enumerate(steps) if str(s.get("uses", "")).startswith("actions/upload-artifact@")), None)
        if upload is None:
            found.append("publish has no step that uploads the published tags")
        elif upload < move:
            found.append("the published tags are uploaded before the cascade extended them")
    for where, run in _all_runs(docs):
        if re.search(r"--cascade\b", re.sub(r"(?m)^\s*#.*$", "", run)):
            found.append(f"{where}: --cascade moves a floating tag at push time")
        if "cascade repair" in run and where != "release.yml:publish":
            found.append(f"{where}: cascade repair outside publish")
    return found


def prune_reconcile_findings(docs: Docs) -> list[str]:
    """The prune reconciliation only removes rows and still runs, and fails the job, after a failed prune.

    Both prune jobs: `release.yml:prune` (candidates) and `prune.yml:canary`. The canary job also announces its
    selection, ephemerally, before it prunes it.
    """
    found = []
    for where in ("release.yml:prune", "prune.yml:canary"):
        steps = _job(docs, where)["steps"]
        prune = next((s for s in steps if s.get("id") == "prune"), None)
        announce = next((s for s in steps if "--refresh" in str(s.get("run"))), None)
        if prune is None or announce is None:
            found.append(f"{where}: no prune step or no `--refresh` announce step")
            continue
        run, condition = str(announce["run"]), str(announce.get("if"))
        if "--tags-from-registry" in run:
            found.append(f"{where}: the reconciliation adds registry tags")
        if "!cancelled()" not in condition or "steps.prune.conclusion != 'skipped'" not in condition or "outputs.pruned" in condition:
            found.append(f"{where}: the reconciliation does not run after a failed prune ({condition})")
        if prune.get("continue-on-error"):
            found.append(f"{where}: continue-on-error masks a failed prune")
    canary = _job(docs, "prune.yml:canary")["steps"]
    first = next((i for i, s in enumerate(canary) if re.search(r"package announce --ephemeral --tags-file\b", str(s.get("run")))), None)
    pruned = next((i for i, s in enumerate(canary) if s.get("id") == "prune"), None)
    if first is None or pruned is None or first > pruned:
        found.append("prune.yml:canary does not announce its selection ephemerally before it prunes")
    return found


MODES = ("release", "candidate", "dev")

_GATE_RESULTS = {"plan": "success", "publish": "success", "github-release": "success", "announce": "success"}
# (job, mode, track, result overrides, runs?) over the real `if:` of `announce` and `prune`; `prune.yml` owns every dev prune.
_GATE_CASES = [
    *[("announce", mode, "", {}, True) for mode in ("release", "candidate")],
    *[("announce", mode, "", {"github-release": result}, False) for mode in ("release", "candidate") for result in ("skipped", "failure", "cancelled")],
    *[("announce", mode, "", {"publish": result}, False) for mode in ("release", "candidate") for result in ("failure", "cancelled", "skipped")],
    ("announce", "dev", "canary", {"github-release": "skipped"}, True),
    ("announce", "dev", "canary", {"github-release": "failure"}, False),
    ("announce", "dev", "canary", {"github-release": "cancelled"}, False),
    ("announce", "dev", "canary", {"github-release": "skipped", "publish": "failure"}, False),
    ("announce", "dev", "pr12", {"github-release": "skipped"}, False),
    ("announce", "dev", "dev", {"github-release": "skipped"}, False),
    ("prune", "release", "", {}, True),
    ("prune", "candidate", "", {}, False),
    ("prune", "dev", "canary", {}, False),
    ("prune", "dev", "pr12", {}, False),
    ("prune", "dev", "dev", {}, False),
]


def gate_findings(docs: Docs) -> list[str]:
    """`announce` reads the result of what it needs and only the canary track may pass a skipped `github-release`;
    `prune` is release mode alone; `publish` waits for a green `verify` and `readiness` except on a dev build."""
    jobs = docs["release.yml"]["jobs"]
    found = [f"`{name}` has no status function in its `if:`" for name in ("announce",) if "!cancelled()" not in str(jobs[name].get("if"))]
    for name, mode, track, overrides, expected in _GATE_CASES:
        got = _eval_if(jobs[name]["if"], mode, {**_GATE_RESULTS, **overrides}, {"track": track})
        if got is not expected:
            found.append(f"`{name}` in {mode} mode, track {track!r}, with {overrides}: runs={got}, expected {expected}")
    for name in ("verify", "readiness"):
        for mode in MODES:
            if _eval_if(jobs[name]["if"], mode) is not (mode != "dev"):
                found.append(f"{name} runs in {mode} mode: {jobs[name]['if']!r}")
    for mode in MODES:
        for verify in ("success", "skipped", "failure", "cancelled"):
            for readiness in ("success", "skipped", "failure", "cancelled"):
                results = {"plan": "success", "build": "success", "sbom": "success", "verify": verify, "readiness": readiness}
                expected = (verify == "success" and readiness == "success") or (mode == "dev" and verify == "skipped" and readiness == "skipped")
                if _eval_if(jobs["publish"]["if"], mode, results) is not expected:
                    found.append(f"publish in {mode} mode with verify {verify}, readiness {readiness} should be {expected}")
    for broken in ("plan", "build", "sbom"):
        results = {"plan": "success", "build": "success", "sbom": "success", "verify": "success", "readiness": "success", broken: "failure"}
        if _eval_if(jobs["publish"]["if"], "release", results):
            found.append(f"publish runs with {broken} failed")
    for needed in ("readiness", "verify"):
        if needed not in _needs(jobs["publish"]):
            found.append(f"publish does not need {needed}")
    return found


_UNPINNED_CLIFF = re.compile(r"(?<!ocx exec -g release -- )\bgit-cliff\b")


def release_tool_findings(docs: Docs) -> list[str]:
    """In `release.yml` and `backfill.yml`, git-cliff runs only as the pinned `release` group tool.

    `taiki-e/install-action` without a version runs the latest upstream binary, in jobs that hold
    `contents: write` or a registry token.
    """
    found, cliff_calls = [], 0
    for where, job in _jobs({n: d for n, d in docs.items() if n in ("release.yml", "backfill.yml")}):
        for step in job.get("steps") or []:
            if str(step.get("uses", "")).startswith("taiki-e/install-action"):
                found.append(f"{where} installs a tool with taiki-e/install-action instead of `ocx exec`")
        code = "\n".join(line for line in _code(job).splitlines() if "echo" not in line)
        cliff_calls += code.count("git-cliff")
        found += [f"{where} runs git-cliff outside `ocx exec -g release --`" for _ in _UNPINNED_CLIFF.finditer(code)]
    if cliff_calls < 1:
        found.append(f"{cliff_calls} git-cliff calls found, expected 1 (the reader stopped matching)")
    return found


def platform_loop_findings(docs: Docs) -> list[str]:
    """Every step looping over `$PLATFORMS` reads the rows on fd 3; the pushing and the verifying loops also assert they covered every row.

    The SBOM loop only writes files `publish` then names with `--sbom`, so a missing row fails there instead.
    """
    found, loops = [], 0
    for where, job in _jobs(docs):
        for step in job.get("steps") or []:
            run = _code({"steps": [step]})
            if 'done 3<<< "$PLATFORMS"' not in run and 'done <<< "$PLATFORMS"' not in run:
                continue
            loops += 1
            at = f"{where} step {step.get('name')!r}"
            if 'done 3<<< "$PLATFORMS"' not in run or "<&3" not in run:
                found.append(f"{at} reads the platform rows on stdin, where a command in the body can swallow them")
            counted = "publish_mirror_package.sh" in run or "package verify" in run
            if counted and ('expected="$(grep -c . <<< "$PLATFORMS")"' not in run or not re.search(r"\(\(\w+ != expected\)\)", run)):
                found.append(f"{at} never compares the platforms it covered with the platforms published")
    if loops != 3:
        found.append(f"{loops} `$PLATFORMS` loops found, expected 3 (sbom, push, verify; the reader stopped matching)")
    return found


def description_findings(docs: Docs) -> list[str]:
    """The catalog description is refreshed before the floating tags move, from CATALOG.md and the logo."""
    steps = docs["release.yml"]["jobs"]["publish"]["steps"]
    cascade, desc = _step_index(steps, "package cascade repair"), _step_index(steps, "package description push")
    if cascade is None or desc is None:
        return [f"publish has no unique cascade/description step: {(cascade, desc)}"]
    run = _code({"steps": [steps[desc]]})
    found = [] if desc < cascade else ["the description is pushed after the cascade"]
    found += [f"the description push lacks {flag}" for flag in ("--readme CATALOG.md", "--logo assets/logo.svg") if flag not in run]
    return found


def entry_findings(docs: Docs) -> list[str]:
    """`release.yml` and `backfill.yml` are dispatch-only, hold no default token, pin the repository id, and backfill waits for its reviewer."""
    found = []
    for name, inputs in (("release.yml", {"mode", "version", "prune_after_days"}), ("backfill.yml", {"tags", "dry_run", "limit"})):
        document = docs[name]
        triggers = _triggers(document)
        if set(triggers) != {"workflow_dispatch"}:
            found.append(f"{name} triggers on {sorted(triggers)}, not a dispatch alone")
        elif set(triggers["workflow_dispatch"]["inputs"]) != inputs:
            found.append(f"{name} inputs are {sorted(triggers['workflow_dispatch']['inputs'])}")
        if document.get("permissions") != {}:
            found.append(f"{name} has no top-level `permissions: {{}}`")
        if str(document["env"].get("REPOSITORY_ID")) != "1266566464":
            found.append(f"{name} does not pin the repository id in its env block")
        found += [f"{name}:{job} sets REPOSITORY_ID of its own" for job, body in document["jobs"].items() if "REPOSITORY_ID" in (body.get("env") or {})]
    options = _triggers(docs["release.yml"])["workflow_dispatch"]["inputs"]["mode"]["options"]
    if options != list(MODES):
        found.append(f"release.yml modes are {options}")
    if docs["backfill.yml"]["jobs"]["backfill"].get("environment") != "release-backfill":
        found.append("backfill's job does not run in the `release-backfill` environment")
    return found


def prune_entry_findings(docs: Docs) -> list[str]:
    """`prune.yml` is a schedule and a dispatch with the ref and repository guard, the grants and the environment pinned."""
    document = docs["prune.yml"]
    found = []
    triggers = _triggers(document)
    if set(triggers) != {"schedule", "workflow_dispatch"}:
        found.append(f"prune.yml triggers on {sorted(triggers)}")
    elif set(triggers["workflow_dispatch"]["inputs"]) != {"prune_after_days"}:
        found.append(f"prune.yml inputs are {sorted(triggers['workflow_dispatch']['inputs'])}")
    if document.get("permissions") != {}:
        found.append("prune.yml has no top-level `permissions: {}`")
    if str(document["env"].get("REPOSITORY_ID")) != "1266566464":
        found.append("prune.yml does not pin the repository id in its env block")
    if set(document["jobs"]) != {"dev", "canary"}:
        found.append(f"prune.yml jobs are {sorted(document['jobs'])}")
        return found
    for name, job in document["jobs"].items():
        first = (job.get("steps") or [{}])[0]
        if '"$GITHUB_REF" != "refs/heads/main"' not in str(first.get("run")) or "REPOSITORY_ID_ACTUAL" not in str(first.get("env")):
            found.append(f"prune.yml:{name} does not start with the ref and repository guard")
        if not job.get("timeout-minutes"):
            found.append(f"prune.yml:{name} has no timeout-minutes")
    jobs = document["jobs"]
    if jobs["canary"].get("environment") != "ocx.sh":
        found.append("prune.yml:canary does not run in the `ocx.sh` environment")
    if jobs["dev"].get("environment") or "OCX_ANNOUNCE_TOKEN" in str(jobs["dev"]):
        found.append("prune.yml:dev has an environment or the announce token")
    return found


_CANARY_GUARDS = (
    "github.event.workflow_run.conclusion == 'success'",
    "github.event.workflow_run.event == 'push'",
    "github.event.workflow_run.head_repository.full_name == github.repository",
)
_CANARY_DISPATCH = "gh workflow run release.yml --ref main -f mode=dev"


def canary_findings(docs: Docs) -> list[str]:
    """`canary.yml` dispatches `release.yml` in dev mode on main, only after a green verified push to main of this repository."""
    document = docs["canary.yml"]
    triggers = _triggers(document)
    if not isinstance(triggers, dict) or set(triggers) != {"workflow_run"}:
        return [f"canary.yml triggers are {triggers!r}, expected exactly workflow_run"]
    found = []
    run = triggers["workflow_run"]
    for key, expected in (("workflows", ["verify-basic"]), ("types", ["completed"]), ("branches", ["main"])):
        if run.get(key) != expected:
            found.append(f"workflow_run.{key} is {run.get(key)!r}, expected {expected!r}")
    if document.get("permissions") != {}:
        found.append(f"top-level permissions are {document.get('permissions')!r}, expected {{}}")
    if list(document["jobs"]) != ["dispatch"]:
        return [*found, f"canary.yml jobs are {list(document['jobs'])}, expected [dispatch]"]
    job = document["jobs"]["dispatch"]
    if job.get("permissions") != {"actions": "write", "contents": "read"}:
        found.append(f"the dispatch job permissions are {job.get('permissions')!r}")
    condition = str(job.get("if"))
    found += [f"the dispatch job `if:` lacks `{guard}`" for guard in _CANARY_GUARDS if guard not in condition]
    # Exactly the three guards, joined by `&&`: a substring check stays green on `<guard> || true`.
    body = re.fullmatch(r"\$\{\{(.*)\}\}", condition.strip(), re.DOTALL)
    clauses = [" ".join(c.split()) for c in (body.group(1) if body else condition).split("&&")]
    if clauses != list(_CANARY_GUARDS):
        found.append(f"the dispatch job `if:` is not exactly the three guards joined by &&: {clauses!r}")
    steps = job.get("steps") or []
    found += [f"a step uses {step['uses']}: the job runs no code but its own script" for step in steps if "uses" in step]
    found += ["a `run:` interpolates an expression" for step in steps if "${{" in str(step.get("run", ""))]
    code = _code(job)
    if '"$tip" != "$HEAD_SHA"' not in code:
        found.append("the run body does not compare the main tip with the verified SHA")
    # A mismatch must exit before the dispatch: nothing else in the branch, nothing between its `fi` and the dispatch.
    if not re.search(r'^if \[\[ "\$tip" != "\$HEAD_SHA" \]\]; then\n(?:\s*echo [^\n]*\n)?\s*exit 0\nfi\ngh workflow run [^\n]*$', code, re.MULTILINE):
        found.append("the tip check is not an `if` that only echoes and exits 0, directly before the dispatch")
    tip_steps = [step for step in steps if '"$tip" != "$HEAD_SHA"' in str(step.get("run", ""))]
    if len(tip_steps) != 1:
        found.append(f"{len(tip_steps)} steps hold the tip check, expected exactly one")
    else:
        tip_step = tip_steps[0]
        if (tip_step.get("env") or {}).get("HEAD_SHA") != "${{ github.event.workflow_run.head_sha }}":
            found.append(f"the tip check's HEAD_SHA is {(tip_step.get('env') or {}).get('HEAD_SHA')!r}, expected the verified workflow_run head_sha")
        tip_code = _code({"steps": [tip_step]})
        assigns = [line.strip() for line in tip_code.splitlines() if re.match(r"\s*(?:export\s+|declare\s+|local\s+|readonly\s+)?tip=", line)]
        if not any(re.fullmatch(r'tip="\$\(gh api "repos/\$GH_REPO/commits/main" --jq \.sha\)"', a) for a in assigns):
            found.append("the main tip is not read from the `gh api` commits/main lookup")
        if len(assigns) != 1:
            found.append(f"the tip check assigns `tip` {len(assigns)} times, expected exactly the one API lookup")
    dispatches = [line.strip() for line in code.splitlines() if "gh workflow run" in line]
    if dispatches != [_CANARY_DISPATCH]:
        found.append(f"the run body dispatches {dispatches!r}, expected exactly {_CANARY_DISPATCH!r}")
    return found


def announce_token_findings(docs: Docs) -> list[str]:
    """`OCX_ANNOUNCE_TOKEN` reaches only a step that announces, and an announcing step holds no registry credential."""
    found = []
    for where, job in _jobs(docs):
        for step in job.get("steps") or []:
            if "OCX_ANNOUNCE_TOKEN" in (step.get("env") or {}) and "package announce" not in str(step.get("run")):
                found.append(f"{where}: step {step.get('name')!r} carries OCX_ANNOUNCE_TOKEN without announcing")
            write_auth = sorted(k for k in (step.get("env") or {}) if k.startswith("OCX_AUTH_"))
            if "package announce" in str(step.get("run")) and write_auth:
                found.append(f"{where}: announce step {step.get('name')!r} carries registry credentials {write_auth}")
            if "package prune" in str(step.get("run")) and "package announce" in str(step.get("run")):
                found.append(f"{where}: step {step.get('name')!r} prunes and announces in one step")
    announcing = [where for where, job in _jobs(docs) if any("OCX_ANNOUNCE_TOKEN" in (s.get("env") or {}) for s in job.get("steps") or [])]
    if len(announcing) < 3:
        found.append(f"only {announcing} carry the announce token (the reader stopped matching)")
    return found


def comment_job_findings(docs: Docs) -> list[str]:
    """The pull request comment job holds `pull-requests: write` alone, runs one script, and only for a published pull request build."""
    document = docs["release.yml"]
    jobs = document["jobs"]
    job = jobs["comment"]
    found = []
    if job.get("permissions") != {"pull-requests": "write"}:
        found.append(f"comment permissions are {job.get('permissions')!r}, expected only pull-requests: write")
    found += [f"`{name}` holds pull-requests: write" for name, other in jobs.items() if name != "comment" and (other.get("permissions") or {}).get("pull-requests") == "write"]
    if (document.get("permissions") or {}).get("pull-requests") == "write":
        found.append("the workflow level grants pull-requests: write")
    steps = job.get("steps") or []
    if len(steps) != 1 or "run" not in steps[0]:
        found.append("comment is not one `run:` step")
    found += [f"comment has a `uses:` step: {step['uses']}" for step in steps if "uses" in step]
    found += ["comment interpolates an expression into a `run:`" for step in steps if "${{" in str(step.get("run", ""))]
    if not job.get("timeout-minutes"):
        found.append("comment has no timeout-minutes")
    if _needs(job) != ["plan", "publish"]:
        found.append(f"comment needs {_needs(job)}")
    for mode, track, pull_request, publish, expected in (
        ("dev", "pr12", "12", "success", True),
        ("dev", "pr12", "12", "failure", False),
        ("dev", "pr12", "12", "cancelled", False),
        ("dev", "dev", "", "success", False),
        ("dev", "canary", "", "success", False),
        ("release", "", "", "success", False),
    ):
        got = _eval_if(job["if"], mode, {"publish": publish}, {"track": track, "pull_request": pull_request})
        if got is not expected:
            found.append(f"comment in {mode} mode, track {track!r}, publish {publish}: runs={got}, expected {expected}")
    return found


def cache_credential_findings(docs: Docs) -> list[str]:
    """The stamped release build and everything else `release.yml` calls hold no cache write credential: stamped bytes are the release.

    Adapted from ocx's `cross.yml` sweep: here the stamped build is `build` (build-matrix.yml), and `verify` may pass the read credential only.
    """
    release = docs["release.yml"]
    found = []
    build = release["jobs"]["build"]
    if build.get("uses") != "./.github/workflows/build-matrix.yml":
        found.append("release.yml:build no longer calls build-matrix.yml (the reader stopped matching)")
    if build.get("secrets"):
        found.append(f"release.yml:build passes {sorted(build['secrets'])} to the stamped build")
    for where, job in _jobs({"release.yml": release}):
        if any("WRITE" in name or "WRITE" in str(value) for name, value in (job.get("secrets") or {}).items()):
            found.append(f"{where} passes a cache write credential to a called workflow")
    return found


def _callee(job: dict[str, Any]) -> dict[str, Any]:
    return _load(REPO_ROOT / str(job["uses"]).removeprefix("./"))


def caller_input_findings(docs: Docs) -> list[str]:
    """Every input and secret `release.yml` passes to a workflow it calls is declared by that workflow, and every required input is passed."""
    found, callers = [], 0
    for where, job in _jobs({"release.yml": docs["release.yml"]}):
        if not str(job.get("uses", "")).startswith("./.github/workflows/"):
            continue
        callers += 1
        declared = _triggers(_callee(job))["workflow_call"]
        inputs, secrets = declared.get("inputs") or {}, declared.get("secrets") or {}
        found += [f"{where} passes input {k}, which {job['uses']} does not declare" for k in job.get("with") or {} if k not in inputs]
        found += [f"{where} passes secret {k}, which {job['uses']} does not declare" for k in job.get("secrets") or {} if k not in secrets]
        found += [
            f"{where} omits input {k}, which {job['uses']} requires"
            for k, spec in inputs.items()
            if (spec or {}).get("required") and "default" not in spec and k not in (job.get("with") or {})
        ]
    if callers != 4:
        found.append(f"read {callers} release.yml callers, expected 4 (verify-version, verify, readiness, build)")
    return found


_PUBLISH_SCRIPT = REPO_ROOT / "scripts" / "publish_mirror_package.sh"
_BACKFILL_SCRIPT = REPO_ROOT / "scripts" / "backfill_release.py"
_SBOM_SCRIPT = REPO_ROOT / "scripts" / "release_sbom.py"


def _long_flags(text: str) -> set[str]:
    return set(re.findall(r"(?<![\w-])(--[a-z][a-z0-9-]*)", text))


def cli_findings(docs: Docs) -> list[str]:
    """The workflows call the release scripts with flags those scripts parse, and with every flag they require."""
    publish_flags = set(re.findall(r"^\s+(--[a-z][a-z-]*)\)", _PUBLISH_SCRIPT.read_text(encoding="utf-8"), re.MULTILINE)) | {"--help"}
    py_flags = {path: set(re.findall(r'add_argument\(\s*"(--[a-z-]+)"', path.read_text(encoding="utf-8"))) for path in (_BACKFILL_SCRIPT, _SBOM_SCRIPT)}
    py_required = {_BACKFILL_SCRIPT: {"--package", "--identity", "--issuer"}, _SBOM_SCRIPT: {"--target", "--output"}}
    found, calls = [], {"publish": 0, "backfill": 0, "sbom": 0}
    for where, run in _all_runs(docs):
        joined = "\n".join(line for line in run.splitlines() if not line.lstrip().startswith("#")).replace("\\\n", " ")
        if "publish_mirror_package.sh" in joined:
            calls["publish"] += 1
            used = _long_flags(joined)
            found += [f"{where} passes {f}, which publish_mirror_package.sh does not parse" for f in sorted(used - publish_flags)]
            found += [f"{where} omits {f}, which publish_mirror_package.sh requires" for f in sorted({"--identifier", "--platform", "--binary"} - used)]
        for path, label in ((_BACKFILL_SCRIPT, "backfill"), (_SBOM_SCRIPT, "sbom")):
            if path.name in joined:
                calls[label] += 1
                used = _long_flags(joined.split(path.name, 1)[1])
                found += [f"{where} passes {f}, which {path.name} does not parse" for f in sorted(used - py_flags[path])]
                found += [f"{where} omits {f}, which {path.name} requires" for f in sorted(py_required[path] - used)]
    found += [f"no step calls the {name} script (the reader stopped matching)" for name, count in calls.items() if not count]
    return found


CHECKS: dict[str, Callable[[Docs], list[str]]] = {
    "sign": sign_findings,
    "dev-isolation": dev_isolation_findings,
    "force": force_findings,
    "prerelease": prerelease_findings,
    "tag-and-release": tag_and_release_findings,
    "readiness": readiness_findings,
    "identity": identity_findings,
    "status-function": status_function_findings,
    "concurrency": concurrency_findings,
    "floating-tags": floating_tag_findings,
    "prune-reconcile": prune_reconcile_findings,
    "gate": gate_findings,
    "release-tools": release_tool_findings,
    "platform-loops": platform_loop_findings,
    "description": description_findings,
    "entry": entry_findings,
    "prune-entry": prune_entry_findings,
    "canary": canary_findings,
    "announce-token": announce_token_findings,
    "comment-job": comment_job_findings,
    "cache-credential": cache_credential_findings,
    "caller-inputs": caller_input_findings,
    "cli": cli_findings,
}


def cache_findings(task: dict[str, Any]) -> list[str]:
    """The stamped release build reads and writes no cache, whatever `~/.bazelrc` or `--config=ci` says."""
    flags = str(task["vars"]["FLAGS"]).split()
    found = [
        f"build:release FLAGS lacks {flag}"
        for flag in ("--remote_cache=", "--disk_cache=", "--noremote_accept_cached", "--noremote_upload_local_results")
        if flag not in flags
    ]
    cmds = [c if isinstance(c, str) else str(c.get("cmd", "")) for c in task["cmds"]]
    for verb in ("build", "cquery"):
        lines = [c for c in cmds if f"{{{{.BAZEL}}}} {verb} " in c or f"$({{{{.BAZEL}}}} {verb} " in c]
        if len(lines) != 1 or "{{.FLAGS}}" not in lines[0]:
            found.append(f"the `bazel {verb}` line does not use {{{{.FLAGS}}}}")
    return found


CANARY_MODE = "${{ inputs.mode == 'dev' && needs.plan.outputs.track == 'canary' && 'canary' || inputs.mode }}"


def pin_findings(action: dict[str, Any], docs: Docs) -> list[str]:
    """`release-ocx` verifies its pin: identity per mode, signature on the locked subject, and the locked bytes."""
    run = "\n".join(str(s.get("run", "")) for s in action["runs"]["steps"])
    needles = {
        "the identity gate": 'python3 scripts/release_ocx_identity.py "$MODE" "$identity"',
        "the signature check": 'ocx package verify --certificate-identity "$identity" --certificate-oidc-issuer "$issuer" "$subject"',
        "the locked subject": 'tomllib.load(open("ocx.lock", "rb"))',
        "the release-group row filter": 'tool.get("group") != "release"',
        "the locked-install checksum": '"$(sum "$actual")" != "$(sum "$locked")"',
    }
    found = [f"release-ocx lost {name}" for name, needle in needles.items() if needle not in run]
    calls: dict[str, int] = {}
    for where, job in _jobs(docs):
        for step in job.get("steps") or []:
            if step.get("uses") != "./.github/actions/release-ocx":
                continue
            mode = (step.get("with") or {}).get("mode")
            calls[where.split(":")[0]] = calls.get(where.split(":")[0], 0) + 1
            if not mode:
                found.append(f"{where}: release-ocx called without mode:")
            elif where.startswith("release.yml:") and mode != CANARY_MODE:
                # A canary lands in the release package and the index: its ocx must be the strict `canary` mode.
                found.append(f"{where}: release-ocx mode is not the canary-resolving expression")
            elif where.startswith("prune.yml:") and mode != "prune":
                found.append(f"{where}: release-ocx mode is not prune")
            elif where.startswith("backfill.yml:") and mode != "backfill":
                found.append(f"{where}: release-ocx mode is not backfill")
            elif where.startswith("canary.yml:"):
                found.append(f"{where}: canary.yml runs no release-ocx; it only dispatches release.yml")
    for name in ("release.yml", "prune.yml", "backfill.yml"):
        if not calls.get(name):
            found.append(f"{name} has no release-ocx call site (the reader stopped matching)")
    return found


def _task() -> dict[str, Any]:
    return _load(BAZEL_TASKFILE)["tasks"]["build:release"]


@pytest.mark.parametrize("family", CHECKS)
def test_the_live_workflows_hold_the_guard(family: str) -> None:
    assert CHECKS[family](_documents()) == []


# ---------------------------------------------------------------------------
# Negative controls: one mutation per family, on a deep copy in memory.
# ---------------------------------------------------------------------------


def _replace_run(docs: Docs, where: str, old: str, new: str) -> None:
    landed = False
    for step in _job(docs, where).get("steps") or []:
        if old in str(step.get("run", "")):
            step["run"] = step["run"].replace(old, new)
            landed = True
    assert landed, f"mutation did not land: {old!r} is in no step of {where}"


def _replace_if(docs: Docs, where: str, old: str, new: str) -> None:
    job = _job(docs, where)
    assert old in job["if"], f"mutation did not land: {old!r} not in the if of {where}"
    job["if"] = job["if"].replace(old, new)


def _move_cascade_before_verify(docs: Docs) -> None:
    steps = _job(docs, "release.yml:publish")["steps"]
    move = next(i for i, s in enumerate(steps) if "cascade repair" in str(s.get("run")))
    steps.insert(move - 2, steps.pop(move))


def _swap_steps(docs: Docs, where: str, first: Callable[[dict[str, Any]], bool], second: Callable[[dict[str, Any]], bool]) -> None:
    steps = _job(docs, where)["steps"]
    a = next(i for i, s in enumerate(steps) if first(s))
    b = next(i for i, s in enumerate(steps) if second(s))
    steps[a], steps[b] = steps[b], steps[a]


def _step_where(docs: Docs, where: str, needle: str) -> dict[str, Any]:
    return next(s for s in _job(docs, where)["steps"] if needle in str(s.get("run")))


def _add_env(docs: Docs, where: str, needle: str, key: str, value: str) -> None:
    _step_where(docs, where, needle).setdefault("env", {})[key] = value


def _edit_text(docs: Docs, name: str, old: str, new: str) -> None:
    """Replace `old` in the dumped YAML of one document and reload it, for edits a dict assignment cannot express."""
    text = yaml.safe_dump(docs[name], sort_keys=False)
    assert old in text, f"mutation did not land: {old!r} is not in {name}"
    docs[name] = yaml.safe_load(text.replace(old, new, 1))


CONTROLS: dict[str, Callable[[Docs], None]] = {
    "sign": lambda d: _job(d, "release.yml:github-release").setdefault("permissions", {}).update({"id-token": "write"}),
    "dev-isolation": lambda d: _replace_if(
        d, "release.yml:announce", "needs.plan.outputs.track == 'canary' && ", ""
    ),
    "force": lambda d: _replace_run(d, "prune.yml:canary", "package prune --forge github", "package prune --forge github --force"),
    "prerelease": lambda d: _replace_run(d, "release.yml:github-release", "args+=(--prerelease)", "args+=(--latest)"),
    "tag-and-release": lambda d: _job(d, "release.yml:github-release").update(needs=["plan"]),
    "readiness": lambda d: _replace_if(d, "release.yml:publish", "needs.readiness.result == 'success'", "true"),
    "identity": lambda d: _replace_run(d, "release.yml:publish", 'identity="$RELEASE_IDENTITY"', 'identity="${{ github.workflow_ref }}"'),
    "status-function": lambda d: _replace_if(d, "release.yml:comment", "!cancelled() && ", ""),
    "concurrency": lambda d: d["release.yml"]["concurrency"].update(group="release-${{ inputs.mode == 'dev' && 'release' || 'release' }}"),
    "floating-tags": _move_cascade_before_verify,
    "prune-reconcile": lambda d: _replace_run(d, "release.yml:prune", "announce --refresh", "announce --refresh --tags-from-registry"),
    "gate": lambda d: _replace_if(d, "release.yml:announce", "(needs.github-release.result == 'success'", "(true"),
    "release-tools": lambda d: _replace_run(d, "release.yml:github-release", "ocx exec -g release -- git-cliff", "git-cliff"),
    "platform-loops": lambda d: _replace_run(d, "release.yml:publish", 'done 3<<< "$PLATFORMS"', 'done <<< "$PLATFORMS"'),
    "description": lambda d: _swap_steps(
        d, "release.yml:publish", lambda s: "package description push" in str(s.get("run")), lambda s: "cascade repair" in str(s.get("run"))
    ),
    "entry": lambda d: _job(d, "backfill.yml:backfill").pop("environment"),
    "prune-entry": lambda d: d["prune.yml"].update(permissions={"contents": "read"}),
    "canary": lambda d: _replace_run(d, "canary.yml:dispatch", "-f mode=dev", "-f mode=release"),
    "announce-token": lambda d: _add_env(d, "prune.yml:canary", "package prune", "OCX_ANNOUNCE_TOKEN", "${{ secrets.OCX_ANNOUNCE_TOKEN }}"),
    "comment-job": lambda d: _job(d, "release.yml:comment")["permissions"].update({"contents": "read"}),
    "cache-credential": lambda d: _job(d, "release.yml:build").update(secrets={"BAZEL_CACHE_WRITE_AUTH": "${{ secrets.BAZEL_CACHE_WRITE_AUTH }}"}),
    "caller-inputs": lambda d: _job(d, "release.yml:readiness")["with"].update(ref=_job(d, "release.yml:readiness")["with"].pop("sha")),
    "cli": lambda d: _replace_run(d, "release.yml:publish", '--platform "$platform"', ""),
}


def _plan_widened(docs: Docs) -> None:
    _replace_run(docs, "release.yml:plan", 'if [[ "$track" == canary ]]; then', "if true; then")


def _prune_before_announce(docs: Docs) -> None:
    _swap_steps(docs, "prune.yml:canary", lambda s: s.get("name") == "Announce the selected tags", lambda s: s.get("id") == "prune")


def _drop_publish_id_token(docs: Docs) -> None:
    del _job(docs, "release.yml:publish")["permissions"]["id-token"]


def _dev_prune_names_release(docs: Docs) -> None:
    _step_where(docs, "prune.yml:dev", "package prune")["env"]["PACKAGE"] = "${{ env.PACKAGE_RELEASE }}"


def _add_job(docs: Docs, name: str, job: str, body: dict[str, Any]) -> None:
    docs[name]["jobs"][job] = body


def _move_cascade_first(docs: Docs) -> None:
    steps = _job(docs, "release.yml:publish")["steps"]
    cascade = next(i for i, s in enumerate(steps) if "cascade repair" in str(s.get("run")))
    verify = next(i for i, s in enumerate(steps) if "package verify" in str(s.get("run")))
    steps.insert(verify, steps.pop(cascade))


def _drop_cascade(docs: Docs) -> None:
    steps = _job(docs, "release.yml:publish")["steps"]
    del steps[next(i for i, s in enumerate(steps) if "cascade repair" in str(s.get("run")))]


def _step_with_continue_on_error(docs: Docs) -> None:
    next(s for s in _job(docs, "prune.yml:canary")["steps"] if s.get("id") == "prune")["continue-on-error"] = True


def _canary_uses_step(docs: Docs) -> None:
    _job(docs, "canary.yml:dispatch")["steps"].insert(0, {"uses": "actions/checkout@v7"})


def _canary_expression_in_run(docs: Docs) -> None:
    _replace_run(docs, "canary.yml:dispatch", "set -euo pipefail", "set -euo pipefail\necho ${{ github.event.workflow_run.head_branch }}")


# More than one mutation for the families that guard more than one property: each must red its family on its own.
EXTRA_CONTROLS: dict[str, tuple[str, Callable[[Docs], None]]] = {
    "sign-publish-loses-id-token": ("sign", _drop_publish_id_token),
    "sign-backfill-stops-signing": ("sign", lambda d: _replace_run(d, "backfill.yml:backfill", "scripts/backfill_release.py", "echo")),
    "sign-a-sign-call-in-github-release": ("sign", lambda d: _replace_run(d, "release.yml:github-release", "gh release create", "ocx package sign x; gh release create")),
    "sign-prune-yml-gains-id-token": ("sign", lambda d: _job(d, "prune.yml:canary")["permissions"].update({"id-token": "write"})),
    "sign-canary-yml-gains-id-token": ("sign", lambda d: _job(d, "canary.yml:dispatch")["permissions"].update({"id-token": "write"})),
    "sign-comment-gains-id-token": ("sign", lambda d: _job(d, "release.yml:comment")["permissions"].update({"id-token": "write"})),
    "dev-isolation-canary-override-widened": ("dev-isolation", _plan_widened),
    "dev-isolation-dev-arm-picks-release": ("dev-isolation", lambda d: _replace_run(d, "release.yml:plan", 'package="$PACKAGE_DEV"', 'package="$PACKAGE_RELEASE"')),
    "dev-isolation-github-release-runs-on-dev": ("dev-isolation", lambda d: _replace_if(d, "release.yml:github-release", "inputs.mode != 'dev'", "true")),
    "dev-isolation-prune-dev-back-in-release-yml": ("dev-isolation", lambda d: _add_job(d, "release.yml", "prune-dev", {"steps": []})),
    "dev-isolation-dev-prune-of-the-release-package": ("dev-isolation", _dev_prune_names_release),
    "dev-isolation-release-prune-selects-canary": ("dev-isolation", lambda d: _replace_run(d, "release.yml:prune", "release_prune_select.py candidates", "release_prune_select.py canary")),
    "dev-isolation-canary-track-not-tied-to-main": ("dev-isolation", lambda d: _replace_run(d, "release.yml:plan", '"$GITHUB_REF" == "refs/heads/main"', '"$GITHUB_REF" != ""')),
    "force-release-prune-forces": ("force", lambda d: _replace_run(d, "release.yml:prune", "package prune --forge github", "package prune --forge github --force")),
    "force-dev-prune-not-pinned-to-dev": ("force", _dev_prune_names_release),
    "force-dev-prune-loses-its-force": ("force", lambda d: _replace_run(d, "prune.yml:dev", "--forge github --force", "--forge github")),
    "prerelease-release-mode-is-prerelease": ("prerelease", lambda d: _replace_run(d, "release.yml:github-release", '"$MODE" == candidate', '"$MODE" == release')),
    "prerelease-both-branches": ("prerelease", lambda d: _replace_run(d, "release.yml:github-release", "args+=(--latest)", "args+=(--latest --prerelease)")),
    "tag-announce-before-the-release": ("tag-and-release", lambda d: _job(d, "release.yml:announce").update(needs=["plan", "publish"])),
    "tag-a-tag-in-plan": ("tag-and-release", lambda d: _replace_run(d, "release.yml:plan", "set -euo pipefail", "set -euo pipefail\ngit tag v1")),
    "readiness-publish-without-readiness": ("readiness", lambda d: _job(d, "release.yml:publish").update(needs=["plan", "verify", "build", "sbom"])),
    "identity-literal-names-another-ref": ("identity", lambda d: d["release.yml"]["env"].update(RELEASE_IDENTITY="${{ github.server_url }}/ocx-sh/ocx-mirror/.github/workflows/release.yml@refs/heads/dev")),
    "identity-an-expression-in-run": ("identity", lambda d: _replace_run(d, "release.yml:publish", 'identity="$RELEASE_IDENTITY"', 'identity="${{ github.workflow_ref }}"')),
    "floating-tags-push-cascades": ("floating-tags", lambda d: _replace_run(d, "release.yml:publish", "flags=(--sign --tags-file", "flags=(--sign --cascade --tags-file")),
    "floating-tags-repair-unscoped": ("floating-tags", lambda d: _replace_run(d, "release.yml:publish", 'cascade repair --tags-file "$tags_file" "${PACKAGE}:${TAG}"', 'cascade repair --tags-file "$tags_file" "${PACKAGE}"')),
    "floating-tags-verify-drops-the-attestation": ("floating-tags", lambda d: _replace_run(d, "release.yml:publish", "--attestation --type cyclonedx", "--type cyclonedx")),
    "floating-tags-cascade-missing": ("floating-tags", _drop_cascade),
    "prune-reconcile-canary-adds-registry-tags": ("prune-reconcile", lambda d: _replace_run(d, "prune.yml:canary", "announce --refresh", "announce --tags-from-registry --refresh")),
    "prune-reconcile-release-skips-after-a-failed-prune": ("prune-reconcile", lambda d: _step_where(d, "release.yml:prune", "--refresh").update({"if": "${{ !cancelled() }}"})),
    "prune-reconcile-canary-skips-after-a-failed-prune": ("prune-reconcile", lambda d: _step_where(d, "prune.yml:canary", "--refresh").update({"if": "${{ !cancelled() }}"})),
    "prune-reconcile-canary-prune-masks-a-failure": ("prune-reconcile", _step_with_continue_on_error),
    "prune-reconcile-canary-prunes-before-it-announces": ("prune-reconcile", _prune_before_announce),
    "prune-reconcile-canary-announce-drops-ephemeral": ("prune-reconcile", lambda d: _replace_run(d, "prune.yml:canary", "announce --ephemeral --tags-file", "announce --tags-file")),
    "gate-announce-drops-the-track-condition": ("gate", lambda d: _replace_if(d, "release.yml:announce", "needs.plan.outputs.track == 'canary' && ", "")),
    "gate-announce-passes-every-dev-track": ("gate", lambda d: _replace_if(d, "release.yml:announce", "needs.plan.outputs.track == 'canary'", "needs.plan.outputs.track != ''")),
    "gate-announce-drops-the-publish-result": ("gate", lambda d: _replace_if(d, "release.yml:announce", "needs.publish.result == 'success'", "true")),
    "gate-prune-runs-for-a-candidate": ("gate", lambda d: _replace_if(d, "release.yml:prune", "inputs.mode == 'release'", "inputs.mode != 'dev'")),
    "gate-prune-runs-for-the-canary-track": ("gate", lambda d: _replace_if(d, "release.yml:prune", "inputs.mode == 'release'", "needs.plan.outputs.track == 'canary' || inputs.mode == 'release'")),
    "gate-skipped-readiness-passes-a-release": ("gate", lambda d: _replace_if(d, "release.yml:publish", "(inputs.mode == 'dev' && needs.verify.result == 'skipped' && needs.readiness.result == 'skipped')", "(needs.verify.result == 'skipped' && needs.readiness.result == 'skipped')")),
    "gate-a-failed-sbom-publishes": ("gate", lambda d: _replace_if(d, "release.yml:publish", "needs.sbom.result == 'success'", "true")),
    "gate-a-candidate-skips-readiness": ("gate", lambda d: _replace_if(d, "release.yml:readiness", "inputs.mode != 'dev'", "inputs.mode == 'release'")),
    "release-tools-floating-install": ("release-tools", lambda d: _job(d, "release.yml:github-release")["steps"].insert(0, {"uses": "taiki-e/install-action@v2", "with": {"tool": "git-cliff"}})),
    "platform-loops-no-count-check": ("platform-loops", lambda d: _replace_run(d, "release.yml:publish", "if ((published != expected)); then", "if false; then")),
    "platform-loops-loop-reads-stdin": ("platform-loops", lambda d: _replace_run(d, "release.yml:publish", "while read -r asset platform <&3; do", "while read -r asset platform; do")),
    "description-wrong-readme": ("description", lambda d: _replace_run(d, "release.yml:publish", "--readme CATALOG.md", "--readme README.md")),
    "description-wrong-logo": ("description", lambda d: _replace_run(d, "release.yml:publish", "--logo assets/logo.svg", "--logo assets/other.svg")),
    "description-missing": ("description", lambda d: _job(d, "release.yml:publish").update(steps=[s for s in _job(d, "release.yml:publish")["steps"] if "description push" not in str(s.get("run"))])),
    "entry-release-on-push": ("entry", lambda d: d["release.yml"].update({"on": {"push": {"branches": ["main"]}, "workflow_dispatch": _triggers(d["release.yml"])["workflow_dispatch"]}})),
    "entry-backfill-grants-by-default": ("entry", lambda d: d["backfill.yml"].update(permissions={"contents": "read"})),
    "entry-release-pins-another-repository": ("entry", lambda d: d["release.yml"]["env"].update(REPOSITORY_ID="1")),
    "entry-a-job-sets-its-own-repository": ("entry", lambda d: _job(d, "release.yml:plan").update(env={"REPOSITORY_ID": "1"})),
    "entry-backfill-renames-limit": ("entry", lambda d: _edit_text(d, "backfill.yml", "limit:", "cap:")),
    "prune-entry-push-trigger": ("prune-entry", lambda d: d["prune.yml"].update({"on": {"push": {"branches": ["main"]}, "workflow_dispatch": _triggers(d["prune.yml"])["workflow_dispatch"]}})),
    "prune-entry-renamed-input": ("prune-entry", lambda d: _edit_text(d, "prune.yml", "prune_after_days:", "other_days:")),
    "prune-entry-another-repository": ("prune-entry", lambda d: d["prune.yml"]["env"].update(REPOSITORY_ID="1")),
    "prune-entry-guard-drops-the-ref": ("prune-entry", lambda d: _replace_run(d, "prune.yml:dev", '"$GITHUB_REF" != "refs/heads/main"', '"$GITHUB_REF" != ""')),
    "prune-entry-canary-without-its-environment": ("prune-entry", lambda d: _job(d, "prune.yml:canary").pop("environment")),
    "prune-entry-dev-gains-an-environment": ("prune-entry", lambda d: _job(d, "prune.yml:dev").update(environment="ocx.sh")),
    "prune-entry-canary-without-a-timeout": ("prune-entry", lambda d: _job(d, "prune.yml:canary").pop("timeout-minutes")),
    "canary-no-success-check": ("canary", lambda d: _job(d, "canary.yml:dispatch").update({"if": _job(d, "canary.yml:dispatch")["if"].replace(_CANARY_GUARDS[0], "true")})),
    "canary-no-push-check": ("canary", lambda d: _job(d, "canary.yml:dispatch").update({"if": _job(d, "canary.yml:dispatch")["if"].replace(_CANARY_GUARDS[1], "true")})),
    "canary-no-repository-check": ("canary", lambda d: _job(d, "canary.yml:dispatch").update({"if": _job(d, "canary.yml:dispatch")["if"].replace(_CANARY_GUARDS[2], "true")})),
    "canary-no-tip-check": ("canary", lambda d: _replace_run(d, "canary.yml:dispatch", 'if [[ "$tip" != "$HEAD_SHA" ]]; then', "if false; then")),
    "canary-job-holds-contents-write": ("canary", lambda d: _job(d, "canary.yml:dispatch")["permissions"].update(contents="write")),
    "canary-top-level-permissions-widen": ("canary", lambda d: d["canary.yml"].update(permissions={"contents": "read"})),
    "canary-watches-another-branch": ("canary", lambda d: _triggers(d["canary.yml"])["workflow_run"].update(branches=["main", "dev"])),
    "canary-a-uses-step": ("canary", _canary_uses_step),
    "canary-an-expression-in-run": ("canary", _canary_expression_in_run),
    "canary-dispatches-another-ref": ("canary", lambda d: _replace_run(d, "canary.yml:dispatch", "--ref main", "--ref dev")),
    "announce-token-release-prune-step-carries-it": ("announce-token", lambda d: _add_env(d, "release.yml:prune", "package prune", "OCX_ANNOUNCE_TOKEN", "${{ secrets.OCX_ANNOUNCE_TOKEN }}")),
    "announce-token-refresh-announce-carries-a-registry-credential": ("announce-token", lambda d: _add_env(d, "prune.yml:canary", "--refresh", "OCX_AUTH_ghcr_io_TOKEN", "${{ secrets.GITHUB_TOKEN }}")),
    "announce-token-ephemeral-announce-carries-a-registry-credential": ("announce-token", lambda d: _add_env(d, "prune.yml:canary", "--ephemeral --tags-file", "OCX_AUTH_ghcr_io_TOKEN", "${{ secrets.GITHUB_TOKEN }}")),
    "announce-token-release-announce-carries-a-registry-credential": ("announce-token", lambda d: _add_env(d, "release.yml:announce", "package announce", "OCX_AUTH_ghcr_io_TOKEN", "${{ secrets.GITHUB_TOKEN }}")),
    "comment-job-plan-gains-pull-requests-write": ("comment-job", lambda d: _job(d, "release.yml:plan")["permissions"].update({"pull-requests": "write"})),
    "comment-job-loses-its-timeout": ("comment-job", lambda d: _job(d, "release.yml:comment").pop("timeout-minutes")),
    "comment-job-runs-without-a-pull-request": ("comment-job", lambda d: _replace_if(d, "release.yml:comment", " && needs.plan.outputs.pull_request != ''", "")),
    "comment-job-runs-after-a-failed-publish": ("comment-job", lambda d: _replace_if(d, "release.yml:comment", "needs.publish.result == 'success'", "true")),
    "comment-job-gains-a-uses-step": ("comment-job", lambda d: _job(d, "release.yml:comment")["steps"].append({"uses": "actions/checkout@v7"})),
    "caller-inputs-build-input-renamed": ("caller-inputs", lambda d: _job(d, "release.yml:build")["with"].update(version=_job(d, "release.yml:build")["with"].pop("version_semver"))),
    "caller-inputs-verify-version-input-renamed": ("caller-inputs", lambda d: _job(d, "release.yml:verify-version")["with"].update(tag=_job(d, "release.yml:verify-version")["with"].pop("version"))),
    "caller-inputs-secret-undeclared": ("caller-inputs", lambda d: _job(d, "release.yml:verify")["secrets"].update(BAZEL_CACHE_OTHER="x")),
    "cli-sbom-flag-it-does-not-parse": ("cli", lambda d: _replace_run(d, "release.yml:sbom", "--output", "--out")),
    "cli-backfill-flag-renamed": ("cli", lambda d: _replace_run(d, "backfill.yml:backfill", "--issuer", "--oidc-issuer")),
    "cli-publish-flag-misspelled": ("cli", lambda d: _replace_run(d, "release.yml:publish", '--annotation "org', '--annotations "org')),
    "cache-credential-build-secret-named-write": ("cache-credential", lambda d: _job(d, "release.yml:verify")["secrets"].update(BAZEL_CACHE_WRITE_AUTH="${{ secrets.BAZEL_CACHE_WRITE_AUTH }}")),
    "release-tools-bare-git-cliff-in-backfill": ("release-tools", lambda d: _replace_run(d, "backfill.yml:backfill", "scripts/backfill_release.py", "scripts/backfill_release.py; git-cliff")),
}


def _upload_before_cascade(docs: Docs) -> None:
    steps = _job(docs, "release.yml:publish")["steps"]
    upload = next(i for i, s in enumerate(steps) if str(s.get("uses", "")).startswith("actions/upload-artifact@"))
    cascade = next(i for i, s in enumerate(steps) if "cascade repair" in str(s.get("run")))
    steps.insert(cascade, steps.pop(upload))


def _drop_upload(docs: Docs) -> None:
    steps = _job(docs, "release.yml:publish")["steps"]
    kept = [s for s in steps if not str(s.get("uses", "")).startswith("actions/upload-artifact@")]
    assert len(kept) < len(steps), "mutation did not land: no upload step"
    steps[:] = kept


def _canary_head_sha_github_sha(docs: Docs) -> None:
    for step in _job(docs, "canary.yml:dispatch")["steps"]:
        if "HEAD_SHA" in (step.get("env") or {}):
            step["env"]["HEAD_SHA"] = "${{ github.sha }}"
            return
    raise AssertionError("mutation did not land: no HEAD_SHA env")


def _canary_if_or_true(docs: Docs) -> None:
    job = _job(docs, "canary.yml:dispatch")
    job["if"] = job["if"].replace(" }}", " || true }}")


# Controls that reach ONE branch of a check and assert ITS finding text: a bare "any finding" is also satisfied by
# the step locator or an unrelated branch failing first.
FINDING_CONTROLS: dict[str, tuple[str, Callable[[Docs], None], str]] = {
    "floating-tags-verify-checks-another-attestation-type": (
        "floating-tags",
        lambda d: _replace_run(d, "release.yml:publish", "--attestation --type cyclonedx", "--attestation --type slsaprovenance"),
        "the verify step does not check the signature and the cyclonedx attestation",
    ),
    "floating-tags-upload-before-the-cascade": (
        "floating-tags",
        _upload_before_cascade,
        "the published tags are uploaded before the cascade extended them",
    ),
    "floating-tags-upload-step-missing": (
        "floating-tags",
        _drop_upload,
        "publish has no step that uploads the published tags",
    ),
    "canary-tip-taken-from-head-sha": (
        "canary",
        lambda d: _replace_run(d, "canary.yml:dispatch", 'tip="$(gh api "repos/$GH_REPO/commits/main" --jq .sha)"', 'tip="$HEAD_SHA"'),
        "the main tip is not read from the `gh api` commits/main lookup",
    ),
    "canary-head-sha-is-github-sha": (
        "canary",
        _canary_head_sha_github_sha,
        "the tip check's HEAD_SHA is",
    ),
    "canary-if-carries-on-after-the-guards": ("canary", _canary_if_or_true, "is not exactly the three guards joined by &&"),
    "canary-tip-mismatch-carries-on": (
        "canary",
        lambda d: _replace_run(d, "canary.yml:dispatch", "exit 0", "true"),
        "the tip check is not an `if` that only echoes and exits 0",
    ),
    "canary-tip-check-or-true": (
        "canary",
        lambda d: _replace_run(d, "canary.yml:dispatch", 'if [[ "$tip" != "$HEAD_SHA" ]]; then', 'if [[ "$tip" != "$HEAD_SHA" ]] || true; then'),
        "the tip check is not an `if` that only echoes and exits 0",
    ),
}


@pytest.mark.parametrize("name", FINDING_CONTROLS)
def test_the_finding_control_reds_its_own_branch(name: str) -> None:
    family, mutate, expected = FINDING_CONTROLS[name]
    docs = _documents()
    mutated = copy.deepcopy(docs)
    mutate(mutated)
    assert mutated != docs, "the mutation changed nothing"
    findings = CHECKS[family](mutated)
    assert any(expected in finding for finding in findings), f"{family} on {name}: no finding contains {expected!r}: {findings}"


def test_every_family_has_a_control() -> None:
    assert CONTROLS.keys() == CHECKS.keys()


@pytest.mark.parametrize("name", EXTRA_CONTROLS)
def test_the_extra_control_reds_its_family(name: str) -> None:
    family, mutate = EXTRA_CONTROLS[name]
    docs = _documents()
    mutated = copy.deepcopy(docs)
    mutate(mutated)
    assert mutated != docs, "the mutation changed nothing"
    assert CHECKS[family](mutated), f"{family}: the check stayed green on {name}"


@pytest.mark.parametrize("family", CHECKS)
def test_the_control_reds_the_guard(family: str) -> None:
    docs = _documents()
    mutated = copy.deepcopy(docs)
    CONTROLS[family](mutated)
    assert mutated != docs, "the mutation changed nothing"
    assert CHECKS[family](mutated), f"{family}: the check stayed green on the mutation"


def test_the_stamped_release_build_carries_no_cache() -> None:
    assert cache_findings(_task()) == []


def test_the_release_ocx_action_verifies_its_pin() -> None:
    assert pin_findings(_load(RELEASE_OCX), _documents()) == []


def test_the_cache_control_reds_a_dropped_flag_and_an_unshared_flag_list() -> None:
    task = _task()
    dropped = copy.deepcopy(task)
    dropped["vars"]["FLAGS"] = dropped["vars"]["FLAGS"].replace("--remote_cache=", "")
    assert cache_findings(dropped)
    unshared = copy.deepcopy(task)
    unshared["cmds"] = [
        c.replace("cquery {{.FLAGS}}", "cquery") if isinstance(c, str) else {**c, "cmd": c.get("cmd", "").replace("cquery {{.FLAGS}}", "cquery")}
        for c in unshared["cmds"]
    ]
    assert unshared["cmds"] != task["cmds"] and cache_findings(unshared)


def test_the_pin_control_reds_a_dropped_verify_and_a_call_site_without_mode() -> None:
    action, docs = _load(RELEASE_OCX), _documents()
    dropped = copy.deepcopy(action)
    for step in dropped["runs"]["steps"]:
        step["run"] = str(step.get("run", "")).replace("ocx package verify --certificate-identity", "true --certificate-identity")
    assert pin_findings(dropped, docs)
    ungrouped = copy.deepcopy(action)
    for step in ungrouped["runs"]["steps"]:
        step["run"] = str(step.get("run", "")).replace(' or tool.get("group") != "release"', "")
    assert ungrouped != action and pin_findings(ungrouped, docs)
    modeless = copy.deepcopy(docs)
    for step in _job(modeless, "release.yml:publish")["steps"]:
        if step.get("uses") == "./.github/actions/release-ocx":
            step["with"] = {}
    assert pin_findings(action, modeless)
    # A canary published under the dev-mode gate (any branch-built ocx) is the regression this family exists for.
    devgate = copy.deepcopy(docs)
    for step in _job(devgate, "release.yml:publish")["steps"]:
        if step.get("uses") == "./.github/actions/release-ocx":
            step["with"] = {"mode": "${{ inputs.mode }}"}
    assert pin_findings(action, devgate)
    unpruned = copy.deepcopy(docs)
    for step in _job(unpruned, "prune.yml:canary")["steps"]:
        if step.get("uses") == "./.github/actions/release-ocx":
            step["with"] = {"mode": "dev"}
    assert pin_findings(action, unpruned)
    unbackfilled = copy.deepcopy(docs)
    for step in _job(unbackfilled, "backfill.yml:backfill")["steps"]:
        if step.get("uses") == "./.github/actions/release-ocx":
            step["with"] = {"mode": "release"}
    assert pin_findings(action, unbackfilled)
    # canary.yml runs no code of its own, so a release-ocx call there is a new way to reach the release package.
    canary = copy.deepcopy(docs)
    _job(canary, "canary.yml:dispatch")["steps"].insert(0, {"uses": "./.github/actions/release-ocx", "with": {"mode": "canary"}})
    assert pin_findings(action, canary)


# ---------------------------------------------------------------------------
# Behaviour: the real `run:` bodies, run in a scratch directory against stub `ocx`, `gh` and `git` calls. Each test
# runs the step as written, then the step with its guard deleted, and the guard must be what made the difference.
# ---------------------------------------------------------------------------

RELEASE_PACKAGE, DEV_PACKAGE = "ghcr.io/ocx-sh/ocx/mirror", "ghcr.io/ocx-sh/dev/mirror"
LOGICAL_PACKAGE = "ocx.sh/ocx/mirror"


def _release_env() -> dict[str, str]:
    return {k: str(v) for k, v in _load(WORKFLOWS / "release.yml")["env"].items()}


def _prune_env() -> dict[str, str]:
    return {k: str(v) for k, v in _load(WORKFLOWS / "prune.yml")["env"].items()}


def _bin(tmp: Path) -> Path:
    """A stub `bin` directory whose `python3` is this interpreter, so the steps run the same Python the suite does."""
    path = tmp / "bin"
    path.mkdir(parents=True, exist_ok=True)
    python = path / "python3"
    python.write_text(f'#!/bin/sh\nexec "{sys.executable}" "$@"\n', encoding="utf-8")
    python.chmod(0o755)
    return path


def _write_exe(path: Path, text: str) -> None:
    path.write_text(text, encoding="utf-8")
    path.chmod(0o755)


def _git(repo: Path, *args: str) -> None:
    subprocess.run(
        ["git", "-c", "commit.gpgsign=false", "-c", "user.name=t", "-c", "user.email=t@t", *args],
        cwd=repo,
        check=True,
        capture_output=True,
        encoding="utf-8",
    )


def _run_plan(script: str, tmp: Path, cargo_version: str = "0.10.0", **env: str) -> tuple[int, dict[str, str]]:
    """Run the `plan` step in a repository that holds the tag `v0.9.0`; return exit code and `GITHUB_OUTPUT`."""
    repo = tmp / "repo"
    if not repo.exists():
        repo.mkdir(parents=True)
        _git(repo, "init", "-q")
        _git(repo, "commit", "-q", "--allow-empty", "-m", "x")
        _git(repo, "tag", "v0.9.0")
        (repo / "Cargo.toml").write_text(f'[workspace.package]\nversion = "{cargo_version}"\n', encoding="utf-8")
        bin_dir = _bin(tmp)
        # The dev arm asks for the open pull request of the branch; any other query shape is refused (exit 64), so a
        # lookup that drops the owner-qualified `head=` cannot pass.
        _write_exe(
            bin_dir / "gh",
            "#!/bin/sh\n"
            'echo "$*" >> "$HOME/gh.calls"\n'
            '[ "$1 $2 $3 $4" = "api -X GET repos/$GITHUB_REPOSITORY/pulls" ] || exit 64\n'
            '[ "$5 $6 $7 $8" = "-f state=open -f head=$GITHUB_REPOSITORY_OWNER:$GITHUB_REF_NAME" ] || exit 64\n'
            '[ "${STUB_GH_EXIT:-0}" = 0 ] || exit "$STUB_GH_EXIT"\n'
            'echo "${STUB_PR:-}"\n',
        )
    output = tmp / "out"
    output.write_text("", encoding="utf-8")
    (tmp / "plan.sh").write_text(script, encoding="utf-8")
    environment = {
        "PATH": f"{tmp / 'bin'}:{os.environ['PATH']}",
        "HOME": str(tmp),
        **_release_env(),
        "GITHUB_OUTPUT": str(output),
        "GITHUB_REF": "refs/heads/main",
        "GITHUB_REF_NAME": "main",
        "GITHUB_REPOSITORY": "ocx-sh/ocx-mirror",
        "GITHUB_REPOSITORY_OWNER": "ocx-sh",
        "GH_TOKEN": "t",
        "REPOSITORY_ID_ACTUAL": _release_env()["REPOSITORY_ID"],
        **env,
    }
    done = subprocess.run(["bash", str(tmp / "plan.sh")], cwd=repo, env=environment, capture_output=True, encoding="utf-8", check=False)
    pairs = [line.partition("=") for line in output.read_text(encoding="utf-8").splitlines()]
    return done.returncode, {k: v for k, _, v in pairs}


def _plan_script() -> str:
    return _step_script(_documents(), "release.yml:plan", "case \"$MODE\"")


def test_the_plan_step_plans_a_release_a_candidate_and_a_dev_build_on_its_track(tmp_path: Path) -> None:
    script = _plan_script()
    code, out = _run_plan(script, tmp_path, MODE="release", VERSION="0.10.0")
    assert code == 0 and re.fullmatch(r"0\.10\.0_\d{14}", out["tag"]), (code, out)
    assert (out["semver"], out["git_tag"], out["package"]) == ("0.10.0", "v0.10.0", RELEASE_PACKAGE)
    assert (out["track"], out["pull_request"]) == ("", ""), out
    code, out = _run_plan(script, tmp_path, MODE="candidate", VERSION="0.10.0-rc1")
    assert code == 0 and re.fullmatch(r"0\.10\.0-rc1_\d{14}", out["tag"]), (code, out)
    assert out["package"] == RELEASE_PACKAGE and out["git_tag"] == "v0.10.0-rc1"
    assert (out["track"], out["pull_request"]) == ("", ""), out
    code, out = _run_plan(script, tmp_path, MODE="dev", VERSION="")
    assert code == 0 and re.fullmatch(r"0\.10\.0-canary_\d{14}", out["tag"]), (code, out)
    assert re.fullmatch(r"0\.10\.0-canary\+\d{14}", out["semver"]) and out["git_tag"] == "", out
    assert (out["package"], out["track"], out["pull_request"]) == (RELEASE_PACKAGE, "canary", ""), out
    assert not (tmp_path / "gh.calls").exists(), "a run on main asked GitHub for a pull request"
    feature = {"MODE": "dev", "VERSION": "", "GITHUB_REF": "refs/heads/feature", "GITHUB_REF_NAME": "feature"}
    code, out = _run_plan(script, tmp_path, STUB_PR="12", **feature)
    assert code == 0 and re.fullmatch(r"0\.10\.0-pr12_\d{14}", out["tag"]), (code, out)
    assert re.fullmatch(r"0\.10\.0-pr12\+\d{14}", out["semver"]) and out["git_tag"] == "", out
    assert (out["package"], out["track"], out["pull_request"]) == (DEV_PACKAGE, "pr12", "12"), out
    code, out = _run_plan(script, tmp_path, **feature)
    assert code == 0 and re.fullmatch(r"0\.10\.0-dev_\d{14}", out["tag"]), (code, out)
    assert re.fullmatch(r"0\.10\.0-dev\+\d{14}", out["semver"]) and out["git_tag"] == "", out
    assert (out["package"], out["track"], out["pull_request"]) == (DEV_PACKAGE, "dev", ""), out
    code, out = _run_plan(script, tmp_path / "other", cargo_version="0.11.0", MODE="dev", VERSION="")
    assert code == 0 and re.fullmatch(r"0\.11\.0-canary_\d{14}", out["tag"]), (code, out)


_CANARY_OVERRIDE = 'if [[ "$track" == canary ]]; then'


def test_widening_the_canary_override_to_every_dev_build_puts_a_pull_request_build_in_the_release_package(tmp_path: Path) -> None:
    script = _plan_script()
    assert _CANARY_OVERRIDE in script, f"mutation did not land: {_CANARY_OVERRIDE!r} is not in the plan step"
    feature = {"MODE": "dev", "VERSION": "", "GITHUB_REF": "refs/heads/feature", "GITHUB_REF_NAME": "feature", "STUB_PR": "12"}
    _, real = _run_plan(script, tmp_path / "real", **feature)
    _, widened = _run_plan(script.replace(_CANARY_OVERRIDE, 'if [[ -n "$track" ]]; then', 1), tmp_path / "widened", **feature)
    assert real["package"] == DEV_PACKAGE and widened["package"] == RELEASE_PACKAGE, (real, widened)


_PR_LOOKUP = """pr="$(gh api -X GET "repos/$GITHUB_REPOSITORY/pulls" -f state=open -f head="$GITHUB_REPOSITORY_OWNER:$GITHUB_REF_NAME" --jq '.[0].number // empty')\""""
_ON_FEATURE = {"MODE": "dev", "VERSION": "", "GITHUB_REF": "refs/heads/feature", "GITHUB_REF_NAME": "feature"}

_PLAN_REFUSALS = {
    "a-wrong-ref": ('[[ "$GITHUB_REF" != "refs/heads/main" ]]', "false", {"MODE": "release", "VERSION": "0.10.0", "GITHUB_REF": "refs/heads/feature"}),
    "a-wrong-repository": ('[[ "$REPOSITORY_ID_ACTUAL" != "$REPOSITORY_ID" ]]', "false", {"MODE": "release", "VERSION": "0.10.0", "REPOSITORY_ID_ACTUAL": "1"}),
    "a-dotted-candidate": ('! [[ "$VERSION" =~ $shape ]]', "false", {"MODE": "candidate", "VERSION": "0.10.0-rc.1"}),
    "a-release-with-a-prerelease": ('! [[ "$VERSION" =~ $shape ]]', "false", {"MODE": "release", "VERSION": "0.10.0-rc1"}),
    "a-candidate-without-a-number": ('! [[ "$VERSION" =~ $shape ]]', "false", {"MODE": "candidate", "VERSION": "0.10.0-rc"}),
    "an-existing-tag": ('git rev-parse --quiet --verify "refs/tags/v${VERSION}" >/dev/null', "false", {"MODE": "release", "VERSION": "0.9.0"}),
    "a-failed-pull-request-lookup": (_PR_LOOKUP, _PR_LOOKUP + " || true", {**_ON_FEATURE, "STUB_GH_EXIT": "1"}),
    "a-non-numeric-pull-request": ('[[ "$pr" =~ ^[1-9][0-9]*$ ]]', "true", {**_ON_FEATURE, "STUB_PR": "12abc"}),
    "a-zero-pull-request": ('[[ "$pr" =~ ^[1-9][0-9]*$ ]]', "true", {**_ON_FEATURE, "STUB_PR": "0"}),
    "an-unparseable-cargo-version": ('! [[ "$core" =~ ^[0-9]+\\.[0-9]+\\.[0-9]+$ ]]', "false", {"MODE": "dev", "VERSION": "", "cargo_version": "0.11"}),
    "an-unknown-mode": ("echo \"::error::mode '$MODE' is not release, candidate or dev\"\n    exit 1", "semver=x tag=x package=x git_tag=x", {"MODE": "publish", "VERSION": "0.10.0"}),
}


@pytest.mark.parametrize("case", sorted(_PLAN_REFUSALS))
def test_the_plan_step_refuses_what_it_guards_and_a_deleted_guard_lets_it_through(case: str, tmp_path: Path) -> None:
    guard, replacement, env = _PLAN_REFUSALS[case]
    script = _plan_script()
    assert guard in script, f"mutation did not land: {guard!r} is not in the plan step"
    code, _ = _run_plan(script, tmp_path / "real", **env)
    assert code != 0, f"the real plan step accepted {case}"
    code, _ = _run_plan(script.replace(guard, replacement, 1), tmp_path / "mutated", **env)
    assert code == 0, f"deleting the {case} guard still refused: the test cannot tell the guard from another failure"


_OPEN_PRS_WRITE = '> "$RUNNER_TEMP/open-prs.txt"'


def _run_prune(
    script: str, tmp: Path, tags: list[str], open_prs: str = "", gh_exit: int = 0, env: dict[str, str] | None = None
) -> tuple[int, list[str] | None]:
    """Run a prune step; the exit code and the argv of `ocx package prune` (None when it was never called).

    The step runs in the repository root, so it calls the real `scripts/release_prune_select.py`.
    """
    bin_dir = _bin(tmp)
    (tmp / "runner").mkdir()
    listing = tmp / "listing.json"
    listing.write_text(json.dumps({"items": [{"tags": tags}]}), encoding="utf-8")
    log = tmp / "prune.log"
    _write_exe(
        bin_dir / "ocx",
        "#!/bin/sh\n"
        'if [ "$#" -eq 6 ] && [ "$1 $2 $3 $4 $5" = "--remote --format json index list" ]; then echo "$6" >> "$STUB_LIST_LOG"; cat "$STUB_LISTING"\n'
        'elif [ "$1 $2" = "package prune" ]; then echo "$*" >> "$STUB_PRUNE_LOG"\n'
        'else echo "ocx stub: unexpected argv: $*" >&2; exit 64; fi\n',
    )
    _write_exe(
        bin_dir / "gh",
        "#!/bin/sh\n"
        '[ "$#" -eq 10 ] && [ "$1" = pr ] && [ "$2" = list ] && [ "$3" = --state ] && [ "$4" = open ] && [ "$5" = --limit ] && [ "$6" = 1000 ] '
        '&& [ "$7" = --json ] && [ "$8" = number ] && [ "$9" = --jq ] && [ "${10}" = ".[].number" ] || { echo "gh stub: unexpected argv: $*" >&2; exit 64; }\n'
        '[ -n "$GH_REPO" ] && [ -n "$GH_TOKEN" ] || { echo "gh stub: no GH_REPO or GH_TOKEN" >&2; exit 64; }\n'
        '[ "$STUB_GH_EXIT" = 0 ] || exit "$STUB_GH_EXIT"\n'
        'for n in $STUB_OPEN; do echo "$n"; done\n',
    )
    (tmp / "prune.sh").write_text(script, encoding="utf-8")
    done = subprocess.run(
        ["bash", str(tmp / "prune.sh")],
        cwd=REPO_ROOT,
        env={
            "PATH": f"{bin_dir}:{os.environ['PATH']}",
            "HOME": str(tmp),
            "RUNNER_TEMP": str(tmp / "runner"),
            "PACKAGE": DEV_PACKAGE,
            "DAYS": "30",
            "KEEP": "3",
            "GH_REPO": "ocx-sh/ocx-mirror",
            "GH_TOKEN": "t",
            "STUB_LISTING": str(listing),
            "STUB_LIST_LOG": str(tmp / "list.log"),
            "STUB_PRUNE_LOG": str(log),
            "STUB_OPEN": open_prs,
            "STUB_GH_EXIT": str(gh_exit),
            **(env or {}),
        },
        capture_output=True,
        encoding="utf-8",
        check=False,
    )
    # Both prune steps list the registry first, and exactly once, whatever happens after.
    package = (env or {}).get("PACKAGE_RELEASE", DEV_PACKAGE)
    listed = (tmp / "list.log").read_text(encoding="utf-8").splitlines() if (tmp / "list.log").exists() else []
    assert listed == [package], f"the step listed {listed}, expected one listing of {package}"
    return done.returncode, (log.read_text(encoding="utf-8").splitlines() if log.exists() else None)


# One stamp per process: a listing and the assertion on it are built on either side of a subprocess run, and a
# stamp read twice differs once the clock crosses a second.
_RECENT = (datetime.now(UTC) - timedelta(days=1)).strftime("%Y%m%d%H%M%S")

_CANARY_LISTING = [
    "0.9.0-canary", "0.10.0-canary",
    "0.9.0-canary_20260101000000", "0.10.0-canary_20260102000000", "0.10.0-canary_20260103000000",
    "0.10.0-canary_20260104000000", "0.10.0-canary_20260105000000",
]
_OLD_CANDIDATES = ["0.9.0_20200102000000", "0.9.0-rc1_20200101000000", "0.9.0-rc1", "0.10.0-rc1_20200101000000", "0.10.0-rc1"]
_RELEASE_LISTING = ["0.9.0", "latest", *_OLD_CANDIDATES, *_CANARY_LISTING]


def _dev_package_listing() -> list[str]:
    return [
        "0.10.0-dev", f"0.10.0-dev_{_RECENT}", "0.10.0-dev_20200101000000",
        "0.9.0-canary", "0.10.0-canary",
        "0.9.0-canary_20260101000000", "0.10.0-canary_20260102000000", "0.10.0-canary_20260103000000",
        "0.10.0-canary_20260104000000", "0.10.0-canary_20260105000000",
        "0.10.0-pr7", f"0.10.0-pr7_{_RECENT}", "0.10.0-pr9", f"0.10.0-pr9_{_RECENT}",
        "0.10.0", "latest",
    ]


def _dev_prune_script() -> str:
    return str(_job(_documents(), "prune.yml:dev")["steps"][-1]["run"])


def test_the_dev_prune_hands_prune_exactly_the_stale_dev_and_the_closed_pull_request_builds_and_no_canary(tmp_path: Path) -> None:
    code, lines = _run_prune(_dev_prune_script(), tmp_path, _dev_package_listing(), open_prs="9")
    assert code == 0 and lines is not None and len(lines) == 1, (code, lines)
    argv = lines[0].split()
    assert argv[:3] == ["package", "prune", "--forge"] and "--force" in argv, argv
    package = argv.index(DEV_PACKAGE)
    assert set(argv[package + 1 :]) == {"0.10.0-dev_20200101000000", f"0.10.0-pr7_{_RECENT}", "0.10.0-pr7"}, argv
    assert not any("canary" in word for word in argv), f"the dev prune selected a canary tag: {argv}"


def test_the_dev_prune_calls_prune_for_nothing_when_nothing_is_stale_whatever_canary_tags_are_listed(tmp_path: Path) -> None:
    tags = ["0.10.0-dev", f"0.10.0-dev_{_RECENT}", "0.10.0-pr9", f"0.10.0-pr9_{_RECENT}", *_CANARY_LISTING]
    code, lines = _run_prune(_dev_prune_script(), tmp_path, tags, open_prs="9")
    assert (code, lines) == (0, None)


def test_a_failed_open_pull_request_listing_aborts_the_prune_before_it_deletes_anything(tmp_path: Path) -> None:
    script = _dev_prune_script()
    code, lines = _run_prune(script, tmp_path / "real", _dev_package_listing(), open_prs="9", gh_exit=1)
    assert code != 0 and lines is None, (code, lines)
    # Swallowing the failure leaves an empty list, which reads as "no pull request is open": every PR build goes.
    assert _OPEN_PRS_WRITE in script, f"mutation did not land: {_OPEN_PRS_WRITE!r} is not in the prune step"
    code, lines = _run_prune(script.replace(_OPEN_PRS_WRITE, _OPEN_PRS_WRITE + " || true", 1), tmp_path / "mutated", _dev_package_listing(), open_prs="9", gh_exit=1)
    assert code == 0 and lines is not None and f"0.10.0-pr9_{_RECENT}" in lines[0].split(), (code, lines)


def _release_prune_script() -> str:
    return str(next(s for s in _job(_documents(), "release.yml:prune")["steps"] if s.get("id") == "prune")["run"])


def _prune_argv(lines: list[str] | None, package: str) -> tuple[list[str], list[str]]:
    """`(flags, tags)` of the one `ocx package prune` call, split at the logical package."""
    assert lines is not None and len(lines) == 1, lines
    argv = lines[0].split()
    at = argv.index(package)
    return argv[:at], argv[at + 1 :]


def test_the_release_run_prunes_only_the_candidates_of_a_shipped_version_and_never_a_canary(tmp_path: Path) -> None:
    code, lines = _run_prune(_release_prune_script(), tmp_path, _RELEASE_LISTING, env=_release_env())
    assert code == 0, code
    flags, tags = _prune_argv(lines, LOGICAL_PACKAGE)
    assert "--force" not in flags and set(tags) == {"0.9.0-rc1_20200101000000", "0.9.0-rc1"}, (flags, tags)
    assert not any("canary" in tag for tag in tags), tags


@pytest.mark.parametrize(
    ("old", "new"),
    [
        pytest.param('release_prune_select.py candidates --days "$DAYS"', "release_prune_select.py canary --keep 3", id="release-run-selects-canaries"),
        pytest.param("ocx package prune --forge github --tags-file", "ocx package prune --forge github --force --tags-file", id="release-package-prune-forces"),
    ],
)
def test_the_release_prune_mutations_change_what_the_step_hands_prune(tmp_path: Path, old: str, new: str) -> None:
    script = _release_prune_script()
    assert old in script, f"mutation did not land: {old!r} is not in the prune step"
    real = _run_prune(script, tmp_path / "real", _RELEASE_LISTING, env=_release_env())
    mutated = _run_prune(script.replace(old, new, 1), tmp_path / "mutated", _RELEASE_LISTING, env=_release_env())
    assert real != mutated


_CANARY_SELECTED = {"0.9.0-canary_20260101000000", "0.9.0-canary", "0.10.0-canary_20260102000000"}
_CANARY_STUB = """#!/bin/sh
case "$*" in
  "--remote --format json index list "*) cat "$STUB_LISTING"; exit 0 ;;
  "package announce "*|"package prune "*) ;;
  *) exit 64 ;;
esac
file=
prev=
for a in "$@"; do
  [ "$prev" = --tags-file ] && file=$a
  prev=$a
done
if [ "$2" = announce ] && [ -n "$file" ]; then
  printf '%s :: %s\\n' "$*" "$(tr '\\n' ' ' < "$file")" >> "$STUB_LOG"
else
  echo "$*" >> "$STUB_LOG"
fi
"""


def _run_canary(tmp: Path, tags: list[str], docs: Docs | None = None) -> list[str]:
    """Run the `prune.yml` canary job's steps in order; every `ocx package` call, an announce with its tags file appended after ` :: `.

    A step runs when its `if:` allows: `steps.select.outputs.any` is the select step's own output, and the
    closing announce follows whether `prune` ran. Only `select` and the steps that call `ocx package` run: the
    ref/repository guard, the checkout and the release-ocx install are NOT run here (the structural families
    `prune-entry` and `release-tools` cover them).
    """
    steps = _job(docs or _documents(), "prune.yml:canary")["steps"]
    bin_dir = _bin(tmp)
    (tmp / "runner").mkdir()
    (tmp / "listing.json").write_text(json.dumps({"items": [{"tags": tags}]}), encoding="utf-8")
    _write_exe(bin_dir / "ocx", _CANARY_STUB)
    output, log = tmp / "output", tmp / "calls.log"
    output.touch()
    env = {
        **_prune_env(),
        "PATH": f"{bin_dir}:{os.environ['PATH']}",
        "HOME": str(tmp),
        "RUNNER_TEMP": str(tmp / "runner"),
        "GITHUB_OUTPUT": str(output),
        "KEEP": _prune_env()["CANARY_KEEP"],
        "OCX_ANNOUNCE_TOKEN": "t",
        "STUB_LISTING": str(tmp / "listing.json"),
        "STUB_LOG": str(log),
    }
    ran: set[str] = set()
    for step in steps:
        if "package" not in str(step.get("run")) and step.get("id") != "select":
            continue
        condition = str(step.get("if", ""))
        if "steps.select.outputs.any" in condition and "any=true" not in output.read_text(encoding="utf-8"):
            continue
        if "steps.prune.conclusion" in condition and "prune" not in ran:
            continue
        done = subprocess.run(["bash", "-c", str(step["run"])], cwd=REPO_ROOT, env=env, capture_output=True, encoding="utf-8", check=False)
        assert done.returncode == 0, (step.get("name"), done.stderr)
        ran.add(str(step.get("id")))
    return log.read_text(encoding="utf-8").splitlines() if log.exists() else []


def _canary_prune_findings(calls: list[str]) -> list[str]:
    """Announce the selection ephemerally from a tags file, then prune exactly it without `--force`, then reconcile."""
    if len(calls) != 3:
        return [f"expected announce, prune, reconcile; got {calls}"]
    found = []
    head, _, listed = calls[0].partition(" :: ")
    words = head.split()
    if words[:2] != ["package", "announce"] or "--ephemeral" not in words or "--tags-file" not in words:
        found.append(f"the first call is not an ephemeral announce of a tags file: {calls[0]}")
    if set(listed.split()) != _CANARY_SELECTED:
        found.append(f"the announced tags are {sorted(listed.split())}")
    prune = calls[1].split()
    if prune[:2] != ["package", "prune"] or "--force" in prune or set(prune[prune.index(LOGICAL_PACKAGE) + 1 :]) != _CANARY_SELECTED:
        found.append(f"the second call is not a plain prune of the selected tags: {calls[1]}")
    if not calls[2].startswith("package announce --refresh") or "--ephemeral" in calls[2]:
        found.append(f"the last call is not the refresh announce: {calls[2]}")
    return found


def test_the_canary_job_announces_the_selection_ephemerally_before_it_prunes_exactly_that_selection_without_force(tmp_path: Path) -> None:
    assert _canary_prune_findings(_run_canary(tmp_path, _RELEASE_LISTING)) == []


def test_the_canary_job_calls_neither_announce_nor_prune_when_no_canary_is_beyond_the_keep_count(tmp_path: Path) -> None:
    assert _run_canary(tmp_path, ["0.10.0-canary", *_CANARY_LISTING[-3:], *_OLD_CANDIDATES]) == []


# Each mutation and the finding of `_canary_prune_findings` it must produce (a substring), not merely "a finding":
# the call-count line alone would satisfy a bare truthiness assertion for an unrelated mutation.
_CANARY_MUTATIONS = {
    "announce-drops-ephemeral": ("the first call is not an ephemeral announce", ("announce --ephemeral --tags-file", "announce --tags-file")),
    "announce-swaps-in-tags-from-registry": (
        "the first call is not an ephemeral announce",
        ('--ephemeral --tags-file "$RUNNER_TEMP/selected.txt"', "--ephemeral --tags-from-registry"),
    ),
    "prune-forces": ("the second call is not a plain prune", ("package prune --forge github --tags-file", "package prune --forge github --force --tags-file")),
    "select-picks-candidates": ("the announced tags are", ("release_prune_select.py canary --keep", "release_prune_select.py candidates --days")),
    "prune-before-announce": ("the first call is not an ephemeral announce", None),
}


@pytest.mark.parametrize("name", _CANARY_MUTATIONS)
def test_the_canary_mutations_change_what_the_steps_call(tmp_path: Path, name: str) -> None:
    expected, edit = _CANARY_MUTATIONS[name]
    original = _documents()
    mutated = copy.deepcopy(original)
    if edit is None:
        _prune_before_announce(mutated)
    else:
        _replace_run(mutated, "prune.yml:canary", *edit)
    assert mutated != original, "the mutation changed nothing"
    findings = _canary_prune_findings(_run_canary(tmp_path, _RELEASE_LISTING, mutated))
    assert any(expected in finding for finding in findings), f"{name}: no finding contains {expected!r}: {findings}"


_EPHEMERAL_RULE = '[[ "$MODE" == release ]] || flags+=(--ephemeral)'


def _run_announce(tmp: Path, script: str, mode: str) -> list[str]:
    """Run the announce step; the argv `ocx package announce` received."""
    bin_dir = _bin(tmp)
    _write_exe(bin_dir / "ocx", '#!/bin/sh\necho "$*" >> "$STUB_LOG"\n')
    tags = tmp / "tags.txt"
    tags.write_text("1.0.0\n", encoding="utf-8")
    (tmp / "announce.sh").write_text(script, encoding="utf-8")
    done = subprocess.run(
        ["bash", str(tmp / "announce.sh")],
        env={
            "PATH": f"{bin_dir}:{os.environ['PATH']}",
            "STUB_LOG": str(tmp / "argv.log"),
            **_release_env(),
            "MODE": mode,
            "TAGS_FILE": str(tags),
            "OCX_ANNOUNCE_TOKEN": "t",
        },
        capture_output=True,
        encoding="utf-8",
        check=False,
    )
    assert done.returncode == 0, done.stderr
    return (tmp / "argv.log").read_text(encoding="utf-8").split()


def _announce_script() -> str:
    return _step_script(_documents(), "release.yml:announce", "package announce")


@pytest.mark.parametrize(("mode", "ephemeral"), [("dev", True), ("candidate", True), ("release", False)])
def test_announce_marks_every_row_ephemeral_except_a_releases(tmp_path: Path, mode: str, ephemeral: bool) -> None:
    argv = _run_announce(tmp_path, _announce_script(), mode)
    assert ("--ephemeral" in argv) is ephemeral, argv


@pytest.mark.parametrize(
    ("replacement", "mode"),
    [
        pytest.param('[[ "$MODE" == candidate ]] && flags+=(--ephemeral)', "dev", id="a-canary-announces-durable"),
        pytest.param("flags+=(--ephemeral)", "release", id="a-release-announces-ephemeral"),
    ],
)
def test_the_announce_mutations_flip_the_ephemeral_flag(tmp_path: Path, replacement: str, mode: str) -> None:
    script = _announce_script()
    assert _EPHEMERAL_RULE in script, f"mutation did not land: {_EPHEMERAL_RULE!r} is not in the announce step"
    real = _run_announce(tmp_path / "real", script, mode)
    mutated = _run_announce(tmp_path / "mutated", script.replace(_EPHEMERAL_RULE, replacement, 1), mode)
    assert ("--ephemeral" in real) is not ("--ephemeral" in mutated)


_MARKER = "<!-- ocx-pr-build -->"
_BOT = "github-actions[bot]"


def _comment_script() -> str:
    return _step_script(_documents(), "release.yml:comment", "gh api")


def _run_comment(tmp: Path, script: str, comments: list[dict[str, Any]], list_exit: int = 0) -> tuple[int, list[str], str]:
    """Run the comment step; the exit code, the `gh` calls (listing included) and the body last written."""
    bin_dir = _bin(tmp)
    (tmp / "runner").mkdir()
    listing = tmp / "comments.json"
    listing.write_text(json.dumps(comments), encoding="utf-8")
    _write_exe(
        bin_dir / "gh",
        "#!/bin/sh\n"
        'if [ "$1 $2" = "api --paginate" ]; then\n'
        '  echo "LIST $3" >> "$STUB_GH_LOG"\n'
        '  [ "$STUB_LIST_EXIT" = 0 ] || exit "$STUB_LIST_EXIT"\n'
        '  while [ "$#" -gt 0 ]; do [ "$1" = --jq ] && { jq -r "$2" "$STUB_COMMENTS"; exit 0; }; shift; done\n'
        "  exit 64\n"
        "fi\n"
        '[ "$1 $2 $5" = "api -X -F" ] || exit 64\n'
        'echo "$3 $4" >> "$STUB_GH_LOG"\n'
        'cat "${6#body=@}" > "$STUB_BODY"\n',
    )
    (tmp / "comment.sh").write_text(script, encoding="utf-8")
    done = subprocess.run(
        ["bash", str(tmp / "comment.sh")],
        env={
            "PATH": f"{bin_dir}:{os.environ['PATH']}",
            "RUNNER_TEMP": str(tmp / "runner"),
            "GITHUB_REPOSITORY": "ocx-sh/ocx-mirror",
            "GITHUB_SHA": "abc123def",
            "GH_TOKEN": "t",
            "PACKAGE": DEV_PACKAGE,
            "TAG": "0.10.0-pr12_20261117000000",
            "PULL_REQUEST": "12",
            "STUB_COMMENTS": str(listing),
            "STUB_LIST_EXIT": str(list_exit),
            "STUB_GH_LOG": str(tmp / "gh.log"),
            "STUB_BODY": str(tmp / "body.md"),
        },
        capture_output=True,
        encoding="utf-8",
        check=False,
    )
    calls = (tmp / "gh.log").read_text(encoding="utf-8").splitlines() if (tmp / "gh.log").exists() else []
    body = (tmp / "body.md").read_text(encoding="utf-8") if (tmp / "body.md").exists() else ""
    return done.returncode, calls, body


def _writes(calls: list[str]) -> list[str]:
    return [call for call in calls if not call.startswith("LIST ")]


_POST = "POST repos/ocx-sh/ocx-mirror/issues/12/comments"
_OWN = {"id": 42, "user": {"login": _BOT}, "body": f"{_MARKER}\nold"}
_FOREIGN_MARKER = {"id": 7, "user": {"login": "someone"}, "body": f"{_MARKER}\nforged"}
_BOT_WITHOUT_MARKER = {"id": 8, "user": {"login": _BOT}, "body": "a different bot comment"}


def test_a_first_pull_request_build_posts_one_comment_with_both_addresses_and_the_marker_first(tmp_path: Path) -> None:
    code, calls, body = _run_comment(tmp_path, _comment_script(), [])
    assert code == 0 and _writes(calls) == [_POST], (code, calls)
    assert body.splitlines()[0] == _MARKER, body
    assert f"`{DEV_PACKAGE}:0.10.0-pr12`" in body and f"`{DEV_PACKAGE}:0.10.0-pr12_20261117000000`" in body, body
    assert "abc123def" in body and f"ocx --remote package exec {DEV_PACKAGE}:0.10.0-pr12 -- ocx-mirror version" in body, body


def test_a_later_build_edits_the_bots_marker_comment_in_place(tmp_path: Path) -> None:
    code, calls, _ = _run_comment(tmp_path, _comment_script(), [_FOREIGN_MARKER, _OWN, _BOT_WITHOUT_MARKER])
    assert code == 0 and _writes(calls) == ["PATCH repos/ocx-sh/ocx-mirror/issues/comments/42"], (code, calls)


def test_a_marker_comment_by_anyone_else_is_ignored_and_never_edited(tmp_path: Path) -> None:
    code, calls, _ = _run_comment(tmp_path, _comment_script(), [_FOREIGN_MARKER, _BOT_WITHOUT_MARKER])
    assert code == 0 and _writes(calls) == [_POST], (code, calls)


def test_a_failed_listing_fails_the_step_and_writes_nothing(tmp_path: Path) -> None:
    code, calls, _ = _run_comment(tmp_path, _comment_script(), [_OWN], list_exit=1)
    assert code != 0 and calls == ["LIST repos/ocx-sh/ocx-mirror/issues/12/comments"], (code, calls)


@pytest.mark.parametrize(
    ("old", "new", "comments", "list_exit", "different"),
    [
        pytest.param('.user.login == "github-actions[bot]" and ', "", [_FOREIGN_MARKER], 0, "edited a foreign comment", id="author-filter-dropped"),
        pytest.param("| .id')\"", "| .id')\" || true", [_OWN], 1, "fell through to a write", id="listing-failure-swallowed"),
        pytest.param('startswith("<!-- ocx-pr-build -->")', 'startswith("")', [_BOT_WITHOUT_MARKER], 0, "edited an unmarked comment", id="marker-filter-dropped"),
    ],
)
def test_the_comment_mutations_change_what_the_step_writes(tmp_path: Path, old: str, new: str, comments: list[dict[str, Any]], list_exit: int, different: str) -> None:
    script = _comment_script()
    assert old in script, f"mutation did not land: {old!r} is not in the comment step"
    real = _run_comment(tmp_path / "real", script, comments, list_exit)
    mutated = _run_comment(tmp_path / "mutated", script.replace(old, new, 1), comments, list_exit)
    assert real[:2] != mutated[:2], f"the mutation that {different} left the step unchanged"


_MAIN_IDENTITY = "https://github.com/ocx-sh/ocx-mirror/.github/workflows/release.yml@refs/heads/main"
_BRANCH_IDENTITY = "https://github.com/ocx-sh/ocx-mirror/.github/workflows/release.yml@refs/heads/feature"


def _run_verify_step(tmp: Path, docs: Docs, package: str) -> list[str]:
    """Run the verify step with a recording `ocx`, from a feature-branch run; the certificate identities it asked for."""
    script = _step_script(docs, "release.yml:publish", "package verify")
    bin_dir = _bin(tmp)
    _write_exe(bin_dir / "ocx", '#!/bin/sh\nprintf "%s\\n" "$*" >> "$STUB_LOG"\n')
    log = tmp / "argv.log"
    log.write_text("", encoding="utf-8")
    (tmp / "verify.sh").write_text(script, encoding="utf-8")
    done = subprocess.run(
        ["bash", str(tmp / "verify.sh")],
        env={
            "PATH": f"{bin_dir}:{os.environ['PATH']}",
            "STUB_LOG": str(log),
            **_release_env(),
            "RELEASE_IDENTITY": _MAIN_IDENTITY,
            "PACKAGE": package,
            "TAG": "1.0.0_1",
            "GITHUB_SERVER_URL": "https://github.com",
            "GITHUB_WORKFLOW_REF": "ocx-sh/ocx-mirror/.github/workflows/release.yml@refs/heads/feature",
        },
        capture_output=True,
        encoding="utf-8",
        check=False,
    )
    assert done.returncode == 0, done.stderr
    lines = log.read_text(encoding="utf-8").splitlines()
    return sorted({m.group(1) for line in lines if (m := re.search(r"--certificate-identity (\S+)", line))})


@pytest.mark.parametrize(
    ("package", "identity"),
    [
        pytest.param(RELEASE_PACKAGE, _MAIN_IDENTITY, id="release-package-keeps-the-pin-on-a-branch-run"),
        pytest.param(DEV_PACKAGE, _BRANCH_IDENTITY, id="dev-package-takes-the-run-ref"),
    ],
)
def test_the_verify_step_pins_main_for_anything_but_the_dev_package_and_the_run_ref_for_the_dev_package(tmp_path: Path, package: str, identity: str) -> None:
    assert _run_verify_step(tmp_path, _documents(), package) == [identity]


@pytest.mark.parametrize(
    ("old", "new", "package"),
    [
        pytest.param('identity="$RELEASE_IDENTITY"', 'identity="${GITHUB_SERVER_URL}/${GITHUB_WORKFLOW_REF}"', RELEASE_PACKAGE, id="release-package-trusts-the-run-ref"),
        pytest.param('[[ "$PACKAGE" == "$PACKAGE_DEV" ]]', '[[ "$PACKAGE" != "$PACKAGE" ]]', DEV_PACKAGE, id="dev-package-pinned-to-main"),
    ],
)
def test_the_verify_step_mutations_red_the_identity_choice(tmp_path: Path, old: str, new: str, package: str) -> None:
    mutated = _documents()
    _replace_run(mutated, "release.yml:publish", old, new)
    assert _run_verify_step(tmp_path / "mutated", mutated, package) != _run_verify_step(tmp_path / "real", _documents(), package)


if __name__ == "__main__":
    raise SystemExit(pytest.main([__file__, "-q", "-p", "no:cacheprovider"]))
