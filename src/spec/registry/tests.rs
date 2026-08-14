// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `RegistrySpec` test modules — the schema (C-001…C-004), the validation
//! rules (C-006), and the loader (C-007).
//!
//! Each child reaches the parent module's private items through
//! `use super::super::*;` and the shared fixtures through `use super::support::*;`.
//!
//! The 65-class validation *corpus* is not here: it lives as one fixture per
//! rejected document under `tests/fixtures/invalid_registry/`, driven by
//! `tests/registry_spec_validation.rs`. What stays Rust is what a fixture loop
//! cannot express — specs that must be **valid**, the pairing where one added
//! source flips a verdict, and everything that goes through the loader.

#[path = "tests/support.rs"]
mod support;

#[path = "tests/load.rs"]
mod load;
#[path = "tests/types.rs"]
mod types;
#[path = "tests/validate.rs"]
mod validate;
