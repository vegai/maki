//! Tests of the bash plugin that put a fake `rtk` first on the `PATH`. That
//! changes the whole process's environment, so they live in their own test
//! binary and need cargo nextest, which gives every test its own process.
#![cfg(unix)]

use std::env;
use std::fs;
use std::iter;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use maki_agent::AgentMode;
use maki_agent::ToolOutput;
use maki_agent::tools::ToolRegistry;
use maki_agent::tools::test_support::stub_ctx;
use maki_lua::PluginHost;
use serde_json::{Map, json};
use smol::Timer;
use smol::future::or;
use tempfile::tempdir;
use test_case::test_case;

const BASH_SRC: &str = include_str!("../../plugins/bash/init.lua");
const BASH_TOOL: &str = "bash";
/// Set by cargo nextest in every test process.
const NEXTEST: &str = "NEXTEST";
const PATH: &str = "PATH";
const OWNER_ONLY: u32 = 0o700;
const DEADLINE: Duration = Duration::from_secs(60);
/// A fake `rtk` that rewrites every command into one printing @REWRITTEN@.
const REWRITING_RTK: &str =
    "#!/bin/sh\n[ \"$1\" = --version ] && exit 0\necho 'echo @REWRITTEN@'\n";
const REWRITTEN: &str = "rewritten-by-rtk";
const TWO_LINE_COMMAND: &str = "echo first\necho second";
const ONE_LINE_COMMAND: &str = "echo first";
const ONE_LINE_WITH_NEWLINE: &str = "echo first\n";
const SECOND_LINE_OUTPUT: &str = "second";

fn put_first_on_path(dir: &Path) {
    assert!(
        env::var_os(NEXTEST).is_some(),
        "these tests change the process environment, so run them with cargo nextest, which gives each test its own process"
    );
    let path = env::var_os(PATH).unwrap_or_default();
    let entries = iter::once(dir.to_path_buf()).chain(env::split_paths(&path));
    // SAFETY: nextest runs each test in its own process, which the assert
    // checks, and the plugin host starts after the change.
    unsafe { env::set_var(PATH, env::join_paths(entries).unwrap()) };
}

/// rtk rewrites single commands. A multiline script must execute exactly as the user approved it.
#[test_case(TWO_LINE_COMMAND, SECOND_LINE_OUTPUT ; "a_multi_line_command_runs_as_approved")]
#[test_case(ONE_LINE_COMMAND, REWRITTEN ; "a_single_line_is_rewritten")]
#[test_case(ONE_LINE_WITH_NEWLINE, REWRITTEN ; "a_single_line_ending_in_a_newline_is_rewritten")]
fn rtk_rewrites_a_single_line_only(command: &str, want: &str) {
    let tools = tempdir().unwrap();
    let rtk = tools.path().join("rtk");
    fs::write(&rtk, REWRITING_RTK.replace("@REWRITTEN@", REWRITTEN)).unwrap();
    fs::set_permissions(&rtk, fs::Permissions::from_mode(OWNER_ONLY)).unwrap();
    put_first_on_path(tools.path());
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source_with_opts(BASH_TOOL, BASH_SRC, Map::new())
        .unwrap();
    let mut ctx = stub_ctx(&AgentMode::Build);
    ctx.config.rtk = true;
    let input = json!({ "command": command, "description": "a command" });
    let inv = reg.get(BASH_TOOL).unwrap().tool.parse(&input).unwrap();

    let expired = async {
        Timer::after(DEADLINE).await;
        None
    };
    let output = smol::block_on(or(async { Some(inv.execute(&ctx).await) }, expired))
        .unwrap_or_else(|| panic!("bash did not reply in {DEADLINE:?}"))
        .output
        .unwrap();

    let (ToolOutput::Plain(reply) | ToolOutput::Markdown(reply)) = output else {
        panic!("incorrect output: {output:?}");
    };
    let text = reply.text;
    assert!(text.contains(want), "{text}");
}
