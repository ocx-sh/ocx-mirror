# Handover — feature request: `containers[].setup` for runtime deps the base image lacks

**Status:** PROPOSAL, unimplemented. Nothing built or changed.
**Date:** 2026-08-02.
**Origin:** the 40-repo `ocx-contrib` fleet migration. One package cannot be
published at all and three others ship reduced platform coverage, all for the
same missing capability.
**Ask:** a per-container `setup` command list, run before the test in that leg.

## The gap

`os.features` can state a **libc family and nothing else**. `ContainerConfig`
(`src/spec/platforms_config.rs`) accepts `image`, `shell`, `id` — **no way to
provision the image**. So a package whose artifact needs any shared library the
base image does not carry has nowhere to say so, and its container leg fails for
a reason unrelated to the claim the leg exists to test.

Four packages hit this in one migration:

| Package | Needs | Consequence today |
|---|---|---|
| `pnpm/pnpm` | `libatomic.so.1` (glibc), `libgcc_s` (musl) | **Cannot publish. All six Linux legs red.** |
| `oven-sh/bun` | `libstdc++.so.6` (musl builds) | musl platforms **withheld**; glibc only |
| `anomalyco/opencode` | `libstdc++.so.6` (musl builds) | musl platforms **withheld**; glibc only |
| `powershell/powershell` | libicu | forced onto vendor images (`mcr.microsoft.com/dotnet/runtime-deps`) |

Verbatim failures from `ocx-contrib/mirror-pnpm`, one run, both leg types:

```
# glibc legs (ubuntu:24.04, fedora:40)
pnpm: error while loading shared libraries: libatomic.so.1:
      cannot open shared object file: No such file or directory

# musl leg (alpine:3.20)
Error relocating .../content/pnpm: __addtf3: symbol not found
Error relocating .../content/pnpm: __divti3: symbol not found
Error relocating .../content/pnpm: __eqtf2:  symbol not found
```

(The musl symbols are libgcc compiler-runtime helpers — 128-bit integer and
soft-float — which musl does not provide.)

## Proposal

Add `setup` to `ContainerConfig` as an **ordered list of shell commands**:

```yaml
platforms:
  "linux/amd64+libc.musl":
    runner: ubuntu-latest
    containers:
      - image: "alpine:3.20"
        shell: sh
        setup:
          - apk add --no-cache libstdc++
  "linux/amd64+libc.glibc":
    runner: ubuntu-latest
    containers:
      - image: "ubuntu:24.04"
        shell: bash
        setup:
          - apt-get update
          - apt-get install -y libatomic1
      - image: "fedora:40"
        shell: bash
        setup:
          - dnf install -y libatomic
```

Absent `setup` ⇒ today's behaviour exactly. Reuse across legs comes free from
YAML anchors, which the fleet already uses via `mirror-base.yml`.

## Implementation note — this needs `docker build`, not a longer `docker run`

The generated workflow currently wraps **every** `ocx package test` in its own
`docker run --rm`, with the static `ocx` bind-mounted:

```bash
docker run --rm -i --platform "${{ matrix.docker_platform }}" \
  -v "${GITHUB_WORKSPACE}:${GITHUB_WORKSPACE}" -w "${GITHUB_WORKSPACE}" \
  -v "${OCX_CONTAINER_BIN}:/usr/local/bin/ocx:ro" \
  -v /etc/ssl/certs/ca-certificates.crt:/etc/ssl/certs/ca-certificates.crt:ro \
  -e OCX_HOME=/tmp/ocx-home -e OCX_NO_UPDATE_CHECK=1 \
  "${CONTAINER_IMAGE}" ocx "$@"
```

Because each invocation is a fresh throwaway container, prepending the setup
commands inside that `docker run` would re-run them **per test, per version** —
an `apt-get update` on every leg of a 50-version backfill. Correct but wasteful.

**Do it as a build step instead:** when a leg declares `setup`, emit one
`docker build` per leg producing a locally-tagged image from the base plus the
setup commands as `RUN` layers, then point `CONTAINER_IMAGE` at that tag. Docker's
layer cache then makes it once per leg rather than once per test. Roughly:

```
FROM <image>
RUN <setup[0]>
RUN <setup[1]>
```

Emit each command under the leg's `shell`, and fail the leg if any exits
non-zero — a silently-skipped install produces a test failure that reads as an
artifact defect, which is the confusion this feature exists to remove.

### Stale doc comment, worth fixing in the same change

`src/spec/platforms_config.rs` currently says:

> In container mode the OCX binary is injected via a per-leg ephemeral
> Dockerfile `ADD` before each test leg runs.

