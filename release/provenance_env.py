#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 The OCX Authors
"""The `release_provenance` action (release/provenance.bzl): one rustc env file.

    provenance_env.py <stable-status.txt> <out.env> <triple> <rustc> <debug>

Every `STABLE_OCX_MIRROR_<VAR> <value>` line of the workspace status becomes
`<VAR>=<value>`, in file order; every other line (Bazel's own BUILD_* keys) is
dropped. Then the three values the target's rust toolchain decides:
`VERGEN_CARGO_TARGET_TRIPLE`, `VERGEN_RUSTC_SEMVER`, `VERGEN_CARGO_DEBUG`.

Python, not a shell action: this runs on the darwin and windows release hosts
too, through the hermetic interpreter rules_python already fetches, so no host
shell or sed is an input. Written with `\n` line ends on every host — a
Windows text-mode `\r\n` would put a `\r` at the end of every value.
"""

from __future__ import annotations

import re
import sys

LINE = re.compile(r"STABLE_OCX_MIRROR_([A-Za-z0-9_]*) (.*)")


def main(status: str, out: str, triple: str, rustc: str, debug: str) -> int:
    # Split on `\n` alone, as sed does; a `\r` before it is a line end too.
    with open(status, encoding="utf-8", errors="surrogateescape", newline="") as f:
        lines = [line.removesuffix("\r") for line in f.read().split("\n")]
    env = [f"{m[1]}={m[2]}" for m in map(LINE.fullmatch, lines) if m]
    env += [
        f"VERGEN_CARGO_TARGET_TRIPLE={triple}",
        f"VERGEN_RUSTC_SEMVER={rustc}",
        f"VERGEN_CARGO_DEBUG={debug}",
    ]
    with open(out, "w", encoding="utf-8", errors="surrogateescape", newline="\n") as f:
        f.write("".join(line + "\n" for line in env))
    return 0


if __name__ == "__main__":
    sys.exit(main(*sys.argv[1:]))
