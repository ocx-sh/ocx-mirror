#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 The OCX Authors
"""Refuse an ocx signer identity a release mode must not trust.

Usage: release_ocx_identity.py MODE IDENTITY

Only `dev` (published to the dev package alone) may run an ocx signed from a branch build. Every other mode,
an unknown one included, needs the exact identity of ocx's main-branch release workflow.
"""

from __future__ import annotations

import sys

MAIN_IDENTITY = "https://github.com/ocx-sh/ocx/.github/workflows/release.yml@refs/heads/main"
MAIN_ONLY_MODES = {"release", "candidate", "backfill"}


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print("usage: release_ocx_identity.py MODE IDENTITY", file=sys.stderr)
        return 2
    mode, identity = argv
    if mode == "dev" or (mode in MAIN_ONLY_MODES and identity == MAIN_IDENTITY):
        return 0
    print(
        f"::error::mode '{mode}' needs an ocx signed from ocx main ({MAIN_IDENTITY}), not '{identity}'; "
        "pin a released ocx built from ocx main and swap ocx-identity.env with it"
    )
    return 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
