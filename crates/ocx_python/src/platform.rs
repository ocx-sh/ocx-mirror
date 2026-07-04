// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Platform & axis model: L1 wheel-tag→facts, L2 facts→OCX encoding, and
//! marker-environment derivation.
//!
//! A Python target is 5-axis — `(os, arch, libc{family,floor}, python, abi)` —
//! but an OCX platform carries only os/arch. The mapping is layered
//! (design spec, "Platform & axis model"):
//!
//! - **L1** ([`parse_platform_tag`]): a PEP 425/600/656 wheel platform tag →
//!   [`PlatformFacts`]. Frozen in code, identical across every namespace
//!   writer — this is what the crate protects.
//! - **L2** ([`encode_l2`]): [`PlatformFacts`]-derived target → an OCX
//!   [`Platform`](ocx_lib::oci::Platform) plus a variant tag prefix.
//!   Grammar-versioned in code ([`L2_GRAMMAR_VERSION`]), never user config.
//! - **L3**: the user-facing spec surface (platform keys + variant
//!   constraints) — lives in the mirror, not here.
//!
//! [`marker_environment`] derives the PEP 508 evaluation environment from the
//! L1 facts and the interpreter pin; `select` feeds it to `uv-pep508`.

use ocx_lib::oci::Platform;

/// The grammar version of the L2 facts→OCX encoding.
///
/// v1 encodes os/arch into the OCX platform key and libc/ABI into a mirror
/// variant tag prefix. The planned v2 (`+libc.gnu` platform grammar) moves
/// libc into the platform key itself; a v1→v2 migration is a republish. L1
/// facts are stable across both.
pub const L2_GRAMMAR_VERSION: u32 = 1;

/// The operating-system axis of a Python target.
///
/// Mirrors [`ocx_lib::oci::OperatingSystem`]'s supported set; kept as an
/// `ocx_python`-owned enum so the L1 fact table does not depend on OCX's
/// serialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TargetOperatingSystem {
    /// Linux (`manylinux` / `musllinux` wheel tags).
    Linux,
    /// macOS (`macosx` wheel tags).
    Darwin,
    /// Windows (`win_*` wheel tags).
    Windows,
}

/// The CPU-architecture axis of a Python target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TargetArchitecture {
    /// x86-64 (`x86_64` / `amd64` / `AMD64` wheel-tag spellings).
    Amd64,
    /// AArch64 (`aarch64` / `arm64` wheel-tag spellings).
    Arm64,
}

/// A dynamic-link libc family with a versioned floor (Linux only).
///
/// Both families are dynamic-link with per-family floors: PEP 600 glibc ≥ X.Y
/// (`manylinux`) and PEP 656 musl ≥ X.Y (`musllinux` — NOT static musl).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LibcFamily {
    /// glibc (`manylinux` tags).
    Gnu,
    /// musl (`musllinux` tags).
    Musl,
}

/// A libc family together with its minimum version floor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibcConstraint {
    /// The libc family.
    pub family: LibcFamily,
    /// The minimum floor, in wheel-tag spelling (`"2_28"` for `manylinux_2_28`,
    /// `"1_2"` for `musllinux_1_2`).
    pub floor: String,
}

/// The Python implementation axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Implementation {
    /// CPython (`cp` ABI tags, `implementation_name == "cpython"`).
    CPython,
}

/// L1 facts extracted from a wheel platform tag (PEP 425/600/656).
///
/// Frozen in code: `manylinux_2_28_x86_64` → `{Linux, Amd64, gnu≥2.28}`,
/// `musllinux_1_2_aarch64` → `{Linux, Arm64, musl≥1.2}`,
/// `macosx_11_0_arm64` → `{Darwin, Arm64, os_version_min="11.0"}`,
/// `win_amd64` → `{Windows, Amd64}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformFacts {
    /// The operating-system axis.
    pub operating_system: TargetOperatingSystem,
    /// The CPU-architecture axis.
    pub architecture: TargetArchitecture,
    /// The libc constraint (Linux only; `None` for macOS/Windows).
    pub libc: Option<LibcConstraint>,
    /// The minimum OS version (macOS deployment target, e.g. `"11.0"`);
    /// `None` when the tag carries no OS-version floor.
    pub os_version_min: Option<String>,
}

/// The variant-constraint vocabulary, bounded to L1 fact fields (design spec,
/// "Variant constraint vocabulary").
///
/// A variant is a bounded set of L1-fact constraints, never a free-form tag
/// regex: `default = {libc: gnu, min_manylinux: "2_28"}`,
/// `musl = {libc: musl, min_musllinux: "1_2"}`, `cp313t = {abi: "cp313t"}`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VariantConstraints {
    /// The required libc family (Linux). `None` leaves it unconstrained.
    pub libc: Option<LibcFamily>,
    /// The minimum `manylinux` floor (e.g. `"2_28"`), when `libc` is glibc.
    pub min_manylinux: Option<String>,
    /// The minimum `musllinux` floor (e.g. `"1_2"`), when `libc` is musl.
    pub min_musllinux: Option<String>,
    /// A required ABI override (e.g. `"cp313t"` for free-threaded CPython).
    /// `None` means the interpreter pin's primary ABI.
    pub abi: Option<String>,
}

