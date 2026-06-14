---
title: OCX Mirror
description: Mirror upstream tool releases into OCI registries as OCX packages
keywords: [mirror, oci, registry, github-releases, ci, ocx]
---

# OCX Mirror

Mirror upstream tool releases — GitHub Releases or URL indexes — into any OCI
registry as [OCX](https://github.com/ocx-sh/ocx) packages.

Describe one tool in a YAML spec (where releases come from, which asset belongs
to which platform, where to publish) and `ocx-mirror` mirrors every matching
upstream version into your registry. Consumers then install the tool with
`ocx` on any platform.

## Highlights

- **Sources** — GitHub Releases or URL indexes (remote JSON, inline versions,
  or a generator command).
- **YAML spec** — one `mirror.yml` per tool: source, per-platform asset
  regexes, target registry, version bounds.
- **Two-phase pipeline** — prepare (download, verify, bundle) runs
  concurrently; push runs sequentially by version so cascade tags
  (`X.Y.Z` → `X.Y` → `X` → `latest`) always land in semver order.
- **Generated CI pipelines** — `pipeline generate ci` renders complete GitHub
  Actions workflows that discover new versions on a schedule, smoke-test every
  `(version, platform)` pair before publishing, and report results to Discord.

## Usage

```sh
ocx --global add ocx.sh/ocx/mirror

ocx-mirror package validate mirror.yml   # check a spec
ocx-mirror package check mirror.yml      # dry-run: what would be mirrored
ocx-mirror package sync mirror.yml       # mirror upstream releases into the registry
```

## Links

- Documentation: <https://ocx-sh.github.io/ocx-mirror/>
- Source: <https://github.com/ocx-sh/ocx-mirror>
- License: Apache-2.0
