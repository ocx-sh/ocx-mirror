# Getting Started

This walkthrough mirrors a real tool — [shfmt][shfmt], a single-binary shell formatter released on [GitHub Releases][github-releases] — into an OCI registry. You will write a `mirror.yml`, validate it, dry-run it, sync it, and finally scaffold a mirror repository with generated CI workflows.

## Install ocx-mirror {#install}

```sh
ocx --global add ocx.sh/ocx/mirror
```

## Step 1: Describe the tool {#first-spec}

A mirror spec answers three questions: where do releases come from, which asset belongs to which platform, and where should packages go. Create `mirror.yml`:

```yaml
name: shfmt

target:
  registry: ocx.sh        # any OCI registry you can push to
  repository: shfmt

source:
  type: github_release
  owner: mvdan
  repo: sh
  tag_pattern: "^v(?P<version>\\d+\\.\\d+\\.\\d+)$"

assets:
  linux/amd64:
    - "shfmt_v.*_linux_amd64$"
  darwin/arm64:
    - "shfmt_v.*_darwin_arm64$"

# shfmt ships raw executables, not archives.
asset_type:
  type: binary
  name: shfmt
```

Three things to note:

- `tag_pattern` is a regex with a named `(?P<version>...)` capture group — it turns upstream git tags (`v3.10.0`) into package versions (`3.10.0`).
- Each `assets` entry is a list of regexes matched against upstream asset filenames. Exactly one asset must match per platform; zero matches skips the platform for that version.
- `asset_type` defaults to `archive` (extract a tar/zip). Single-binary tools use `binary` with the executable name.

The full field list lives in the [mirror.yml reference][ref-mirror-yml].

## Step 2: Validate the spec {#validate}

```sh
ocx-mirror validate mirror.yml
```

Schema errors, invalid regexes, and missing capture groups are reported with exit code 65 — nothing touches the network.

## Step 3: Dry run {#check}

`check` runs the full discovery pass — list upstream releases, resolve assets per platform, compare against the tags already in the target registry — without downloading or pushing anything:

```sh
ocx-mirror check mirror.yml
```

The output is a table of `(version, platform)` pairs and what a real run would do, followed by a `total / pushed / skipped / failed` summary. Use `--latest` to restrict to the highest version, or `--version 3.10.0` for one exact version.

## Step 4: Mirror {#sync}

```sh
ocx-mirror sync mirror.yml --latest
```

`sync` downloads the matched assets, bundles them as OCX packages, and pushes one tag per `(version, platform)` — concurrently for downloads, sequentially for pushes so rolling tags cascade in semver order. Drop `--latest` to backfill every version the spec's filters admit.

!!! tip "Registry credentials"
    `ocx-mirror` reuses OCX's registry auth. Credentials from `docker login <registry>` are picked up automatically via the Docker credential fallback.

!!! tip "GitHub API rate limits"
    Release listing is unauthenticated by default (60 requests/hour). Set `GITHUB_TOKEN` to raise the quota to 5 000 requests/hour — required for backfilling release-heavy tools.

## Step 5: Scaffold a mirror repository {#pipeline}

One-shot syncs work, but a mirror should run on a schedule and never publish a broken binary. `pipeline generate ci` renders complete [GitHub Actions][github-actions] workflows that discover new versions, build bundles, smoke-test every `(version, platform)` pair on a real runner, and only push the green ones.

The generated pipeline needs two more spec sections — what to test and where to test it:

```yaml
tests:
  - name: version
    command: shfmt --version

platforms:
  linux/amd64:
    runner: ubuntu-latest
  darwin/arm64:
    runner: macos-latest
```

Then, from the root of the mirror repository:

```sh
ocx-mirror pipeline generate ci
```

This writes three workflows under `.github/workflows/`:

| File | Purpose |
|------|---------|
| `mirror.yml` | The pipeline: discover → prepare → test → push → notify |
| `describe.yml` | Publishes catalog metadata (README + logo) to the registry |
| `verify-generated.yml` | Drift guard — fails CI when generated workflows are hand-edited |

The workflows are generated files: edit the spec, re-run `ocx-mirror pipeline generate ci`, and commit the result. `--check` mode (used by the drift guard) exits 65 when the committed files no longer match the spec.

Finally, configure two repository secrets so the push job can log in to the target registry: `OCX_MIRROR_REGISTRY_USER` and `OCX_MIRROR_REGISTRY_TOKEN`. Without them the pipeline still runs — in test/validation mode with the registry push skipped.

## Next steps {#next}

- Pin version windows, exclude broken releases per platform, and wire up Discord reports — [mirror.yml reference][ref-mirror-yml].
- All subcommands and flags — [CLI reference][ref-cli].
- Variables read by the pipeline subcommands — [Environment reference][ref-environment].

<!-- external -->
[shfmt]: https://github.com/mvdan/sh
[github-releases]: https://docs.github.com/en/repositories/releasing-projects-on-github/about-releases
[github-actions]: https://docs.github.com/en/actions

<!-- internal -->
[ref-mirror-yml]: ./reference/mirror-yml.md
[ref-cli]: ./reference/cli.md
[ref-environment]: ./reference/environment.md
