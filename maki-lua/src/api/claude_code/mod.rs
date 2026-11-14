use crate::plugin_permissions::PluginPermissions;
#[cfg(any(test, not(target_os = "linux")))]
use maki_providers::claude_code::UNSUPPORTED_SYSTEM;
use maki_providers::claude_code::{
    CONFIG_DIR_OPTION, DEFAULT_EXECUTABLE, DEFAULT_MAX_CONCURRENT, EXECUTABLE_OPTION,
    HOME_STATE_PREFIX, MAX_CONCURRENT_OPTION, MAX_EVENT_BYTES, MIN_MAX_CONCURRENT,
    STARTUP_TIMEOUT_SECS,
};
use maki_providers::process::{STDERR_LINE_BYTES, STDERR_TAIL_LINES};
use mlua::{Lua, Result as LuaResult, Table};

pub(crate) const MODULE: &str = "maki.claude_code.internal";

fn add_defaults(table: &Table) -> LuaResult<()> {
    table.set("home_state_prefix", HOME_STATE_PREFIX)?;
    table.set("startup_timeout_secs", STARTUP_TIMEOUT_SECS)?;
    table.set("error_bytes", STDERR_LINE_BYTES * STDERR_TAIL_LINES)?;
    table.set("max_event_bytes", MAX_EVENT_BYTES)?;
    table.set("default_executable", DEFAULT_EXECUTABLE)?;
    table.set("default_provider_max_concurrent", DEFAULT_MAX_CONCURRENT)?;
    table.set("min_provider_max_concurrent", MIN_MAX_CONCURRENT)?;
    table.set("executable_option", EXECUTABLE_OPTION)?;
    table.set("config_dir_option", CONFIG_DIR_OPTION)?;
    table.set("max_concurrent_option", MAX_CONCURRENT_OPTION)
}

#[cfg(target_os = "linux")]
mod blobs;
#[cfg(target_os = "linux")]
mod fd;
#[cfg(target_os = "linux")]
mod native;
#[cfg(target_os = "linux")]
mod snapshot;

pub(crate) fn create_private_module(
    lua: &Lua,
    perms: &PluginPermissions,
) -> LuaResult<(&'static str, Table)> {
    #[cfg(target_os = "linux")]
    {
        let table = native::create_table(lua, perms)?;
        add_defaults(&table)?;
        Ok((MODULE, table))
    }
    #[cfg(not(target_os = "linux"))]
    create_unsupported_module(lua, perms)
}

#[cfg(any(test, not(target_os = "linux")))]
pub(crate) fn create_unsupported_module(
    lua: &Lua,
    _perms: &PluginPermissions,
) -> LuaResult<(&'static str, Table)> {
    let table = lua.create_table()?;
    table.set("unsupported", UNSUPPORTED_SYSTEM)?;
    add_defaults(&table)?;
    Ok((MODULE, table))
}
