// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Installing this binary's tracing subscriber.
//!
//! A **verbatim copy** of `external/ocx/crates/ocx_cli/src/tracing_init{.rs,
//! /log_level.rs,/log_settings.rs}`, with the builder methods this binary
//! never calls (`with_filter`, `with_console_filter`, `with_console_events`,
//! `console_events`, `init`) and `build_env_filter`'s `extra_name` /
//! `extra_filter` parameters dropped. Copied rather than rewritten because
//! the `OCX_LOG_CONSOLE` → `OCX_LOG` → `RUST_LOG` → INFO cascade is a
//! contract shared with `ocx`, and its asymmetric predicate (`OCX_LOG*`
//! honoured on merely being set, `RUST_LOG` only when non-empty) is the kind
//! of rule a paraphrase breaks silently.
//!
//! Why a copy at all: `tracing_init` lives in `ocx_cli`, whose package is the
//! `ocx` **application**. Linking it for these two types dragged 160 packages
//! — the whole Starlark host, an LSP/DAP/REPL stack and the gix family — into
//! this binary's build graph. Follow-up, upstream and not blocking: extract
//! those three files into a leaf crate `ocx_tracing` (`ocx_console` +
//! `tracing-subscriber` + `clap` + `log`), which violates neither C-030 (no
//! classification in it) nor D-009 (`ocx_console` still names no subscriber);
//! this file is deleted on the first submodule bump that carries it.
//!
//! Deciding *what* gets logged and *where it is written* is an application
//! concern: the verbosity flag is clap vocabulary, the `OCX_LOG` cascade is
//! this binary's contract, and only a process that owns `main` may install a
//! global subscriber at all. So the console library paints and suspends, and
//! the wiring that turns it into a `tracing_subscriber` layer lives here.
//!
//! [`ProgressLogWriter`] is the whole seam: it adapts the console's
//! bar-suspending writer to `MakeWriter` without the console knowing the trait
//! exists.

use ocx_console::ColorMode;
use ocx_console::progress::{LogWriter, LogWriterHandle, ProgressManager};

/// Adapts the console's bar-suspending stderr writer to `tracing_subscriber`.
///
/// The fmt layer asks for one writer per event; [`LogWriter::handle`] hands
/// back a buffer that flushes inside `MultiProgress::suspend` on drop, so a log
/// line never tears an active bar. The trait impl lives on this newtype rather
/// than on `LogWriter` itself because the trait is `tracing-subscriber`'s, and
/// a console must not depend on the subscriber it happens to be rendered
/// through.
pub struct ProgressLogWriter(pub LogWriter);

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for ProgressLogWriter {
    type Writer = LogWriterHandle;

    fn make_writer(&'a self) -> Self::Writer {
        self.0.handle()
    }
}

/// Log level for controlling the verbosity of logging output.
///
/// Implements `clap::ValueEnum` for use as a CLI flag (`--log-level`).
#[derive(Clone, Copy, Debug)]
pub enum LogLevel {
    /// Log everything, including very detailed information typically only useful for debugging
    Trace,
    /// Log detailed information, typically of interest only when diagnosing problems.
    Debug,
    /// Log informational messages, warnings, and errors
    Info,
    /// Only log warnings and errors
    Warn,
    /// Only log errors
    Error,
    /// Disable all logging output
    Off,
}

impl clap::ValueEnum for LogLevel {
    fn value_variants<'a>() -> &'a [Self] {
        &[Self::Trace, Self::Debug, Self::Info, Self::Warn, Self::Error, Self::Off]
    }

    fn to_possible_value(&self) -> Option<clap::builder::PossibleValue> {
        use clap::builder::PossibleValue;

        Some(match self {
            Self::Trace => PossibleValue::new("trace"),
            Self::Debug => PossibleValue::new("debug"),
            Self::Info => PossibleValue::new("info"),
            Self::Warn => PossibleValue::new("warn"),
            Self::Error => PossibleValue::new("error"),
            Self::Off => PossibleValue::new("off"),
        })
    }
}

impl From<LogLevel> for tracing_subscriber::filter::LevelFilter {
    fn from(val: LogLevel) -> Self {
        match val {
            LogLevel::Trace => tracing_subscriber::filter::LevelFilter::TRACE,
            LogLevel::Debug => tracing_subscriber::filter::LevelFilter::DEBUG,
            LogLevel::Info => tracing_subscriber::filter::LevelFilter::INFO,
            LogLevel::Warn => tracing_subscriber::filter::LevelFilter::WARN,
            LogLevel::Error => tracing_subscriber::filter::LevelFilter::ERROR,
            LogLevel::Off => tracing_subscriber::filter::LevelFilter::OFF,
        }
    }
}

