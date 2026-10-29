use maki_providers::claude_code::{
    CONFIG_DIR_OPTION, DEFAULT_EXECUTABLE, DEFAULT_MAX_CONCURRENT, EXECUTABLE_OPTION,
    MAX_CONCURRENT_OPTION, MAX_EVENT_BYTES, MIN_MAX_CONCURRENT, STARTUP_TIMEOUT_SECS,
};
use mlua::{Result as LuaResult, Table};

pub(crate) const MODULE: &str = "maki.claude_code.internal";

pub(super) fn add_defaults(table: &Table) -> LuaResult<()> {
    table.set("startup_timeout_secs", STARTUP_TIMEOUT_SECS)?;
    table.set("max_event_bytes", MAX_EVENT_BYTES)?;
    table.set("default_executable", DEFAULT_EXECUTABLE)?;
    table.set("default_provider_max_concurrent", DEFAULT_MAX_CONCURRENT)?;
    table.set("min_provider_max_concurrent", MIN_MAX_CONCURRENT)?;
    table.set("executable_option", EXECUTABLE_OPTION)?;
    table.set("config_dir_option", CONFIG_DIR_OPTION)?;
    table.set("max_concurrent_option", MAX_CONCURRENT_OPTION)
}
