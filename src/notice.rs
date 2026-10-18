use maki_storage::id::SessionRef;
use serde::Serialize;
use uuid::Uuid;

pub const NOTIFICATION: &str = "notification";
const PRIORITY: &str = "low";
const PROVIDER_WAITING: &str = "provider_waiting";

#[derive(Serialize)]
struct SystemEvent<'a, T> {
    #[serde(rename = "type")]
    event_type: &'static str,
    subtype: &'static str,
    #[serde(flatten)]
    extra: T,
    uuid: String,
    session_id: &'a SessionRef,
}

pub fn system_event<T: Serialize>(
    session_id: &SessionRef,
    subtype: &'static str,
    extra: T,
) -> impl Serialize {
    SystemEvent {
        event_type: "system",
        subtype,
        extra,
        uuid: wire_uuid(),
        session_id,
    }
}

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

#[allow(clippy::disallowed_methods)]
pub fn wire_uuid() -> String {
    Uuid::now_v7().to_string()
}
