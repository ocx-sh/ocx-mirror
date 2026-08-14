"""Builds `registry.yml` (`RegistrySpec`) documents for `registry sync` acceptance tests.

Ground truth for field names: `RegistrySpec`/`RegistrySource` in
`src/spec/registry.rs`. Written as JSON, not hand-rolled YAML text — YAML 1.2
is a superset of JSON, `serde_yaml_ng` (the parser `load_registry_spec` uses)
reads it unchanged, and `json.dumps` needs no new test dependency and cannot
mis-escape a value the way a hand-rolled emitter could.
"""

from __future__ import annotations

import dataclasses
import json
from pathlib import Path
from typing import Literal

OnError = Literal["continue", "fail_fast"]


@dataclasses.dataclass(slots=True)
class SourceSpec:
    """One `sources[]` entry (mirrors `RegistrySource`).

    `trusted_hosts` defaults empty — the SSRF guard stays live unless a
    scenario opts a host in explicitly. Do not "fix" a spec that leaves this
    empty on purpose (S-007 asserts the guard fires for exactly that spec);
    give that scenario its own `SourceSpec` rather than widening a shared one.
    """

    registry: str
    index: str
    as_name: str | None = None
    include: list[str] = dataclasses.field(default_factory=list)
    exclude: list[str] = dataclasses.field(default_factory=list)
    trusted_hosts: list[str] = dataclasses.field(default_factory=list)

    def to_dict(self) -> dict[str, object]:
        document: dict[str, object] = {"registry": self.registry, "index": self.index}
        if self.as_name is not None:
            document["as"] = self.as_name
        if self.include:
            document["include"] = self.include
        if self.exclude:
            document["exclude"] = self.exclude
        if self.trusted_hosts:
            document["trusted_hosts"] = self.trusted_hosts
        return document


def write_registry_spec(
    path: Path,
    *,
    target_registry: str,
    target_repository: str,
    output: Path,
    sources: list[SourceSpec],
    destination: str = "{namespace}/{package}",
    on_error: OnError = "continue",
    kind: str | None = "registry",
    extra: dict[str, object] | None = None,
) -> None:
    """Writes a `registry.yml` (`RegistrySpec`) document to `path`.

    `kind` is the discriminator the pre-scan reads before typed
    deserialization (C-005); it is not a `RegistrySpec` field, and an absent
    or wrong value is a hard exit 64. Pass `kind=None` to write a document
    without it — the only reason to do so is to test that rejection.

    `extra` merges arbitrary top-level keys into the document, for the
    documents whose whole point is being invalid (a `password:` key, an
    unknown field).
    """
    document: dict[str, object] = {
        "target": {"registry": target_registry, "repository": target_repository},
        "output": str(output),
        "destination": destination,
        "on_error": on_error,
        "sources": [source.to_dict() for source in sources],
    }
    if kind is not None:
        document["kind"] = kind
    if extra:
        document.update(extra)
    path.write_text(json.dumps(document))
