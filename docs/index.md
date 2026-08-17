# ocx-mirror

Upstream projects publish their releases as loose archives on [GitHub Releases][github-releases] or ad-hoc download pages. Consuming them from CI means hand-maintaining URL matrices per platform, re-checking for new versions, and hoping nobody published a broken binary.

`ocx-mirror` automates that. You describe one tool in a YAML spec — where releases come from, which asset belongs to which platform, where to publish — and `ocx-mirror` mirrors every matching upstream version into an OCI registry as an [OCX][ocx] package. Consumers then install the tool with `ocx add`, on any platform, from your registry.

## What it does

- **Sources** — [GitHub Releases][github-releases] or URL indexes (remote JSON, inline versions, or a generator command).
- **YAML spec** — one [`mirror.yml`][ref-mirror-yml] per tool: source, per-platform asset regexes, target registry, version bounds.
- **Two-phase pipeline** — prepare (download, verify, bundle) runs concurrently; push runs sequentially by version so cascade tags (`X.Y.Z` → `X.Y` → `X` → `latest`) always land in semver order.
- **Generated CI pipelines** — [`pipeline generate ci`][cli-generate-ci] renders complete [GitHub Actions][github-actions] workflows that discover new versions on a schedule, smoke-test every `(version, platform)` pair before publishing, and report results to [Discord][discord].

## Beyond one tool at a time

Two sibling namespaces serve corporate deployments that cannot reach the public internet:

- **[`registry sync`][ref-registry-yml]** copies whole upstream OCX indexes — every package they list — into a registry you control, and writes the index tree describing your copy.
- **[`dist sync`][ref-dist-yml]** mirrors the bootstrap layer: ocx's own release archives and the `dist.json` manifest naming them, into a generic HTTP store. It is what lets a machine with no route to `github.com` install `ocx` in the first place.

## Install

`ocx-mirror` is itself distributed as an OCX package:

```sh
ocx --global add ocx.sh/ocx/mirror
```

## Where to go next

- [Getting Started][getting-started] — write your first `mirror.yml`, dry-run it, sync it, and scaffold a mirror repository.
- [mirror.yml reference][ref-mirror-yml] — every spec field.
- [CLI reference][ref-cli] — all subcommands and flags.
- [Environment reference][ref-environment] — variables read by `ocx-mirror` and set by generated workflows.

<!-- external -->
[ocx]: https://github.com/ocx-sh/ocx
[github-releases]: https://docs.github.com/en/repositories/releasing-projects-on-github/about-releases
[github-actions]: https://docs.github.com/en/actions
[discord]: https://discord.com/developers/docs/resources/webhook

<!-- internal -->
[getting-started]: ./getting-started.md
[ref-mirror-yml]: ./reference/mirror-yml.md
[ref-registry-yml]: ./reference/registry-yml.md
[ref-dist-yml]: ./reference/dist-yml.md
[ref-cli]: ./reference/cli.md
[ref-environment]: ./reference/environment.md
[cli-generate-ci]: ./reference/cli.md#pipeline-generate-ci
