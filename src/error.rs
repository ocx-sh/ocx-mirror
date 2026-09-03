// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use ocx_lib::cli::ExitCode;

#[derive(Debug)]
#[non_exhaustive]
pub enum MirrorError {
    /// Spec file has validation errors (YAML parse, schema, regex, etc.)
    SpecInvalid(Vec<String>),
    /// Spec file could not be read from disk.
    SpecNotFound(String),
    /// Runtime error during mirror execution (download, push, verify failures).
    ExecutionFailed(Vec<String>),
    /// Error fetching upstream version information from source (GitHub, URL index).
    SourceError(String),
    /// Error reading published state from the target registry (tag list,
    /// per-tag manifests). Fail-safe counterpart to `SourceError`: a
    /// transient target read failure must abort instead of classifying
    /// published versions as absent (issue #157).
    TargetError(String),

    // ── Pipeline variants (added in test-pipeline phase) ────────────────────
    /// Content-policy violation in `mirror.yml`: hardcoded webhook URL, empty
    /// `tests:` list, bad runner label, or ambiguous shell on non-standard image.
    /// Distinct from `SpecInvalid` (which is structural/schema) — this covers
    /// mirror-author configuration choices the renderer rejects by policy.
    SpecUsageError(String),
    /// `--check` mode detected drift between `mirror.yml` and generated files.
    RendererDrift(Vec<String>),
    /// A JUNIT XML file could not be parsed or is missing required attributes.
    JunitParseError(String),
    /// `run-summary.json` is missing, malformed, or has an unrecognised schema version.
    RunSummaryError(String),
    /// `plan.json` is missing, malformed, or does not carry the data the
    /// consuming subcommand needs (e.g. a `prepare --plan` invocation against
    /// a plan without resolved assets, or a version absent from the plan).
    PlanError(String),
    /// Template render failure or write failure for a generated file.
    TemplateError(String),
    /// Discord webhook returned 5xx or the request timed out.
    WebhookUnavailable(String),
    /// Discord webhook returned 401/403 — secret rotated or misconfigured.
    WebhookPermissionDenied(String),
    /// `ocx package cascade repair` ran and findings remain on the target
    /// repository. The audit outcome, not a failure of the repair — carries
    /// the target `<registry>/<repository>`.
    CascadeUnrepaired(String),
    /// A `pylock`-sourced mirror spec could not be resolved into plan
    /// entries: no locked package matches the spec's app name, an invalid
    /// platform/variant mapping, or `ocx_python::select_wheels` found no
    /// compatible wheel. The underlying cause is malformed spec/lock content,
    /// not a transient resource — same exit class as `SpecInvalid`. Covers
    /// W2.2's single `select_wheels` call site (the plan phase); the fuller
    /// `ocx_python` error-type wiring across all `resolve_assets` call sites
    /// (`LockError`/`SelectError`/`CollisionError`/`ComposeError`) is a
    /// separate follow-up.
    PylockError(String),
    /// A `source.type: pypi` discovery failure classified as malformed
    /// input: the PyPI JSON API returned 404 (package name not found on this
    /// index). Same exit class as `PylockError`/`SpecInvalid` — a genuinely
    /// unreachable index (connection refused, timeout, 5xx, malformed JSON
    /// body) stays `SourceError` (69). See `source::pypi::classify_error`.
    PypiError(String),

    // ── `registry sync` variants ────────────────────────────────────────────
    /// A write into the served index tree under `output:` failed — a root
    /// document, `c/index.json`, `config.json`, or a dispatch object.
    ///
    /// Not structurally forced: every existing variant carries a freeform
    /// `String`. It earns its own identity because the output tree is a
    /// distinct failure surface with a distinct remedy (fix the filesystem,
    /// re-run), and reusing `TemplateError` — which shares the exit code —
    /// would make the message lie about what failed.
    IndexWriteError(String),
    /// A source index declared a `config.json` `format_version` above the one
    /// `ocx_lib` supports; carries the version the source declared.
    ///
    /// Its own variant because the outcome is not transient: a source on a
    /// newer format stays on it, so classifying this as `SourceError` (69)
    /// would make CI retry forever on something retry can never fix. The run
    /// writes nothing.
    IndexFormatUnsupported(u64),

