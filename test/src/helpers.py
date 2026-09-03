"""Docker-compose registry helpers for the mirror acceptance-test suite."""
from __future__ import annotations

import hashlib
import http.client
import io
import json
import os
import shutil
import subprocess
import sys
import tarfile
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from typing import NamedTuple

# ---------------------------------------------------------------------------
# Paths
# ---------------------------------------------------------------------------

PROJECT_ROOT = Path(__file__).resolve().parent.parent.parent
COMPOSE_FILE = Path(__file__).resolve().parent.parent / "docker-compose.yml"


def main_checkout_root() -> Path:
    """Resolve the main repo checkout, even when running inside a worktree.

    ``PROJECT_ROOT`` is derived from ``__file__``, so inside an agent worktree
    (``.agents/worktrees/<name>/``) it resolves to the worktree root, not the
    main checkout -- and a sibling-repo lookup (`DEFAULT_SIGSTORE_COMPOSE`,
    the trusted-root path) built on top of it silently points one level too
    shallow (``.agents/worktrees/ocx/...`` instead of ``../ocx/...``).
    ``git rev-parse --git-common-dir`` resolves a worktree to the *main*
    repo's `.git` directory regardless of how deep the worktree sits; its
    parent is the main checkout. Falls back to ``PROJECT_ROOT`` when git
    itself is unavailable (e.g. a source tarball with no `.git` at all).
    """
    try:
        result = subprocess.run(
            ["git", "rev-parse", "--path-format=absolute", "--git-common-dir"],
            cwd=PROJECT_ROOT,
            capture_output=True,
            text=True,
            check=True,
        )
    except (OSError, subprocess.CalledProcessError):
        return PROJECT_ROOT
    return Path(result.stdout.strip()).parent

# ---------------------------------------------------------------------------
# Docker-compose helpers
# ---------------------------------------------------------------------------


#: Host address the `zot` compose service publishes (test/docker-compose.yml).
DEFAULT_ZOT_REGISTRY = "localhost:5011"


def zot_registry_address() -> str:
    """The zot registry's host address, read from ``ZOT_REGISTRY`` at call time.

    One seat, deliberately (C-073, amended 2026-09-02): both the
    ``zot_registry`` fixture and ``MirrorRunner.env``'s
    ``OCX_INSECURE_REGISTRIES`` need this address, and two independent reads
    let a machine that moves zot off 5011 get a runner still declaring 5011
    insecure -- the push then fails on TLS against an address nothing serves.
    Read per call rather than bound at import so a test can override it.
    """
    return os.environ.get("ZOT_REGISTRY", DEFAULT_ZOT_REGISTRY)


def registry_is_reachable(registry: str) -> bool:
    """Return True if the registry responds to ``GET /v2/``."""
    try:
        urllib.request.urlopen(f"http://{registry}/v2/", timeout=2)
        return True
    except (urllib.error.URLError, OSError):
        return False


def start_registry(registry: str, service: str = "registry") -> None:
    """Start one compose service if the registry it serves is not already up.

    ``service`` is named rather than defaulted to the whole project because
    the two registries can be up independently. GitHub Actions supplies the
    source registry as a job *service container* bound to host 5001, so a
    bare ``docker compose up -d`` — which starts every service — fails to
    bind that port and exits 1, taking the whole session down before a test
    runs. Bringing up only the service that owns the address asked for works
    whether the other one is already bound, bound by something else, or not
    running at all.
    """
    if registry_is_reachable(registry):
        return

    result = subprocess.run(
        ["docker", "compose", "-f", str(COMPOSE_FILE), "up", "-d", service],
        capture_output=True,
        text=True,
    )
    # Surfaced rather than left to `check=True`: a bare CalledProcessError
    # reports the argv and swallows the stderr that says *why*, which is the
    # only part that identifies a port clash.
    if result.returncode != 0:
        raise RuntimeError(
            f"docker compose up -d {service} failed with exit {result.returncode}\n"
            f"stdout: {result.stdout.strip()}\nstderr: {result.stderr.strip()}"
        )

    # Wait for the registry to become reachable (up to 15 s).
    for _ in range(30):
        if registry_is_reachable(registry):
            return
        time.sleep(0.5)

    raise RuntimeError(f"Registry at {registry} did not become reachable")


