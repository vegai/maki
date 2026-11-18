use maki_agent::DoneReason;
use maki_storage::id::SessionRef;
use serde::Serialize;
use uuid::Uuid;

pub const NOTIFICATION: &str = "notification";
const PRIORITY: &str = "low";
const PROVIDER_WAITING: &str = "provider_waiting";
pub const INIT: &str = "init";
pub const API_RETRY: &str = "api_retry";
pub const NOTICE: &str = "notice";
pub const SUCCESS: &str = "success";
pub const RESULT: &str = "result";
pub const ERROR_DURING_EXECUTION: &str = "error_during_execution";
const SYSTEM: &str = "system";
const ERROR_MAX_TURNS: &str = "error_max_turns";

const TOOL_NAME_MAP: &[(&str, &str)] = &[
    ("bash", "Bash"),
    ("read", "Read"),
    ("edit", "Edit"),
    ("write", "Write"),
    ("grep", "Grep"),
    ("glob", "Glob"),
    ("todo_write", "TodoWrite"),
    ("webfetch", "WebFetch"),
    ("websearch", "WebSearch"),
    ("task", "Task"),
    ("multiedit", "MultiEdit"),
    ("code_execution", "CodeExecution"),
    ("index", "Index"),
    ("memory", "Memory"),
    ("question", "Question"),
    ("skill", "Skill"),
];

pub fn maki_to_claude_tool_name(name: &str) -> &str {
    TOOL_NAME_MAP
        .iter()
        .find(|(local, _)| *local == name)
        .map(|(_, wire)| *wire)
        .unwrap_or(name)
}

pub fn claude_to_maki_tool_name(name: &str) -> &str {
    TOOL_NAME_MAP
        .iter()
        .find(|(_, wire)| *wire == name)
        .map(|(local, _)| *local)
        .unwrap_or(name)
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

/// A `system` event with no Claude Code equivalent. Without it a woken turn's assistant messages follow the last turn's with nothing
/// between them that says what woke it.
#[derive(Serialize)]
pub struct NoticeEvent<'a> {
    pub text: &'a str,
}

#[derive(Serialize)]
pub struct RetryEvent<'a> {
    pub attempt: u32,
    pub retry_delay_ms: u64,
    pub error: &'a str,
}

#[derive(Serialize)]
pub struct SystemEvent<'a, T> {
    #[serde(rename = "type")]
    event_type: &'static str,
    subtype: &'static str,
    #[serde(flatten)]
    extra: T,
    uuid: String,
    session_id: &'a SessionRef,
}

pub fn system_event<'a, T: Serialize>(
    session_id: &'a SessionRef,
    subtype: &'static str,
    extra: T,
) -> SystemEvent<'a, T> {
    SystemEvent {
        event_type: SYSTEM,
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
        DoneReason::MaxTurns => Some(ERROR_MAX_TURNS),
        DoneReason::Cancelled | DoneReason::Dropped => Some(ERROR_DURING_EXECUTION),
        DoneReason::EndTurn | DoneReason::MaxTokens | DoneReason::Compact => None,
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use std::collections::HashSet;

    use super::{TOOL_NAME_MAP, claude_to_maki_tool_name, maki_to_claude_tool_name};

    #[test]
    fn sdk_tool_names_are_unique_and_round_trip() {
        let mut seen = HashSet::new();
        for &(maki, claude) in TOOL_NAME_MAP {
            assert!(seen.insert(maki) && seen.insert(claude), "{maki}/{claude}");
            assert_eq!(maki_to_claude_tool_name(maki), claude);
            assert_eq!(claude_to_maki_tool_name(claude), maki);
        }
    }

    #[test_case("unknown_tool" ; "maki_spelling")]
    #[test_case("UnknownTool" ; "claude_spelling")]
    fn unknown_tool_names_pass_through(name: &str) {
        assert_eq!(maki_to_claude_tool_name(name), name);
        assert_eq!(claude_to_maki_tool_name(name), name);
    }
}
