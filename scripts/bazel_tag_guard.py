#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 The OCX Authors
"""`bazel:tag:guard` — every Bazel test here stays remotely cacheable and honestly keyed.

    scripts/bazel_tag_guard.py --bazel '<bazel argv>'   # the gate: two queries over //...
    scripts/bazel_tag_guard.py --self-test

Adapted port of ocx's `scripts/bazel_tag_guard.py` (C-011). ocx guards 181
per-module acceptance targets whose results never leave the host; this repo
has one coarse `//test:acceptance` whose result the CI main-push lane uploads
to the shared cache, so only the clauses that carry over are ported, plus the
two the upload adds:

* `tag-cache-suppressed` — no test in `//...` carries a tag that stops its
  result being shared, unless `ALLOWED_TAGS` names that label and tag (empty:
  the owner's bar is a fully cached second CI run). The tag set is the one
  the pinned binary knows (9.2.0's `ExecutionRequirements` / `TargetUtils`
  strings): `external` (no reuse at all), `local` and `no-remote` (no remote
  cache), `no-cache`, `no-remote-cache`, `no-remote-cache-upload`.
  `no-remote-exec` is not one — it forbids remote execution, not caching.
* `accept-undeclared` — `//test:acceptance`'s input closure still holds the
  binary under test, the pinned ocx, and both compose files with the Sigstore
  tree. ocx's `tag-acceptance-undeclared`, for this target's inputs.
* `env-inherit` — an inherited value is the test's spawn environment, so it
  keys the result: a host-specific value splits CI's entries from a
  developer's, and one that picks a verdict has no place in a shared key.
  Only `//test:acceptance` may inherit, and only `INHERIT_ALLOWED`
  (test/BUILD.bazel says why each cannot pick a verdict).
* `rc-volatile` — the checked-in `.bazelrc` passes no ambient value into an
  action key: no `--action_env`, `--test_env`, `--stamp` or
  `--workspace_status_command` (ocx ADR ruling 3).

The judge is pure (`findings()` over captured query output), so the
self-test proves every clause red and green without a Bazel server.
"""

from __future__ import annotations

import argparse
import json
import re
import shlex
import subprocess
from pathlib import Path

from _gate import Finding, codes, expect, report

REPO_ROOT = Path(__file__).resolve().parent.parent
BAZELRC = REPO_ROOT / ".bazelrc"

ACCEPTANCE = "//test:acceptance"

#: Tags that stop a test result being shared through the remote cache.
CACHE_TAGS = frozenset({"external", "local", "no-cache", "no-remote", "no-remote-cache", "no-remote-cache-upload"})

#: `{label: {tag, ...}}` a target may carry despite `CACHE_TAGS`. Empty by the
#: owner's bar; an entry here is a test CI re-runs on every push.
ALLOWED_TAGS: dict[str, frozenset[str]] = {}

#: What `//test:acceptance`'s closure must hold for its cached verdict to move
#: with the thing it judged.
ACCEPTANCE_INPUTS = frozenset({
    "//:ocx-mirror",
    "//:ocx.lock",
    "//test:docker-compose.yml",
    "@ocx_test//:docker-compose.yml",
    "@ocx_test//:sigstore",
    "@tools//:ocx",
})

#: The only inherited environment, and only on `//test:acceptance`.
INHERIT_ALLOWED = frozenset({"DOCKER_CONFIG", "DOCKER_HOST", "HOME", "USER", "XDG_RUNTIME_DIR"})

#: `.bazelrc` flags that feed an ambient value into an action key.
RC_VOLATILE = re.compile(r"(^|\s)--(action_env|test_env|host_action_env|stamp|workspace_status_command)\b")


def string_list(rule: dict, name: str) -> list[str]:
    for attribute in rule.get("attribute", []):
        if attribute.get("name") == name:
            return list(attribute.get("stringListValue", []))
    return []


def read_tests(jsonl: str) -> dict[str, dict]:
    """`bazel query 'tests(//...)' --output=streamed_jsonproto` -> `{label: rule}`."""
    tests = {}
    for line in jsonl.splitlines():
        if line.strip():
            record = json.loads(line)
            if record.get("type") == "RULE":
                tests[record["rule"]["name"]] = record["rule"]
    return tests


def findings(tests: dict[str, dict], closure: set[str], bazelrc: str) -> list[Finding]:
    out: list[Finding] = []
    if ACCEPTANCE not in tests:
        out.append(Finding("accept-missing", f"bazel tag guard: {ACCEPTANCE} is not a test in //... any more"))
    for label, rule in sorted(tests.items()):
        suppressed = set(string_list(rule, "tags")) & CACHE_TAGS - ALLOWED_TAGS.get(label, frozenset())
        if suppressed:
            out.append(Finding(
                "tag-cache-suppressed",
                f"bazel tag guard: {label} carries {sorted(suppressed)} — its result would never be shared, "
                "so a second CI run re-executes it; declare the inputs instead (or name it in ALLOWED_TAGS, "
                "with the reason, and accept an uncached test)",
            ))
        allowed = INHERIT_ALLOWED if label == ACCEPTANCE else frozenset()
        extra = set(string_list(rule, "env_inherit")) - allowed
        if extra:
            out.append(Finding(
                "env-inherit",
                f"bazel tag guard: {label} inherits {sorted(extra)} — env_inherit values are outside the action "
                "key, so a verdict recorded under one value is served under another; pin the value in the "
                "runner or declare it as data",
            ))
    missing = ACCEPTANCE_INPUTS - closure
    if missing:
        out.append(Finding(
            "accept-undeclared",
            f"bazel tag guard: {ACCEPTANCE}'s input closure lacks {sorted(missing)} — a change there would be "
            "served a cached verdict about the old one",
        ))
    for number, line in enumerate(bazelrc.splitlines(), 1):
        if not line.lstrip().startswith("#") and RC_VOLATILE.search(line):
            out.append(Finding(
                "rc-volatile",
                f"bazel tag guard: .bazelrc:{number} `{line.strip()}` feeds an ambient value into action keys",
            ))
    return out