# ---------------------------------------------------------------------------
# Real-`ocx` package push helper (W3: pypi/mount acceptance suites)
# ---------------------------------------------------------------------------


def push_stub_ocx_package(
    ocx_binary: Path,
    registry: str,
    ref: str,
    work_dir: Path,
    *,
    content: bytes = b"stub",
) -> None:
    """Pushes a minimal one-layer Bundle package to ``{registry}/{ref}`` via
    the real ``ocx`` binary.

    ``content`` is the layer's only file. It is a parameter because the layer
    is otherwise byte-identical between calls, so two versions of the same
    package would land on one manifest digest — and a cascade scenario
    (S-009) needs `1.2` and `latest` to resolve to *different* digests for
    the assertion to be able to fail.

    Used to stand in for a private interpreter package: `ocx-mirror`'s
    in-process interpreter-digest resolution (``fetch_manifest_digest``)
    talks to the registry directly (not via a subprocess), so it needs a
    real manifest to resolve — unlike `materialize_interpreter`'s own
    `OCX_BINARY_PIN`-stubbed `ocx package pull`, which is a separate,
    file-system-only fake that never touches the registry. Content is a
    throwaway marker file; nothing downstream executes it.
    """
    work_dir.mkdir(parents=True, exist_ok=True)
    metadata_path = work_dir / "stub-metadata.json"
    # No `platform` key: ocx >= 0.5.6 retired it from the sidecar — the
    # platform travels solely on the explicit `-p` flag passed below.
    metadata_path.write_text(json.dumps({"type": "bundle", "version": 1}))

    layer_path = work_dir / "stub-layer.tar.gz"
    with tarfile.open(layer_path, "w:gz") as tar:
        info = tarfile.TarInfo(name="bin/marker")
        info.size = len(content)
        tar.addfile(info, io.BytesIO(content))

    env = {
        "PATH": os.environ.get("PATH", ""),
        "OCX_INSECURE_REGISTRIES": registry,
        "OCX_HOME": str(work_dir / "ocx-home"),
    }
    result = subprocess.run(
        [
            str(ocx_binary),
            "--format",
            "json",
            "package",
            "push",
            "-p",
            "linux/amd64",
            "-i",
            f"{registry}/{ref}",
            "-m",
            str(metadata_path),
            str(layer_path),
        ],
        capture_output=True,
        text=True,
        env=env,
    )
    assert result.returncode == 0, f"failed to push stub package {ref} to {registry}: {result.stderr}"


def push_ocx_description(
    ocx_binary: Path,
    registry: str,
    repository: str,
    work_dir: Path,
    *,
    readme: str = "# stub\n",
) -> None:
    """Pushes a `__ocx.desc` description to ``{registry}/{repository}`` via the real ``ocx`` binary.

    The reserved description tag never appears in a root's ``tags{}`` (ocx
    filters reserved tags at render), so it is fetched by name per package —
    which is what S-012 checks travelled.
    """
    work_dir.mkdir(parents=True, exist_ok=True)
    readme_path = work_dir / "README.md"
    readme_path.write_text(readme)

    env = {
        "PATH": os.environ.get("PATH", ""),
        "OCX_INSECURE_REGISTRIES": registry,
        "OCX_HOME": str(work_dir / "ocx-home"),
    }
    result = subprocess.run(
        [
            str(ocx_binary),
            "package",
            "description",
            "push",
            f"{registry}/{repository}",
            "--readme",
            str(readme_path),
        ],
        capture_output=True,
        text=True,
        env=env,
    )
    assert result.returncode == 0, f"failed to describe {repository} on {registry}: {result.stderr}"


