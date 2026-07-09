// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Platform & axis model: L1 wheel-tag→facts and marker-environment
//! derivation.
//!
//! A Python target is 5-axis — `(os, arch, libc{family,floor}, python, abi)` —
//! but an OCX platform carries only os/arch (+ an optional `libc.*`
//! `os.features` entry, owned by the consumer's platform key):
//!
//! - **L1** ([`parse_platform_tag`]): a PEP 425/600/656 wheel platform tag →
//!   [`PlatformFacts`]. Frozen in code, identical across every namespace
//!   writer — this is what the crate protects.
//! - The published platform is the consumer's (the mirror's) declared key —
//!   the spec `wheels:` map, including any `+libc.glibc`/`+libc.musl`
//!   `os.features` suffix. This crate never computes it from wheel contents.
//!
//! [`marker_environment`] derives the PEP 508 evaluation environment from the
//! L1 facts and the interpreter pin; `select` feeds it to `uv-pep508`.

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
    /// Ordered wheel platform-tag-prefix list (e.g. `["any"]`,
    /// `["manylinux", "any"]`). A NON-empty list is an **admissibility
    /// filter plus ranking**: `select`'s `pick_wheel` excludes any
    /// tag-compatible wheel
    /// whose platform tags match no listed prefix, and ranks survivors by the
    /// position of their highest-priority matching prefix (first-listed =
    /// most preferred) before falling back to `uv-platform-tags`' own
    /// `TagPriority`. The filter never re-admits a wheel excluded by the
    /// libc/floor constraints above — it only narrows and reorders wheels
    /// that already passed tag-compatibility. `None`/empty keeps today's
    /// TagPriority-only ordering, unchanged (backcompat; the mirror always
    /// passes a non-empty filter derived from its `wheels:` platform key).
    pub wheel_priority: Option<Vec<String>>,
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

