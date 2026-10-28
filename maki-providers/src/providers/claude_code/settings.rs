use serde_json::{Value, json};

pub const EMPTY_MCP_CONFIG: &str = r#"{"mcpServers":{}}"#;
pub const STARTUP_TIMEOUT_SECS: u64 = 60;
pub(super) const MCP_CONFIG_FLAG: &str = "--mcp-config";
const APPEND_SYSTEM_PROMPT_FLAG: &str = "--append-system-prompt";
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

pub(super) fn worker_args(
    model: &str,
    tools: &str,
    mode: &str,
    settings: &Value,
    contract: &str,
) -> Vec<String> {
    let mut args = base_args(model, tools, mode, &settings.to_string());
    args.extend(
        [
            MCP_CONFIG_FLAG,
            EMPTY_MCP_CONFIG,
            APPEND_SYSTEM_PROMPT_FLAG,
            contract,
        ]
        .map(str::to_owned),
    );
    args
}