def fetch_manifest(registry: str, repository: str, reference: str) -> tuple[str, bytes]:
    """GETs `repository:reference`'s manifest from `registry` over the raw OCI HTTP API.

    Returns the digest the registry claims (`Docker-Content-Digest`) and the
    exact response bytes — the verbatim form a published-shape index tree's
    dispatch object must carry (`src.static_index.write_published_index_tree`).
    """
    request = urllib.request.Request(f"http://{registry}/v2/{repository}/manifests/{reference}")
    request.add_header(
        "Accept",
        "application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json",
    )
    with urllib.request.urlopen(request) as response:
        return response.headers["Docker-Content-Digest"], response.read()


def put_manifest(registry: str, repository: str, reference: str, body: bytes, media_type: str) -> str:
    """PUTs a raw manifest to `repository:reference` on `registry`, bypassing `ocx` entirely.

    The only way to seed a descriptor `ocx`'s own writers never produce — an
    attestation/referrer child with no `platform` key (S-024). There is no
    `oras`/`crane` dependency to reach for instead, and `push_stub_ocx_package`
    goes through the real `ocx` binary, which never omits `platform`.

    A malformed `body` surfaces as `urllib.error.HTTPError` from `urlopen`
    itself (the registry answers 400) — no extra handling needed here, same
    as `fetch_manifest`.

    Returns the digest the registry claims for the pushed content
    (`Docker-Content-Digest`).
    """
    request = urllib.request.Request(
        f"http://{registry}/v2/{repository}/manifests/{reference}",
        data=body,
        method="PUT",
        headers={"Content-Type": media_type},
    )
    with urllib.request.urlopen(request) as response:
        return response.headers["Docker-Content-Digest"]


def put_blob(registry: str, repository: str, payload: bytes) -> tuple[str, int]:
    """Uploads `payload` as a blob of `repository` on `registry`, returning its digest and size.

    The sibling of `put_manifest` for the other half of the OCI write API: a
    two-step monolithic upload (`POST /blobs/uploads/` for a session, then
    `PUT ?digest=` with the bytes). It exists so a seeded referrer can carry
    content its subject does not — the only way to assert what a dry run's
    byte estimate actually measured, rather than that it measured something.

    The registry answers the POST with a `Location` that may be relative;
    `urllib.parse.urljoin` against the request URL resolves both forms.
    """
    digest = f"sha256:{hashlib.sha256(payload).hexdigest()}"
    start = f"http://{registry}/v2/{repository}/blobs/uploads/"
    with urllib.request.urlopen(urllib.request.Request(start, data=b"", method="POST")) as response:
        session = urllib.parse.urljoin(start, response.headers["Location"])

    separator = "&" if "?" in session else "?"
    request = urllib.request.Request(
        f"{session}{separator}digest={digest}",
        data=payload,
        method="PUT",
        headers={"Content-Type": "application/octet-stream"},
    )
    with urllib.request.urlopen(request):
        return digest, len(payload)


# ---------------------------------------------------------------------------
# Sigstore harness (WP 5, C-073) — mirror-signing acceptance tier
# ---------------------------------------------------------------------------
#
# Bring up ocx's OWN `sigstore` compose profile by explicit service name
# (adr_mirror_signing.md D6a Option 2) rather than duplicating its ~180
# lines of compose and committed CA key material, and rather than
# `include:`/`extends`-ing the whole file — the latter merges ocx's own
# registries in too, which collide with this harness's `registry`/
# `mirror_registry` on 5001/5002 and silently point tests at the wrong stack.

