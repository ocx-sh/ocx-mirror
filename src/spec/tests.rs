// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `spec` test modules, split by concern.
//!
//! Each child reaches the parent module's private items through
//! `use super::super::*;` and the cross-module helpers through
//! `use super::support::*;`. `#[path]` resolves a module's children
//! against the directory holding its own file, so each child names its
//! own path — that keeps the corpus in `tests/` rather than beside the
//! production modules.

#[path = "tests/support.rs"]
mod support;

#[path = "tests/announce.rs"]
mod announce;
#[path = "tests/applicability.rs"]
mod applicability;
#[path = "tests/env_sources.rs"]
mod env_sources;
#[path = "tests/extends.rs"]
mod extends;
#[path = "tests/notify.rs"]
mod notify;
#[path = "tests/parse_sources.rs"]
mod parse_sources;
#[path = "tests/pipeline_schema.rs"]
mod pipeline_schema;
#[path = "tests/schema.rs"]
mod schema;
#[path = "tests/test_entries.rs"]
mod test_entries;
#[path = "tests/variants.rs"]
mod variants;
