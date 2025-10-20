use std::error::Error as StdError;
use std::fs;
use std::fs::OpenOptions;
use std::path::PathBuf;
use std::sync::Mutex;

use thiserror::Error;
use tracing_subscriber::{fmt, EnvFilter};

use crate::LoggerConfig;

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

pub fn init_tracing(config: &LoggerConfig) -> Result<(), LoggingError> {
    let filter =
        EnvFilter::try_new(&config.level).map_err(|source| LoggingError::InvalidLevel {
            level: config.level.clone(),
            source,
        })?;

    if let Some(path) = &config.file {
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
                path: path.clone(),
                source,
            })?;

        fmt()
            .with_env_filter(filter)
            .with_writer(Mutex::new(file))
            .try_init()
            .map_err(LoggingError::Install)?;
    } else {
        fmt()
            .with_env_filter(filter)
            .try_init()
            .map_err(LoggingError::Install)?;
    }

    Ok(())
}
