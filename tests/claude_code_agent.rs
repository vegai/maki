//! Use fixture config and state to isolate maki from developer settings. The fake CLI spends
//! no subscription quota.
//!
//! The live agent test uses the installed CLI and the developer's login. Run it only on request:
//!
//! `cargo nextest run -p maki --test claude_code_agent --run-ignored only -E 'test(/live_/)'`
#![cfg(target_os = "linux")]

use std::env;
use std::fs::{self, Permissions};
use std::io::Read;
use std::iter;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::Duration;

use serde_json::Value;
use test_case::test_case;
use wait_timeout::ChildExt;

#[path = "support/isolation.rs"]
mod isolation;
#[path = "support/wire.rs"]
#[allow(dead_code)]
mod wire;
use isolation::Isolation;
use wire::{INIT, JSON_FORMAT, RESULT, STREAM_FORMAT, SYSTEM};

const GLOSSARY: &str = "glossary.txt";
/// Only the glossary contains this value. A reply with it establishes that the subagent
/// read the glossary.
const MEANING: &str = "FIRST-LETTER-7";
const DENY_BASH: &str = "[bash]\ndeny = [\"*\"]\n";
const PROMPT: &str = "Do the two steps, in sequence, with your tools. Then reply.\n\
1. Run the shell command `echo hello` with the bash tool.\n\
2. Call the task tool with subagent_type \"research\". Tell it to read glossary.txt and to \
give the text for the word alpha there.\n\
Then tell if the command ran, and give the text that the subagent found, without a change.";
const DENIED: &str = "denied";
const MODEL: &str = "claude-code/claude-haiku-4-5";
const RUN_LIMIT: Duration = Duration::from_secs(300);
/// The global `init.lua` with the plugin on and @OPTIONS@ beside `enabled`.
const PLUGIN_ON: &str = "maki.setup({ plugins = { claude_code = { enabled = true@OPTIONS@ } } })\n";
/// A `claude` older than any version maki runs, so the provider stops at the
/// version check and nothing else runs.
/// The stand-in touches `claude.ran` next to itself whenever it runs.
const OLD_CLAUDE: &str = "#!/bin/sh\ntouch \"$0.ran\"\necho '1.0.0 (Claude Code)'\n";
const RAN_EXTENSION: &str = "ran";
const TOO_OLD: &str = "1.0.0 is older than";
const PLUGIN_OFF: &str = "enable the claude_code plugin in init.lua";
const OWNER_ONLY: u32 = 0o700;
const CLAUDE_CONFIG_ENV: &str = "CLAUDE_CONFIG_DIR";
const PATH: &str = "PATH";
const CLAUDE_TOOL: &str = "claude_code";
const IMPORT_TOOL: &str = "claude_code_import";
const REVIEW_INIT: &str = r#"maki.setup({ plugins = {
  claude_code = { enabled = true, executable = "@EXECUTABLE@" },
  bash = { enabled = false }, write = { enabled = false }, edit = { enabled = false },
  task = { enabled = false }, memory = { enabled = false }, code_execution = { enabled = false },
  batch = { enabled = false }, todo_write = { enabled = false }, question = { enabled = false },
  webfetch = { enabled = false }, websearch = { enabled = false }, skill = { enabled = false },
} })"#;
const REVIEW_TOOLS: [&str; 5] = ["glob", "grep", "index", "list", "read"];
const REVIEW_ALL_TOOLS: [&str; 7] = [
    CLAUDE_TOOL,
    IMPORT_TOOL,
    "glob",
    "grep",
    "index",
    "list",
    "read",
];
const UNMATCHED_TOOL_WARNING: &str = "no loaded tool matches";
const CLAUDE_TOOL_SUGGESTION: &str = "Did you mean 'claude_code'?";
/// A run's tool calls and their results, from its stream-json.
struct Run {
    events: Vec<Value>,
}

impl Run {
    /// Each `tool_use` block with the id of the subagent call it ran in, or
    /// `None` at the top level.
    fn calls(&self) -> impl Iterator<Item = (Option<&str>, &Value)> {
        self.blocks("assistant", "tool_use")
    }

    fn result_of(&self, id: &str) -> Option<&Value> {
        self.blocks("user", "tool_result")
            .map(|(_, block)| block)
            .find(|block| block["tool_use_id"] == id)
    }

