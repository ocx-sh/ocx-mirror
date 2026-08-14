// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `plan` test modules, split by concern.
//!
//! Each child reaches the parent module's private items through
//! `use super::super::*;` and the cross-module helpers through
//! `use super::support::*;`. `#[path]` resolves a module's children against
//! the directory holding its own file, so each child names its own path.

#[path = "tests/support.rs"]
mod support;

#[path = "tests/estimate.rs"]
mod estimate;
#[path = "tests/expand.rs"]
mod expand;
#[path = "tests/short_circuit.rs"]
mod short_circuit;
