//! Tests of the bash plugin that put a fake `rtk` first on the `PATH`. That
//! changes the whole process's environment, so they live in their own test
//! binary and need cargo nextest, which gives every test its own process.
#![cfg(unix)]

use std::sync::Arc;
use std::time::Duration;

use maki_agent::AgentMode;
use maki_agent::ToolOutput;
use maki_agent::tools::ToolRegistry;
use maki_agent::tools::test_support::stub_ctx;
use maki_lua::PluginHost;
use serde_json::{Map, json};
use tempfile::tempdir;
use test_case::test_case;

#[path = "claude_code/support.rs"]
#[allow(dead_code)]
mod support;

use support::{BASH_SRC, environment, executable, within};

const BASH_TOOL: &str = "bash";
const DEADLINE: Duration = Duration::from_secs(60);
/// A fake `rtk` that rewrites every command into one printing @REWRITTEN@.
const REWRITING_RTK: &str =
    "#!/bin/sh\n[ \"$1\" = --version ] && exit 0\necho 'echo @REWRITTEN@'\n";
const REWRITTEN: &str = "rewritten-by-rtk";
const TWO_LINE_COMMAND: &str = "echo first\necho second";
const ONE_LINE_COMMAND: &str = "echo first";
const ONE_LINE_WITH_NEWLINE: &str = "echo first\n";
const SECOND_LINE_OUTPUT: &str = "second";

/// rtk rewrites single commands. A multiline script must execute exactly as the user approved it.
#[test_case(TWO_LINE_COMMAND, SECOND_LINE_OUTPUT ; "a_multi_line_command_runs_as_approved")]
#[test_case(ONE_LINE_COMMAND, REWRITTEN ; "a_single_line_is_rewritten")]
#[test_case(ONE_LINE_WITH_NEWLINE, REWRITTEN ; "a_single_line_ending_in_a_newline_is_rewritten")]
fn rtk_rewrites_a_single_line_only(command: &str, want: &str) {
    let tools = tempdir().unwrap();
    executable(
        tools.path(),
        "rtk",
        &REWRITING_RTK.replace("@REWRITTEN@", REWRITTEN),
    );
    let _path = environment::EnvVar::prepend_path(tools.path());
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source_with_opts(BASH_TOOL, BASH_SRC, Map::new())
        .unwrap();
    let mut ctx = stub_ctx(&AgentMode::Build);
    ctx.config.rtk = true;
    let input = json!({ "command": command, "description": "a command" });
    let inv = reg.get(BASH_TOOL).unwrap().tool.parse(&input).unwrap();

    let output = smol::block_on(within(DEADLINE, inv.execute(&ctx)))
        .output
        .unwrap();

    let (ToolOutput::Plain(reply) | ToolOutput::Markdown(reply)) = output else {
        panic!("incorrect output: {output:?}");
    };
    let text = reply.text;
    assert!(text.contains(want), "{text}");
}