def query(bazel: list[str], expr: str, output: str) -> str:
    result = subprocess.run(
        [*bazel, "query", expr, f"--output={output}"], capture_output=True, text=True, encoding="utf-8", check=False
    )
    if result.returncode != 0:
        raise SystemExit(f"bazel tag guard: `bazel query {expr}` exited {result.returncode}:\n{result.stderr[-2000:]}")
    return result.stdout


def run(bazel: list[str]) -> int:
    tests = read_tests(query(bazel, "tests(//...)", "streamed_jsonproto"))
    closure = set(query(bazel, f"deps({ACCEPTANCE})", "label").split())
    found = findings(tests, closure, BAZELRC.read_text(encoding="utf-8"))
    if not found:
        print(
            f"bazel tag guard: {len(tests)} test target(s), none cache-suppressed or inheriting outside "
            f"{ACCEPTANCE}'s list; its closure holds all {len(ACCEPTANCE_INPUTS)} named inputs; .bazelrc clean"
        )
    return report(found)


# ---------------------------------------------------------------------------
# Self-test
# ---------------------------------------------------------------------------


def _rule(label: str, tags: tuple[str, ...] | list[str] = (), env_inherit: tuple[str, ...] | list[str] = ()) -> str:
    attributes = [{"name": "tags", "stringListValue": list(tags)}]
    if env_inherit:
        attributes.append({"name": "env_inherit", "stringListValue": list(env_inherit)})
    return json.dumps({"type": "RULE", "rule": {"name": label, "ruleClass": "x_test", "attribute": attributes}})


def self_test() -> int:
    unit = "//crates/a:a_test"
    good = "\n".join([
        _rule(unit),
        _rule(ACCEPTANCE, ["exclusive", "no-sandbox"], sorted(INHERIT_ALLOWED)),
    ])
    closure = set(ACCEPTANCE_INPUTS) | {"//test:conftest.py"}
    rc = "build --disk_cache=~/.cache/x\n# build --action_env=PATH (a comment)\n"
    expect(not findings(read_tests(good), closure, rc), f"the clean graph must be green: {codes(findings(read_tests(good), closure, rc))}")
    print("GUARD GREEN : no cache tag, acceptance's closure complete, inherit list exact, rc clean")

    def red(label: str, tests: str, want: str, closure: set[str] = closure, rc: str = rc) -> None:
        got = codes(findings(read_tests(tests), closure, rc))
        expect(got == [want], f"{label}: want [{want!r}], got {got}")
        print(f"GUARD RED   : {label} -> {want}")

    for tag in sorted(CACHE_TAGS):
        red(f"a unit test tagged {tag}", good + "\n" + _rule("//:t", [tag]), "tag-cache-suppressed")
    red("acceptance tagged no-remote-cache",
        "\n".join([_rule(unit), _rule(ACCEPTANCE, ["no-remote-cache"], sorted(INHERIT_ALLOWED))]), "tag-cache-suppressed")
    expect(not findings(read_tests(good + "\n" + _rule("//:t", ["no-remote-exec", "exclusive"])), closure, rc),
           "no-remote-exec forbids remote execution only and must stay green")
    ALLOWED_TAGS["//:t"] = frozenset({"external"})
    try:
        expect(not findings(read_tests(good + "\n" + _rule("//:t", ["external"])), closure, rc),
               "an ALLOWED_TAGS entry must green exactly its tag")
        red("an allow-listed label with another tag", good + "\n" + _rule("//:t", ["external", "local"]),
            "tag-cache-suppressed")
    finally:
        del ALLOWED_TAGS["//:t"]
    print("GUARD GREEN : no-remote-exec, and an allow-listed label/tag pair")

    for dropped in sorted(ACCEPTANCE_INPUTS):
        red(f"acceptance closure without {dropped}", good, "accept-undeclared", closure=closure - {dropped})
    red("acceptance gone", _rule(unit), "accept-missing")
    red("acceptance inherits CI",
        "\n".join([_rule(unit), _rule(ACCEPTANCE, [], [*sorted(INHERIT_ALLOWED), "CI"])]), "env-inherit")
    red("a unit test inherits HOME", good + "\n" + _rule("//:t", [], ["HOME"]), "env-inherit")
    for flag in ("--action_env=PATH", "--test_env=CI", "--stamp", "--workspace_status_command=tools/status.sh"):
        red(f".bazelrc `build {flag}`", good, "rc-volatile", rc=rc + f"build {flag}\n")
    expect(not findings(read_tests(good), closure, rc + "build --nostamp\n"), "--nostamp must stay green")
    print("bazel tag guard self-test: every clause shown red, the clean graph shown green")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--self-test", action="store_true")
    mode.add_argument("--bazel", help="the Bazel command, shell-quoted")
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    return run(shlex.split(args.bazel))


if __name__ == "__main__":
    raise SystemExit(main())
