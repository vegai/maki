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
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::{TempDir, tempdir};
use test_case::test_case;

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
const POLL: Duration = Duration::from_millis(100);
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
/// maki takes these from the caller's environment, so the child can find its
/// tools and the `claude` CLI.
const PASSED: [&str; 4] = [PATH, "USER", "LANG", "TERM"];

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
            .rfind(|event| event["type"] == "result")
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
    _root: TempDir,
    base: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempdir().unwrap();
        let fixture = Self {
            base: root.path().canonicalize().unwrap(),
            _root: root,
        };
        let project = fixture.dir("project");
        fs::write(project.join(GLOSSARY), format!("alpha: {MEANING}\n")).unwrap();
        let config = fixture.dir("config/maki");
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
        fs::write(self.dir("config/maki").join("init.lua"), init).unwrap();
    }

    fn dir(&self, name: &str) -> PathBuf {
        let path = self.base.join(name);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn maki(&self, model: &str, prompt: &str, format: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_maki"));
        command
            .args(["-p", "--model", model, "--output-format", format, prompt])
            .current_dir(self.dir("project"))
            .env_clear()
            .env("HOME", self.dir("home"))
            .env("XDG_CONFIG_HOME", self.dir("config"))
            .env("XDG_STATE_HOME", self.dir("state"))
            .env("XDG_DATA_HOME", self.dir("data"))
            .env("XDG_CACHE_HOME", self.dir("cache"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for name in PASSED {
            if let Some(value) = env::var_os(name) {
                command.env(name, value);
            }
        }
        command
    }
}

fn run_maki(model: &str) -> Run {
    let fixture = Fixture::new();
    fixture.enable_plugin(None);
    let real_home = PathBuf::from(env::var_os("HOME").expect("HOME is set"));
    let claude_config =
        env::var_os(CLAUDE_CONFIG_ENV).map_or_else(|| real_home.join(".claude"), PathBuf::from);
    let mut command = fixture.maki(model, PROMPT, "stream-json");
    command.env(CLAUDE_CONFIG_ENV, claude_config);
    let (stdout, stderr) = finish(command);
    let events: Vec<Value> = stdout
        .split(|&byte| byte == b'\n')
        .filter_map(|line| serde_json::from_slice(line).ok())
        .collect();
    assert!(!events.is_empty(), "maki printed no events: {stderr}");
    Run { events }
}

/// Read both output streams concurrently so stderr cannot fill its pipe and block maki.
fn finish(mut command: Command) -> (Vec<u8>, String) {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
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
    let deadline = Instant::now() + RUN_LIMIT;
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("maki did not complete in {RUN_LIMIT:?}");
        }
        thread::sleep(POLL);
    }
    (out.join().unwrap(), err.join().unwrap())
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
    let bin = fixture.dir("bin");
    let claude = stand_in(&bin);
    if plugin {
        fixture.enable_plugin(Some(&claude));
    }
    let caller_path = env::var_os(PATH).unwrap_or_default();
    let path = env::join_paths(iter::once(bin).chain(env::split_paths(&caller_path))).unwrap();
    let mut command = fixture.maki(MODEL, PROMPT, "json");
    command.env(PATH, path);
    let (stdout, stderr) = finish(command);
    assert_eq!(
        claude.with_extension(RAN_EXTENSION).exists(),
        plugin,
        "whether a claude started"
    );

    let result: Value =
        serde_json::from_slice(&stdout).unwrap_or_else(|_| panic!("there is no result: {stderr}"));
    assert_eq!(result["is_error"], true, "{result}");
    let reason = result["result"].as_str().unwrap_or_default();
    assert!(reason.contains(want), "got: {reason}");
}

fn stand_in(dir: &Path) -> PathBuf {
    let path = dir.join("claude");
    fs::write(&path, OLD_CLAUDE).unwrap();
    fs::set_permissions(&path, Permissions::from_mode(OWNER_ONLY)).unwrap();
    path
}
