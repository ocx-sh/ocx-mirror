# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
[0.4.0]: https://github.com/ocx-sh/ocx-mirror/tree/v0.4.0

