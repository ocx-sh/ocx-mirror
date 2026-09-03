// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `patch` test modules, split by concern.
//!
//! Each child reaches the parent module's private items through
//! `use super::super::*;` and the cross-module helpers through
//! `use super::support::*;`. `#[path]` resolves a module's children
//! against the directory holding its own file, so each child names its
//! own path — that keeps the corpus in `tests/` rather than beside the
//! production modules.

#[path = "tests/support.rs"]
mod support;

#[path = "tests/argv.rs"]
mod argv;
#[path = "tests/skip_gate.rs"]
mod skip_gate;
#[path = "tests/verdict.rs"]
mod verdict;
#[path = "tests/version_selection.rs"]
mod version_selection;
