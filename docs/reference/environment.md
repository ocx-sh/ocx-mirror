# Environment Reference

Environment variables read by `ocx-mirror` and the secrets consumed by the [GitHub Actions][github-actions-docs] workflows that [`pipeline generate ci`][cli-generate-ci] renders. None of these are read by the `ocx` binary itself; for OCX's own variables see the [OCX environment reference][ocx-env].

## Variables read by ocx-mirror {#read-by-ocx-mirror}

### `GITHUB_TOKEN` {#github-token}

Authenticates GitHub API release listing for `github_release` sources. Optional: without it, release listing runs against the unauthenticated quota (60 requests/hour) instead of the authenticated one (5 000 requests/hour) — backfilling release-heavy tools needs the token.

```sh
GITHUB_TOKEN=ghp_... ocx-mirror sync mirror.yml
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
    `ocx-mirror pipeline generate ci` rejects any `mirror.yml` where `notify.discord.webhook_secret` contains a URL (matching `https?://`, `discord.com`, or `discordapp.com`) with exit 64. This prevents live webhook URLs from being committed to the repository.

<!-- external -->
[github-actions-docs]: https://docs.github.com/en/actions
[github-actions-secrets]: https://docs.github.com/en/actions/security-for-github-actions/security-guides/using-secrets-in-github-actions
[discord]: https://discord.com/developers/docs/resources/webhook
[ocx-env]: https://ocx.sh/docs/reference/environment

<!-- internal -->
[cli-generate-ci]: ./cli.md#pipeline-generate-ci
[cli-plan]: ./cli.md#pipeline-plan
[cli-push]: ./cli.md#pipeline-push
[cli-notify]: ./cli.md#pipeline-notify
