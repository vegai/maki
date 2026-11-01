use crate::plugin_permissions::PluginPermissions;
#[cfg(any(test, not(target_os = "linux")))]
use maki_providers::claude_code::UNSUPPORTED_SYSTEM;
use maki_providers::claude_code::{
    CONFIG_DIR_OPTION, DEFAULT_EXECUTABLE, DEFAULT_MAX_CONCURRENT, EXECUTABLE_OPTION,
    MAX_CONCURRENT_OPTION, MAX_EVENT_BYTES, MIN_MAX_CONCURRENT, STARTUP_TIMEOUT_SECS,
};
use mlua::{Lua, Result as LuaResult, Table};

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

#[cfg(target_os = "linux")]
mod blobs;
#[cfg(target_os = "linux")]
mod native;
#[cfg(target_os = "linux")]
mod snapshot;

pub(crate) fn create_private_module(
    lua: &Lua,
    perms: &PluginPermissions,
) -> LuaResult<(&'static str, Table)> {
    #[cfg(target_os = "linux")]
    let table = native::create_table(lua, perms)?;
    #[cfg(not(target_os = "linux"))]
    let table = {
        let _ = perms;
        unsupported_table(lua)?
    };
    add_defaults(&table)?;
    Ok((MODULE, table))
}

#[cfg(any(test, not(target_os = "linux")))]
fn unsupported_table(lua: &Lua) -> LuaResult<Table> {
    let table = lua.create_table()?;
    table.set("unsupported", UNSUPPORTED_SYSTEM)?;
    Ok(table)
}

#[cfg(test)]
pub(crate) fn create_unsupported_module(
    lua: &Lua,
    _perms: &PluginPermissions,
) -> LuaResult<(&'static str, Table)> {
    let table = unsupported_table(lua)?;
    add_defaults(&table)?;
    Ok((MODULE, table))
}
