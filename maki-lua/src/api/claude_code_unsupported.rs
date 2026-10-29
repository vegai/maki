use maki_providers::claude_code::UNSUPPORTED_SYSTEM;
use mlua::{Lua, Result as LuaResult, Table};

use super::claude_code_common::{MODULE, add_defaults};
use crate::plugin_permissions::PluginPermissions;

pub(crate) fn create_private_module(
    lua: &Lua,
    _perms: &PluginPermissions,
) -> LuaResult<(&'static str, Table)> {
    let table = lua.create_table()?;
    add_defaults(&table)?;
    table.set("unsupported", UNSUPPORTED_SYSTEM)?;
    Ok((MODULE, table))
}
