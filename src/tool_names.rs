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

    use super::{TOOL_NAME_MAP, claude_to_maki_tool_name, maki_to_claude_tool_name};

    #[test_case(TOOL_NAME_MAP ; "all_mapped_tools")]
    fn tool_names_round_trip(names: &[(&str, &str)]) {
        for &(maki, claude) in names {
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
