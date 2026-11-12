# Research: CI-neutral build-job contracts

## Metadata

**Date:** 2026-11-08
**Domain:** ci-cd
**Triggered by:** `/hex-architect` revision of `adr_cargo_source_kind.md` (dossier `.agents/discussions/cargo-native-build.md`) — `plan.json` must describe per-platform build legs completely enough for a third-party renderer (GitLab, Jenkins, cron) to render the whole pipeline from the file alone
**Expires:** 2027-05-08
**Axes:** design-pattern precedent; data model / compatibility

## Direct Answer

Describe build work as a **flat job list with explicit `needs` edges** (a DAG as data), not as matrix axes. Every target CI can express a flat list with predecessor edges (GitLab `needs:`, Buildkite `depends_on`, Tekton `runAfter`, Argo DAG); a matrix is a rendering optimisation the renderer can derive, and putting it in the contract bakes in one forge's expansion rules — cargo-dist's GitHub-only `ci` object is the cautionary precedent.

Per job: `id`, `kind`, `phase` (`build` | `test` | `push`, so stage-based CIs derive stages), `needs`, `requires` (runner label set by reference to the existing `legs.<platform>`, optional digest-pinned `container.image`, `toolchain`), `inputs`/`outputs` (named artifacts; transport is the renderer's), and a `credentials` class (`none` for build/test, `registry-write` for push only). Build jobs report output digests (e.g. a `result.json`); the push job verifies before pushing. Integer `schema_version`, additive-only within a version, unknown fields ignored, but **fail loudly on an unknown `kind` or `credentials` value** — silently skipping a `registry-write` job is a correctness bug.

## Key findings

1. **cargo-dist** — `dist-manifest.json` (`cargo-dist-schema`: `DistManifest`, `Release`, `Artifact`, `Asset`) is forge-neutral for artifacts, but its `CiInfo` has only a GitHub backend (`GithubCiInfo`, `GithubMatrix`, `GithubRunner`); no documented non-GitHub consumer. Versions by crate version + format epochs (`DIST_EPOCH_1_MAX`, `DIST_EPOCH_2_MAX`; epoch 2 changed artifact IDs and broke download URLs). [docs.rs/cargo-dist-schema](https://docs.rs/cargo-dist-schema/latest/cargo_dist_schema/), [CI book](https://axodotdev.github.io/cargo-dist/book/ci/index.html) (search summary only), [CLI manual](https://axodotdev.github.io/cargo-dist/book/reference/cli.html) (search summary only — `dist plan` JSON and GitHub job split unverified).
2. **GitLab dynamic child pipelines** — a job writes YAML as an artifact, a trigger job `include: artifact`s it; no CI/CD variables in the dynamic include; nesting ≤ 2 levels; ≤ 3 included config files per child; 1000 downstream pipelines per hierarchy; cross-pipeline artifact fetch (`needs:pipeline:job`) is **Premium/Ultimate** → keep build → test → push inside **one** generated child pipeline for free-tier users. `parallel:matrix` max 200 jobs and a 50-job `needs` default are mentioned but the fetch was truncated — verify. [downstream pipelines](https://docs.gitlab.com/ci/pipelines/downstream_pipelines/), [CI YAML](https://docs.gitlab.com/ci/yaml/).
3. **Buildkite** — `buildkite-agent pipeline upload` adds steps at runtime; each step gets a clean checkout, data moves via artifacts (generator-emits-jobs pattern). [defining steps](https://buildkite.com/docs/pipelines/defining-steps#dynamic-pipelines).
4. **nix-eval-jobs** — one JSON line per job (`attr`, `drvPath`, `name`, `outputs`, `system`); `system` doubles as runner selector; one job's eval failure doesn't fail the jobset. [nix-eval-jobs](https://github.com/Mic92/nix-eval-jobs).
5. **conda-smithy** — `provider` maps build platform → CI service; `build_platform` picks native vs cross; `github_actions_labels` sets `runs-on` (`self-hosted@` prefix) — platform→provider mapping lives in config, not in the contract (matches ocx-mirror's runner label set). [conda-forge.yml](https://conda-forge.org/docs/maintainer/conda_forge_yml/).
6. **SLSA v1.1** — the build must not reach platform secrets (signing key); provenance from the control plane, outputs identified by digest (Build L3). Secretless build + digest-verifying credentialed push is an inference from this, not stated there. [SLSA requirements](https://slsa.dev/spec/v1.1/requirements).

## Trends

- Established: generated-artifact child pipelines (GitLab), `pipeline upload` (Buildkite), per-job JSON (nix-eval-jobs), matrix-from-data rendering (conda-smithy).
- Trending: secretless build vs credentialed publish (SLSA); digest-bound hand-offs and attestations.
- Emerging: no cross-vendor build-graph standard confirmed (CDEvents / Tekton / Argo closest, adoption unconfirmed).
- Declining: forge-specific matrix objects in the contract; stage-only pipelines without DAG edges.

## Recommendation (researcher's)

Additive optional `jobs[]` (v4 readers unaffected; bump to v5 only if it becomes required); job ids like `prepare:<version>:<platform>`; reference `legs.<platform>` for runner labels rather than copying; per-job command is the real contract (`ocx-mirror … --job <id>` or equivalent); any `render --forge gitlab` is convenience over the same file; digest binding via build-written results verified by push; publish the JSON Schema; mind GitLab job-count limits — let the renderer group by version.

## Gaps

Jenkins, Tekton/Argo, Dagger, Earthly, Homebrew test-bot, cargo-quickinstall, Hydra, Release Please not researched. cargo-dist CLI/CI pages and GitLab YAML limits only partially fetched.