    // ── Signing variants ───────────────────────────────────────────────────
    /// An `ocx package push --sign` or `ocx package sign` child failed.
    ///
    /// Carries the child's own exit code rather than a message so
    /// [`Self::kind_exit_code`] can hand it straight back (C-056): the sign
    /// taxonomy is `ocx`'s, and collapsing 83 (Rekor down, retryable) onto 85
    /// (key backend not built) would tell an operator to fix the wrong thing.
    /// `target` names the reference — never a secret, which by construction
    /// never reaches an argv word or a message.
    SignFailed { target: String, code: i32 },
    /// A `sign:` ref names material that cannot be reached: an unset
    /// environment variable, or a file that is missing, unreadable, oversized
    /// or not UTF-8.
    ///
    /// `field` is the dotted spec field (`sign.key.passphrase`) and `source`
    /// the variable name or path — never the value (C-054), which is why this
    /// is a struct variant rather than a formatted `String`.
    SignMaterialMissing { field: String, source: String },

    // ── `dist sync` variants ────────────────────────────────────────────────
    /// The upstream `dist.json` declared a `schema` this binary cannot
    /// re-emit; carries the version the manifest declared.
    ///
    /// Its own variant for the same reason as
    /// [`Self::IndexFormatUnsupported`]: the outcome is not transient, so
    /// classifying it as `SourceError` (69) would have CI retry forever on
    /// something retry can never fix. A newer schema may reorder or re-nest
    /// what the jq-free installers parse positionally, so the run writes
    /// nothing rather than guessing.
    DistSchemaUnsupported(u64),
}

impl MirrorError {
    /// Map a [`MirrorError`] variant to its [`ExitCode`].
    ///
    /// `ExecutionFailed` is intentionally fixed to `Failure (1)` because the
    /// current variant carries `Vec<String>` (stringified error messages),
    /// not a structured inner error to delegate to. Refactoring the variant
    /// to carry `anyhow::Error` is tracked as a follow-up so per-cause exit
    /// codes can be surfaced through the mirror pipeline.
    pub fn kind_exit_code(&self) -> ExitCode {
        match self {
            Self::SpecInvalid(_) => ExitCode::DataError,
            Self::SpecNotFound(_) => ExitCode::NotFound,
            Self::ExecutionFailed(_) => ExitCode::Failure,
            Self::SourceError(_) => ExitCode::Unavailable,
            Self::TargetError(_) => ExitCode::Unavailable,
            // Pipeline variants
            Self::SpecUsageError(_) => ExitCode::UsageError,
            Self::RendererDrift(_) => ExitCode::DataError,
            Self::JunitParseError(_) => ExitCode::DataError,
            Self::RunSummaryError(_) => ExitCode::DataError,
            Self::PlanError(_) => ExitCode::DataError,
            Self::TemplateError(_) => ExitCode::IoError,
            Self::WebhookUnavailable(_) => ExitCode::Unavailable,
            Self::WebhookPermissionDenied(_) => ExitCode::PermissionDenied,
            Self::CascadeUnrepaired(_) => ExitCode::DataError,
            Self::PylockError(_) => ExitCode::DataError,
            Self::PypiError(_) => ExitCode::DataError,
            // `registry sync` variants
            Self::IndexWriteError(_) => ExitCode::IoError,
            Self::IndexFormatUnsupported(_) => ExitCode::DataError,
            // Signing variants
            Self::SignFailed { code, .. } => sign_exit_code(*code),
            Self::SignMaterialMissing { .. } => ExitCode::ConfigError,
            // `dist sync` variants
            Self::DistSchemaUnsupported(_) => ExitCode::DataError,
        }
    }
}

