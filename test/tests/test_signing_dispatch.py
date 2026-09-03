# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 The OCX Authors
"""S-052 — signing survives the `ocx mirror …` plugin-dispatch shape.

`ocx` scrubs its bearer credentials from a dispatched plugin's environment
(`crates/ocx_lib/src/env.rs:238` names the set; `crates/ocx_cli/src/app/
plugin_dispatch.rs:192` is where the removal reaches the child), so a mirror
run as a plugin sees `OCX_IDENTITY_TOKEN`, `OCX_KEY_PASSWORD` and
`OCX_SIGNING_KEY` unset no matter what the operator exported.

Every test here drives the *whole* pipeline through `ocx mirror package
pipeline push` and asks the registry what landed — the negative cases too, via
`_publish_unchecked`, because stopping at `pipeline plan` would make their
registry-emptiness assertions vacuous.

The two scrubbed-name cases are not symmetric, and the asymmetry is the whole
reason the mirror refuses those names at spec validation:

* `sign.keyless.identity_token` fails **closed** on its own — the mirror
  resolves that value itself, before it assembles a push, so a run that cannot
  resolve it never tags anything.
* `sign.key`/`key.ref` failed **open** — the ref goes to the ocx child
  verbatim, and `ocx package push --sign` writes the whole cascade before the
  signature fails, so the run published every rolling tag unsigned and then
  exited non-zero.

Reuses `test_signing`'s fixtures and helpers verbatim: the only variable under
test is the invocation shape.
"""
from __future__ import annotations

import os
import shutil
from pathlib import Path

import pytest

from src.mirror_runner import MirrorRunner
from test_signing import (
    COSIGN_KEY,
    COSIGN_PUB,
    KEY_PASSWORD,
    PLATFORM,
    SIGSTORE_IDENTITY,
    SIGSTORE_ISSUER,
    _manifest,
    _publish,
    _registry_tags,
    _signature_referrers,
    _verify,
)

# Re-exported so pytest resolves them here: both are module-local fixtures of
# `test_signing`, not conftest.
from test_signing import signing_mirror, signing_spec  # noqa: F401  isort:skip


@pytest.fixture()
def dispatch_mirror(signing_mirror: MirrorRunner, real_ocx_binary: Path, tmp_path: Path) -> MirrorRunner:
    """`signing_mirror`, but every invocation goes through `ocx mirror …`.

    An `ocx-mirror` symlink on a private PATH entry is what `ocx`'s
    `which::which("ocx-mirror")` resolves; the runner's binary becomes `ocx`
    itself and every argv gains the `mirror` subcommand word.
    """
    bin_dir = tmp_path / "plugin-bin"
    bin_dir.mkdir()
    (bin_dir / "ocx-mirror").symlink_to(signing_mirror.binary)

    signing_mirror.binary = real_ocx_binary
    signing_mirror.env["PATH"] = str(bin_dir) + os.pathsep + signing_mirror.env["PATH"]

    # A released `ocx-mirror` on the ambient PATH would be dispatched instead of
    # the build under test, and every assertion below would be about that binary.
    resolved = shutil.which("ocx-mirror", path=signing_mirror.env["PATH"])
    assert resolved == str(bin_dir / "ocx-mirror"), resolved

    direct_run = signing_mirror.run
    signing_mirror.run = lambda *args, **kwargs: direct_run("mirror", *args, **kwargs)
    return signing_mirror


def _key_spec(spec: Path, ref: str) -> None:
    """Rewrite the fixture's `sign:` block to key mode against `ref`."""
    head, _, _ = spec.read_text().partition("sign:")
    spec.write_text(
        head + "sign:\n  key:\n" + f"    ref: {ref}\n" + "    passphrase: env://MIRROR_KEY_PASSPHRASE\n"
    )


def _publish_unchecked(mirror: MirrorRunner, spec: Path, work: Path):
    """Drive the pipeline as far as it gets, tolerating failure at any step.

    `_publish` checks `plan` and `prepare`, so a spec refused at load raises
    instead of returning — and stopping at `plan` instead would make the
    registry-emptiness assertions **vacuous**: `plan` publishes nothing
    whether or not the refusal exists. Going the whole way is what makes
    "no tags" evidence: without the guard this reaches `push`, which tags the
    cascade before the signature fails.
    """
    work.mkdir(parents=True, exist_ok=True)
    planned = mirror.run(
        "package", "pipeline", "plan", "--spec", str(spec), "--format", "json", check=False
    )
    if planned.returncode != 0:
        return planned
    return _publish(mirror, spec, work, check=False)[1]


