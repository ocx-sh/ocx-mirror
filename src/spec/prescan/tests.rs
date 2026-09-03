// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `prescan` test modules, one per C-005 job.
//!
//! Each child reaches the parent module's private items through
//! `use super::super::*;` and the shared fixtures through `use super::support::*;`.

#[path = "tests/support.rs"]
mod support;

#[path = "tests/credentials.rs"]
mod credentials;
#[path = "tests/index_userinfo.rs"]
mod index_userinfo;
#[path = "tests/kind.rs"]
mod kind;
#[path = "tests/mirror.rs"]
mod mirror;