    fn blocks<'a>(
        &'a self,
        kind: &'a str,
        block_type: &'a str,
    ) -> impl Iterator<Item = (Option<&'a str>, &'a Value)> {
        self.events
            .iter()
            .filter(move |event| event["type"] == kind)
            .flat_map(|event| {
                let parent = event["parent_tool_use_id"].as_str();
                event["message"]["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(move |block| (parent, block))
            })
            .filter(move |(_, block)| block["type"] == block_type)
    }

    fn result(&self) -> &Value {
        self.events
            .iter()
            .rfind(|event| event["type"] == RESULT)
            .expect("the run printed no result")
    }

    fn calls_made(&self) -> String {
        self.calls()
            .map(|(parent, call)| format!("{parent:?} {} {}", call["name"], call["input"]))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// maki's config, state and project in fixture directories, with bash denied
/// by a global rule. The claude_code plugin stays off until `enable_plugin`.
struct Fixture {
    isolation: Isolation,
}

impl Fixture {
    fn new() -> Self {
        let fixture = Self {
            isolation: Isolation::new(),
        };
        let project = fixture.isolation.dir("project");
        fs::write(project.join(GLOSSARY), format!("alpha: {MEANING}\n")).unwrap();
        let config = fixture.isolation.dir("config/maki");
        fs::write(config.join("permissions.toml"), DENY_BASH).unwrap();
        fixture
    }

    /// Turns the plugin on in the global `init.lua`. Given `executable`, the
    /// plugin and the provider both run it.
    fn enable_plugin(&self, executable: Option<&Path>) {
        let options = executable.map_or_else(String::new, |path| {
            format!(", executable = \"{}\"", path.display())
        });
        let init = PLUGIN_ON.replace("@OPTIONS@", &options);
        fs::write(self.isolation.dir("config/maki").join("init.lua"), init).unwrap();
    }

    fn maki(&self, model: &str, prompt: &str, format: &str) -> Command {
        let mut command = self.isolation.command();
        command
            .args(["-p", "--model", model, "--output-format", format, prompt])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }
}

fn run_maki(model: &str) -> Run {
    let fixture = Fixture::new();
    fixture.enable_plugin(None);
    let real_home = PathBuf::from(env::var_os("HOME").expect("HOME is set"));
    let claude_config =
        env::var_os(CLAUDE_CONFIG_ENV).map_or_else(|| real_home.join(".claude"), PathBuf::from);
    let mut command = fixture.maki(model, PROMPT, STREAM_FORMAT);
    command.env(CLAUDE_CONFIG_ENV, claude_config);
    let Output { stdout, stderr, .. } = finish(command);
    let stderr = String::from_utf8(stderr).unwrap();
    let events: Vec<Value> = stdout
        .split(|&byte| byte == b'\n')
        .filter_map(|line| serde_json::from_slice(line).ok())
        .collect();
    assert!(!events.is_empty(), "maki printed no events: {stderr}");
    Run { events }
}

/// Read both output streams concurrently so stderr cannot fill its pipe and block maki.
fn finish(mut command: Command) -> Output {
    let mut child = command.spawn().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });
    let err = thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });
    let status = child.wait_timeout(RUN_LIMIT).unwrap().unwrap_or_else(|| {
        let _ = child.kill();
        let _ = child.wait();
        panic!("maki did not complete in {RUN_LIMIT:?}");
    });
    Output {
        status,
        stdout: out.join().unwrap(),
        stderr: err.join().unwrap().into_bytes(),
    }
}

