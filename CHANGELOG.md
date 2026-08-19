# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.5.6] - 2026-08-19

### Added

- Host-keyed credentials and platform TLS roots for HTTP legs *(auth)*
- Discover through the Simple Repository API, indexes list *(pypi)*

### Fixed

- Never answer a netrc `default` entry *(auth)*
- Honour RUST_LOG and surface why a fetch failed *(cli)*
- Route every production client through the trust-root factory *(http)*

## [0.5.5] - 2026-08-18

### Added

- Preserve upstream pointers by default *(registry)*
- Mirror the OCX distribution into a generic store *(dist)*

### Release

- V0.5.5

## [0.5.4] - 2026-08-14

### Added

- Python wheel → OCX packaging library *(ocx_python)*
- Env sources — pylock & pypi, wheels platform keys, env pipeline & container CI *(mirror)*
- Alias newest non-semver env version under :latest *(push)*
- Add the e2e-test skill (tiered e2e strategy) *(claude)*
- Mirror whole index sources into a corporate registry *(registry)*

### Changed

- Extract push_with_retry from invoke_push *(push)*
- Extract the push test module into push/tests/ *(push)*
- Extract the ci renderer test module into ci/tests/ *(generate)*
- Extract the spec test module into spec/tests/ *(spec)*
- Move the crate-wide env lock into src/test_support.rs *(test)*
- Add a library target with a minimal public surface *(crate)*
- Move the rejected-document corpus into tests/fixtures/invalid *(spec)*
- Move the ocx subprocess surface into pipeline/ocx_cli *(pipeline)*
- Move target_registry down and close the command subtree *(pipeline)*
- Move pep440_sort_key beside the other version ordering *(filter)*
- Extract the remaining oversized test modules *(pipeline)*
- Split the ci renderer into per-concern modules *(generate)*
- Split the push driver into per-concern modules *(push)*
- Extract the notify test module into notify/tests/ *(notify)*
- Split validation, loading and platform keys out of spec.rs *(spec)*
- Split env sources and drift detection out of plan.rs *(plan)*
- Remove the three verified duplicate helpers

### Documentation

- Record why the push env lock pins its tests to one binary *(test)*
- Sync the module map with the new layout *(claude)*
- Drop fixed-defect references from the e2e-test skill *(claude)*
- Document the registry sync verb and registry.yml *(registry)*

### Fixed

- Strip CR from every jq capture in generated test scripts *(generate)*
- Cascade the platforms an earlier run published *(push)*
- Stamp env tags with build_timestamp *(plan)*
- Bound the PEP 440 releases the ocx version parser rejects *(filter)*
- Ship a `python` entrypoint in every composed env *(python)*
- Apply the opus review findings on the env-defect fixes *(review)*
- Close the defects the review round found *(registry)*
- Escape foreign tag strings on the message path *(registry)*
- Bound the blob read and the manifest recursion depth *(registry)*
- Start only the compose service the harness asked for *(test)*

### Release

- V0.5.4

## [0.5.3] - 2026-08-06

### Added

- Render a cascade repair workflow *(pipeline)*
- Retry the container image pull *(generate)*
- Optional scheduled auto-repair for the cascade workflow *(pipeline)*
- Optional schedule for the announce-from-registry workflow *(pipeline)*

### Documentation

- Drop the false download resumption claims

### Release

- V0.5.3

## [0.5.2] - 2026-08-03

### Documentation

- Document concurrency and the exec-bit normalisation *(mirror-yml)*
- Drop nonexistent pytest --no-build flag from single-test examples

### Fixed

- Make declared binaries executable in archive bundles *(pipeline)*
- Retry transient push failures up to max_retries *(pipeline)*
- Retry only exit-75 push failures per ocx 0.5.3 *(pipeline)*
- Never chmod through a symlink or a hard link *(pipeline)*
- Raise the push timeout to a hang backstop and name the budget *(pipeline)*
- Isolate acceptance registry from sibling compose projects *(test)*

### Release

- V0.5.2

## [0.5.1] - 2026-08-02

### Added

- Pin setup-ocx to the renderer ocx version *(generate)*
- Accept containers[].setup *(spec)*
- Build a leg's container image from setup *(ci)*

### Documentation

- Document containers[].setup and containers[].id *(mirror-yml)*

### Fixed

- Bump container-leg ocx to v0.5.2 *(generate)*
- Reject unknown platform and container fields *(spec)*
- Reject a trailing-backslash setup command *(spec)*

### Release

- V0.5.1

## [0.5.0] - 2026-07-31

### Added