def test_the_plugin_dispatch_shape_still_signs(
    dispatch_mirror: MirrorRunner,
    signing_spec: Path,
    real_ocx_binary: Path,
    sigstore_stack,
    zot_registry: str,
    unique_mirror_repo: str,
    tmp_path: Path,
) -> None:
    """S-052's head claim, at the shape a plugin user actually types.

    Identical assertions to `test_keyless_signs_every_platform_manifest_and_
    the_index`, so a difference in outcome is a difference in invocation shape
    and nothing else.
    """
    version, _ = _publish(dispatch_mirror, signing_spec, tmp_path / "work")

    index_digest, index_body = _manifest(zot_registry, unique_mirror_repo, version)
    child_digest = index_body["manifests"][0]["digest"]
    assert len(_signature_referrers(zot_registry, unique_mirror_repo, index_digest)) == 1, (
        "the index carries no signature under `ocx mirror` dispatch"
    )
    assert len(_signature_referrers(zot_registry, unique_mirror_repo, child_digest)) == 1, (
        f"the {PLATFORM} manifest carries no signature under `ocx mirror` dispatch"
    )

    verified = _verify(
        real_ocx_binary,
        dispatch_mirror,
        sigstore_stack,
        f"{zot_registry}/{unique_mirror_repo}:{version}",
        "--rekor-url", sigstore_stack.rekor_url,
        "--certificate-identity", SIGSTORE_IDENTITY,
        "--certificate-oidc-issuer", SIGSTORE_ISSUER,
    )
    assert verified.returncode == 0, f"{verified.stdout}\n{verified.stderr}"


def test_key_env_under_an_ocx_owned_name_is_refused_before_anything_publishes(
    dispatch_mirror: MirrorRunner,
    signing_spec: Path,
    zot_registry: str,
    unique_mirror_repo: str,
    tmp_path: Path,
) -> None:
    """`sign.key.ref: env://OCX_SIGNING_KEY` is refused at spec validation.

    `--key env://NAME` reaches the ocx child verbatim (D1), and the child reads
    the variable from the environment the dispatch just emptied — so the
    reference can never resolve. Left to run, `ocx package push --sign` writes
    the whole cascade *before* the signature fails, publishing an unsigned
    package while reporting failure. The refusal converts that into exit 64
    with nothing published.
    """
    _key_spec(signing_spec, "env://OCX_SIGNING_KEY")
    dispatch_mirror.env["OCX_SIGNING_KEY"] = COSIGN_KEY.read_text()
    dispatch_mirror.env["MIRROR_KEY_PASSPHRASE"] = KEY_PASSWORD

    result = _publish_unchecked(dispatch_mirror, signing_spec, tmp_path / "work")

    assert result.returncode == 64, f"{result.stdout}\n{result.stderr}"
    assert "sign.key.ref" in result.stderr, result.stderr
    assert "OCX_SIGNING_KEY" in result.stderr, result.stderr
    assert "plugin dispatch" in result.stderr, result.stderr
    assert COSIGN_KEY.read_text() not in result.stderr, "the refusal echoed the key material"
    assert _registry_tags(zot_registry, unique_mirror_repo) == set(), (
        "a refused spec must not have advanced a tag"
    )


def test_identity_token_under_the_ocx_owned_name_is_refused_before_anything_publishes(
    dispatch_mirror: MirrorRunner,
    signing_spec: Path,
    zot_registry: str,
    unique_mirror_repo: str,
    tmp_path: Path,
) -> None:
    """The keyless seat of the same refusal.

    `identity_token` is immune to the scrub only because the mirror resolves it
    from a name of the operator's choosing; name the ocx-owned variable and the
    mirror's *own* lookup is what the dispatch emptied.
    """
    spec = signing_spec.read_text().replace(
        [l for l in signing_spec.read_text().splitlines() if "identity_token:" in l][0],
        "    identity_token: env://OCX_IDENTITY_TOKEN",
    )
    signing_spec.write_text(spec)
    dispatch_mirror.env["OCX_IDENTITY_TOKEN"] = "a-token-the-dispatch-will-remove"

    result = _publish_unchecked(dispatch_mirror, signing_spec, tmp_path / "work")

    assert result.returncode == 64, f"{result.stdout}\n{result.stderr}"
    assert "sign.keyless.identity_token" in result.stderr, result.stderr
    assert "OCX_IDENTITY_TOKEN" in result.stderr, result.stderr
    assert _registry_tags(zot_registry, unique_mirror_repo) == set(), (
        "a refused spec must not have advanced a tag"
    )


def test_key_env_under_an_operator_named_variable_survives_the_dispatch(
    dispatch_mirror: MirrorRunner,
    signing_spec: Path,
    real_ocx_binary: Path,
    sigstore_stack,
    zot_registry: str,
    unique_mirror_repo: str,
    tmp_path: Path,
) -> None:
    """The documented workaround: a name `ocx` does not own is not scrubbed."""
    _key_spec(signing_spec, "env://MIRROR_SIGNING_KEY")
    dispatch_mirror.env["MIRROR_SIGNING_KEY"] = COSIGN_KEY.read_text()
    dispatch_mirror.env["MIRROR_KEY_PASSPHRASE"] = KEY_PASSWORD

    version, _ = _publish(dispatch_mirror, signing_spec, tmp_path / "work")

    index_digest, index_body = _manifest(zot_registry, unique_mirror_repo, version)
    child_digest = index_body["manifests"][0]["digest"]
    assert _signature_referrers(zot_registry, unique_mirror_repo, index_digest), "the index is unsigned"
    assert len(_signature_referrers(zot_registry, unique_mirror_repo, child_digest)) == 1, (
        f"the {PLATFORM} manifest is unsigned"
    )

    verified = _verify(
        real_ocx_binary,
        dispatch_mirror,
        sigstore_stack,
        f"{zot_registry}/{unique_mirror_repo}:{version}",
        "--key", str(COSIGN_PUB),
    )
    assert verified.returncode == 0, f"{verified.stdout}\n{verified.stderr}"