That is not what the renderer emits — there is no `docker build` and no
Dockerfile anywhere in the generated workflows; `ocx` arrives by bind mount
(`-v "${OCX_CONTAINER_BIN}:/usr/local/bin/ocx:ro"`). Either the comment predates
a rewrite or describes an intended design that was never built. Implementing this
proposal would make the comment true again, so correct or delete it deliberately
rather than leaving both readings in the tree.

## Alternatives considered and rejected

### 1. A bespoke "test image" repository — rejected, and this is the important one

Build `ocx-contrib/test-images` publishing e.g. `ocx-test-glibc` with libatomic,
libstdc++ and libicu preinstalled, and point the legs at it.

**This converts a true red into a false green.** The container matrix exists so
that "the artifact runs on a real host" is *evidence*; `docs/reference/mirror-yml.md`
puts it as "an artifact that links glibc reds its Alpine leg instead of shipping a
false claim". A kitchen-sink image makes `pnpm` pass against a host **no consumer
has**, and it then fails on the user's stock machine exactly as the currently
published `ocx.sh/pnpm` already does (reproduced on Fedora, `ubuntu:24.04` and
`fedora:40`). The images' value comes precisely from being unremarkable.

Secondary cost: multi-arch builds (amd64 + arm64), a publish pipeline, CVE
tracking and base bumps — and it inserts a new supply-chain dependency in front
of all 40 mirrors' CI.

**Not the same thing:** `powershell/powershell` testing against
`mcr.microsoft.com/dotnet/runtime-deps` is legitimate — that is the *vendor's own
documented runtime base*, not a bespoke image built to make a red go away.

### 2. Declarative `packages: ["libatomic1"]` — rejected

Requires ocx-mirror to infer the package manager from the image *and* solve the
naming problem, which is not inferable:

| Need | Alpine | Debian/Ubuntu | Fedora |
|---|---|---|---|
| libatomic | `libatomic` | `libatomic1` | `libatomic` |
| libstdc++ | `libstdc++` | `libstdc++6` | `libstdc++` |

`packages: ["libatomic1"]` would be silently wrong on Fedora. Staying declarative
forces a per-distro map in the spec anyway — more machinery, less clarity, and a
name table nobody wants to maintain. `setup` removes the problem instead of
solving it: whoever chose the image already knows its package names.

(Precedent cuts the same way — `infer_libc_from_image` in `src/spec.rs` is
deliberately prefix inference with a `ponytail:` comment saying to add an explicit
field when the corpus outgrows it. Inference works there because there are two
answers; package naming has one per distro per library.)

### 3. `setup_script: path/to/setup.sh` — rejected for now

The repo already carries **two conflicting path conventions** — `metadata` and
`catalog` resolve from the spec's directory, `tests[].script` from the repo root
— and that is a documented footgun. A third path field is a third chance to get
it wrong, and the file would additionally have to be plumbed *into* the container.
Shellcheck-ability is the one genuine argument for it and is weak at this size
(one to three lines per leg).

`setup` as a list does not block adding `setup_script` later, nor a declarative
`packages` beside it.

## Deliberate scope limit

`setup` is for **provisioning the image so the artifact can load** — not a general
pre-test hook. A leg's worth is that it makes a narrow, honest claim: *this
artifact runs on stock image X plus these named packages*. A general script hook
invites fetching fixtures, starting daemons and seeding services, after which a
green leg stops being a clean statement about the artifact.

The list form is self-limiting by design: growth past a handful of lines is a
visible signal the leg is doing too much, and it is visible precisely because it
sits next to the image it provisions.

## Acceptance

- `setup` absent ⇒ byte-identical generated workflow to today (the whole fleet
  must regenerate clean).
- A leg with `setup` builds its image once and reuses it across every version in
  that leg.
- A failing setup command reds the leg with a message naming the setup step, not
  a downstream test failure.
- `ocx-mirror package validate` rejects `setup` on a platform with no
  `containers` (native legs have nothing to provision), consistent with how
  `containers` is already rejected on `darwin/*` and `windows/*`.
- Unblocks `pnpm/pnpm`, and lets the withheld `+libc.musl` platforms return for
  `oven-sh/bun` and `anomalyco/opencode`.

## What this does NOT fix

`os.features` still cannot express "needs libatomic", so a **consumer** installing
`pnpm/pnpm` is still never told. `setup` makes the requirement visible to
maintainers and testable in CI; it does not make resolution fail loudly on a host
missing the library. That remains a separate, larger question for ocx itself.
