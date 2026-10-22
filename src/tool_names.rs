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
