use serde::Serialize;

pub const NOTIFICATION: &str = "notification";
const PRIORITY: &str = "low";
const PROVIDER_WAITING: &str = "provider_waiting";

#[derive(Serialize)]
pub struct ProviderNotice<'a> {
    key: String,
    text: &'a str,
    priority: &'static str,
}

impl<'a> ProviderNotice<'a> {
    pub fn new(message: Option<&'a str>, parent: Option<&str>) -> Self {
        Self {
            key: match parent {
                Some(id) => format!("{PROVIDER_WAITING}:{id}"),
                None => PROVIDER_WAITING.to_owned(),
            },
            text: message.unwrap_or_default(),
            priority: PRIORITY,
        }
    }
}
