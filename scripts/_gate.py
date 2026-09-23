# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 The OCX Authors
"""Finding/report/expect shared by the `scripts/bazel_*.py` gates.

Mirror of the helper block ocx keeps in `scripts/bazel_gate_proofs.py`. Not an
entry point: every gate is run as `python3 scripts/<gate>.py`, which puts this
directory on `sys.path[0]`, so a plain `from _gate import ...` resolves.
"""

from __future__ import annotations

import dataclasses
import sys
from pathlib import Path


@dataclasses.dataclass(frozen=True)
class Finding:
    """One exit-1 reason. `code` is what tests assert on; `message` is stderr.

    Separate on purpose: asserting a substring of an English sentence is one of
    the cheapest ways to write an assertion that also matches the opposite
    outcome.
    """

    code: str
    message: str


def codes(findings: list[Finding]) -> list[str]:
    return sorted({finding.code for finding in findings})


def report(findings: list[Finding]) -> int:
    """Exit 0 and silent, or exit 1 with one line per finding."""
    # stdout is block-buffered when piped and stderr is not, so without this
    # the findings land above the self-test lines that introduce them.
    sys.stdout.flush()
    for finding in findings:
        print(finding.message, file=sys.stderr)
    return 1 if findings else 0


def expect(condition: bool, problem: str) -> None:
    """A loud exit — a bare `assert` vanishes under `python3 -O`."""
    if not condition:
        raise SystemExit(f"{Path(sys.argv[0]).stem} self-test: {problem}")