/// An `ocx` child's exit code, classified through `ocx`'s own taxonomy (C-056).
///
/// Written out rather than derived: `ExitCode` has no `TryFrom<u8>`, and a
/// blanket transmute would invent variants for the codes it deliberately leaves
/// unclaimed. Anything unrecognised — including 0, which a failing child cannot
/// have produced — falls through to `Failure`, so a newer `ocx` allocating a
/// code this binary predates degrades to 1 rather than to a wrong meaning.
pub(crate) fn sign_exit_code(code: i32) -> ExitCode {
    match code {
        c if c == ExitCode::UsageError as i32 => ExitCode::UsageError,
        c if c == ExitCode::DataError as i32 => ExitCode::DataError,
        c if c == ExitCode::Unavailable as i32 => ExitCode::Unavailable,
        c if c == ExitCode::IoError as i32 => ExitCode::IoError,
        c if c == ExitCode::TempFail as i32 => ExitCode::TempFail,
        c if c == ExitCode::PermissionDenied as i32 => ExitCode::PermissionDenied,
        c if c == ExitCode::ConfigError as i32 => ExitCode::ConfigError,
        c if c == ExitCode::NotFound as i32 => ExitCode::NotFound,
        c if c == ExitCode::AuthError as i32 => ExitCode::AuthError,
        c if c == ExitCode::PolicyBlocked as i32 => ExitCode::PolicyBlocked,
        c if c == ExitCode::DirtyRcBlock as i32 => ExitCode::DirtyRcBlock,
        c if c == ExitCode::TransparencyLogUnavailable as i32 => ExitCode::TransparencyLogUnavailable,
        c if c == ExitCode::ReferrersUnsupported as i32 => ExitCode::ReferrersUnsupported,
        c if c == ExitCode::UnsupportedKeyBackend as i32 => ExitCode::UnsupportedKeyBackend,
        _ => ExitCode::Failure,
    }
}

impl std::fmt::Display for MirrorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SpecInvalid(errors) => {
                writeln!(f, "invalid mirror spec:")?;
                for error in errors {
                    writeln!(f, "  - {error}")?;
                }
                Ok(())
            }
            Self::SpecNotFound(path) => write!(f, "mirror spec not found: {path}"),
            Self::ExecutionFailed(errors) => {
                writeln!(f, "mirror execution failed:")?;
                for error in errors {
                    writeln!(f, "  - {error}")?;
                }
                Ok(())
            }
            Self::SourceError(msg) => write!(f, "source error: {msg}"),
            Self::TargetError(msg) => write!(f, "target registry error: {msg}"),
            // Pipeline variants — lowercase, no trailing punctuation (quality-rust-errors.md)
            Self::SpecUsageError(msg) => write!(f, "mirror spec usage error: {msg}"),
            Self::RendererDrift(paths) => {
                writeln!(f, "renderer drift detected:")?;
                for path in paths {
                    writeln!(f, "  - {path}")?;
                }
                Ok(())
            }
            Self::JunitParseError(msg) => write!(f, "JUNIT parse error: {msg}"),
            Self::RunSummaryError(msg) => write!(f, "run-summary error: {msg}"),
            Self::PlanError(msg) => write!(f, "plan error: {msg}"),
            Self::TemplateError(msg) => write!(f, "template error: {msg}"),
            Self::WebhookUnavailable(msg) => write!(f, "webhook unavailable: {msg}"),
            Self::WebhookPermissionDenied(msg) => write!(f, "webhook permission denied: {msg}"),
            Self::CascadeUnrepaired(target) => write!(f, "cascade findings remain for {target}"),
            Self::PylockError(msg) => write!(f, "pylock error: {msg}"),
            Self::PypiError(msg) => write!(f, "pypi error: {msg}"),
            // `registry sync` variants
            Self::IndexWriteError(msg) => write!(f, "index write error: {msg}"),
            Self::IndexFormatUnsupported(version) => {
                write!(f, "unsupported source index format_version: {version}")
            }
            // Signing variants
            Self::SignFailed { target, code } => write!(f, "signing {target} failed with exit code {code}"),
            Self::SignMaterialMissing { field, source } => {
                write!(f, "signing material for {field} is unreachable: {source}")
            }
            // `dist sync` variants
            Self::DistSchemaUnsupported(schema) => {
                write!(f, "unsupported dist.json schema: {schema}")
            }
        }
    }
}

