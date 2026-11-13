#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 The OCX Authors
"""Refuse a release whose exact commit has no green readiness run.

    python3 scripts/release_readiness_gate.py --sha <40-hex commit>
    python3 scripts/release_readiness_gate.py --self-test

Asks GitHub (through `gh`) for a successful `release-readiness.yml` run whose head is exactly `--sha`
(`task release:tag` on HEAD and `readiness-gate.yml` before `host` both call it); a run on an
ancestor or sibling commit is no evidence. Every non-zero exit refuses the release: 1 no green run,
2 `--sha` is not a full 40-hex commit, 69 `gh` missing or unauthenticated, 74 any other `gh` failure,
a `gh` call over 60s, or unexpected JSON. `--self-test` fakes `gh` and shows every exit code. Exit 0 prints the run URL and run-name, so a `BYPASS` run is named in the log.
"""

from __future__ import annotations

import argparse
import contextlib
import io
import json
import re
import shutil
import subprocess
import sys

WORKFLOW = "release-readiness.yml"
GH_TIMEOUT_SECONDS = 60
FULL_SHA = re.compile(r"[0-9a-f]{40}")
# What `gh` says on stderr when it has no usable credential; its exit code (1 or 4) is shared with
# every other failure, so the words decide.
UNAUTHENTICATED = re.compile(r"gh auth login|GH_TOKEN|GITHUB_TOKEN|authentication|HTTP 401", re.IGNORECASE)


def trusted(run_id: int) -> bool:
    """True when the run is this workflow file on a branch of the gated repo, not a fork's copy.

    A fork PR's label run executes the workflow file from the PR, so the fork controls what it reports.
    Raises RuntimeError (caught in `run`, exit 74) when `gh` fails or hangs.
    """
    try:
        done = subprocess.run(
            ["gh", "api", f"repos/{{owner}}/{{repo}}/actions/runs/{run_id}"],
            capture_output=True,
            text=True,
            encoding="utf-8",
            check=False,
            timeout=GH_TIMEOUT_SECONDS,
        )
    except subprocess.TimeoutExpired as error:
        raise RuntimeError(f"gh api timed out after {GH_TIMEOUT_SECONDS}s") from error
    if done.returncode != 0:
        raise RuntimeError(f"gh api failed ({done.returncode}): {done.stderr.strip()}")
    detail = json.loads(done.stdout)
    path = detail["path"].split("@", 1)[0]
    return detail["head_repository"]["full_name"] == detail["repository"]["full_name"] and path == f".github/workflows/{WORKFLOW}"


def run(sha: str) -> int:
    if shutil.which("gh") is None:
        print("readiness gate: `gh` is not installed", file=sys.stderr)
        return 69
    try:
        done = subprocess.run(
            [
                "gh",
                "run",
                "list",
                "--workflow",
                WORKFLOW,
                "--commit",
                sha,
                "--status",
                "success",
                "--limit",
                "100",
                "--json",
                "url,displayTitle,headSha,conclusion,databaseId",
            ],
            capture_output=True,
            text=True,
            encoding="utf-8",
            check=False,
            timeout=GH_TIMEOUT_SECONDS,
        )
    except subprocess.TimeoutExpired:
        print(f"readiness gate: gh run list timed out after {GH_TIMEOUT_SECONDS}s", file=sys.stderr)
        return 74
    if done.returncode != 0:
        print(f"readiness gate: gh run list failed ({done.returncode}): {done.stderr.strip()}", file=sys.stderr)
        if "default branch" in done.stderr:
            # GitHub knows a workflow only once its file is on the default branch: it cannot be dispatched and
            # lists no runs. A `pull_request` run needs no default-branch copy, so the label route still works.
            print(f"readiness gate: {WORKFLOW} is not on the default branch yet; until it lands, apply the `run-readiness` label to the pull request carrying {sha[:12]}", file=sys.stderr)
        return 69 if UNAUTHENTICATED.search(done.stderr) else 74
    try:
        runs = json.loads(done.stdout)
        # Asked for by status, but re-checked here: the exit status below must not rest on a flag's meaning.
        # A `pull_request: labeled` event for an unrelated label starts a run named `readiness-ignored ...`
        # (release-readiness.yml); it must never count, whatever conclusion GitHub gives an all-skipped run.
        green = [
            r
            for r in runs
            if r["headSha"] == sha and r["conclusion"] == "success" and r["displayTitle"].startswith("readiness ")
        ]
    except (ValueError, TypeError, KeyError) as error:
        print(f"readiness gate: unexpected `gh run list` output ({error!r}): {done.stdout[:200]!r}", file=sys.stderr)
        return 74
    if not green:
        print(f"readiness gate: no successful {WORKFLOW} run for {sha}", file=sys.stderr)
        print(f"dispatch it on that commit: gh workflow run {WORKFLOW} --ref <branch holding {sha[:12]}>", file=sys.stderr)
        return 1
    found = None
    for candidate in green:
        try:
            if trusted(candidate["databaseId"]):
                found = candidate
                break
        except (RuntimeError, ValueError, TypeError, KeyError) as error:
            print(f"readiness gate: could not verify run {candidate.get('url')}: {error}", file=sys.stderr)
            return 74
    if found is None:
        print(f"readiness gate: no {WORKFLOW} run for {sha} came from this repository's own workflow file", file=sys.stderr)
        return 1
    print(f"readiness gate: {found['url']}")
    print(f"readiness gate: run-name '{found['displayTitle']}'")
    if "BYPASS" in found["displayTitle"]:
        print("readiness gate: this run is a BYPASS - the test jobs were skipped, the reason is in the run-name")
    return 0


