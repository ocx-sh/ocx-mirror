#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 The OCX Authors
"""Action keys out of a Bazel compact execution log.

    scripts/bazel_execlog_keys.py --log <execlog.zst> --out <keys.tsv>
    scripts/bazel_execlog_keys.py --self-test

Only the `main` push writes the shared cache, so a branch run can never show a
cache hit. What it can show is that two runs on one tree asked the cache for
the same keys: `verify.yml` writes `--execution_log_compact_file` on each CI
Bazel test invocation, and this turns each log into one sorted
`<label>\\t<mnemonic>\\t<hash>/<size>` line per spawn. `diff` of two runs' files
is the comparison; an empty diff is two runs with identical action keys.

The log is a zstd stream of varint-length-delimited `ExecLogEntry` protos
(src/main/protobuf/spawn.proto at Bazel 9.2.0). Only `Spawn` entries (field 7)
are read, and of them only `target_label` (7), `mnemonic` (8), `cache_hit`
(12) and `digest` (16) — the action digest, which Bazel records only while a
remote cache is configured. Cache hits go to stdout as a count, never into the
file, so a warm run's file equals a cold run's.

Why a derived file and not the raw log alone: the raw log is protobuf nobody
can diff without Bazel's converter (not shipped in the Bazel binary). The raw
log is uploaded beside it for the inputs, once a key does differ.

No protobuf dependency: four fields of one message is a wire-format walk.
"""

from __future__ import annotations

import argparse
import subprocess
import sys
from collections.abc import Iterator
from pathlib import Path

from _gate import expect

SPAWN, ARGS, LABEL, MNEMONIC, CACHE_HIT, DIGEST = 7, 1, 7, 8, 12, 16
# The follow-up spawn a freshly *executed* test gets: it turns that run's
# test.log into test.xml, so its key carries the log's timings and differs on
# every execution. A cached test never spawns it — it is not a lookup the
# second run makes — so it is left out of the key file and counted instead.
XML_GENERATOR = "tools/test/generate-xml.sh"
HASH, SIZE = 1, 2


def _varint(buf: bytes, i: int) -> tuple[int, int]:
    shift = value = 0
    while True:
        if i >= len(buf):
            raise ValueError("truncated varint")
        byte = buf[i]
        i += 1
        value |= (byte & 0x7F) << shift
        if byte < 0x80:
            return value, i
        shift += 7


def _fields(buf: bytes) -> Iterator[tuple[int, int | bytes]]:
    """`(field number, value)` of one message; unknown fields are skipped, not refused."""
    i = 0
    while i < len(buf):
        key, i = _varint(buf, i)
        wire = key & 7
        if wire == 0:
            value, i = _varint(buf, i)
        elif wire == 2:
            size, i = _varint(buf, i)
            value, i = buf[i : i + size], i + size
        elif wire in (1, 5):
            width = 8 if wire == 1 else 4
            value, i = buf[i : i + width], i + width
        else:
            raise ValueError(f"unsupported wire type {wire}")
        if i > len(buf):
            raise ValueError("truncated field")
        yield key >> 3, value


def spawns(stream: bytes) -> Iterator[dict[int, int | bytes]]:
    i = 0
    while i < len(stream):
        size, i = _varint(stream, i)
        entry, i = stream[i : i + size], i + size
        if i > len(stream):
            raise ValueError("truncated entry")
        for number, value in _fields(entry):
            if number == SPAWN and isinstance(value, bytes):
                # First occurrence wins: `args` is repeated, and argv[0] is the
                # one that names the program.
                spawn: dict[int, int | bytes] = {}
                for field, item in _fields(value):
                    spawn.setdefault(field, item)
                yield spawn


def keys(stream: bytes) -> tuple[list[str], int, int, int]:
    """(sorted key lines, spawns carrying no digest, cache hits, test.xml generators left out)."""
    lines, undigested, hits, generators = [], 0, 0, 0
    for spawn in spawns(stream):
        if bytes(spawn.get(ARGS, b"")).decode().endswith(XML_GENERATOR):
            generators += 1
            continue
        label = bytes(spawn.get(LABEL, b"")).decode() or "-"
        mnemonic = bytes(spawn.get(MNEMONIC, b"")).decode() or "-"
        digest = dict(_fields(bytes(spawn[DIGEST]))) if DIGEST in spawn else {}
        if HASH in digest:
            key = f"{bytes(digest[HASH]).decode()}/{digest.get(SIZE, 0)}"
        else:
            key, undigested = "-", undigested + 1
        hits += 1 if spawn.get(CACHE_HIT) else 0
        lines.append(f"{label}\t{mnemonic}\t{key}")
    return sorted(lines), undigested, hits, generators


