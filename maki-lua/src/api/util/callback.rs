use mlua::thread::ThreadStatus;
use mlua::{Function, IntoLuaMulti, Lua, Result as LuaResult};

const YIELDED: &str = "inline callbacks must finish without yielding";

pub(crate) fn call_inline(lua: &Lua, callback: Function, args: impl IntoLuaMulti) -> LuaResult<()> {
    let thread = lua.create_thread(callback)?;
    thread.resume::<()>(args)?;
    if thread.status() != ThreadStatus::Finished {
        return Err(mlua::Error::runtime(YIELDED));
    }
    Ok(())
}