class SigstoreStack(NamedTuple):
    """Handle to a running `sigstore` compose profile and the spec rendered against it.

    Fields are appended, never reordered (C-073): consumers index them by
    name, but the tuple form is positional and a reorder is a silent
    argument swap.
    """

    compose_file: Path
    #: Host-side base URLs of the running stack -- what `mirror.yml`'s
    #: `sign.keyless.fulcio`/`.rekor` refs resolve to through
    #: `SIGSTORE_FULCIO_URL`/`SIGSTORE_REKOR_URL`.
    fulcio_url: str
    rekor_url: str
    #: The dex identity token, minted per session, mode 0600.
    token_path: Path
    #: The rendered spec and the verification-side config beside it -- the
    #: two paths a signing run needs (`OCX_CONFIG` is the latter).
    spec_path: Path
    config_path: Path


#: Default location of the sibling ocx checkout's sigstore compose file,
#: resolved relative to the *main checkout* root (adr_mirror_signing.md
#: C-073) -- not `PROJECT_ROOT`, which is worktree-local. Overridable
#: per-machine via OCX_SIGSTORE_COMPOSE.
DEFAULT_SIGSTORE_COMPOSE = main_checkout_root().parent / "ocx" / "test" / "docker-compose.yml"

#: Explicit service names for ocx's `sigstore` compose profile, named rather
#: than "the whole profile" so this harness's own registries are never
#: touched. `trillian-log-signer` MUST be listed explicitly: it appears in no
#: `depends_on`, and without it Rekor accepts entries that are never
#: integrated -- every keyless test then fails in a way that reads as a
#: Fulcio bug (adr_mirror_signing.md D6a).
SIGSTORE_SERVICES: tuple[str, ...] = (
    "dex",
    "sigstore-ct",
    "fulcio",
    "sigstore-mysql",
    "trillian-log-server",
    "trillian-log-signer",
    "rekor",
)

#: Readiness endpoint per service, as a path under that service's host base
#: URL (adr_mirror_signing.md D6). Fulcio is polled on `/api/v2/trustBundle`
#: rather than ocx's own `/api/v1/rootCert`: the trust bundle is what a
#: keyless verify actually consumes, so a Fulcio serving only the legacy
#: route would read as ready and fail at the first verify.
SIGSTORE_READINESS: tuple[tuple[str, str], ...] = (
    ("dex", "/dex/healthz"),
    ("fulcio", "/api/v2/trustBundle"),
    ("rekor", "/api/v1/log"),
)

#: The fixture directory `render_signing_fixture` renders from.
SIGNING_FIXTURE_DIR = Path(__file__).resolve().parent.parent / "fixtures" / "signing"


def sigstore_compose_path() -> Path:
    """Resolve the sibling ocx checkout's compose file, honouring the override.

    Deliberately does *not* check the file exists: the S-059 skip guard is
    the caller's, and its reason has to name the path that was looked at --
    "keyless tier skipped" with no path is indistinguishable from a broken
    default on a machine that does have the sibling clone.
    """
    return Path(os.environ.get("OCX_SIGSTORE_COMPOSE", str(DEFAULT_SIGSTORE_COMPOSE)))


def sigstore_base_urls() -> dict[str, str]:
    """Host-side base URL per polled service, from the ``OCX_TEST_*_PORT`` vars.

    The same variables the sibling ocx compose file parametrises its
    published ports with -- never hardcoded, so a machine running a second
    stack off the defaults still resolves the right ports.
    """
    return {
        "dex": f"http://localhost:{os.environ.get('OCX_TEST_DEX_PORT', '5556')}",
        "fulcio": f"http://localhost:{os.environ.get('OCX_TEST_FULCIO_PORT', '5555')}",
        "rekor": f"http://localhost:{os.environ.get('OCX_TEST_REKOR_PORT', '3000')}",
    }