impl PythonTarget {
    /// The effective ABI tag for this target: the variant override, else the
    /// interpreter pin's primary ABI.
    ///
    /// Single source of truth for both `select` (wheel ranking/ABI-consistency
    /// check) and `compose` (per-wheel ABI check) — a target whose variant
    /// overrides the ABI (e.g. free-threaded `cp313t`) must be judged by that
    /// override everywhere, not just where the interpreter pin happens to match.
    pub fn effective_abi(&self) -> &str {
        self.variant.abi.as_deref().unwrap_or(self.interpreter.abi.as_str())
    }
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

/// **L1**: parses a concrete wheel platform tag into [`PlatformFacts`].
///
/// Frozen fact table (PEP 425/600/656). Covers the PEP 600 `manylinux_X_Y_*`
/// and PEP 656 `musllinux_X_Y_*` grammars, the legacy `manylinux1/2010/2014`
/// aliases (mapped to their equivalent glibc floors), bare `linux_*`, `macosx_*`
/// (with a deployment-target floor), and `win_*`.
///
/// # Agnostic and non-platform tokens
///
/// `any`, `py2.py3`, and `abi3` carry **no** concrete os/arch/libc facts: `any`
/// is the platform-agnostic wildcard, `py2.py3` a Python-axis union tag, and
/// `abi3` an ABI-axis tag spanning CPython minors. They are resolved by
/// `select` through tag-compatibility semantics (an `any` wheel matches every
/// target), never by L1 fact-equality, so this function reports them as
/// [`PlatformError::AgnosticTag`] rather than inventing a platform. A target's
/// platform key is always concrete, so the real selection pipeline only ever
/// fact-parses concrete tags here.
///
/// # Errors
///
/// Returns [`PlatformError::UnsupportedTag`] for a tag whose OS/arch is outside
/// OCX's supported set (e.g. `s390x`, `ppc64le`, 32-bit `win32`, macOS
/// `universal2` fat binaries), [`PlatformError::AgnosticTag`] for the
/// agnostic/non-platform tokens above, and [`PlatformError::MalformedTag`] for a
/// tag that does not parse as a platform tag.
pub fn parse_platform_tag(tag: &str) -> Result<PlatformFacts, PlatformError> {
    // Agnostic wildcard + non-platform-axis tokens: no concrete facts.
    if matches!(tag, "any" | "abi3" | "py2.py3") {
        return Err(PlatformError::AgnosticTag { tag: tag.to_string() });
    }

    // PEP 600 glibc: manylinux_${major}_${minor}_${arch}.
    if let Some(rest) = tag.strip_prefix("manylinux_") {
        let (major, minor, arch) = split_versioned(rest, tag)?;
        return Ok(PlatformFacts {
            operating_system: TargetOperatingSystem::Linux,
            architecture: parse_linux_arch(arch, tag)?,
            libc: Some(LibcConstraint {
                family: LibcFamily::Gnu,
                floor: format!("{major}_{minor}"),
            }),
            os_version_min: None,
        });
    }

    // PEP 656 musl: musllinux_${major}_${minor}_${arch}.
    if let Some(rest) = tag.strip_prefix("musllinux_") {
        let (major, minor, arch) = split_versioned(rest, tag)?;
        return Ok(PlatformFacts {
            operating_system: TargetOperatingSystem::Linux,
            architecture: parse_linux_arch(arch, tag)?,
            libc: Some(LibcConstraint {
                family: LibcFamily::Musl,
                floor: format!("{major}_{minor}"),
            }),
            os_version_min: None,
        });
    }

    // Legacy glibc aliases (PEP 513/571/599): fixed floors, arch suffix only.
    for (prefix, floor) in [
        ("manylinux1_", "2_5"),
        ("manylinux2010_", "2_12"),
        ("manylinux2014_", "2_17"),
    ] {
        if let Some(arch) = tag.strip_prefix(prefix) {
            return Ok(PlatformFacts {
                operating_system: TargetOperatingSystem::Linux,
                architecture: parse_linux_arch(arch, tag)?,
                libc: Some(LibcConstraint {
                    family: LibcFamily::Gnu,
                    floor: floor.to_string(),
                }),
                os_version_min: None,
            });
        }
    }

    // Bare `linux_${arch}`: a valid PEP 425 platform tag with no manylinux /
    // musllinux libc guarantee (local build; PyPI rejects it for upload).
    if let Some(arch) = tag.strip_prefix("linux_") {
        return Ok(PlatformFacts {
            operating_system: TargetOperatingSystem::Linux,
            architecture: parse_linux_arch(arch, tag)?,
            libc: None,
            os_version_min: None,
        });
    }

    // macOS: macosx_${major}_${minor}_${arch}, floor = deployment target.
    if let Some(rest) = tag.strip_prefix("macosx_") {
        let (major, minor, arch) = split_versioned(rest, tag)?;
        return Ok(PlatformFacts {
            operating_system: TargetOperatingSystem::Darwin,
            architecture: parse_macos_arch(arch, tag)?,
            libc: None,
            os_version_min: Some(format!("{major}.{minor}")),
        });
    }

    // Windows: win_amd64 / win_arm64 (win32 = 32-bit x86, outside OCX's set).
    if tag == "win32" {
        return Err(PlatformError::UnsupportedTag { tag: tag.to_string() });
    }
    if let Some(arch) = tag.strip_prefix("win_") {
        return Ok(PlatformFacts {
            operating_system: TargetOperatingSystem::Windows,
            architecture: parse_win_arch(arch, tag)?,
            libc: None,
            os_version_min: None,
        });
    }

    Err(PlatformError::MalformedTag { tag: tag.to_string() })
}

/// Derives the PEP 508 [`MarkerEnvironment`] for a target from its L1 facts and
/// interpreter pin.
///
/// Pure mapping over the versioned derivation table (design spec, wheel
/// selection algorithm step 1) — infallible.
pub fn marker_environment(facts: &PlatformFacts, interpreter: &InterpreterPin) -> MarkerEnvironment {
    let os = facts.operating_system;
    // `sys_platform` / `platform_system` / `os_name` are pure OS-axis mappings;
    // `os_name` is `posix` for every Unix-like OS (Linux + Darwin), `nt` for
    // Windows.
    let (sys_platform, platform_system, os_name) = match os {
        TargetOperatingSystem::Linux => ("linux", "Linux", "posix"),
        TargetOperatingSystem::Darwin => ("darwin", "Darwin", "posix"),
        TargetOperatingSystem::Windows => ("win32", "Windows", "nt"),
    };
    let (implementation_name, platform_python_implementation) = match interpreter.implementation {
        Implementation::CPython => ("cpython", "CPython"),
    };
    MarkerEnvironment {
        python_version: interpreter.python_version.clone(),
        python_full_version: interpreter.python_full_version.clone(),
        sys_platform: sys_platform.to_string(),
        platform_machine: platform_machine(os, facts.architecture).to_string(),
        platform_system: platform_system.to_string(),
        os_name: os_name.to_string(),
        implementation_name: implementation_name.to_string(),
        platform_python_implementation: platform_python_implementation.to_string(),
    }
}

// ── L1 helpers (frozen table) ───────────────────────────────────────────────

/// Splits a `${major}_${minor}_${arch}` remainder (post-prefix) into its three
/// parts, keeping the arch intact even though it may itself contain `_`
/// (`x86_64`). Rejects a non-numeric or missing version component.
fn split_versioned<'a>(rest: &'a str, tag: &str) -> Result<(&'a str, &'a str, &'a str), PlatformError> {
    let mut parts = rest.splitn(3, '_');
    let (Some(major), Some(minor), Some(arch)) = (parts.next(), parts.next(), parts.next()) else {
        return Err(PlatformError::MalformedTag { tag: tag.to_string() });
    };
    let numeric = |component: &str| !component.is_empty() && component.bytes().all(|byte| byte.is_ascii_digit());
    if !numeric(major) || !numeric(minor) || arch.is_empty() {
        return Err(PlatformError::MalformedTag { tag: tag.to_string() });
    }
    Ok((major, minor, arch))
}

