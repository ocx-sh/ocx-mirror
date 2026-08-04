# Environment Reference

Environment variables read by `ocx-mirror` and the secrets consumed by the [GitHub Actions][github-actions-docs] workflows that [`pipeline generate ci`][cli-generate-ci] renders. None of these are read by the `ocx` binary itself; for OCX's own variables see the [OCX environment reference][ocx-env].

## Variables read by ocx-mirror {#read-by-ocx-mirror}

### `GITHUB_TOKEN` {#github-token}

Authenticates GitHub API release listing for `github_release` sources. Optional: without it, release listing runs against the unauthenticated quota (60 requests/hour) instead of the authenticated one (5 000 requests/hour) — backfilling release-heavy tools needs the token.

```sh
GITHUB_TOKEN=ghp_... ocx-mirror package sync mirror.yml
```

**Scope:** Any command that crawls a `github_release` source — `sync`, `check`, `pipeline plan`, and `pipeline prepare` without `--plan`. The generated `discover` job forwards the workflow's `GITHUB_TOKEN` automatically.

### `GITHUB_ACTIONS` {#github-actions}

Set to `true` by [GitHub Actions][github-actions-docs] runners. When [`pipeline plan`][cli-plan] is invoked without `--format`, it selects JSON output automatically if this variable is `true`, plain output otherwise.

**Scope:** `pipeline plan`.

### `OCX_MIRROR_JOB_URL` {#ocx-mirror-job-url}

The HTML URL of the running push job. [`pipeline push`][cli-push] reads it at startup and stamps it into `run-summary.json`, so the [Discord][discord] report can link green rows and push-tier failures back to the push logs. Test-tier failures link to their matrix-leg URL parsed from the JUnit `ci.job.url` property instead.

The generated workflow resolves this URL via `gh api` before invoking `pipeline push` — GitHub Actions exposes no default variable carrying the per-job URL.

**Scope:** `pipeline push`.

### `OCX_MIRROR_DISCORD_HOOK` {#ocx-mirror-discord-hook}

The [Discord][discord] webhook URL used by [`pipeline notify`][cli-notify]. The name is fixed by convention: the spec's `notify.discord.webhook_secret` field selects *which* [GitHub Actions secret][github-actions-secrets] holds the URL, and the generated workflow maps that secret onto this variable in the notify job:

```yaml
# In the generated workflow (do not write by hand):
env:
  OCX_MIRROR_DISCORD_HOOK: ${{ secrets.DISCORD_WEBHOOK_URL }}
```

**Scope:** `pipeline notify`.

### `OCX_MIRROR_DISCORD_USER_ID` {#ocx-mirror-discord-user-id}

Discord user ID (snowflake) to mention when a run carries failures. Non-secret — the workflow renderer inlines `notify.discord.user_id` from the spec into the notify job's `env:` under this name. Unset or empty means no mention.

**Scope:** `pipeline notify`.

### `GITHUB_SERVER_URL` / `GITHUB_REPOSITORY` / `GITHUB_SHA` {#annotation-env}

The [OCI annotations][oci-annotations] recorded on every published image index. [GitHub Actions][github-actions-docs] sets all three as default variables in every step, so the generated workflows pass nothing explicitly:

| Variable | Annotation |
|----------|------------|
| `GITHUB_SERVER_URL` + `GITHUB_REPOSITORY` | `org.opencontainers.image.source` = `$GITHUB_SERVER_URL/$GITHUB_REPOSITORY` — the mirror repository, which is what [GHCR][ghcr-source] uses to link the package to a repository and inherit its permissions |
| `GITHUB_SHA` | `org.opencontainers.image.revision` |

A missing or blank variable means its annotation is not written; `image.source` needs both halves. Outside CI nothing is emitted and the push leaves the registry's existing annotations alone.

