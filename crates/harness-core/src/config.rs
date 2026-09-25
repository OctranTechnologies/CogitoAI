use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{Error, Id};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessConfig {
    pub workspace_root: PathBuf,
    pub max_iterations: usize,
    pub log_level: String,
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            workspace_root: PathBuf::from("."),
            max_iterations: 32,
            log_level: "info".to_owned(),
        }
    }
}

impl HarnessConfig {
    pub fn validate(&self) -> Result<(), Error> {
        if self.max_iterations == 0 {
            return Err(Error::InvalidConfig {
                reason: "max_iterations must be greater than zero".to_owned(),
            });
        }
        if self.log_level.trim().is_empty() {
            return Err(Error::InvalidConfig {
                reason: "log_level must not be empty".to_owned(),
            });
        }
        Ok(())
    }
}

pub fn init_logging(level: &str) -> Result<(), Error> {
    let filter =
        tracing_subscriber::EnvFilter::try_new(level).map_err(|error| Error::InvalidConfig {
            reason: format!("invalid log level: {error}"),
        })?;
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .try_init()
        .map_err(|error| Error::Logging(error.to_string()))
}

pub fn session_id(value: &str) -> Result<Id, Error> {
    Id::new(value)
}