impl std::error::Error for MirrorError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_invalid_maps_to_data_error() {
        // Plan taxonomy: SpecInvalid → DataError (65) — spec content is malformed input.
        let err = MirrorError::SpecInvalid(vec!["invalid field 'foo'".into()]);
        assert_eq!(err.kind_exit_code(), ExitCode::DataError);
    }

    #[test]
    fn spec_not_found_maps_to_not_found() {
        // Plan taxonomy: SpecNotFound → NotFound (79) — spec file absent from disk.
        let err = MirrorError::SpecNotFound("mirror-cmake.yml".into());
        assert_eq!(err.kind_exit_code(), ExitCode::NotFound);
    }

    #[test]
    fn execution_failed_maps_to_failure() {
        // Plan taxonomy: ExecutionFailed → Failure (1).
        // Divergence from per-cause classification: the variant carries Vec<String>
        // (stringified error messages), not a structured inner error to delegate to.
        // Refactoring the variant to carry structured errors is a follow-up.
        let err = MirrorError::ExecutionFailed(vec!["download failed for cmake 3.28".into()]);
        assert_eq!(err.kind_exit_code(), ExitCode::Failure);
    }

    #[test]
    fn source_error_maps_to_unavailable() {
        // Plan taxonomy: SourceError → Unavailable (69) — upstream source unreachable.
        let err = MirrorError::SourceError("GitHub API returned 503".into());
        assert_eq!(err.kind_exit_code(), ExitCode::Unavailable);
    }

    #[test]
    fn plan_error_maps_to_data_error() {
        // Issue #160: PlanError → DataError (65) — plan.json input is malformed
        // or lacks resolved assets; same class as RunSummaryError.
        let err = MirrorError::PlanError("version '1.2.3' not present in plan".into());
        assert_eq!(err.kind_exit_code(), ExitCode::DataError);
    }

    #[test]
    fn cascade_unrepaired_maps_to_data_error() {
        // 65 is `ocx package cascade repair`'s own "findings remain" code, and
        // this variant exists to carry it through unchanged — a dispatch that
        // reported a broken cascade must not read as the tool failing (1).
        let err = MirrorError::CascadeUnrepaired("ghcr.io/ocx-sh/cmake".into());
        assert_eq!(err.kind_exit_code(), ExitCode::DataError);
    }

    #[test]
    fn pylock_error_maps_to_data_error() {
        // W2.2: select_wheels failures (no compatible wheel) surface via
        // PylockError → DataError (65) — malformed lock/spec content, not a
        // transient resource.
        let err = MirrorError::PylockError("no compatible wheel for 'numpy' on linux/amd64".into());
        assert_eq!(err.kind_exit_code(), ExitCode::DataError);
    }

    #[test]
    fn pypi_error_maps_to_data_error() {
        // plan_python_mirror_v2 W1.A1: a 404 from the PyPI JSON API (unknown
        // package name) is malformed input, not a transient resource — same
        // exit class as PylockError.
        let err = MirrorError::PypiError("package 'nonexistent' not found (404)".into());
        assert_eq!(err.kind_exit_code(), ExitCode::DataError);
    }

    #[test]
    fn index_write_error_maps_to_io_error() {
        // C-041: a failed write into the served `output:` tree is an I/O
        // failure (74), not a data one — the bytes were fine, the filesystem
        // refused them. It does not reuse `TemplateError` (also 74) because
        // the two name different remedies to the operator.
        let err = MirrorError::IndexWriteError("failed to write ./public/ocx.sh/c/index.json".into());
        assert_eq!(err.kind_exit_code(), ExitCode::IoError);
    }

    #[test]
    fn index_format_unsupported_maps_to_data_error() {
        // C-041: a source declaring a newer index format is malformed input
        // for this ocx (65), never `SourceError` (69) — 69 reads as transient
        // and would have CI retry forever on something retry cannot fix.
        let err = MirrorError::IndexFormatUnsupported(2);
        assert_eq!(err.kind_exit_code(), ExitCode::DataError);
    }

    /// C-056: the child's own sign taxonomy is carried through unchanged.
    ///
    /// Table rather than one assertion per code, because the failure this
    /// stops is a *collapse* — every arm answering `Failure` passes any
    /// single-code test written for a code that happens to be unmapped, and
    /// the operator then reads "the tool failed" for a Rekor outage, a
    /// registry with no Referrers API, and a key backend that was never built.
    #[test]
    fn a_failed_sign_carries_the_child_exit_code() {
        let cases = [
            (83, ExitCode::TransparencyLogUnavailable),
            (84, ExitCode::ReferrersUnsupported),
            (85, ExitCode::UnsupportedKeyBackend),
            (80, ExitCode::AuthError),
            (77, ExitCode::PermissionDenied),
            (78, ExitCode::ConfigError),
            (75, ExitCode::TempFail),
            (65, ExitCode::DataError),
            (64, ExitCode::UsageError),
            // Unrecognised — including a code a newer `ocx` may allocate —
            // degrades to 1 rather than to a wrong meaning.
            (0xFF, ExitCode::Failure),
            // 0 cannot come from a failing child; it must not read as success.
            (0, ExitCode::Failure),
        ];

        for (code, expected) in cases {
            let error = MirrorError::SignFailed {
                target: "ghcr.io/ocx-sh/shfmt:3.8.0".into(),
                code,
            };
            assert_eq!(error.kind_exit_code(), expected, "exit code {code}");
        }
    }

    /// The reference is named and nothing else is: the two secret-class values
    /// never reach this variant, so there is nothing here to redact.
    #[test]
    fn a_failed_sign_names_the_reference_and_the_code() {
        let rendered = MirrorError::SignFailed {
            target: "ghcr.io/ocx-sh/shfmt:3.8.0".into(),
            code: 83,
        }
        .to_string();
        assert!(rendered.contains("ghcr.io/ocx-sh/shfmt:3.8.0"), "got: {rendered}");
        assert!(rendered.contains("83"), "got: {rendered}");
    }

    /// C-054/C-055: the message names the field and the variable, and 78 sends
    /// the operator to the runner's configuration rather than to the spec's
    /// syntax (65) or to a generic failure (1).
    #[test]
    fn missing_sign_material_maps_to_config_error() {
        let error = MirrorError::SignMaterialMissing {
            field: "sign.keyless.identity_token".into(),
            source: "environment variable SIGSTORE_ID_TOKEN is not set".into(),
        };
        assert_eq!(error.kind_exit_code(), ExitCode::ConfigError);
        let rendered = error.to_string();
        assert!(rendered.contains("sign.keyless.identity_token"), "got: {rendered}");
        assert!(rendered.contains("SIGSTORE_ID_TOKEN"), "got: {rendered}");
    }

    #[test]
    fn target_error_maps_to_unavailable() {
        // Issue #157: TargetError → Unavailable (69) — target registry read
        // failed; the plan aborts instead of re-flagging published versions.
        let err = MirrorError::TargetError("registry returned 503".into());
        assert_eq!(err.kind_exit_code(), ExitCode::Unavailable);
    }
}