/// Maps a manylinux/musllinux/bare-linux arch token to OCX's supported set.
///
/// Valid manylinux arches outside the set (`i686`, `ppc64le`, `s390x`,
/// `armv7l`, …) are [`PlatformError::UnsupportedTag`].
fn parse_linux_arch(token: &str, tag: &str) -> Result<TargetArchitecture, PlatformError> {
    match token {
        "x86_64" => Ok(TargetArchitecture::Amd64),
        "aarch64" => Ok(TargetArchitecture::Arm64),
        _ => Err(PlatformError::UnsupportedTag { tag: tag.to_string() }),
    }
}

/// Maps a macOS arch token to OCX's supported set. `universal2` / `intel` /
/// `fat*` are multi-arch fat binaries — no single concrete arch, so they are
/// [`PlatformError::UnsupportedTag`] here and resolved by `select` via
/// tag-compat semantics against a concrete target.
fn parse_macos_arch(token: &str, tag: &str) -> Result<TargetArchitecture, PlatformError> {
    match token {
        "x86_64" => Ok(TargetArchitecture::Amd64),
        "arm64" => Ok(TargetArchitecture::Arm64),
        _ => Err(PlatformError::UnsupportedTag { tag: tag.to_string() }),
    }
}

/// Maps a Windows arch suffix (post-`win_`) to OCX's supported set.
fn parse_win_arch(token: &str, tag: &str) -> Result<TargetArchitecture, PlatformError> {
    match token {
        "amd64" => Ok(TargetArchitecture::Amd64),
        "arm64" => Ok(TargetArchitecture::Arm64),
        _ => Err(PlatformError::UnsupportedTag { tag: tag.to_string() }),
    }
}

/// The `platform_machine` marker value — OS-dependent: Linux reports
/// `x86_64`/`aarch64`, macOS `x86_64`/`arm64`, Windows `AMD64`/`ARM64`.
fn platform_machine(os: TargetOperatingSystem, arch: TargetArchitecture) -> &'static str {
    match (os, arch) {
        (TargetOperatingSystem::Windows, TargetArchitecture::Amd64) => "AMD64",
        (TargetOperatingSystem::Windows, TargetArchitecture::Arm64) => "ARM64",
        (TargetOperatingSystem::Linux, TargetArchitecture::Amd64)
        | (TargetOperatingSystem::Darwin, TargetArchitecture::Amd64) => "x86_64",
        (TargetOperatingSystem::Linux, TargetArchitecture::Arm64) => "aarch64",
        (TargetOperatingSystem::Darwin, TargetArchitecture::Arm64) => "arm64",
    }
}

