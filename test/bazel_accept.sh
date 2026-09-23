#!/usr/bin/env bash
# Runner of //test:acceptance (adr_bazel_crate_split.md § C7). Runs the suite in
# the SOURCE tree against the Bazel-built ocx-mirror, the ocx.lock-pinned ocx
# and the submodule's Sigstore stack — never conftest's fallbacks (test/bin, a
# cargo build of external/ocx, the sibling ../ocx checkout), which are inputs
# this target does not declare. Its result is remote-cached (test/BUILD.bazel),
# so everything that can pick a verdict is pinned here or declared there.
#
# argv: <rlocationpath //:ocx-mirror> <rlocationpath @tools//:ocx>
#       <rlocationpath @tools//:uv> <rlocationpath @ocx_test//:docker-compose.yml>
set -euo pipefail

fail() {
    echo "acceptance sh_test: $*" >&2
    exit 1
}

[ "$#" -eq 4 ] || fail "usage: <ocx-mirror> <ocx> <uv> <sigstore compose> rlocationpaths, got $# argument(s)"
for v in TEST_SRCDIR TEST_WORKSPACE TEST_TMPDIR XML_OUTPUT_FILE HOME; do
    [ -n "${!v:-}" ] || fail "$v is unset - run this through \`bazel test //test:acceptance\`"
done

runfile() {
    local p="$TEST_SRCDIR/$1"
    [ -e "$p" ] || fail "no runfile $1 at $p"
    readlink -f "$p"
}

mirror=$(runfile "$1")
uv=$(runfile "$3")
[ -x "$mirror" ] && [ -x "$uv" ] || fail "the ocx-mirror or uv runfile is not executable ($mirror, $uv)"

# @tools//:ocx is a rules_ocx launcher that exports OCX_HOME=<host store>
# before exec'ing the pinned binary. The suite hands every ocx it spawns a
# per-test OCX_HOME, which the launcher would overwrite — so the suite gets the
# binary the launcher execs, read off its single `exec '<path>' "$@"` line.
launcher=$(runfile "$2")
ocx=$(sed -n "s/^exec '\\([^']*\\)' \"\\\$@\"\$/\\1/p" "$launcher")
[ "$(printf '%s\n' "$ocx" | grep -c .)" = 1 ] && [ -x "$ocx" ] ||
    fail "cannot read the pinned ocx binary off the @tools//:ocx launcher $launcher (expected one exec '<path>' \"\$@\" line) - rules_ocx changed its launcher shape"

# The Sigstore compose file, resolved to the real file: its `./sigstore/...`
# bind mounts resolve beside it, and a runfiles directory of per-file symlinks
# would hand the containers links to host paths they cannot see.
compose=$(runfile "$4")
[ -d "$(dirname "$compose")/sigstore" ] || fail "no sigstore/ beside $compose - @ocx_test is not the external/ocx/test tree"

# The suite resolves its paths through `__file__`, so it runs in the source
# tree: under `no-sandbox` the conftest.py runfile is a symlink into it.
anchor="$TEST_SRCDIR/$TEST_WORKSPACE/test/conftest.py"
[ -e "$anchor" ] || fail "no runfile at $anchor - conftest.py is not in this target's data"
suite=$(dirname "$(readlink -f "$anchor")")
case "$suite" in
    *.runfiles/* | */bazel-out/*) fail "runfiles are copies here, so $suite is not the source tree (the target must stay no-sandbox)" ;;
esac
[ -f "$suite/pyproject.toml" ] || fail "$suite carries no pyproject.toml - wrong tree"

export OCX_MIRROR_COMMAND="$mirror" OCX_COMMAND="$ocx" OCX_TEST_BINARY="$ocx"

# The Sigstore stack as its OWN compose project on its own host ports. The
# harness's default shares the sibling ocx checkout's running stack (project
# `test`, ports 5556/5555/3000/6962/8091); `up -d` from this file under that
# project would recreate those containers from another path under whoever
# runs them. Fixed values, not inherited ones: a port is where the verdict's
# stack lives, and an inherited value is outside the key.
export OCX_SIGSTORE_COMPOSE="$compose" OCX_SIGSTORE_PROJECT=ocx-mirror-sigstore
export OCX_TEST_DEX_PORT=5566 OCX_TEST_FULCIO_PORT=5565 OCX_TEST_REKOR_PORT=3010
export OCX_TEST_CT_PORT=6972 OCX_TEST_TRILLIAN_SIGNER_PORT=8101

# The venv is per run, under TEST_TMPDIR: never in the source tree (test/.venv
# belongs to `task test:parallel`), never shared between checkouts or
# concurrent runs, and uv.lock (a declared input) fully determines it. uv's
# cache stays at its HOME default, so rebuilding it is a hardlink pass.
# `--locked` refuses a stale uv.lock instead of rewriting a declared input.
export UV_PROJECT_ENVIRONMENT="$TEST_TMPDIR/venv"
# The interpreter too: left to discovery, uv takes whatever python3 the host
# has first on PATH (3.14 here, 3.12 on an ubuntu runner) — a verdict input
# no key sees. An exact uv-managed build is a function of this line and the
# ocx.lock-pinned uv; a host without it downloads it once.
export UV_PYTHON=3.13.12 UV_PYTHON_PREFERENCE=only-managed

# pytest's arenas NOT under TEST_TMPDIR: Bazel symlinks the workspace .git into
# the execroot that holds it, and the mirror walks up to the nearest .git
# (SpecSlot) — a tmp_path inside a repository turns renderer tests red. Same
# disk-backed, repo-free root the suite uses outside Bazel.
TMPDIR="$HOME/.cache/ocx-mirror-test-tmp"
mkdir -p "$TMPDIR"
export TMPDIR

# The docker CLI keeps this host's config (compose plugin, context) even though
# HOME moves below.
export DOCKER_CONFIG="${DOCKER_CONFIG:-$HOME/.docker}"

# A fresh HOME per run for pytest and every binary it spawns: a remote-cached
# verdict must not depend on this host's ~/.netrc, ~/.config/ocx, a warm ~/.ocx
# store or ~/.cache. uv itself keeps the real HOME (its cache, its interpreters).
home=$(mktemp -d "$TMPDIR/home.XXXXXXXX")
trap 'chmod -R u+w "$home" 2>/dev/null; rm -rf "$home"' EXIT

cd "$suite"
set -- "$uv" run --locked env HOME="$home" pytest -n auto --junit-xml="$XML_OUTPUT_FILE"

# One acceptance run at a time on this HOST (ocx's runner, test/bazel.bzl):
# `exclusive` serialises one invocation's tests only, and a run from another
# checkout drives the same registries and recreates the Sigstore project from
# its own path. Keyed under HOME, not the checkout, so every checkout meets it.
lock_dir="$HOME/.cache/ocx-mirror"
mkdir -p "$lock_dir"
if command -v flock >/dev/null 2>&1; then
    flock "$lock_dir/acceptance.lock" "$@"
else
    echo "acceptance sh_test: no flock(1) on this host - running unserialised" >&2
    "$@"
fi
