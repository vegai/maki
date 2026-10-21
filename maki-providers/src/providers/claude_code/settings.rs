use serde_json::{Value, json};

pub const EMPTY_MCP_CONFIG: &str = r#"{"mcpServers":{}}"#;
pub const STARTUP_TIMEOUT_SECS: u64 = 60;
const BASE_ARGS: &[&str] = &[
    "--print",
    "--input-format",
    "stream-json",
    "--output-format",
    "stream-json",
    "--verbose",
    "--no-session-persistence",
    "--restricted",
    "--permission-prompts",
    "none",
    "--setting-sources",
    "",
    "--strict-mcp-config",
];

pub fn base_args(model: &str, tools: &str, mode: &str, settings: &str) -> Vec<String> {
    let mut args: Vec<_> = BASE_ARGS.iter().map(|arg| (*arg).to_owned()).collect();
    args.extend(
        [
            "--model",
            model,
            "--tools",
            tools,
            "--permission-mode",
            mode,
            "--settings",
            settings,
        ]
        .map(str::to_owned),
    );
    args
}

pub(crate) fn base_settings() -> Value {
    json!({ "disableAllHooks": true, "autoMemoryEnabled": false, "claudeMdExcludes": ["**"], "disableClaudeAiConnectors": true })
}
