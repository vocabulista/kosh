use std::error::Error as StdError;

use thiserror::Error;
use tracing_subscriber::{fmt, EnvFilter};

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
}

pub fn init_tracing(level: &str) -> Result<(), LoggingError> {
    let filter = EnvFilter::try_new(level).map_err(|source| LoggingError::InvalidLevel {
        level: level.to_owned(),
        source,
    })?;

    fmt()
        .with_env_filter(filter)
        .try_init()
        .map_err(LoggingError::Install)?;

    Ok(())
}
