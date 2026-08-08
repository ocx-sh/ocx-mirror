---
name: e2e-test
description: Tiered end-to-end testing of ocx-mirror against its real downstream users. Use when asked to e2e-test or integration-test the current state, verify main against the ocx-contrib fleet, test an unreleased build in a real mirror repo, or validate the python-mirror (pypi env) pipeline end to end. Routes between the offline acceptance harness, a local-registry integration run against real contrib specs, and the dev.ocx.sh dev-channel runbook — no OCX release required for any tier.
user-invocable: true
disable-model-invocation: false
---

# e2e-test — tiered end-to-end verification

Three tiers, cheapest first. Pick the lowest tier that answers the
question; escalate only when the answer needs infrastructure the lower
tier fakes. No tier publishes an OCX release.

| Tier | What runs | Egress | Cost | Proves |
|------|-----------|--------|------|--------|
| **0 — Acceptance harness** | `task test` (pytest, `test/tests/`) | none (registry :5001, local asset/PyPI/webhook fakes) | minutes | pipeline code paths vs fixtures |
| **1 — Local integration** | current `main` binary vs real `~/dev/ocx-contrib` specs, pushes to `localhost:5001` | reads only: GitHub API, PyPI, ocx.sh pulls | ~30 min | real specs, real assets, real push/cascade/wheel-mount, runtime of published packages |
| **2 — Dev channel** | Deploy Dev → `dev.ocx.sh/ocx/mirror:<ver>-dev_<ts>` → pin in a downstream repo branch → dispatch generated CI | full GitHub + registries | hours | the real thing: GHA matrix, `ocx package test` containers, announce |

## Decision guide

- Changed pipeline/spec/renderer code, want confidence before commit →
  **Tier 0**, plus **Tier 1** if the change touches push, cascade,
  env packages, or the renderer's contract with the fleet.
- "Does main still serve the existing fleet?" → **Tier 1** fleet sweep
  (validate + drift; drift after template changes is expected —
  non-0/65 exits are the signal).
- Anything involving real container test legs, GHA semantics, announce,
  or a release candidate → **Tier 2**.

## Quick start

```sh
# Tier 0
task test                        # full acceptance suite (builds binary)
cd test && uv run pytest tests/test_mirror_pypi.py -v   # one file

# Tier 1 — read references/local-pipeline.md for the full verified recipes
cargo build --release
docker compose -f test/docker-compose.yml up -d   # registry on :5001
export OCX_INSECURE_REGISTRIES=localhost:5001

# Tier 2 — read references/dev-channel.md for the runbook
gh workflow run "Deploy Dev" --repo ocx-sh/ocx-mirror --ref <branch>
```

Tier 2 dispatches real cloud workflows and publishes dev artifacts —
get explicit user confirmation before dispatching.

## References

- `references/local-pipeline.md` — Tier 1 verified recipes: fleet sweep,
  archive/binary pipeline (hexyl/jq), python env pipeline (pipx recreation
  incl. interpreter copy), gotchas, what the tier does/doesn't prove.
- `references/dev-channel.md` — Tier 2 runbook: Deploy Dev flow, the
  one-line downstream `ocx.toml` pin + `ocx lock`, bootstrap assertions,
  precedent repos.

## Known coverage gaps (as of 2026-08-08)

- Env-package (`pylock`/`pypi`) `push` has **zero** in-repo acceptance
  coverage — pytest stops at `prepare`; the pylock positive e2e is
  `@pytest.mark.skip`. Tier 1's pipx recipe is the working substitute.
- Fabricated-green JUnit in Tiers 0/1 means real container test legs are
  only proven in Tier 2 (or a manual `ocx package test`).
- Multi-platform index merge and `+libc.*` push gating are unit-tested
  but not acceptance-tested.