- Push registry catalog description and logo *(publish)*
- One Discord message per published version (#10) *(notify)*
- Warn on build_timestamp none with cascade *(spec)*
- Unique work dirs per libc os_features variant *(pipeline)*
- Record OCI annotations on every published index *(push)*
- Publish mirrors to GHCR and announce them into the OCX index (#20) *(announce)*
- Run the container test matrix under each image's own libc (#25) *(ci)*
- Announce every registry tag into the index *(pipeline)*
- Render one workflow set per mirror spec *(generate)*
- Detect published metadata drift *(plan)*
- Patch published metadata without re-mirroring *(pipeline)*
- Emit a dispatch-only patch workflow per spec *(generate)*
- Bin_scan derives the published binaries claim from the bundle *(mirror)*
- Reject a bin_scan whose metadata gives the scan nowhere to look *(mirror)*
- Declare ocx-mirror's interface binary explicitly *(packaging)*
- Check the declared libc against the packaged binaries *(mirror)*

### Changed

- Nest package-mirroring commands under `package` *(cli)*
- One call formats and levels every announce report *(announce)*
- Keep the digest short-circuit when the spec declares binaries *(plan)*

### Documentation

- Record CLI namespace restructure; reserve `registry` *(adr)*
- Document build_timestamp and GC-safe publishing *(publishing)*
- Document libc os_features asset keys *(spec)*
- Fleet-rollout handover for the GHCR + index migration (#26) *(artifacts)*
- A customManager that matches nothing reports nothing (#28) *(artifacts)*
- Promote the unchecked-green rule out of R8 (#33) *(artifacts)*
- Document multi-spec mirror repositories *(reference)*
- Document the variants and metadata spec keys *(reference)*
- Document pipeline patch and metadata drift *(reference)*
- Warn that a bare ${installPath} PATH var scans to binaries: [] *(mirror)*
- The bare-${installPath} hazard is now rejected, not warned about *(mirror)*
- Name the asymmetric-archive case the per-file bin_scan check exists for *(mirror)*

### Fixed

- Bump setup-ocx pin to v1.3.0 for ocx 0.4.3 tar.gz assets *(ci)*
- Drive the argv env-leak test through the injected lookup *(test)*
- Credential the ghcr.io jobs that read and write the target (#22) *(ci)*
- Record the platform in the sidecar it writes (#23) *(prepare)*
- Spell the registry out in the identifier it hands ocx (#24) *(push)*
- Drop release_tag, a required field with no consumer (#27) *(spec)*
- The pipeline fixture has never parsed (#30) *(test)*
- Read what the announce did, not whether it exited *(push)*
- Announce-from-registry needs read, not write, on packages *(pipeline)*
- Keep the announce dry run out of the shared temp dir *(pipeline)*
- Trigger a spec's workflows on its extends chain *(generate)*
- Reject a tests script path that resolves to nothing *(generate)*
- Make a dry run unmistakable in the announce log *(announce)*
- Report what a run did, and make a dry run say it did nothing *(announce)*
- Report the patch-driven announce like every other announce *(patch)*
- Infer the repo root from the git repository, not the spec set *(generate)*
- The spec owns a declared binaries claim, everywhere *(mirror)*
- Round-2 findings — verify can fail again, patch refuses layout changes *(mirror)*
- Round-2 review findings — strict variant keys, honest resume docs *(mirror)*
- Relock the toolchain for ocx 0.5.0 *(ci)*

### Release

- V0.5.0

## [0.4.0] - 2026-06-12

### Added

- Add ocx-mirror prototype for mirroring GitHub releases to OCI registries
- Separate strip_components for rebundling and support multiple --version flags *(mirror)*
- Add package pull, ci export command, and setup-ocx GitHub Action *(ci)*
- Add package describe and package info commands
- Add bun and git-cliff mirrors, restructure mirror layout *(mirror)*
- Add per-platform strip_components config *(mirror)*
- Add generator-based url_index sources *(mirror)*
- Support tag-scoped index update *(index)*
- Add spec extends, --latest flag, and backfill order *(mirror)*
- Add --color flag with NO_COLOR/CLICOLOR support *(cli)*
- Add asset_type config with binary support and shfmt mirror *(mirror)*
- Enable parallel XZ compression by default *(compression)*
- Auto-detect progress indicators based on stderr TTY *(cli)*
- Add transfer progress bars to push and pull operations *(oci)*
- Add package variant support
- Per-platform asset_type override + lychee mirror *(mirror)*
- Multi-layer package push and pull (#20) *(package)* **BREAKING**
- Typed exit codes and error normalization *(cli)*
- Package entry points *(package)* **BREAKING**
- --build-timestamp + dev.ocx.sh continuous deploy
- Add ocx login and ocx logout commands *(cli)* **BREAKING**
- Decorated table output with per-column/cell styles *(cli)*
- Client-declared registry mirrors via [mirrors] config *(oci)*
- Pipeline subcommand + per-platform applicability + Discord/JUnit reporting *(mirror)*
- Drift guard ignores action-pin bumps; SHA-pin setup-ocx *(mirror)*

### Changed

- Rework table printer styling and clean up idioms *(cli)*
- Migrate to thiserror with typed subsystem errors *(error)*

### Documentation

- Add mkdocs-material site (index, getting started, CLI/spec/env reference)

### Fixed

- Clippy warning, test build target, and mirror test assertions
- Replace ring with aws-lc-rs to fix aarch64-pc-windows-msvc release build
- Verify file digest with manifest-declared algorithm *(mirror)*
- Harden config loader, fix error chain rendering, and extend exit-code coverage *(config,cli)*
- Make download tests fast and meaningful *(ocx-mirror)*
- Stop baking metadata.json into bundle content *(mirror)*
- Fail-safe target-registry reads in discover and sync *(mirror)*
- Stop prepare legs re-crawling the source (N+1 crawls) *(mirror)*

### Release

- V0.4.0
[0.5.6]: https://github.com/ocx-sh/ocx-mirror/compare/v0.5.5..v0.5.6
[0.5.5]: https://github.com/ocx-sh/ocx-mirror/compare/v0.5.4..v0.5.5
[0.5.4]: https://github.com/ocx-sh/ocx-mirror/compare/v0.5.3..v0.5.4
[0.5.3]: https://github.com/ocx-sh/ocx-mirror/compare/v0.5.2..v0.5.3
[0.5.2]: https://github.com/ocx-sh/ocx-mirror/compare/v0.5.1..v0.5.2
[0.5.1]: https://github.com/ocx-sh/ocx-mirror/compare/v0.5.0..v0.5.1
[0.5.0]: https://github.com/ocx-sh/ocx-mirror/compare/v0.4.0..v0.5.0
[0.4.0]: https://github.com/ocx-sh/ocx-mirror/tree/v0.4.0

