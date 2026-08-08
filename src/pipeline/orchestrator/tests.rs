// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `orchestrator` test modules, split by concern.
//!
//! Each child reaches the parent module's private items through
//! `use super::super::*;` and the cross-module helpers through
//! `use super::support::*;`. `#[path]` resolves a module's children
//! against the directory holding its own file, so each child names its
//! own path — that keeps the corpus in `tests/` rather than beside the
//! production modules.

#[path = "tests/support.rs"]
mod support;

#[path = "tests/bin_scan.rs"]
mod bin_scan;
#[path = "tests/libc_lint.rs"]
mod libc_lint;
