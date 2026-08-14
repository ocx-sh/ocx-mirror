// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `catalog` test modules, one per contract group.
//!
//! Each child reaches the parent module's private items through
//! `use super::super::*;` and the shared loopback server through
//! `use super::support::*;`.

#[path = "tests/support.rs"]
mod support;

#[path = "tests/client.rs"]
mod client;
#[path = "tests/fetch.rs"]
mod fetch;
#[path = "tests/ssrf.rs"]
mod ssrf;