These three names are the **complete** environment surface for annotations — the [`annotations:`](./mirror-yml.md#annotations) block is the only other input, and its values are taken verbatim from the spec. Nothing enumerates the process environment. The `ocx` subprocess inherits the runner's environment (including `GH_TOKEN`), and a published index is public, permanent and readable without authentication, so widening this to a prefix match or a caller-named variable would put whatever the runner carries on the wire.

**Scope:** `sync`, `pipeline push`.

### `OCX_ANNOUNCE_TOKEN` {#ocx-announce-token}

The GitHub credential `ocx package announce` uses to push the fork branch and open the index pull request. Read from the environment and handed to the `ocx` subprocess; `ocx-mirror` never stores it and never logs it.

Only mirrors with an [`announce:`][spec-announce] block need it. The commands read it differently:

| Command | Without the token |
|---------|-------------------|
| [`pipeline push`][cli-push] | Degrades: the run publishes normally, emits a GitHub notice, and records `skipped_no_credential` in `run-summary.json`. A mirror without the secret is a valid configuration. |
| [`pipeline announce`][cli-announce] | Fails. Opening the index pull request is the only thing this command does — except under `--dry-run`, which writes to a temporary directory and needs no token. |
| [`pipeline patch`][cli-patch] | Degrades like `pipeline push`: republished manifests land, the index announce is skipped with a GitHub notice. |
| [`pipeline cascade`][cli-cascade] | Degrades like `pipeline push`: repaired tags land on the registry, the index announce is skipped with a GitHub notice. |

The generated workflows thread it in from a repository secret of the same name:

```yaml
# In the generated workflow (do not write by hand):
env:
  OCX_ANNOUNCE_TOKEN: ${{ secrets.OCX_ANNOUNCE_TOKEN }}
```

It is deliberately **not** the workflow's own `GITHUB_TOKEN`: the pull request targets a different repository ([`ocx-sh/index`][index-repo], from a fork), which the run's automatic token cannot reach.

**Scope:** `pipeline push`, `pipeline announce`, `pipeline patch`, `pipeline cascade`.

### Forwarded `OCX_*` variables {#ocx-forwarding}

`ocx-mirror` spawns the `ocx` binary for publishing (`ocx package push --cascade`) and catalog metadata (`ocx package describe`). The child binary is resolved in order: `OCX_BINARY_PIN`, an `ocx` co-located with the `ocx-mirror` executable, then `ocx` on `PATH`.

Resolution-affecting `OCX_*` variables present in the environment are forwarded to that subprocess, so offline mode, registry config, and index paths behave identically inside the child:

`OCX_HOME`, `OCX_DEFAULT_REGISTRY`, `OCX_INSECURE_REGISTRIES`, `OCX_OFFLINE`, `OCX_REMOTE`, `OCX_CONFIG`, `OCX_NO_CONFIG`, `OCX_PROJECT`, `OCX_NO_PROJECT`, `OCX_INDEX`, `OCX_BINARY_PIN`, `OCX_NO_UPDATE_CHECK`, `OCX_NO_MODIFY_PATH`

See the [OCX environment reference][ocx-env] for what each variable does.

**Scope:** `sync`, `pipeline push`, `pipeline describe` (any command that spawns `ocx`).

## Secrets in generated workflows {#workflow-secrets}

The rendered workflows reference repository secrets by name. These are GitHub Actions secrets, not variables `ocx-mirror` reads directly.

### `OCX_MIRROR_REGISTRY_USER` / `OCX_MIRROR_REGISTRY_TOKEN` {#registry-secrets}

Credentials for the target registry. The `push` and `describe` jobs use them for `docker login`, which `ocx` picks up through its Docker credential fallback. When `OCX_MIRROR_REGISTRY_TOKEN` is absent, the registry push is skipped and the repository runs in test/validation mode.

### `DISCORD_WEBHOOK_URL` {#discord-webhook-url}

Conventional name for the secret holding the Discord webhook URL. `mirror.yml`'s `notify.discord.webhook_secret` field names the secret (any `^[A-Z][A-Z0-9_]+$` name works); the generated workflow maps it onto [`OCX_MIRROR_DISCORD_HOOK`](#ocx-mirror-discord-hook).

!!! warning "Never hardcode the URL"
    `ocx-mirror package pipeline generate ci` rejects any `mirror.yml` where `notify.discord.webhook_secret` contains a URL (matching `https?://`, `discord.com`, or `discordapp.com`) with exit 64. This prevents live webhook URLs from being committed to the repository.

<!-- external -->
[github-actions-docs]: https://docs.github.com/en/actions
[github-actions-secrets]: https://docs.github.com/en/actions/security-for-github-actions/security-guides/using-secrets-in-github-actions
[discord]: https://discord.com/developers/docs/resources/webhook
[ocx-env]: https://ocx.sh/docs/reference/environment
[oci-annotations]: https://github.com/opencontainers/image-spec/blob/main/annotations.md
[ghcr-source]: https://docs.github.com/en/packages/working-with-a-github-packages-registry/working-with-the-container-registry#labelling-container-images
[index-repo]: https://github.com/ocx-sh/index

<!-- internal -->
[cli-generate-ci]: ./cli.md#pipeline-generate-ci
[cli-plan]: ./cli.md#pipeline-plan
[cli-push]: ./cli.md#pipeline-push
[cli-notify]: ./cli.md#pipeline-notify
[cli-announce]: ./cli.md#pipeline-announce
[cli-patch]: ./cli.md#pipeline-patch
[cli-cascade]: ./cli.md#pipeline-cascade
[spec-announce]: ./mirror-yml.md#announce
