// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `ocx-mirror registry` — reserved namespace (not yet implemented).
//!
//! Sibling to [`super::package`]. Will host registry-to-registry mirroring:
//! copying repositories/tags between OCI registries, and whole-index
//! mirroring of language ecosystems (e.g. all of PyPI) — as distinct from a
//! single upstream package, which is a `package` source type. Two registry
//! kinds are planned; their shape is deliberately unsettled and will be
//! pinned down in a dedicated ADR.
//!
//! No `RegistryCommand` is wired into the top-level [`crate::command::Command`]
//! enum yet: clap rejects an empty subcommand enum, and a live-but-empty verb
//! would be misleading. The first real registry subcommand introduces the
//! enum and its `Registry` arm.
//
// TODO(registry ADR): add RegistryCommand once the registry-mirroring design
// lands. See .claude/artifacts/adr_cli_namespace_restructure.md and issue #5.