def _sigstore_pending(base: dict[str, str]) -> dict[str, str]:
    """The readiness URLs that are not answering 2xx right now."""
    pending: dict[str, str] = {}
    for service, path in SIGSTORE_READINESS:
        url = base[service] + path
        try:
            with urllib.request.urlopen(url, timeout=3) as response:
                if 200 <= response.status < 300:
                    continue
        except (urllib.error.URLError, OSError, http.client.HTTPException):
            # HTTPException is not an OSError: a service answering mid-startup
            # can close the socket mid-response, and that escapes the retry
            # loop as a bare BadStatusLine without it.
            pass
        pending[service] = url
    return pending


def sigstore_skip_reason() -> str | None:
    """Why the keyless tier cannot run here, or ``None`` when it can.

    Returned rather than raised so the `sigstore_stack` fixture owns the
    `pytest.skip` and the session hook can silently decline the bring-up:
    a machine with no sibling ocx checkout has nothing to start and nothing
    to fail about. The reason names the path that was looked at and the
    override -- "keyless tier skipped" with no path is indistinguishable
    from a broken default on a machine that does have the clone.
    """
    compose_file = sigstore_compose_path()
    if not compose_file.is_file():
        return (
            f"no Sigstore compose file at {compose_file} -- clone ocx-sh/ocx as a sibling "
            "of this repo, or set OCX_SIGSTORE_COMPOSE, to run the keyless-signing tier"
        )
    trusted_root = sigstore_trusted_root(compose_file)
    if not trusted_root.is_file():
        return (
            f"no Sigstore trusted root at {trusted_root} -- run "
            f"{compose_file.parent / 'sigstore' / 'generate-trusted-root.py'} in the sibling "
            "ocx checkout, or set OCX_SIGSTORE_COMPOSE, to run the keyless-signing tier"
        )
    return None


def sigstore_trusted_root(compose_file: Path) -> Path:
    """The trusted-root bundle the sibling checkout generates, beside its compose file."""
    return compose_file.parent / "sigstore" / "trusted_root.json"


def wait_for_sigstore(compose_file: Path | None = None, *, timeout: float = 180.0) -> None:
    """Blocks until every service in ``SIGSTORE_READINESS`` answers, or raises.

    Readiness is polled from the host, never a compose ``healthcheck:`` --
    four of the seven images the `sigstore` profile brings up are distroless
    (no shell, no curl), so a healthcheck line on them would be a green that
    never ran. Endpoints polled: dex ``/dex/healthz``, Fulcio
    ``/api/v2/trustBundle``, Rekor ``/api/v1/log``
    (adr_mirror_signing.md D6). Host ports are read from
    ``OCX_TEST_DEX_PORT``/``OCX_TEST_FULCIO_PORT``/``OCX_TEST_REKOR_PORT``
    (defaults 5556/5555/3000) -- the same variables the sibling ocx compose
    file parametrises -- never hardcoded, so a machine running a second
    stack on the defaults still resolves the right ports.

    `trillian-log-signer` and `sigstore-ct` are brought up but not polled,
    for parity with the sibling checkout's own `wait-for-stack.py`: neither
    serves a readiness route, and a Rekor integration lag behind the signer
    surfaces as a verify failure, not as a readiness failure.

    ``compose_file`` names the stack in the failure message only, and
    defaults to whatever `sigstore_compose_path()` resolves at call time --
    an import-time default would ignore `OCX_SIGSTORE_COMPOSE` and name the
    wrong file in the hint.

    Raises ``RuntimeError`` naming each endpoint that never answered, plus
    the compose file it came from: "the sigstore stack is not up" is not
    actionable when three services could be the one missing.
    """
    compose_file = compose_file or sigstore_compose_path()
    base = sigstore_base_urls()
    deadline = time.monotonic() + timeout
    while True:
        pending = _sigstore_pending(base)
        if not pending:
            return
        if time.monotonic() >= deadline:
            unready = ", ".join(f"{service} did not answer {url}" for service, url in sorted(pending.items()))
            raise RuntimeError(
                f"the sigstore stack from {compose_file} was not ready after {timeout:.0f}s: {unready}\n"
                f"  docker compose -f {compose_file} logs --tail=40 " + " ".join(sorted(pending))
            )
        time.sleep(1.0)