def self_test() -> int:
    """Each exit code against a faked `gh`; a case that gets the wrong code fails the run."""
    sha = "a" * 40
    real_run, real_which = subprocess.run, shutil.which
    stdout, stderr = "", ""

    def fake_run(*_args, **_kwargs):
        if fake_run.code is None:
            raise subprocess.TimeoutExpired("gh", _kwargs["timeout"])
        out = fake_run.detail if _args[0][:2] == ["gh", "api"] else fake_run.out
        return subprocess.CompletedProcess([], fake_run.code, stdout=out, stderr=fake_run.err)

    def case(name: str, want: int, out: str = "[]", err: str = "", code: int | None = 0, gh: bool = True, argv=None, detail: str | None = None) -> None:
        nonlocal stdout, stderr
        fake_run.code, fake_run.out, fake_run.err = code, out, err
        fake_run.detail = detail if detail is not None else run_detail()
        shutil.which = (lambda _name: "/usr/bin/gh") if gh else (lambda _name: None)
        # `contextlib` keeps the case's own diagnostics off the self-test's output.
        with contextlib.redirect_stdout(io.StringIO()) as o, contextlib.redirect_stderr(io.StringIO()) as e:
            try:
                got = main(argv if argv is not None else ["--sha", sha])
            except SystemExit as exit_:
                got = exit_.code
        stdout, stderr = o.getvalue(), e.getvalue()
        if got != want:
            raise SystemExit(f"release readiness gate self-test: {name}: exit {got}, want {want}")
        print(f"ok  {want:>2}  {name}")

    def run_json(title: str, head: str = sha, conclusion: str = "success") -> str:
        return json.dumps([{"url": "https://example.test/run/1", "displayTitle": title, "headSha": head, "conclusion": conclusion, "databaseId": 1}])

    def run_detail(head: str = "o/r", path: str = ".github/workflows/release-readiness.yml") -> str:
        return json.dumps({"head_repository": {"full_name": head}, "repository": {"full_name": "o/r"}, "path": path})

    subprocess.run = fake_run
    try:
        case("green run", 0, out=run_json(f"readiness {sha}"))
        case("BYPASS run passes and says so", 0, out=run_json(f"readiness {sha} BYPASS: gh outage"))
        if "BYPASS" not in stdout:
            raise SystemExit("release readiness gate self-test: a BYPASS run was not named in the output")
        case("no green run, with the dispatch hint", 1, out=run_json(f"readiness {sha}", head="b" * 40))
        if "gh workflow run release-readiness.yml" not in stderr:
            raise SystemExit("release readiness gate self-test: the dispatch hint is missing")
        case("an ignored-label run is no evidence", 1, out=run_json("readiness-ignored triage"))
        case("a fork-headed run is no evidence", 1, out=run_json(f"readiness {sha}"), detail=run_detail(head="fork/r"))
        case("another workflow file is no evidence", 1, out=run_json(f"readiness {sha}"), detail=run_detail(path=".github/workflows/x.yml"))
        case("the workflow path may carry an @ref", 0, out=run_json(f"readiness {sha}"), detail=run_detail(path=".github/workflows/release-readiness.yml@refs/heads/main"))
        case("--sha is not 40 hex", 2, argv=["--sha", "abc123"])
        case("gh missing", 69, gh=False)
        case("gh unauthenticated", 69, code=4, err="To get started with GitHub CLI, please run:  gh auth login")
        case("gh fails otherwise", 74, code=1, err="HTTP 502: bad gateway")
        case("workflow not on the default branch", 74, code=1, err="HTTP 404: workflow release-readiness.yml not found on the default branch")
        if "run-readiness" not in stderr:
            raise SystemExit("release readiness gate self-test: the label hint is missing")
        case("gh hangs past the timeout", 74, code=None)
        if "timed out" not in stderr:
            raise SystemExit("release readiness gate self-test: the timeout line is missing")
        case("gh prints bad JSON", 74, out="not json")
    finally:
        subprocess.run, shutil.which = real_run, real_which
    print("release readiness gate self-test: every exit code shown")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--sha", help="the full 40-hex commit the release is cut from")
    parser.add_argument("--self-test", action="store_true", help="prove every exit code against a faked `gh`")
    args = parser.parse_args(argv)
    if args.self_test:
        return self_test()
    if args.sha is None:
        parser.error("--sha is required")
    if not FULL_SHA.fullmatch(args.sha):
        parser.error(f"--sha must be a full 40-hex commit, got {args.sha!r}")
    return run(args.sha)


if __name__ == "__main__":
    sys.exit(main())
