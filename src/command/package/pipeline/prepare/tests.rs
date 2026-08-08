// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `prepare` test modules, split by concern.
//!
//! Each child reaches the parent module's private items through
//! `use super::super::*;` and the cross-module helpers through
//! `use super::support::*;`. `#[path]` resolves a module's children
//! against the directory holding its own file, so each child names its
//! own path — that keeps the corpus in `tests/` rather than beside the
//! production modules.

#[path = "tests/support.rs"]
mod support;

#[path = "tests/plan_based.rs"]
mod plan_based;
#[path = "tests/pylock_env.rs"]
mod pylock_env;
#[path = "tests/pypi_env.rs"]
mod pypi_env;
#[path = "tests/subcommand.rs"]
mod subcommand;
