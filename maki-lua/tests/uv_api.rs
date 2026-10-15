use std::fs;
#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};
use std::sync::Arc;

use tempfile::tempdir;

use maki_agent::tools::ToolRegistry;
use maki_lua::{Permission, PluginHost, PluginPermissions};

const PERMISSION_DENIED_SUBSTR: &str = "permission denied";
const CWD: &str = "maki.uv.cwd()";
const HOMEDIR: &str = "maki.uv.os_homedir()";
const GETENV: &str = r#"maki.uv.os_getenv("HOME")"#;
const ENVIRON: &str = "maki.uv.os_environ()";
/// What `maki.uv.os_uname()` calls this system.
#[cfg(target_os = "linux")]
const SYSNAME: &str = "Linux";
#[cfg(target_os = "macos")]
const SYSNAME: &str = "Darwin";
#[cfg(windows)]
const SYSNAME: &str = "Windows_NT";

fn setup() -> PluginHost {
    let reg = Arc::new(ToolRegistry::new());
    PluginHost::new(reg).unwrap()
}

#[test]
fn os_getenv_returns_nil_for_missing_var() {
    let host = setup();
    host.load_source(
        "getenv_missing",
        r#"
        local val = maki.uv.os_getenv("MAKI_TEST_VAR_DOES_NOT_EXIST_12345")
        assert(val == nil, "unset var should return nil, got: " .. tostring(val))
        "#,
    )
    .unwrap();
}

/// Cargo and nextest set this variable, so two nil values cannot satisfy the assertion.
#[test]
fn os_environ_agrees_with_os_getenv() {
    setup()
        .load_source(
            "environ",
            r#"local env = maki.uv.os_environ()
            assert(env.CARGO_MANIFEST_DIR ~= nil, "CARGO_MANIFEST_DIR has no value")
            assert(env.CARGO_MANIFEST_DIR == maki.uv.os_getenv("CARGO_MANIFEST_DIR"), "the two values are different")"#,
        )
        .unwrap();
}

/// Needs no permission, because it describes the system, not the user or
/// the host.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[test]
fn os_uname_names_the_system_without_permissions() {
    setup()
        .load_source_with_permissions(
            "uname",
            &format!(
                r#"local u = maki.uv.os_uname()
                assert(u.sysname == "{SYSNAME}", "sysname: " .. tostring(u.sysname))
                assert(type(u.machine) == "string" and u.machine ~= "", "machine: " .. tostring(u.machine))"#
            ),
            PluginPermissions::denied(),
        )
        .unwrap();
}

fn load_with(permission: Permission, chunk: &str) -> Result<(), maki_lua::PluginError> {
    let mut perms = PluginPermissions::denied();
    perms.set(permission, true);
    setup().load_source_with_permissions("uv_perm", chunk, perms)
}

#[test_case::test_case(Permission::FsRead, CWD, "string" ; "fs_read_cwd")]
#[test_case::test_case(Permission::FsRead, HOMEDIR, "string" ; "fs_read_homedir")]
#[test_case::test_case(Permission::Env, GETENV, "string" ; "env_getenv")]
#[test_case::test_case(Permission::Env, ENVIRON, "table" ; "env_environ")]
fn the_permission_the_call_needs_is_enough(permission: Permission, call: &str, kind: &str) {
    let chunk = format!(
        r#"local value = {call}
        assert(type(value) == "{kind}", "the value must be a {kind}, but it is: " .. tostring(value))"#
    );
    load_with(permission, &chunk).unwrap();
}

/// Asking where a file lives must not cost a plugin the key to every secret in
/// the environment, so the two guards do not stand in for each other.
#[test_case::test_case(Permission::Env, CWD, Permission::FsRead ; "env_alone_misses_cwd")]
#[test_case::test_case(Permission::Env, HOMEDIR, Permission::FsRead ; "env_alone_misses_homedir")]
#[test_case::test_case(Permission::FsRead, GETENV, Permission::Env ; "fs_read_alone_misses_getenv")]
#[test_case::test_case(Permission::FsRead, ENVIRON, Permission::Env ; "fs_read_alone_misses_environ")]
fn a_neighbouring_permission_does_not_carry_over(held: Permission, call: &str, needed: Permission) {
    let err = load_with(held, call)
        .expect_err("the guarded call must fail")
        .to_string();
    assert!(err.contains(PERMISSION_DENIED_SUBSTR), "got: {err}");
    assert!(err.contains(&format!("'{needed}'")), "got: {err}");
}

const NATIVE_DIRECTORIES: &str = r#"
local dir = assert(maki.uv.fs_mkdtemp(@TEMPLATE@))
assert(maki.uv.fs_realpath(dir) == dir)
assert(maki.fs.write(maki.fs.joinpath(dir, "kept"), "data"))
local removed, err = maki.uv.fs_rmdir(dir)
assert(removed == nil and err ~= nil)
assert(maki.fs.read(maki.fs.joinpath(dir, "kept")) == "data")
assert(maki.fs.rm(maki.fs.joinpath(dir, "kept")))
assert(maki.uv.fs_rmdir(dir))
"#;

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
local physical = assert(maki.uv.fs_realpath({link}))
assert(physical == {base})
local dir = assert(maki.uv.fs_mkdtemp(maki.fs.joinpath(physical, "private.XXXXXX")))
assert(maki.fs.write({output}, dir))
"#,
        link = serde_json::to_string(&link).unwrap(),
        base = serde_json::to_string(&base.path().canonicalize().unwrap()).unwrap(),
        output = serde_json::to_string(&base.path().join("created")).unwrap()
    );
    setup().load_source("native_private", &source).unwrap();
    let created = fs::read_to_string(base.path().join("created")).unwrap();
    assert_eq!(
        fs::metadata(created).unwrap().permissions().mode() & 0o777,
        0o700
    );
}
