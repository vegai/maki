use serde_json::{Map, Value};
use thiserror::Error as ThisError;

use super::UNSUPPORTED_SYSTEM;
use crate::provider::Provider;
use crate::{AgentError, Timeouts};

#[derive(Debug, ThisError)]
pub enum Error {
    #[error("{UNSUPPORTED_SYSTEM}")]
    UnsupportedSystem,
}

pub fn configure_plugin(_enabled: bool, _options: Option<&Map<String, Value>>) {}

pub(super) fn create(_timeouts: Timeouts) -> Result<Box<dyn Provider>, AgentError> {
    Err(AgentError::Config {
        message: Error::UnsupportedSystem.to_string(),
    })
}
