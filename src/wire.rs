use maki_agent::DoneReason;
use maki_storage::id::SessionRef;
use serde::Serialize;
use uuid::Uuid;

pub const NOTIFICATION: &str = "notification";
const PRIORITY: &str = "low";
const PROVIDER_WAITING: &str = "provider_waiting";
pub const INIT: &str = "init";
pub const API_RETRY: &str = "api_retry";
pub const SUCCESS: &str = "success";
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

    use super::{claude_to_maki_tool_name, maki_to_claude_tool_name};

    #[test_case("bash", "Bash" ; "bash")]
    #[test_case("read", "Read" ; "read")]
    #[test_case("edit", "Edit" ; "edit")]
    #[test_case("write", "Write" ; "write")]
    #[test_case("grep", "Grep" ; "grep")]
    #[test_case("glob", "Glob" ; "glob")]
    #[test_case("todo_write", "TodoWrite" ; "todo_write")]
    #[test_case("webfetch", "WebFetch" ; "webfetch")]
    #[test_case("websearch", "WebSearch" ; "websearch")]
    #[test_case("task", "Task" ; "task")]
    #[test_case("multiedit", "MultiEdit" ; "multiedit")]
    #[test_case("code_execution", "CodeExecution" ; "code_execution")]
    #[test_case("index", "Index" ; "index")]
    #[test_case("memory", "Memory" ; "memory")]
    #[test_case("question", "Question" ; "question")]
    #[test_case("skill", "Skill" ; "skill")]
    fn sdk_tool_names_round_trip(maki: &str, claude: &str) {
        assert_eq!(maki_to_claude_tool_name(maki), claude);
        assert_eq!(claude_to_maki_tool_name(claude), maki);
    }

    #[test_case("unknown_tool" ; "maki_spelling")]
    #[test_case("UnknownTool" ; "claude_spelling")]
    fn unknown_tool_names_pass_through(name: &str) {
        assert_eq!(maki_to_claude_tool_name(name), name);
        assert_eq!(claude_to_maki_tool_name(name), name);
    }
}
