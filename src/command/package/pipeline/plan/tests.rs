// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `plan` test modules, split by concern.
//!
//! Each child reaches the parent module's private items through
//! `use super::super::*;` and the cross-module helpers through
//! `use super::support::*;`. `#[path]` resolves a module's children
//! against the directory holding its own file, so each child names its
//! own path — that keeps the corpus in `tests/` rather than beside the
//! production modules.

#[path = "tests/support.rs"]
mod support;

#[path = "tests/bin_scan_drift.rs"]
mod bin_scan_drift;
#[path = "tests/metadata_drift.rs"]
mod metadata_drift;
#[path = "tests/pylock.rs"]
mod pylock;
#[path = "tests/pypi.rs"]
mod pypi;
#[path = "tests/report.rs"]
mod report;