/// The interpreter pin: the `python`/`abi` axes of the target.
///
/// Sourced from the interpreter package in the lock / spec `python:` block.
/// Drives both marker-environment derivation and the ABI-consistency check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterpreterPin {
    /// `python_version` marker value (major.minor, e.g. `"3.13"`).
    pub python_version: String,
    /// `python_full_version` marker value (major.minor.patch, e.g. `"3.13.1"`).
    pub python_full_version: String,
    /// The primary ABI tag (e.g. `"cp313"`, or `"cp313t"` when free-threaded).
    pub abi: String,
    /// The Python implementation.
    pub implementation: Implementation,
}

/// The os/arch "platform key" of a target — what an L3 platform key selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TargetPlatform {
    /// The operating-system axis.
    pub operating_system: TargetOperatingSystem,
    /// The CPU-architecture axis.
    pub architecture: TargetArchitecture,
}

/// A fully specified selection target: one `(variant, platform key)` pair plus
/// the interpreter pin. One `PythonTarget` = one env composition = one
/// selection run.
///
/// Defined here (the platform/axis module) because its fields are all
/// platform-domain types; `select` and `compose` both consume it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PythonTarget {
    /// The os/arch key selecting the OCX platform.
    pub platform: TargetPlatform,
    /// The variant's L1-fact constraints (libc family, floors, ABI override).
    pub variant: VariantConstraints,
    /// The interpreter pin (python/abi axes).
    pub interpreter: InterpreterPin,
}

/// The derived PEP 508 marker environment for evaluating package markers.
///
/// An `ocx_python`-owned struct (not `uv-pep508`'s type) so the derivation
/// table is the stable, versioned contract; `select` converts it into the
/// `uv-pep508` runtime type internally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkerEnvironment {
    /// `python_version` (major.minor).
    pub python_version: String,
    /// `python_full_version` (major.minor.patch).
    pub python_full_version: String,
    /// `sys_platform` (`"linux"` / `"darwin"` / `"win32"`).
    pub sys_platform: String,
    /// `platform_machine` (`"x86_64"` / `"aarch64"` / `"arm64"` / `"AMD64"`).
    pub platform_machine: String,
    /// `platform_system` (`"Linux"` / `"Darwin"` / `"Windows"`).
    pub platform_system: String,
    /// `os_name` (`"posix"` / `"nt"`).
    pub os_name: String,
    /// `implementation_name` (`"cpython"`).
    pub implementation_name: String,
    /// `platform_python_implementation` (`"CPython"`).
    pub platform_python_implementation: String,
}

/// The L2 v1 encoding of a target: the OCX platform plus a variant tag prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OcxPlatformEncoding {
    /// The OCX platform (os/arch) for the Image Index entry.
    pub platform: Platform,
    /// The mirror variant tag prefix, or `None` for the default (unadorned)
    /// chain. `Some("musl")` / `Some("cp313t")` etc. for non-default variants.
    pub variant_prefix: Option<String>,
}

/// **L1**: parses a wheel platform tag into [`PlatformFacts`].
///
/// Resolves `any` / `py2.py3` / `abi3` and libc floors via `uv-platform-tags`
/// compatibility semantics, never string equality.
///
/// # Errors
///
/// Returns [`PlatformError::UnsupportedTag`] for a tag whose OS/arch is outside
/// OCX's supported set (e.g. `s390x`, `ppc64le`) and
/// [`PlatformError::MalformedTag`] for a tag that does not parse.
pub fn parse_platform_tag(tag: &str) -> Result<PlatformFacts, PlatformError> {
    let _ = tag;
    unimplemented!("W1.2")
}

/// Derives the PEP 508 [`MarkerEnvironment`] for a target from its L1 facts and
/// interpreter pin.
///
/// Pure mapping over the versioned derivation table (design spec, wheel
/// selection algorithm step 1) — infallible.
pub fn marker_environment(facts: &PlatformFacts, interpreter: &InterpreterPin) -> MarkerEnvironment {
    let _ = (facts, interpreter);
    unimplemented!("W1.2")
}

/// **L2**: encodes a target into its OCX [`Platform`] and variant tag prefix.
///
/// v1 ([`L2_GRAMMAR_VERSION`]) maps os/arch to the platform key and libc/ABI to
/// the variant prefix (default = glibc + primary ABI, unadorned).
///
/// # Errors
///
/// Returns [`PlatformError::InvalidVariant`] when the variant constraints are
/// internally inconsistent (e.g. `libc: musl` with a `min_manylinux` floor).
pub fn encode_l2(target: &PythonTarget) -> Result<OcxPlatformEncoding, PlatformError> {
    let _ = target;
    unimplemented!("W1.2")
}

/// Errors from platform-tag parsing and L2 encoding.
///
/// Internal source type: never surfaced to the consumer directly — always
/// wrapped inside [`SelectError`](crate::select::SelectError) or
/// [`ComposeError`](crate::compose::ComposeError).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PlatformError {
    /// The tag's OS/architecture is outside OCX's supported set.
    #[error("unsupported wheel platform tag '{tag}'")]
    UnsupportedTag {
        /// The offending tag.
        tag: String,
    },
    /// The tag does not parse as a PEP 425/600/656 platform tag.
    #[error("malformed wheel platform tag '{tag}'")]
    MalformedTag {
        /// The offending tag.
        tag: String,
    },
    /// The variant constraints are internally inconsistent.
    #[error("invalid variant constraints: {reason}")]
    InvalidVariant {
        /// A short explanation of the inconsistency.
        reason: String,
    },
}
