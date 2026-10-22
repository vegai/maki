use maki_agent::DoneReason;
use maki_storage::id::SessionRef;
use serde::Serialize;
use uuid::Uuid;

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

#[allow(clippy::disallowed_methods)]
pub fn wire_uuid() -> String {
    Uuid::now_v7().to_string()
}

pub fn failure_subtype(reason: DoneReason) -> Option<&'static str> {
    match reason {
        DoneReason::MaxTurns => Some("error_max_turns"),
        DoneReason::Cancelled | DoneReason::Dropped => Some("error_during_execution"),
        DoneReason::EndTurn | DoneReason::MaxTokens | DoneReason::Compact => None,
    }
}
