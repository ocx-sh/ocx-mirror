// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `ci` test modules, split by concern.
//!
//! Each child reaches the parent module's private items through
//! `use super::super::*;` and the cross-module helpers through
//! `use super::support::*;`. `#[path]` resolves a module's children
//! against the directory holding its own file, so each child names its
//! own path — that keeps the corpus in `tests/` rather than beside the
//! production modules.

#[path = "tests/support.rs"]
mod support;

#[path = "tests/container_legs.rs"]
mod container_legs;
#[path = "tests/credentials.rs"]
mod credentials;
#[path = "tests/describe.rs"]
mod describe;
#[path = "tests/drift.rs"]
mod drift;
#[path = "tests/env_sources.rs"]
mod env_sources;
#[path = "tests/ghcr.rs"]
mod ghcr;
#[path = "tests/golden.rs"]
mod golden;
#[path = "tests/multi_spec.rs"]
mod multi_spec;
#[path = "tests/notify.rs"]
mod notify;
#[path = "tests/script_paths.rs"]
mod script_paths;
#[path = "tests/test_entries.rs"]
mod test_entries;