/// Errors from platform-tag parsing.
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
    /// The tag is the platform-agnostic wildcard (`any`) or a Python/ABI-axis
    /// token (`py2.py3`, `abi3`) rather than a platform tag: it carries no
    /// concrete os/arch/libc facts. `select` resolves these by
    /// tag-compatibility semantics (an `any` wheel matches every target,
    /// `abi3` spans CPython minors), never by L1 fact-equality — so L1 fact
    /// parsing reports them here instead of inventing a platform.
    #[error("wheel platform tag '{tag}' carries no concrete platform facts")]
    AgnosticTag {
        /// The agnostic or non-platform-axis tag.
        tag: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gnu(floor: &str) -> Option<LibcConstraint> {
        Some(LibcConstraint {
            family: LibcFamily::Gnu,
            floor: floor.to_string(),
        })
    }

    fn musl(floor: &str) -> Option<LibcConstraint> {
        Some(LibcConstraint {
            family: LibcFamily::Musl,
            floor: floor.to_string(),
        })
    }

    // ── L1: parse_platform_tag — design-spec table rows ─────────────────────

    #[test]
    fn parse_manylinux_pep600_gnu_floor() {
        let facts = parse_platform_tag("manylinux_2_28_x86_64").unwrap();
        assert_eq!(
            facts,
            PlatformFacts {
                operating_system: TargetOperatingSystem::Linux,
                architecture: TargetArchitecture::Amd64,
                libc: gnu("2_28"),
                os_version_min: None,
            }
        );
    }

    #[test]
    fn parse_musllinux_pep656_musl_floor() {
        let facts = parse_platform_tag("musllinux_1_2_aarch64").unwrap();
        assert_eq!(
            facts,
            PlatformFacts {
                operating_system: TargetOperatingSystem::Linux,
                architecture: TargetArchitecture::Arm64,
                libc: musl("1_2"),
                os_version_min: None,
            }
        );
    }

    #[test]
    fn parse_macosx_carries_deployment_floor() {
        let facts = parse_platform_tag("macosx_11_0_arm64").unwrap();
        assert_eq!(
            facts,
            PlatformFacts {
                operating_system: TargetOperatingSystem::Darwin,
                architecture: TargetArchitecture::Arm64,
                libc: None,
                os_version_min: Some("11.0".to_string()),
            }
        );
        // Two-digit minor keeps its dotted spelling.
        let intel = parse_platform_tag("macosx_10_9_x86_64").unwrap();
        assert_eq!(intel.architecture, TargetArchitecture::Amd64);
        assert_eq!(intel.os_version_min.as_deref(), Some("10.9"));
    }

    #[test]
    fn parse_win_amd64_and_arm64() {
        let amd = parse_platform_tag("win_amd64").unwrap();
        assert_eq!(
            amd,
            PlatformFacts {
                operating_system: TargetOperatingSystem::Windows,
                architecture: TargetArchitecture::Amd64,
                libc: None,
                os_version_min: None,
            }
        );
        let arm = parse_platform_tag("win_arm64").unwrap();
        assert_eq!(arm.architecture, TargetArchitecture::Arm64);
    }

    #[test]
    fn parse_legacy_manylinux_aliases_map_to_glibc_floors() {
        for (tag, floor) in [
            ("manylinux1_x86_64", "2_5"),
            ("manylinux2010_x86_64", "2_12"),
            ("manylinux2014_aarch64", "2_17"),
        ] {
            let facts = parse_platform_tag(tag).unwrap();
            assert_eq!(facts.operating_system, TargetOperatingSystem::Linux);
            assert_eq!(
                facts.libc,
                gnu(floor),
                "legacy alias {tag} must map to glibc floor {floor}"
            );
        }
        // Legacy alias arch spelling is honored (aarch64 → Arm64).
        assert_eq!(
            parse_platform_tag("manylinux2014_aarch64").unwrap().architecture,
            TargetArchitecture::Arm64
        );
    }

    #[test]
    fn parse_bare_linux_has_no_libc_constraint() {
        let facts = parse_platform_tag("linux_x86_64").unwrap();
        assert_eq!(
            facts,
            PlatformFacts {
                operating_system: TargetOperatingSystem::Linux,
                architecture: TargetArchitecture::Amd64,
                libc: None,
                os_version_min: None,
            }
        );
    }

    #[test]
    fn parse_agnostic_and_non_platform_tokens() {
        for tag in ["any", "abi3", "py2.py3"] {
            let err = parse_platform_tag(tag).unwrap_err();
            assert!(
                matches!(err, PlatformError::AgnosticTag { tag: ref t } if t == tag),
                "{tag} must report AgnosticTag, got {err:?}"
            );
        }
    }

    #[test]
    fn parse_rejects_unsupported_arch() {
        for tag in [
            "manylinux_2_28_s390x",
            "manylinux2014_ppc64le",
            "musllinux_1_2_armv7l",
            "linux_i686",
            "win32",
            "macosx_11_0_universal2",
        ] {
            assert!(
                matches!(parse_platform_tag(tag), Err(PlatformError::UnsupportedTag { .. })),
                "{tag} must be UnsupportedTag"
            );
        }
    }

    #[test]
    fn parse_rejects_malformed_tags() {
        for tag in [
            "",
            "manylinux_2_x86_64",
            "manylinux_x_y_x86_64",
            "macosx_11_arm64",
            "solaris_amd64",
        ] {
            assert!(
                matches!(parse_platform_tag(tag), Err(PlatformError::MalformedTag { .. })),
                "{tag} must be MalformedTag"
            );
        }
    }

    // ── Marker environment ──────────────────────────────────────────────────

    fn cpython(version: &str, full: &str, abi: &str) -> InterpreterPin {
        InterpreterPin {
            python_version: version.to_string(),
            python_full_version: full.to_string(),
            abi: abi.to_string(),
            implementation: Implementation::CPython,
        }
    }

    #[test]
    fn marker_env_cpython_312_linux_x86_64() {
        let facts = PlatformFacts {
            operating_system: TargetOperatingSystem::Linux,
            architecture: TargetArchitecture::Amd64,
            libc: gnu("2_28"),
            os_version_min: None,
        };
        let env = marker_environment(&facts, &cpython("3.12", "3.12.1", "cp312"));
        assert_eq!(env.python_version, "3.12");
        assert_eq!(env.python_full_version, "3.12.1");
        assert_eq!(env.sys_platform, "linux");
        assert_eq!(env.platform_machine, "x86_64");
        assert_eq!(env.platform_system, "Linux");
        assert_eq!(env.os_name, "posix");
        assert_eq!(env.implementation_name, "cpython");
        assert_eq!(env.platform_python_implementation, "CPython");
    }

    #[test]
    fn marker_env_platform_machine_is_os_dependent() {
        let cases = [
            (TargetOperatingSystem::Linux, TargetArchitecture::Amd64, "x86_64"),
            (TargetOperatingSystem::Linux, TargetArchitecture::Arm64, "aarch64"),
            (TargetOperatingSystem::Darwin, TargetArchitecture::Amd64, "x86_64"),
            (TargetOperatingSystem::Darwin, TargetArchitecture::Arm64, "arm64"),
            (TargetOperatingSystem::Windows, TargetArchitecture::Amd64, "AMD64"),
            (TargetOperatingSystem::Windows, TargetArchitecture::Arm64, "ARM64"),
        ];
        for (os, arch, expected) in cases {
            let facts = PlatformFacts {
                operating_system: os,
                architecture: arch,
                libc: None,
                os_version_min: None,
            };
            let env = marker_environment(&facts, &cpython("3.13", "3.13.0", "cp313"));
            assert_eq!(env.platform_machine, expected, "platform_machine for {os:?}/{arch:?}");
        }
    }

    #[test]
    fn marker_env_windows_and_darwin_os_axis() {
        let win = PlatformFacts {
            operating_system: TargetOperatingSystem::Windows,
            architecture: TargetArchitecture::Amd64,
            libc: None,
            os_version_min: None,
        };
        let env = marker_environment(&win, &cpython("3.12", "3.12.1", "cp312"));
        assert_eq!(
            (
                env.sys_platform.as_str(),
                env.platform_system.as_str(),
                env.os_name.as_str()
            ),
            ("win32", "Windows", "nt")
        );

        let mac = PlatformFacts {
            operating_system: TargetOperatingSystem::Darwin,
            architecture: TargetArchitecture::Arm64,
            libc: None,
            os_version_min: Some("11.0".to_string()),
        };
        let env = marker_environment(&mac, &cpython("3.12", "3.12.1", "cp312"));
        assert_eq!(
            (
                env.sys_platform.as_str(),
                env.platform_system.as_str(),
                env.os_name.as_str()
            ),
            ("darwin", "Darwin", "posix")
        );
    }
}