def mint_identity_token(target: Path) -> Path:
    """Writes a fresh dex OIDC identity token to ``target`` and returns it.

    ``ocx package sign`` has no ``--identity-token <VALUE>`` flag by design --
    a token in argv is world-readable in /proc -- so the token reaches it as
    a file: ``sign.keyless.identity_token: file://<target>``
    (adr_mirror_signing.md D1). The written file must end up ``chmod 0600``:
    ``ocx package sign`` refuses a group- or world-readable token file.

    The token is minted by the sibling checkout's own
    ``test/sigstore/get-token.py`` rather than by a second copy of the
    resource-owner password grant here. That script holds the dex client id,
    secret and test identity the running stack is configured with; a
    duplicate would be the copy that goes stale, and inventing an issuer
    would mint a token Fulcio refuses.
    """
    compose_file = sigstore_compose_path()
    script = compose_file.parent / "sigstore" / "get-token.py"
    if not script.is_file():
        raise RuntimeError(
            f"no dex token minter at {script} -- expected it beside {compose_file}; "
            "set OCX_SIGSTORE_COMPOSE to the sibling ocx checkout's test/docker-compose.yml"
        )
    result = subprocess.run(
        [sys.executable, str(script), "--port", os.environ.get("OCX_TEST_DEX_PORT", "5556")],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise RuntimeError(
            f"{script} failed with exit {result.returncode}\n"
            f"stdout: {result.stdout.strip()}\nstderr: {result.stderr.strip()}"
        )
    token = result.stdout.strip()
    if token.count(".") != 2:
        raise RuntimeError(f"dex did not return a JWT: {token[:80]!r}")

    target.parent.mkdir(parents=True, exist_ok=True)
    # Created at its final mode rather than written-then-chmod: between the
    # two the token would exist at the ambient umask, and a chmod does not
    # revoke a handle already opened. The trailing chmod covers the one case
    # O_CREAT cannot -- a file that already existed, at whatever mode.
    handle = os.open(target, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(handle, "w") as token_file:
        token_file.write(token)
    target.chmod(0o600)
    return target


def render_signing_fixture(dest: Path, trusted_root: Path, token_path: Path) -> Path:
    """Renders `fixtures/signing/` into ``dest`` and returns the `mirror.yml` path.

    The one seat that substitutes ``__TRUSTED_ROOT_PATH__`` (config.toml) and
    ``__IDENTITY_TOKEN_PATH__`` (mirror.yml) -- C-073, amended 2026-09-02.
    Neither path survives being baked in: the trusted root sits in a sibling
    checkout resolved at runtime, and the dex token is minted per session, so
    a spec still carrying either placeholder names a file called
    ``__IDENTITY_TOKEN_PATH__`` and fails a long way from the cause.

    The whole fixture directory is copied, not just the two rendered files:
    `mirror.yml` names `metadata.json` relative to itself, and `config.toml`
    has to sit beside the spec for `OCX_CONFIG` to point at it. Placeholders
    this function does not own -- `__ASSET_PORT__`, substituted per test by
    the materializing fixture -- are copied through untouched.
    """
    shutil.copytree(SIGNING_FIXTURE_DIR, dest, dirs_exist_ok=True)

    # Both substitutions land as absolute paths: `trusted_root` is anchored
    # to the config file's own directory at load time, so a relative one --
    # which a relative OCX_SIGSTORE_COMPOSE produces -- would resolve inside
    # this throwaway spec directory and read as a missing bundle.
    spec = dest / "mirror.yml"
    spec.write_text(spec.read_text().replace("__IDENTITY_TOKEN_PATH__", str(token_path.resolve())))

    config = dest / "config.toml"
    config.write_text(config.read_text().replace("__TRUSTED_ROOT_PATH__", str(trusted_root.resolve())))

    return spec