def run(log: Path, out: Path) -> int:
    stream = subprocess.run(["zstd", "-dc", str(log)], capture_output=True, check=True).stdout
    lines, undigested, hits, generators = keys(stream)
    out.write_text("".join(f"{line}\n" for line in lines))
    print(
        f"bazel execlog: {len(lines)} spawn(s), {hits} cache hit(s), {undigested} without an action digest, "
        f"{generators} test.xml generator(s) left out -> {out}"
    )
    if lines and undigested == len(lines):
        # Bazel records the digest only with a remote cache configured; a file of
        # `-` keys would diff equal for any two trees.
        print("bazel execlog: no spawn carries an action digest - no remote cache was configured", file=sys.stderr)
        return 1
    return 0


def _enc_varint(value: int) -> bytes:
    out = bytearray()
    while True:
        out.append((value & 0x7F) | (0x80 if value > 0x7F else 0))
        value >>= 7
        if not value:
            return bytes(out)


def _enc(number: int, value: int | bytes | str) -> bytes:
    if isinstance(value, int):
        return _enc_varint(number << 3) + _enc_varint(value)
    data = value.encode() if isinstance(value, str) else value
    return _enc_varint(number << 3 | 2) + _enc_varint(len(data)) + data


def _entry(*fields: bytes) -> bytes:
    body = b"".join(fields)
    return _enc_varint(len(body)) + body


def self_test() -> int:
    digest = _enc(HASH, "ab" * 32) + _enc(SIZE, 142)
    stream = (
        _entry(_enc(1, 1), _enc(2, _enc(1, "SHA-256")))  # Invocation: skipped
        + _entry(_enc(1, 2), _enc(3, _enc(1, "bazel-out/f")))  # File: skipped
        + _entry(_enc(SPAWN, _enc(LABEL, "//z:t") + _enc(MNEMONIC, "Rustc") + _enc(CACHE_HIT, 1) + _enc(DIGEST, digest)))
        + _entry(_enc(SPAWN, _enc(LABEL, "//a:t") + _enc(MNEMONIC, "TestRunner") + _enc(DIGEST, digest) + _enc(99, 7)))
        + _entry(
            _enc(
                SPAWN,
                _enc(ARGS, "external/bazel_tools/tools/test/generate-xml.sh")
                + _enc(LABEL, "//a:t")
                + _enc(MNEMONIC, "TestRunner")
                + _enc(DIGEST, _enc(HASH, "cd" * 32)),
            )
        )
    )
    lines, undigested, hits, generators = keys(stream)
    expect(lines == [f"//a:t\tTestRunner\t{'ab' * 32}/142", f"//z:t\tRustc\t{'ab' * 32}/142"], f"green: got {lines}")
    expect((undigested, hits, generators) == (0, 1, 1), f"green: undigested/hits/generators {undigested}/{hits}/{generators}")
    print("GREEN: two spawns, sorted, one hit counted, the test.xml generator and non-spawn entries left out")

    lines, undigested, _, _ = keys(_entry(_enc(SPAWN, _enc(LABEL, "//a:t"))))
    expect(lines == ["//a:t\t-\t-"] and undigested == 1, f"red: an undigested spawn must read as `-`, got {lines}")
    print("RED  : a spawn without an action digest is counted, not dropped")

    try:
        keys(stream[:-3])
    except ValueError:
        print("RED  : a truncated log raises instead of yielding a short key set")
    else:
        expect(False, "red: a truncated log must raise")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--log", type=Path)
    parser.add_argument("--out", type=Path)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    if not (args.log and args.out):
        parser.error("--log and --out are required")
    return run(args.log, args.out)


if __name__ == "__main__":
    sys.exit(main())
