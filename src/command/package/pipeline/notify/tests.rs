// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `notify` test modules, split by concern.
//!
//! Each child reaches the parent module's private items through
//! `use super::super::*;` and the cross-module helpers through
//! `use super::support::*;`. `#[path]` resolves a module's children
//! against the directory holding its own file, so each child names its
//! own path — that keeps the corpus in `tests/` rather than beside the
//! production modules.

#[path = "tests/support.rs"]
mod support;

#[path = "tests/announce_row.rs"]
mod announce_row;
#[path = "tests/embeds.rs"]
mod embeds;
#[path = "tests/http.rs"]
mod http;
#[path = "tests/per_version.rs"]
mod per_version;
