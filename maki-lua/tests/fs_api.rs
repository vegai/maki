use std::fs;
#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};
use std::sync::Arc;

use maki_agent::tools::ToolRegistry;
use maki_lua::PluginHost;
use tempfile::tempdir;

#[cfg(unix)]
use maki_storage::permissions::{PERMISSION_BITS, PRIVATE_DIR_MODE};

const NATIVE_DIRECTORIES: &str = r#"
local dir = assert(maki.fs.mkdtemp(@TEMPLATE@))
assert(maki.fs.realpath(dir) == dir)
assert(maki.fs.write(maki.fs.joinpath(dir, "kept"), "data"))
local removed, err = maki.fs.rm(dir)
assert(removed == nil and err ~= nil)
assert(maki.fs.read(maki.fs.joinpath(dir, "kept")) == "data")
assert(maki.fs.rm(maki.fs.joinpath(dir, "kept")))
assert(maki.fs.rm(dir))
"#;

fn setup() -> PluginHost {
    PluginHost::new(Arc::new(ToolRegistry::new())).unwrap()
}

#[test]
fn native_directory_cleanup_preserves_files() {
    let base = tempdir().unwrap();
    let template = serde_json::to_string(&base.path().join("private.XXXXXX")).unwrap();
    setup()
        .load_source(
            "native_dirs",
            &NATIVE_DIRECTORIES.replace("@TEMPLATE@", &template),
        )
        .unwrap();
    assert_eq!(fs::read_dir(base.path()).unwrap().count(), 0);
}

#[cfg(unix)]
#[test]
fn native_temp_directories_are_private_and_realpath_follows_links() {
    let base = tempdir().unwrap();
    let link = base.path().join("link");
    symlink(base.path(), &link).unwrap();
    let source = format!(
        r#"
local physical = assert(maki.fs.realpath({link}))
assert(physical == {base})
local dir = assert(maki.fs.mkdtemp(maki.fs.joinpath(physical, "private.XXXXXX")))
assert(maki.fs.write({output}, dir))
"#,
        link = serde_json::to_string(&link).unwrap(),
        base = serde_json::to_string(&base.path().canonicalize().unwrap()).unwrap(),
        output = serde_json::to_string(&base.path().join("created")).unwrap()
    );
    setup().load_source("native_private", &source).unwrap();
    let created = fs::read_to_string(base.path().join("created")).unwrap();
    assert_eq!(
        fs::metadata(created).unwrap().permissions().mode() & PERMISSION_BITS,
        PRIVATE_DIR_MODE
    );
}
