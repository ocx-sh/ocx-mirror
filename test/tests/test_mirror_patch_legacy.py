"""#9, the half of the no-eviction claim only registry:2 can witness.

registry:2 keeps every pushed manifest addressable by digest for good; zot drops
a manifest from its index the moment the last tag naming it moves. The pre-patch
image index is pinned by no keep tag (the platform manifest is), so after a patch
it resolves on registry:2 and not on zot. `registry` is therefore overridden to
the docker-only `legacy_registry` for this module, and the fixture chain of
`test_mirror_patch.py` runs against it unchanged.
"""
from __future__ import annotations

import pytest

from test_mirror_patch import (  # noqa: F401  (fixtures, re-exported for pytest)
    Patched,
    _resolves,
    patched,
    published,
)


@pytest.fixture(scope="session")
def registry(legacy_registry: str) -> str:
    return legacy_registry


def test_patch_keeps_the_pre_patch_image_index_resolvable(
    patched: Patched, registry: str, unique_mirror_repo: str
) -> None:
    """#9: a consumer pinned to the pre-patch image index keeps resolving."""
    assert _resolves(registry, unique_mirror_repo, patched.before.index_digest), (
        f"the pre-patch image index {patched.before.index_digest} no longer resolves"
    )
