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
