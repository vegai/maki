use serde_json::{Map, Value};

use super::UNSUPPORTED_SYSTEM;
use crate::provider::Provider;
use crate::{AgentError, Timeouts};

pub fn configure_plugin(_enabled: bool, _options: Option<&Map<String, Value>>) {}

pub(super) fn create(_timeouts: Timeouts) -> Result<Box<dyn Provider>, AgentError> {
    Err(AgentError::Config {
        message: UNSUPPORTED_SYSTEM.to_owned(),
    })
}