/// Tracing subscriber configuration for this binary.
///
/// Supports the following environment variable cascade for log filtering:
/// `OCX_LOG_CONSOLE` → `OCX_LOG` → `RUST_LOG` → default level (INFO).
///
/// # Usage
///
/// ```ignore
/// LogSettings::default()
///     .with_console_level(log_level)
///     .with_stderr_color(color_config.stderr)
///     .init_with_progress(&progress)?;
/// ```
#[derive(Default, Debug, Clone)]
pub struct LogSettings {
    console_level: Option<LogLevel>,
    stderr_color: Option<bool>,
}

impl LogSettings {
    pub fn with_console_level(mut self, level: Option<LogLevel>) -> Self {
        self.console_level = level;
        self
    }

    pub fn with_stderr_color(mut self, enabled: bool) -> Self {
        self.stderr_color = Some(enabled);
        self
    }

    /// Initialize a tracing subscriber whose fmt layer writes through the
    /// given span-free [`ProgressManager`].
    ///
    /// Log lines are flushed inside `MultiProgress::suspend` so they never
    /// tear active progress bars. A disabled manager writes straight to
    /// stderr (the non-TTY path), so callers do not branch on TTY state —
    /// the manager already encodes it. There is no `tracing-indicatif`
    /// layer: progress is driven by RAII guards, not spans
    /// (ADR adr_progress_architecture).
    ///
    /// # Errors
    ///
    /// Returns an error if a global subscriber is already installed, or if
    /// the resolved env var holds an invalid filter directive.
    pub fn init_with_progress(
        self,
        progress: &ProgressManager,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use tracing_subscriber::{layer::SubscriberExt, prelude::*, util::SubscriberInitExt};

        let ansi = self.stderr_color.unwrap_or_else(|| ColorMode::Auto.config().stderr);
        let fmt_layer = tracing_subscriber::fmt::layer()
            .compact()
            .with_ansi(ansi)
            .with_file(false)
            .with_target(false)
            .with_writer(ProgressLogWriter(progress.writer()))
            .with_filter(self.build_env_filter()?);

        tracing_subscriber::registry()
            .with(fmt_layer)
            .try_init()
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    /// Build an `EnvFilter` using the OCX env var cascade.
    ///
    /// Checks in order: `OCX_LOG_CONSOLE` → `OCX_LOG` → `RUST_LOG` → default
    /// level. The predicate is deliberately asymmetric: `OCX_LOG_CONSOLE` and
    /// `OCX_LOG` win on merely being set (even to the empty string), `RUST_LOG`
    /// only when non-empty.
    ///
    /// A `console_level` (the `--log-level` flag) overrides the resolved
    /// variable entirely rather than seeding a default — which is why `main`
    /// passes `None` instead of `Some(Info)`.
    ///
    /// # Errors
    ///
    /// Returns an error if the resolved env var contains an invalid filter directive.
    fn build_env_filter(
        &self,
    ) -> Result<tracing_subscriber::filter::EnvFilter, Box<dyn std::error::Error + Send + Sync>> {
        let builder = tracing_subscriber::EnvFilter::builder();
        let builder = {
            if std::env::var("OCX_LOG_CONSOLE").is_ok() {
                builder.with_env_var("OCX_LOG_CONSOLE")
            } else if std::env::var("OCX_LOG").is_ok() {
                builder.with_env_var("OCX_LOG")
            } else if std::env::var("RUST_LOG").map(|v| !v.is_empty()).unwrap_or(false) {
                builder.with_env_var("RUST_LOG")
            } else {
                builder
            }
        };

        let log_level = self.console_level.map(tracing_subscriber::filter::LevelFilter::from);
        let builder = builder.with_default_directive(
            log_level
                .unwrap_or(tracing_subscriber::filter::LevelFilter::INFO)
                .into(),
        );

        if let Some(console_level) = self.console_level {
            let console_level: tracing_subscriber::filter::LevelFilter = console_level.into();
            Ok(builder.parse(console_level.to_string())?)
        } else {
            Ok(builder.from_env()?)
        }
    }
}
