//! Logging initialisation helpers built around the `tracing` ecosystem.

use std::error::Error as StdError;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use thiserror::Error;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::{LogFormat, LoggerConfig};

/// Failures that can occur while installing logging/tracing primitives.
#[derive(Debug, Error)]
pub enum LoggingError {
    #[error("invalid log level {level}")]
    InvalidLevel {
        level: String,
        #[source]
        source: tracing_subscriber::filter::ParseError,
    },
    #[error("failed to install tracing subscriber")]
    Install(#[source] Box<dyn StdError + Send + Sync>),
    #[error("failed to initialise log file at {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Install a `tracing` subscriber matching the supplied [`LoggerConfig`].
pub fn init_tracing(config: &LoggerConfig) -> Result<(), LoggingError> {
    match config.format {
        LogFormat::Json => init_json_tracing(config),
        LogFormat::Text => init_text_tracing(config),
    }
}

fn init_json_tracing(config: &LoggerConfig) -> Result<(), LoggingError> {
    let filter =
        EnvFilter::try_new(&config.level).map_err(|source| LoggingError::InvalidLevel {
            level: config.level.clone(),
            source,
        })?;

    let build_layer = || {
        fmt::layer()
            .with_target(true)
            .with_thread_ids(true)
            .with_thread_names(true)
            .with_file(true)
            .with_line_number(true)
            .json()
            .with_current_span(true)
            .flatten_event(true)
            .with_span_list(true)
            .with_ansi(false)
    };

    match config.file.as_ref() {
        Some(path) => {
            let layer = build_layer();
            tracing_subscriber::registry()
                .with(filter.clone())
                .with(layer.with_writer(open_log_file(path)?).with_ansi(false))
                .try_init()
                .map_err(|err| LoggingError::Install(Box::new(err)))?;
        }
        None => {
            let layer = build_layer();
            tracing_subscriber::registry()
                .with(filter)
                .with(layer)
                .try_init()
                .map_err(|err| LoggingError::Install(Box::new(err)))?;
        }
    }

    Ok(())
}

fn init_text_tracing(config: &LoggerConfig) -> Result<(), LoggingError> {
    let filter =
        EnvFilter::try_new(&config.level).map_err(|source| LoggingError::InvalidLevel {
            level: config.level.clone(),
            source,
        })?;

    let build_layer = || {
        fmt::layer()
            .with_target(true)
            .with_thread_ids(true)
            .with_thread_names(true)
            .with_file(true)
            .with_line_number(true)
    };

    match config.file.as_ref() {
        Some(path) => {
            let layer = build_layer();
            tracing_subscriber::registry()
                .with(filter.clone())
                .with(layer.with_writer(open_log_file(path)?).with_ansi(false))
                .try_init()
                .map_err(|err| LoggingError::Install(Box::new(err)))?;
        }
        None => {
            let layer = build_layer();
            tracing_subscriber::registry()
                .with(filter)
                .with(layer)
                .try_init()
                .map_err(|err| LoggingError::Install(Box::new(err)))?;
        }
    }

    Ok(())
}

/// Create (and append to) the configured log file, ensuring parent directories exist.
fn open_log_file(path: &Path) -> Result<Mutex<File>, LoggingError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| LoggingError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|source| LoggingError::Io {
            path: path.to_path_buf(),
            source,
        })?;

    Ok(Mutex::new(file))
}