#[test]
fn dry_run_uses_provider_capabilities_and_filters_without_starting_claude() {
    let fixture = Fixture::new();
    let claude = stand_in(&fixture.isolation.dir("bin"));
    let init = REVIEW_INIT.replace("@EXECUTABLE@", &claude.to_string_lossy());
    fs::write(fixture.isolation.dir("config/maki").join("init.lua"), init).unwrap();
    let mut command = fixture.maki(MODEL, PROMPT, STREAM_FORMAT);
    command.args([
        "--dry-run",
        "--disallowed-tools",
        "claude_code,claude_code_import",
    ]);
    let output = finish(command);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let init: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(init["subtype"], INIT);
    assert_eq!(init["thinking"], "low");
    let mut tools: Vec<_> = init["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|name| name.as_str().unwrap())
        .collect();
    tools.sort_unstable();
    assert_eq!(tools, REVIEW_TOOLS);
    assert!(!claude.with_extension(RAN_EXTENSION).exists());
    let sessions = fixture.isolation.dir("state/maki").join("sessions");
    assert!(
        !sessions.exists()
            || !fs::read_dir(sessions).unwrap().any(|entry| entry
                .unwrap()
                .path()
                .extension()
                .is_some_and(|ext| ext == "jsonl"))
    );
}

#[test]
fn unavailable_scratch_fails_per_request_without_starting_claude() {
    let fixture = Fixture::new();
    let claude = stand_in(&fixture.isolation.dir("bin"));
    fixture.enable_plugin(Some(&claude));
    let mut command = fixture.maki(MODEL, PROMPT, STREAM_FORMAT);
    command.current_dir("/");
    let output = finish(command);
    assert!(!output.status.success());
    let events: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(events[0]["subtype"], INIT);
    let result = events.last().unwrap();
    assert_eq!(result["type"], RESULT);
    assert_eq!(result["is_error"], true);
    assert!(result[RESULT].as_str().unwrap().contains("Set TMPDIR"));
    assert!(!claude.with_extension(RAN_EXTENSION).exists());
}

#[test]
fn request_policy_does_not_prevent_provider_creation_or_dry_run() {
    let fixture = Fixture::new();
    let claude = stand_in(&fixture.isolation.dir("bin"));
    fixture.enable_plugin(Some(&claude));
    fs::write(
        fixture
            .isolation
            .dir("project/.claude")
            .join("settings.json"),
        r#"{"apiKeyHelper":"secret-command"}"#,
    )
    .unwrap();
    let mut command = fixture.maki(MODEL, PROMPT, STREAM_FORMAT);
    command.arg("--dry-run");
    let output = finish(command);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let init: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(init["subtype"], INIT);
    assert!(!claude.with_extension(RAN_EXTENSION).exists());
    let output = finish(fixture.maki(MODEL, PROMPT, STREAM_FORMAT));
    assert!(!output.status.success());
    let events: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(events[0]["subtype"], INIT);
    assert_eq!(events.last().unwrap()["is_error"], true);
    assert!(!claude.with_extension(RAN_EXTENSION).exists());
}

#[test_case(JSON_FORMAT, false ; "json_object")]
#[test_case(JSON_FORMAT, true ; "verbose_json_array")]
#[test_case("text", false ; "text_uses_stderr_only")]
#[test_case(STREAM_FORMAT, false ; "sdk_stream_json")]
fn startup_errors_keep_the_output_format(format: &str, verbose: bool) {
    let fixture = Fixture::new();
    let mut command = fixture.maki(MODEL, PROMPT, format);
    if verbose {
        command.arg("--verbose");
    }
    if format == STREAM_FORMAT {
        command.args(["--input-format", STREAM_FORMAT]);
    }
    let output = finish(command);
    assert!(!output.status.success());
    if format == "text" {
        assert!(output.stdout.is_empty());
    } else {
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        let result = if verbose {
            assert_eq!(value.as_array().unwrap().len(), 1);
            &value[0]
        } else {
            &value
        };
        assert_eq!(result["type"], RESULT);
        assert_eq!(result["is_error"], true);
    }
    assert!(String::from_utf8_lossy(&output.stderr).contains(PLUGIN_OFF));
}

fn named<'a>(run: &'a Run, name: &str, parent: Option<&str>) -> Option<&'a Value> {
    run.calls()
        .find(|(under, call)| *under == parent && call["name"] == name)
        .map(|(_, call)| call)
}

fn is_error(result: &Value) -> bool {
    result["is_error"] == true
}

