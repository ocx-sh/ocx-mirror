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

`ocx-mirror` spawns the `ocx` binary for publishing (`ocx package push --cascade`) and catalog metadata (`ocx package description push`). The child binary is resolved in **two** rungs (`src/pipeline/ocx_cli.rs`, `resolve_ocx_binary`): `OCX_BINARY_PIN` if it is set and non-empty — `ocx` sets it itself when the mirror runs under `ocx exec` — otherwise `ocx` on `PATH`. There is deliberately no co-located lookup, so in a generated workflow, where the mirror is invoked directly rather than through `ocx exec`, the child `ocx` is whichever one the project toolchain put on `PATH`.

Whichever of those three wins must be **ocx 0.5.5 or newer**: an older binary rejects the metadata sidecar `pipeline prepare` writes and fails every push with exit 65. See [Push retry][spec-push-retry] for the full contract.

Since the 0.6 CLI rename **three** legs raise that to **ocx 0.6.0 or newer**, each rejected by a 0.5.x binary with exit 64:

| Leg | Spawns | Why 0.5.x refuses it |
|---|---|---|
| `pipeline announce` | `ocx package announce --tags-file` | the flag that replaced `--tags-from-file` |
| `pipeline describe` | `ocx package description push` | `description` did not exist as a subcommand |
| `pipeline cascade` | `ocx package announce --tags-file` (its closing announce) | same flag as the announce leg |

The 0.6.0 floor is therefore the effective floor for any mirror repository that describes, announces, or cascades — which is all of them. Only a plan/prepare/push-only run stays on the older 0.5.5 floor.

Resolution-affecting `OCX_*` variables present in the environment are forwarded to that subprocess, so offline mode, registry config, and index paths behave identically inside the child:

`OCX_HOME`, `OCX_DEFAULT_REGISTRY`, `OCX_INSECURE_REGISTRIES`, `OCX_OFFLINE`, `OCX_REMOTE`, `OCX_CONFIG`, `OCX_NO_CONFIG`, `OCX_PROJECT`, `OCX_NO_PROJECT`, `OCX_INDEX`, `OCX_BINARY_PIN`, `OCX_NO_UPDATE_CHECK`, `OCX_NO_MODIFY_PATH`

See the [OCX environment reference][ocx-env] for what each variable does.

**Scope:** `sync`, `pipeline push`, `pipeline describe` (any command that spawns `ocx`).

### Signing credentials set on the ocx child {#signing-credentials}

`OCX_IDENTITY_TOKEN` and `OCX_KEY_PASSWORD` are the conventional variable names `ocx` itself reads for a keyless identity-token override and a key-mode passphrase. `ocx-mirror` resolves [`sign.keyless.identity_token`][spec-sign] and [`sign.key.passphrase`][spec-sign] — each an `env://`-or-`file://` `Ref`; a literal is refused there — and exports the resolved value onto these two names in every `ocx` child's environment, identically on the subprocess push legs, the closing sign sweep, and the backfill.

On GitLab specifically, `ocx` reads its own ambient `SIGSTORE_ID_TOKEN` for the identity token — the `id_tokens` block in the [backfill job](./cli.md#pipeline-sign) is what supplies it, not the mirror. `OCX_IDENTITY_TOKEN` stays the mirror's explicit path, used when `sign.keyless.identity_token` is set.

This is a deliberate re-supply, not an oversight. `ocx`'s own plugin dispatch (`ocx mirror …`) strips both names from the ambient environment before launching a plugin, on the reasoning that a plugin is third-party code that must not inherit a bearer credential. Resolving the ref itself and re-exporting it is what lets signing survive that path — accepted under this project's threat model, where the execution environment is trusted. It survives only while the ref names a variable of the operator's own choosing: naming one of the scrubbed variables empties the mirror's own lookup, which is why doing so is [refused outright](#plugin-dispatch-scrub).

Neither name is added to [`OCX_VARS`](#ocx-forwarding): the `ocx` child inherits the full process environment on every spawn (no `env_clear()` anywhere in the spawn path), so a value already set on the environment — including these two — reaches the child without an explicit forward entry. `OCX_VARS` exists for variables `ocx-mirror` reads out of *its own* environment and re-declares; these two are values `ocx-mirror` computed, not read.

That same inheritance makes an ambient **`OCX_FROZEN=1` incompatible with signing**: both sign argv builders pass `--remote` unconditionally, and `ocx` refuses `--frozen` together with `--remote` with exit 64 — a guard that fires on the env-sourced value too, because `OCX_FROZEN` reaches it through the flag's clap *default* rather than through a flag `conflicts_with` can see. Unset `OCX_FROZEN` on any job that signs; do not drop `--remote` to keep it, because that is exactly the local-index tag resolution `--remote` was added to stop.

**Scope:** any command that signs — `sync`, `pipeline push`, `pipeline patch`, `pipeline sign`.

### Plugin-dispatch credential scrub, and the spec refusal that closes it {#plugin-dispatch-scrub}

`ocx`'s plugin dispatch strips `OCX_IDENTITY_TOKEN`, `OCX_KEY_PASSWORD` **and** `OCX_SIGNING_KEY` from the ambient environment before launching `ocx-mirror` as a plugin (`ocx mirror package pipeline push`). The scrub reaches every `sign:` field that could name one of them, by two different routes:

- A bare [`sign.key`][spec-sign] or `sign.key.ref` is handed to `ocx` as `--key <ref>` and resolved by the **child**, from the environment the dispatch just emptied.
- `sign.keyless.identity_token` and `sign.key.passphrase` are resolved by the **mirror**, from *its own* environment — which the dispatch emptied one level earlier. The re-supply described [above](#signing-credentials) survives the scrub only because the operator names a variable of their own choosing; name an ocx-owned one and there is nothing left to re-export.

The `--key` route is the dangerous one: `ocx package push --sign` writes the whole cascade before the signature fails, so a run that cannot read its key publishes every rolling tag **unsigned** and then exits non-zero. Reporting failure while the registry holds unsigned artifacts is the worst of both.

`ocx-mirror` therefore **refuses any `sign:` `env://NAME` naming one of those three variables, at spec validation, exit 64** — before a single tag is written. The message names the field and the variable, never the value. The remedy is a rename: pick a name `ocx`'s plugin dispatch does not know to scrub, for example `MIRROR_SIGNING_KEY`, and set `sign.key: env://MIRROR_SIGNING_KEY`.

The refusal is unconditional, not dispatch-detected. A direct, unwrapped `ocx-mirror` invocation — the command line, or a generated workflow's own push step — *can* read `OCX_SIGNING_KEY`, so that one working case is given up deliberately: one `mirror.yml` is read by both invocation shapes, and a spec that signs when run one way and cannot resolve its material when run the other is a trap worth more than the case it costs — whether it then publishes unsigned or fails closed depends on the seat, as the two routes above show. (`ocx exec -- …` and `ocx run -- …` scrub `OCX_SIGNING_KEY` exactly like plugin dispatch, so that case is narrower than it looks.)

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
[spec-push-retry]: ./mirror-yml.md#concurrency-push-retry
[spec-sign]: ./mirror-yml.md#sign