fn text_of(result: &Value) -> String {
    match &result["content"] {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

#[test]
#[ignore = "runs the installed claude CLI on the subscription of the caller"]
fn live_the_agent_runs_its_tools_through_claude_code() {
    let run = run_maki(MODEL);
    let calls = run.calls_made();

    let bash = named(&run, "bash", None)
        .unwrap_or_else(|| panic!("the model did not call bash. Run the test again:\n{calls}"));
    let denial = run
        .result_of(bash["id"].as_str().unwrap())
        .expect("the bash call has no result");
    assert!(is_error(denial), "bash was not denied: {denial}");
    assert!(text_of(denial).contains(DENIED), "{denial}");

    let task = named(&run, "task", None)
        .unwrap_or_else(|| panic!("the model did not call task. Run the test again:\n{calls}"));
    let task_id = task["id"].as_str().unwrap();
    assert!(
        run.calls().any(|(under, _)| under == Some(task_id)),
        "the research subagent made no tool call:\n{calls}"
    );
    let found = run.result_of(task_id).expect("the task call has no result");
    assert!(!is_error(found), "{found}");
    assert!(text_of(found).contains(MEANING), "{found}");

    let result = run.result();
    assert_eq!(result["is_error"], false, "{result}");
    assert_eq!(
        result["total_cost_usd"].as_f64(),
        Some(0.0),
        "the subscription pays for each turn: {result}"
    );
    assert!(
        result["usage"]["output_tokens"]
            .as_u64()
            .is_some_and(|tokens| tokens > 0),
        "the turns still report their usage: {result}"
    );
}

/// Plugin enablement must control provider availability and executable selection before any
/// CLI process starts.
#[test_case(false, PLUGIN_OFF ; "without_the_plugin")]
#[test_case(true, TOO_OLD ; "with_the_plugin")]
fn the_provider_is_on_with_the_plugin(plugin: bool, want: &str) {
    let fixture = Fixture::new();
    let bin = fixture.isolation.dir("bin");
    let claude = stand_in(&bin);
    if plugin {
        fixture.enable_plugin(Some(&claude));
    }
    let caller_path = env::var_os(PATH).unwrap_or_default();
    let path = env::join_paths(iter::once(bin).chain(env::split_paths(&caller_path))).unwrap();
    let mut command = fixture.maki(MODEL, PROMPT, JSON_FORMAT);
    command.env(PATH, path);
    let Output { stdout, stderr, .. } = finish(command);
    let stderr = String::from_utf8(stderr).unwrap();
    assert_eq!(
        claude.with_extension(RAN_EXTENSION).exists(),
        plugin,
        "whether a claude started"
    );

    let result: Value =
        serde_json::from_slice(&stdout).unwrap_or_else(|_| panic!("there is no result: {stderr}"));
    assert_eq!(result["is_error"], true, "{result}");
    let reason = result[RESULT].as_str().unwrap_or_default();
    assert!(reason.contains(want), "got: {reason}");
}

#[test_case("--disallowed-tools", "claude_code,claude_code_import", &REVIEW_TOOLS, None ; "deny_plugin_tools_keeps_the_provider")]
#[test_case("--allowed-tools", "ClaudeCode", &[CLAUDE_TOOL], None ; "allow_a_plugin_tool")]
#[test_case("--allowed-tools", "claude_code_import", &[IMPORT_TOOL], None ; "allow_a_plugin_subtool")]
#[test_case("--allowed-tools", "view_image", &[], None ; "image_support_overrides_an_allow_list")]
#[test_case("--disallowed-tools", "claude-code", &REVIEW_ALL_TOOLS, Some(CLAUDE_TOOL_SUGGESTION) ; "warn_for_misspelled_deny_entry")]
#[test_case("--allowed-tools", "future__GetDocs", &[], None ; "retain_an_unresolved_mcp_entry_without_warning")]
fn cli_filters_plugin_tools_in_the_init_event(
    flag: &str,
    names: &str,
    expected: &[&str],
    warning: Option<&str>,
) {
    let fixture = Fixture::new();
    let claude = stand_in(&fixture.isolation.dir("bin"));
    let init = REVIEW_INIT.replace("@EXECUTABLE@", &claude.to_string_lossy());
    fs::write(fixture.isolation.dir("config/maki").join("init.lua"), init).unwrap();
    let mut command = fixture.maki(MODEL, PROMPT, STREAM_FORMAT);
    command.args([flag, names]);
    let Output { stdout, stderr, .. } = finish(command);
    let stderr = String::from_utf8(stderr).unwrap();
    assert_eq!(
        stderr.contains(UNMATCHED_TOOL_WARNING),
        warning.is_some(),
        "{stderr}"
    );
    if let Some(warning) = warning {
        assert!(stderr.contains(warning), "{stderr}");
    }
    let events: Vec<Value> = String::from_utf8(stdout)
        .unwrap()
        .lines()
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|error| panic!("{error}: {line}\n{stderr}"))
        })
        .collect();
    let init = events
        .iter()
        .find(|event| event["type"] == SYSTEM && event["subtype"] == INIT)
        .unwrap_or_else(|| panic!("no init event: {events:?}\n{stderr}"));
    let mut tools: Vec<&str> = init["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|name| name.as_str().unwrap())
        .collect();
    tools.sort_unstable();
    assert_eq!(tools, expected);
    assert!(
        claude.with_extension(RAN_EXTENSION).exists(),
        "the enabled provider must still run its CLI checks"
    );
}

#[test]
fn a_misspelled_allow_list_stops_before_starting_the_provider() {
    const NO_ALLOWED_TOOLS: &str = "--allowed-tools matches no loaded tools or qualified MCP names";
    let fixture = Fixture::new();
    let claude = stand_in(&fixture.isolation.dir("bin"));
    fixture.enable_plugin(Some(&claude));
    let mut command = fixture.maki(MODEL, PROMPT, STREAM_FORMAT);
    command.args(["--allowed-tools", "raed"]);
    let output = finish(command);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!output.status.success(), "unexpected success: {stderr}");
    assert!(
        stderr.contains(NO_ALLOWED_TOOLS),
        "missing allow-list error: {stderr}"
    );
    assert!(
        !claude.with_extension(RAN_EXTENSION).exists(),
        "the provider started before the invalid allow list was rejected"
    );
}

fn stand_in(dir: &Path) -> PathBuf {
    let path = dir.join("claude");
    fs::write(&path, OLD_CLAUDE).unwrap();
    fs::set_permissions(&path, Permissions::from_mode(OWNER_ONLY)).unwrap();
    path
}
