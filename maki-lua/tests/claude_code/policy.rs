//! Runs the claude_code plugin against a fake `claude` script, so every call
//! runs end to end, with real processes and no quota. The fake answers the
//! handshake from files a test can change between calls, and the prompt it
//! finally receives picks what it does. Each call starts the fake twice: a
//! probe in a temporary directory, then the run in the project.

use std::env;
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use flume::Receiver;
use maki_agent::cancel::CancelToken;
use maki_agent::tools::test_support::stub_ctx_in;
use maki_agent::tools::{ToolContext, ToolRegistry};
use maki_agent::{AgentEvent, Envelope, EventSender, ToolOutput, ToolOutputLines};
use maki_lua::{MAX_INFLIGHT_TOOLS, PluginHost, RestoreItem, RestoreReason};
use serde_json::{Map, Value, json};
use tempfile::{TempDir, tempdir};
use test_case::test_case;

use super::support::{
    DEADLINE, FIXTURE_GLOBAL_RULE, LEFT_BEHIND, MINIMUM_VERSION, PLUGIN_SRC, PROJECT_RULE, TOOL,
    WorkingDir, executable, group_gone, load, running, tool_reply, try_load, wait_until, within,
};

mod coding;

const ANSWER: &str = "The parser lives in src/parse.rs:10.";
const OLDER_VERSION: &str = "2.1.283";
const NEWER_VERSION: &str = "2.1.290";

const ACCOUNT_SUBSCRIPTION: &str = r#"{"current_permission_mode":"default","account":{"apiProvider":"firstParty","subscriptionType":"Claude Pro"}}"#;
const ACCOUNT_LOGGED_OUT: &str =
    r#"{"current_permission_mode":"default","account":{"apiProvider":"firstParty"}}"#;
const HOOKS_OFF: &str =
    r#"{"hooks":[],"events":[],"policy":{"allDisabled":true,"policyHookCount":0}}"#;
/// A managed hook stays on despite `disableAllHooks`, so the listing shows
/// it on and the fake runs it at startup, as Claude Code runs `SessionStart`.
const HOOKS_MANAGED: &str = r#"{"hooks":[{"event":"PreToolUse","source":"policySettings","type":"command"}],"events":[],"policy":{"allDisabled":true,"policyHookCount":1}}"#;
const POLICY_EXTRA_ROOTS: &str = r#"{"permissions":{"additionalDirectories":["/"]}}"#;
const HELPER_SETTINGS: &str = r#"{"apiKeyHelper":"/bin/false"}"#;
/// The read profile's tools. The fake echoes `--tools` in its init event,
/// so only its argv shows that Edit, Write and Bash stay off.
const READ_TOOLS: &str = "Read,Glob,Grep";
const INSTRUCTION_FILE: &str = "AGENTS.md";

/// The leading `-` also proves the prompt does not go through argv.
const ANSWERS: &str = "--answers";
/// More lines than a collapsed view shows, so only the expanded view shows
/// the last one.
const LONG_ANSWER: &str = "one\n\ntwo\n\nthree\n\nfour\n\nfive\n\nsix\n\nthe very last line";
const LAST_ANSWER_LINE: &str = "the very last line";
const LONG_ANSWERS: &str = "--long-answers";
const API_KEY: &str = "api_key";
const INIT_THEN_API_KEY: &str = "init_then_api_key";
const API_KEY_THEN_USAGE: &str = "api_key_then_usage";
/// Holds an in-flight slot until its call is cancelled, and writes its marker
/// once it has the slot.
const SLOT_HOLDER_TOOL: &str = "slot_holder";
const SLOT_HOLDER: &str = r#"
maki.api.register_tool({
  name = "slot_holder",
  description = "holds an in-flight slot",
  schema = { type = "object", properties = { marker = { type = "string" } } },
  audiences = { "main" },
  handler = function(input)
    maki.fs.write(input.marker, "")
    maki.async.sleep(@HANG_MS@)
    return "released"
  end,
})
"#;
const NO_INIT: &str = "no_init";
const CRASH: &str = "crash";
const ANSWER_THEN_CRASH: &str = "answer_then_crash";
const ANSWER_THEN_GARBAGE: &str = "answer_then_garbage";
const ANSWER_THEN_KEEP_ALIVE: &str = "answer_then_keep_alive";
const ANSWER_THEN_BAD_BYTE: &str = "answer_then_bad_byte";
/// A byte that is not UTF-8 reaches the plugin as U+FFFD.
const NOT_AN_EVENT_BYTE: &str = "printed a line that is not an event: \u{FFFD}";
const NOT_AN_EVENT: &str = "printed a line that is not an event: Update available";
const USAGE_THEN_HANG: &str = "usage_then_hang";
/// Start a lingering `sleep` with all its standard streams closed, then
/// answer, or send an init that stops the run. The stray starts first,
/// because a stop can kill the fake right after its init.
const STRAY_THEN_ANSWER: &str = "stray_then_answer";
const STRAY_THEN_API_KEY: &str = "stray_then_api_key";
/// Runs the fake from wherever a test copies this wrapper.
const WRAPPER: &str = "#!/bin/sh\nexec \"@TARGET@\" \"$@\"\n";
/// Files that make the fake hang at one stage until it is killed.
const STALL: &str = "stall";
/// Holds how many seconds to hang, or 30 when empty.
const STALL_VERSION: &str = "stall_version";
const HANG_AFTER_HOOK: &str = "hang_after_hook";
/// Makes the run in the project skip the handshake and reply at once.
const UNASKED: &str = "unasked";
/// Makes the probe answer the handshake and then run as if it had a prompt.
const PROBE_RUNS: &str = "probe_runs";
/// Makes Claude Code answer the settings request with an error.
const REFUSE_SETTINGS: &str = "refuse_settings";
/// Makes Claude Code answer the hooks request twice.
const ANSWER_TWICE: &str = "answer_twice";
const STRAY_ANSWER: &str = "request that maki never sent, or answered it twice";
const SETTINGS_REFUSAL: &str = "settings unavailable";
const SETTINGS_REFUSED: &str = "Claude Code rejected the settings request: settings unavailable";

const LOGIN_HINT: &str = "claude auth login";
const TOO_OLD: &str = "2.1.283 is older than 2.1.284";
const HELPER_CONFLICT: &str = "settings.json: apiKeyHelper";
const LOCAL_HELPER_CONFLICT: &str = "settings.local.json: apiKeyHelper";
/// Part of the plugin's settings, so Claude Code loads no instruction file
/// itself.
const NO_AMBIENT_INSTRUCTIONS: &str = r#""claudeMdExcludes":["**"]"#;
const MANAGED_ROOTS_STOP: &str = "Claude Code policy sets permissions.additionalDirectories";
const CANNOT_CHECK: &str = "settings.json: maki cannot examine it";
const CANNOT_READ: &str = "settings.json: maki cannot read it";
const NOT_AN_OBJECT: &str = "settings.json: is not a JSON object";
/// Where serde reports the end of the broken settings file.
const SYNTAX_ERROR_AT: &str = "settings.json: EOF while parsing a value at line 2 column 17";
const BROKEN_SETTINGS: &str = "{\n  \"apiKeyHelper\":";
const API_KEY_STOP: &str = "API key source ANTHROPIC_API_KEY";
const NO_INIT_STOP: &str = "no init event in 30 s";
const NO_ANSWER_IN_TIME: &str = "did not complete in time";
/// The fake's exit code, when it came, and its stderr, whatever text
/// surrounds them.
const CRASH_REPORT: &[&str] = &["code 3", "before its result", "boom"];
const LATE_CRASH_REPORT: &[&str] = &["code 3", "after its result", "boom"];
const CANCEL_MARKER: &str = "[cancelled by user";
/// One callback shows this line and records the usage beside it.
const USAGE_PROGRESS: &str = "Read src/usage.rs";
const TOOL_USE_ID: &str = "toolu_ask";
/// The smallest `timeout` a call takes.
const RUN_TIMEOUT_SECS: u64 = 30;
const SPENT_BEFORE_CANCEL: &[&str] = &["7 in", "2.0k cache read", "300 cache write", "out unknown"];
const WAITING: &str = "Waiting for a free Claude Code slot";
const FIRST_ID: &str = "toolu_first";
const SECOND_ID: &str = "toolu_second";
/// Stopped at the probe, before the run in the project started.
const PROBED: &str = "version\nstart\n";
const STARTED: &str = "version\nstart\nstart\n";
const HOOK_SENTINEL: &str = ".claude_code_hook_sentinel";
const HOOK_STOP: &str = "started a SessionStart hook";
const PROBE_CRASH: &str = "exited with code 3 during its checks:\nprobe gave up";
const UNASKED_STOP: &str = "started its run before maki accepted its checks";
const PROBE_RUN_STOP: &str = "started a run during its checks";
const ROUTE_CLAIM: &str = "maki found no route conflict";

const CODE_PROFILE: &str = "code";
const MODEL_ALIAS: &str = "haiku";
const RELATIVE_EXECUTABLE: &str = "./claude";
const ONE_SLOT: u64 = 1;
/// Makes the fake list its working directory, then edit, add and delete
/// files there, including a binary one.
const CODE_EDIT: &str = "code_edit";
/// Edits like `code_edit` and replies with `LONG_ANSWER`.
const CODE_LONG: &str = "code_long";
const MAX_TIMEOUT_SECS: u64 = 1800;
const TIMEOUT_TOO_LONG: &str = "the maximum value of the `timeout_secs` option is 1800";
const FULL_MODEL_ID: &str = "claude-sonnet-5";
const BAD_MODEL_OPTION: &str = "the `model` option must be one of";
/// Starts the worker's edits and never finishes them.
const CODE_HANG: &str = "code_hang";
/// The fake writes it once the hanging worker has started.
const WORKING: &str = "working";
/// Longer than `DEADLINE`, so if a kill fails, the test waiting on it fails
/// before the process would stop on its own.
const HANG_SECS: u64 = DEADLINE.as_secs() * 2;
/// Makes the fake add a file named {HOSTILE_NAME}.
const CODE_HOSTILE: &str = "code_hostile";
/// Makes the fake turn `src/lib.rs` into a folder, add a file whose name is
/// not UTF-8, add a file whose name has an escape byte, and add `src/new.rs`.
const CODE_RETYPE: &str = "code_retype";
/// Makes the fake write Claude Code config at the root and in subdirectories,
/// plus an edit.
const CODE_CLAUDE_CONFIG: &str = "code_claude_config";
/// A name that survives only with proper shell quoting: a quote, a command
/// substitution, a space, a glob and a newline.
const HOSTILE_NAME: &str = "src/it's $(touch pwned) *\nnew.rs";
const RELATIVE_CONFIG_DIR: &str = "claude-config";
const NOT_ABSOLUTE: &str = "must be an absolute path, not claude-config";

const SCRIPT: &str = r#"#!/bin/sh
dir=$(dirname "$0")
for p in $(cat "$dir/pids" 2>/dev/null); do
  { tr '\0' ' ' < "/proc/$p/cmdline"; } 2>/dev/null | grep -q "$dir/claude" && echo "alive $p" >> "$dir/overlaps"
done
echo $$ >> "$dir/pids"
if [ "$1" = --version ]; then
  echo version >> "$dir/calls"
  if [ -e "$dir/@STALL_VERSION@" ]; then touch "$dir/version_stalled"; secs=$(cat "$dir/@STALL_VERSION@"); sleep "${secs:-30}"; fi
  cat "$dir/version"; exit 0
fi
echo start >> "$dir/calls"
echo $$ > "$dir/pid"
pwd -P >> "$dir/started_in"
if [ -e "$dir/@STALL@" ]; then touch "$dir/stalled"; sleep @HANG_SECS@; fi
if grep -q policySettings "$dir/hooks.json"; then
  pwd -P >> "$dir/hook_ran_in"; touch "@SENTINEL@"
  printf '%s\n' '{"type":"system","subtype":"hook_started","hook_event":"SessionStart"}'
  if [ -e "$dir/@HANG_AFTER_HOOK@" ]; then sleep @HANG_SECS@; fi
fi
printf '%s\n' "$@" > "$dir/argv"
env > "$dir/env"
printf '%s\n' "${TMPDIR-}" >> "$dir/tmpdirs"
cwd=$(pwd -P)
flags=; mode=; tools=; prev=
for arg; do
  [ "$prev" = --settings ] && flags=$arg
  [ "$prev" = --permission-mode ] && mode=$arg
  [ "$prev" = --tools ] && tools=$arg
  prev=$arg
done
[ "$mode" = manual ] && mode=default
line() { printf '%s\n' "$1"; }
reply() { printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":%s}}\n' "$1" "$2"; }
settings() {
  policy=; [ -e "$dir/policy.json" ] && policy=",{\"source\":\"policySettings\",\"settings\":$(cat "$dir/policy.json")}"
  printf '{"effective":%s,"sources":[{"source":"flagSettings","settings":%s}%s]}' "$flags" "$flags" "$policy"
}
init() { printf '{"type":"system","subtype":"init","apiKeySource":"%s","claude_code_version":"%s","permissionMode":"%s","tools":["%s"],"mcp_servers":[],"plugins":[{"name":"agents-md","path":"builtin","source":"agents-md@builtin"}],"cwd":"%s"}\n' "$1" "$(cut -d' ' -f1 "$dir/version")" "$mode" "$(printf '%s' "$tools" | sed 's/,/","/g')" "$cwd"; }
answer() { line '{"type":"result","subtype":"success","is_error":false,"result":"@ANSWER@"}'; }
stray() { sleep @HANG_SECS@ </dev/null >/dev/null 2>&1 & echo $! > "$dir/stray"; }
usage() { printf '{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","name":"Read","input":{"file_path":"%s/src/usage.rs"}}],"usage":{"input_tokens":7,"cache_read_input_tokens":2000,"cache_creation_input_tokens":300,"output_tokens":1}}}\n' "$cwd"; }
record() { case "$1" in *'"type":"user"'*) printf '%s\n' "$1" > "$dir/prompt" ;; esac; }
if [ -e "$dir/@UNASKED@" ] && [ "$(grep -c '^start$' "$dir/calls")" -ge 2 ]; then
  for n in 1 2 3; do read -r msg; record "$msg"; done
  init none; answer
  while IFS= read -r msg; do record "$msg"; done
  exit 0
fi
scenario=
while IFS= read -r msg; do
  id=$(printf '%s' "$msg" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
  case "$msg" in
    *'"subtype":"initialize"'*) reply "$id" "$(sed "s/\"current_permission_mode\":\"default\"/\"current_permission_mode\":\"$mode\"/" "$dir/account.json")" ;;
    *'"subtype":"get_settings"'*)
      if [ -e "$dir/@REFUSE_SETTINGS@" ]; then
        printf '{"type":"control_response","response":{"subtype":"error","request_id":"%s","error":"@SETTINGS_REFUSAL@"}}\n' "$id"
      else reply "$id" "$(settings)"; fi ;;
    *'"subtype":"get_hooks_listing"'*)
      reply "$id" "$(cat "$dir/hooks.json")"
      if [ -e "$dir/@ANSWER_TWICE@" ]; then reply "$id" "$(cat "$dir/hooks.json")"; fi ;;
    *'"type":"user"'*) printf '%s\n' "$msg" > "$dir/prompt"; scenario=$(printf '%s' "$msg" | sed -n 's/.*"\(content\|text\)":"\([^"]*\)".*/\2/p'); break ;;
  esac
done
if [ -z "$scenario" ] && [ -e "$dir/@PROBE_RUNS@" ]; then init none; answer; fi
if [ -z "$scenario" ] && [ -e "$dir/probe_exit" ]; then echo "probe gave up" >&2; exit "$(cat "$dir/probe_exit")"; fi
[ -n "$scenario" ] || exit 0
echo "print $scenario" >> "$dir/calls"
case "$scenario" in
  --answers) init none; line '{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","name":"Read","input":{"file_path":"src/parse.rs"}}]}}'; answer ;;
  --long-answers) init none; line '{"type":"result","subtype":"success","is_error":false,"result":@LONG_ANSWER@}' ;;
  api_key) init ANTHROPIC_API_KEY; answer; sleep @HANG_SECS@ ;;
  init_then_api_key) init none; init ANTHROPIC_API_KEY; sleep @HANG_SECS@ ;;
  no_init) sleep @HANG_SECS@ ;;
  crash) init none; echo boom >&2; exit 3 ;;
  answer_then_crash) init none; answer; echo boom >&2; exit 3 ;;
  answer_then_garbage) init none; answer; line 'Update available: run claude update' ;;
  answer_then_keep_alive) init none; answer; line '{"type":"keep_alive"}' ;;
  answer_then_bad_byte) init none; answer; printf '\377\n' ;;
  usage_then_hang) init none; usage; sleep @HANG_SECS@ ;;
  api_key_then_usage) init ANTHROPIC_API_KEY; usage; sleep @HANG_SECS@ ;;
  stray_then_answer) stray; init none; answer ;;
  stray_then_api_key) stray; init ANTHROPIC_API_KEY ;;
  code_edit*)
    init none
    find . -path ./.git -prune -o -print | sort > "$dir/snapshot_listing"
    cat src/lib.rs > "$dir/snapshot_lib"
    printf 'worker\n' >> src/lib.rs; printf 'fresh\n' > src/new.rs; rm src/old.rs; printf '\000\001' > logo.bin
    printf '#!/bin/sh\n' > run.sh; chmod +x run.sh
    if [ -d node_modules ]; then printf 'patched\n' >> node_modules/pkg/index.js; fi
    answer ;;
  code_long*)
    init none
    printf 'worker\n' >> src/lib.rs
    line '{"type":"result","subtype":"success","is_error":false,"result":@LONG_ANSWER@}' ;;
  code_hang*)
    init none
    printf 'worker\n' >> src/lib.rs; touch "$dir/@WORKING@"
    sleep @HANG_SECS@ ;;
  code_hostile*)
    init none
    printf 'hostile\n' > "@HOSTILE@"
    answer ;;
  code_retype*)
    init none
    rm src/lib.rs; mkdir src/lib.rs; printf 'worker\n' > src/lib.rs/mod.rs
    printf 'x\n' > "$(printf 'latin\351.txt')"; printf 'x\n' > "$(printf 'esc\033[2J.txt')"
    printf 'fresh\n' > src/new.rs
    answer ;;
  code_claude_config*)
    init none
    mkdir -p .claude src/.claude
    printf '{}\n' > .claude/settings.local.json
    printf '{}\n' > src/.claude/settings.json
    mkdir tools; printf '{}\n' > tools/.claude
    printf 'worker\n' >> src/lib.rs
    answer ;;
  code_redirect*)
    init none
    project=$(cat "$dir/project")
    if [ -d .git ]; then
      ln -sf "$project/src/lib.rs" .git/info/attributes
    else
      rm -f .git; mkdir -p .git/info; ln -s "$project/src/lib.rs" .git/info/attributes
    fi
    printf 'worker\n' >> src/lib.rs
    answer ;;
esac
"#;

/// An empty repository per test, used as the session directory in its
/// contexts. The plugin works there rather than in the process directory,
/// so the developer's Claude Code settings in the checkout cannot affect a
/// test.
struct Project {
    dir: TempDir,
}

impl Project {
    fn new() -> Self {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join(".git")).unwrap();
        Self { dir }
    }

    /// The path as the plugin sees it through `getcwd`.
    fn path(&self) -> PathBuf {
        self.dir.path().canonicalize().unwrap()
    }
}

/// A fake `claude` in its own directory, where it records every call, so a
/// test can see which stages ran, what the child received, and whether a
/// prompt came. Each test gets its own fake and project.
struct FakeClaude {
    dir: TempDir,
    project: Project,
}

impl FakeClaude {
    fn new() -> Self {
        let dir = tempdir().unwrap();
        let script = SCRIPT
            .replace("@ANSWER@", ANSWER)
            .replace(
                "@LONG_ANSWER@",
                &serde_json::to_string(LONG_ANSWER).unwrap(),
            )
            .replace("@SENTINEL@", HOOK_SENTINEL)
            .replace("@STALL@", STALL)
            .replace("@STALL_VERSION@", STALL_VERSION)
            .replace("@HANG_AFTER_HOOK@", HANG_AFTER_HOOK)
            .replace("@UNASKED@", UNASKED)
            .replace("@PROBE_RUNS@", PROBE_RUNS)
            .replace("@REFUSE_SETTINGS@", REFUSE_SETTINGS)
            .replace("@ANSWER_TWICE@", ANSWER_TWICE)
            .replace("@SETTINGS_REFUSAL@", SETTINGS_REFUSAL)
            .replace("@HOSTILE@", &HOSTILE_NAME.replace('$', "\\$"))
            .replace("@WORKING@", WORKING)
            .replace("@HANG_SECS@", &HANG_SECS.to_string());
        executable(dir.path(), "claude", &script);
        fs::create_dir(dir.path().join("config")).unwrap();
        let fake = Self {
            dir,
            project: Project::new(),
        };
        fake.answer_version(MINIMUM_VERSION);
        fake.write("account.json", ACCOUNT_SUBSCRIPTION);
        fake.write("hooks.json", HOOKS_OFF);
        fake
    }

    fn answer_version(&self, version: &str) {
        self.write("version", &format!("{version} (Claude Code)\n"));
    }

    fn write(&self, name: &str, content: &str) {
        fs::write(self.path(name), content).unwrap();
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn settings_path(&self) -> PathBuf {
        self.path("config").join("settings.json")
    }

    fn log(&self, name: &str) -> String {
        fs::read_to_string(self.path(name)).unwrap_or_default()
    }

    /// Returns the directories, other than `project`, where the fake started
    /// and which still exist: probe directories that were never removed.
    fn probe_dirs_left(&self, project: &Path) -> Vec<PathBuf> {
        self.log("started_in")
            .lines()
            .map(PathBuf::from)
            .filter(|dir| dir != project && dir.exists())
            .collect()
    }

    fn pid(&self) -> i32 {
        self.log("pid")
            .trim()
            .parse()
            .expect("the run that started wrote its pid")
    }

    fn opts(&self, max_concurrent: u64) -> Map<String, Value> {
        let mut opts = Map::new();
        opts.insert("executable".into(), json!(self.path("claude")));
        opts.insert("config_dir".into(), json!(self.path("config")));
        opts.insert("max_concurrent".into(), json!(max_concurrent));
        opts
    }

    fn host(&self, max_concurrent: u64) -> (Arc<ToolRegistry>, PluginHost) {
        load(self.opts(max_concurrent))
    }

    fn ask(&self, prompt: &str) -> Result<String, String> {
        let (reg, _host) = self.host(ONE_SLOT);
        smol::block_on(within_deadline(call(&reg, &self.ctx(None, None), prompt)))
    }

    /// A tool context for a session working in this fake's project.
    fn ctx(&self, events: Option<&EventSender>, tool_use_id: Option<&str>) -> ToolContext {
        ctx_in(&self.project.path(), events, tool_use_id)
    }
}

/// A session context in `dir`, without the bash tool's rtk probe. With rtk
/// installed, the probe adds up to two 2 s job waits to each import, and
/// upstream CI has no rtk anyway.
fn ctx_in(dir: &Path, events: Option<&EventSender>, tool_use_id: Option<&str>) -> ToolContext {
    let mut ctx = stub_ctx_in(dir, events, tool_use_id);
    ctx.config.rtk = false;
    ctx
}

fn execute<'a>(
    reg: &ToolRegistry,
    ctx: &'a ToolContext,
    prompt: &str,
) -> impl Future<Output = Result<ToolOutput, String>> + 'a {
    let inv = reg
        .get(TOOL)
        .expect("claude_code registered")
        .tool
        .parse(&json!({ "prompt": prompt, "model": MODEL_ALIAS }))
        .expect("input parses");
    async move { inv.execute(ctx).await.output }
}

async fn call(reg: &ToolRegistry, ctx: &ToolContext, prompt: &str) -> Result<String, String> {
    let input = json!({ "prompt": prompt, "model": MODEL_ALIAS });
    tool_reply(reg, ctx, TOOL, input).await
}

/// What a reloaded session has for the completed call: its text, the `state`
/// it saved, and the rows the user clicked afterwards.
fn restored(prompt: &str, state: Option<Value>, clicks: Vec<usize>) -> RestoreItem {
    RestoreItem {
        tool: Arc::from(TOOL),
        tool_use_id: TOOL_USE_ID.to_owned(),
        output: ANSWER.to_owned(),
        input: json!({ "prompt": prompt }),
        is_error: false,
        tool_output_lines: ToolOutputLines::default(),
        theme_gen: None,
        clicks,
        state,
        task_id: None,
        session_id: None,
        reason: RestoreReason::default(),
    }
}

/// A call that never returns means a permit was not released or a reply was
/// not sent, so the test fails here instead of hanging.
async fn within_deadline<T>(fut: impl Future<Output = T>) -> T {
    within(DEADLINE, fut).await
}

fn assert_mentions(text: &str, parts: &[&str]) {
    for part in parts {
        assert!(text.contains(part), "{part:?} missing from: {text}");
    }
}

/// Waits until a live view on `rx` shows `line`, and on timeout returns the
/// views it saw.
fn wait_for_view_line(rx: &Receiver<Envelope>, line: &str) -> Result<(), Vec<String>> {
    let mut bodies = Vec::new();
    let seen = wait_until(DEADLINE, || {
        bodies.extend(rx.drain().filter_map(|env| match env.event {
            AgentEvent::LiveToolBuf { id, body } => Some((id, body)),
            _ => None,
        }));
        bodies
            .iter()
            .any(|(_, body)| body.take().text().contains(line))
    });
    if seen {
        return Ok(());
    }
    Err(bodies
        .iter()
        .map(|(id, body)| format!("{id}: {}", body.take().text()))
        .collect())
}

/// nextest and cargo export CARGO_* to the test process, so these variables
/// stand in for an exported ANTHROPIC_API_KEY that every test run has.
#[test]
fn an_answered_run_goes_through_every_check() {
    assert!(
        env::vars_os().any(|(name, _)| name.to_string_lossy().starts_with("CARGO_")),
        "test condition: the test process must have a variable that maki does not give"
    );
    let fake = FakeClaude::new();
    assert_eq!(fake.ask(ANSWERS).unwrap(), ANSWER);

    let argv: Vec<String> = fake.log("argv").lines().map(String::from).collect();
    assert!(
        !argv.iter().any(|arg| arg.contains(ANSWERS)),
        "the ANSWERS must not be on the command line"
    );
    assert!(
        argv.windows(2).any(|w| w == ["--tools", READ_TOOLS]),
        "{argv:?}"
    );
    assert!(
        fake.log("prompt").contains(ANSWERS),
        "the ANSWERS go through stdin"
    );
    assert_eq!(fake.log("calls"), format!("{STARTED}print {ANSWERS}\n"));
    let child_env = fake.log("env");
    assert!(
        child_env.lines().any(|l| l.starts_with("PATH=")),
        "PATH must go to the child"
    );
    assert!(
        !child_env.lines().any(|l| l.starts_with("CARGO_")),
        "got:\n{child_env}"
    );
}

fn log_out(fake: &FakeClaude) {
    fake.write("account.json", ACCOUNT_LOGGED_OUT);
}

fn move_to_an_older_version(fake: &FakeClaude) {
    fake.answer_version(OLDER_VERSION);
}

fn skip_an_api_key_helper(fake: &FakeClaude) {
    fs::write(fake.settings_path(), HELPER_SETTINGS).unwrap();
}

fn add_a_managed_hook(fake: &FakeClaude) {
    fake.write("hooks.json", HOOKS_MANAGED);
}

fn add_managed_extra_roots(fake: &FakeClaude) {
    fake.write("policy.json", POLICY_EXTRA_ROOTS);
}

fn stall_every_start(fake: &FakeClaude) {
    fake.write(STALL, "");
}

fn stall_the_version_check(fake: &FakeClaude) {
    fake.write(STALL_VERSION, "");
}

/// Only the hook's event can stop this probe before its time limit, because
/// the probe hangs right after it.
fn add_a_managed_hook_then_hang(fake: &FakeClaude) {
    add_a_managed_hook(fake);
    fake.write(HANG_AFTER_HOOK, "");
}

fn change_nothing(_: &FakeClaude) {}

/// Clean answers, then a failing exit. The answers alone must not start the
/// run.
fn crash_the_probe(fake: &FakeClaude) {
    fake.write("probe_exit", "3");
}

/// Clean answers, then a matching init and result from the probe, which had
/// no prompt.
fn run_in_the_probe(fake: &FakeClaude) {
    fake.write(PROBE_RUNS, "");
}

fn refuse_the_settings_request(fake: &FakeClaude) {
    fake.write(REFUSE_SETTINGS, "");
}

/// A repeated response must not run the checks again, or the prompt could go
/// out twice.
fn answer_the_hooks_twice(fake: &FakeClaude) {
    fake.write(ANSWER_TWICE, "");
}

/// A clean probe, then a run that sends a matching init and result but never
/// answers its checks.
fn answer_without_the_checks(fake: &FakeClaude) {
    fake.write(UNASKED, "");
}

/// The fake records every prompt it reads, so an empty log proves nothing
/// went to Anthropic. The probe directory must be gone before the call is
/// rejected, because maki can exit right after.
#[test_case(log_out, LOGIN_HINT, PROBED ; "logged_out")]
#[test_case(move_to_an_older_version, TOO_OLD, "version\n" ; "an_older_version")]
#[test_case(skip_an_api_key_helper, HELPER_CONFLICT, "" ; "skipped_api_key_helper")]
#[test_case(add_managed_extra_roots, MANAGED_ROOTS_STOP, PROBED ; "managed_extra_roots")]
#[test_case(crash_the_probe, PROBE_CRASH, PROBED ; "probe_crashing_after_clean_answers")]
#[test_case(answer_without_the_checks, UNASKED_STOP, STARTED ; "a_run_answering_before_its_checks")]
#[test_case(run_in_the_probe, PROBE_RUN_STOP, PROBED ; "a_probe_that_starts_a_run")]
#[test_case(refuse_the_settings_request, SETTINGS_REFUSED, PROBED ; "a_refused_check_request")]
#[test_case(answer_the_hooks_twice, STRAY_ANSWER, PROBED ; "a_repeated_check_response")]
fn refused_before_the_prompt_is_sent(setup: fn(&FakeClaude), want: &str, calls: &str) {
    let fake = FakeClaude::new();
    let project = fake.project.path();
    setup(&fake);
    let err = fake.ask(ANSWERS).unwrap_err();
    assert!(err.contains(want), "got: {err}");
    assert_eq!(fake.log("calls"), calls);
    assert_eq!(
        fake.log("prompt"),
        "",
        "a rejected run must not receive a prompt"
    );
    assert_eq!(fake.probe_dirs_left(&project), Vec::<PathBuf>::new());
}

/// A hook that organization policy keeps on runs when Claude Code starts,
/// before any answer can stop it. The fake's hook writes a sentinel into its
/// working directory, which therefore must not be the project. The cleanup
/// removes only that file and then empty directories, so a regression cannot
/// make this test delete anything else.
#[test]
fn a_hook_policy_keeps_on_never_runs_in_the_project() {
    let fake = FakeClaude::new();
    let project = fake.project.path();
    add_a_managed_hook_then_hang(&fake);
    let err = fake.ask(ANSWERS).unwrap_err();

    let leaked = project.join(HOOK_SENTINEL);
    let mutated = leaked.exists();
    let _ = fs::remove_file(&leaked);
    let ran_in: Vec<PathBuf> = fake.log("hook_ran_in").lines().map(PathBuf::from).collect();
    for dir in &ran_in {
        let _ = fs::remove_file(dir.join(HOOK_SENTINEL));
        let _ = fs::remove_dir(dir);
    }
    assert!(!mutated, "the hook wrote into the project");
    assert!(err.contains(HOOK_STOP), "got: {err}");
    for dir in &ran_in {
        assert!(
            err.contains(&format!("{LEFT_BEHIND}{}", dir.display())),
            "the error must give the probe directory that the hook wrote into: {err}"
        );
    }
    assert!(
        !ran_in.is_empty(),
        "test condition: the hook must run at one location"
    );
    assert!(
        ran_in
            .iter()
            .all(|dir| !dir.starts_with(&project) && !project.starts_with(dir)),
        "the hook ran in the project, or in a directory that contains it: {ran_in:?}"
    );
    assert_eq!(
        fake.log("calls"),
        PROBED,
        "the run in the project must not start"
    );
    assert_eq!(fake.log("prompt"), "");
}

/// An option no call could use fails the plugin's load instead of being
/// silently adjusted: a limit longer than a call may run, or a model name
/// that is not an alias.
#[test_case("timeout_secs", json!(MAX_TIMEOUT_SECS + 1), TIMEOUT_TOO_LONG ; "a_timeout_past_the_maximum")]
#[test_case("model", json!(FULL_MODEL_ID), BAD_MODEL_OPTION ; "a_full_model_id")]
fn an_unusable_option_is_refused_at_load(name: &str, value: Value, want: &str) {
    let fake = FakeClaude::new();
    let mut opts = fake.opts(ONE_SLOT);
    opts.insert(name.into(), value);
    let host = PluginHost::new(Arc::new(ToolRegistry::new())).unwrap();
    let err = host
        .load_source_with_opts(TOOL, PLUGIN_SRC, opts)
        .unwrap_err();
    assert!(err.to_string().contains(want), "got: {err}");
}

/// A file that cannot be checked or read could hold an API key helper, so it
/// must stop the start rather than count as missing. These tests avoid mode
/// bits, because root reads through them, and instead use a config
/// "directory" that is a file and a settings "file" that is a directory.
#[test_case(true, CANNOT_CHECK ; "config_dir_cannot_be_searched")]
#[test_case(false, CANNOT_READ ; "settings_file_cannot_be_read")]
fn settings_that_cannot_be_inspected_are_refused(config_is_a_file: bool, want: &str) {
    let fake = FakeClaude::new();
    if config_is_a_file {
        fs::remove_dir(fake.path("config")).unwrap();
        fs::write(fake.path("config"), "").unwrap();
    } else {
        fs::create_dir(fake.settings_path()).unwrap();
    }

    let err = fake.ask(ANSWERS).unwrap_err();
    assert!(err.contains(want), "got: {err}");
    assert_eq!(fake.log("calls"), "");
}

/// A settings file that is not a JSON object could hide an API key helper
/// from the check, so it stops the call the way a helper does. An empty object
/// is fine.
#[test_case("[1]", Some(NOT_AN_OBJECT) ; "an_array")]
#[test_case("[]", Some(NOT_AN_OBJECT) ; "an_empty_array")]
#[test_case(BROKEN_SETTINGS, Some(SYNTAX_ERROR_AT) ; "a_syntax_error")]
#[test_case("{}", None ; "an_empty_object")]
fn a_settings_file_must_be_a_json_object(text: &str, refused: Option<&str>) {
    let fake = FakeClaude::new();
    fs::write(fake.settings_path(), text).unwrap();

    let result = fake.ask(ANSWERS);

    match refused {
        Some(want) => {
            let err = result.unwrap_err();
            assert!(err.contains(want), "got: {err}");
        }
        None => assert_eq!(result.unwrap(), ANSWER),
    }
}

/// A run past its `timeout` is killed, the reply keeps what it spent, and its
/// slot takes the next call.
#[test]
fn a_run_past_its_timeout_is_killed_and_frees_its_slot() {
    let fake = FakeClaude::new();
    let (reg, _host) = fake.host(ONE_SLOT);
    let ctx = fake.ctx(None, None);
    let input =
        json!({ "prompt": USAGE_THEN_HANG, "model": MODEL_ALIAS, "timeout": RUN_TIMEOUT_SECS });
    let err = smol::block_on(within_deadline(tool_reply(&reg, &ctx, TOOL, input))).unwrap_err();
    assert_mentions(&err, &[&format!("timed out after {RUN_TIMEOUT_SECS}s")]);
    assert_mentions(&err, SPENT_BEFORE_CANCEL);
    assert!(
        group_gone(fake.pid(), DEADLINE),
        "no process of the timed-out run must stay"
    );
    let next = smol::block_on(within_deadline(call(&reg, &ctx, ANSWERS)));
    assert!(next.is_ok(), "{next:?}");
}

/// The completed view may claim no route conflict only for a run whose init
/// passed the checks, with no contradicting event.
#[test_case(ANSWERS, true ; "an_answered_run")]
#[test_case(API_KEY, false ; "an_api_key_at_init")]
#[test_case(INIT_THEN_API_KEY, false ; "an_api_key_after_a_clean_init")]
fn the_view_claims_a_clean_route_only_after_init_passes(scenario: &str, claimed: bool) {
    let fake = FakeClaude::new();
    let (reg, _host) = fake.host(ONE_SLOT);
    let (tx, rx) = flume::unbounded::<Envelope>();
    let events = EventSender::new(tx, 0);
    let ctx = fake.ctx(Some(&events), Some(TOOL_USE_ID));
    let _ = smol::block_on(within_deadline(call(&reg, &ctx, scenario)));
    let view = rx
        .drain()
        .filter_map(|env| match env.event {
            AgentEvent::LiveToolBuf { body, .. } => Some(body.take().text()),
            _ => None,
        })
        .last()
        .expect("the call showed a view");
    assert_eq!(view.contains(ROUTE_CLAIM), claimed, "view:\n{view}");
}

/// Claude Code loads no instruction file itself, so maki sends the files its
/// own prompt uses, the project's and the global one, with the task. The
/// global file comes from the fixture, not the developer's config.
#[test]
fn the_instructions_travel_with_the_task() {
    let fake = FakeClaude::new();
    fs::write(fake.project.path().join(INSTRUCTION_FILE), PROJECT_RULE).unwrap();
    assert_eq!(fake.ask(ANSWERS).unwrap(), ANSWER);
    let prompt = fake.log("prompt");
    for rule in [PROJECT_RULE, FIXTURE_GLOBAL_RULE] {
        assert!(prompt.contains(rule), "{rule} missing from: {prompt}");
    }
    assert!(
        fake.log("argv").contains(NO_AMBIENT_INSTRUCTIONS),
        "Claude Code must also load no instructions itself"
    );
}

/// A conflict at init stops the run, but the usage the run printed right
/// after the conflict still counts and survives the process.
#[test]
fn a_stopped_run_keeps_what_it_spent() {
    let err = FakeClaude::new().ask(API_KEY_THEN_USAGE).unwrap_err();
    assert!(err.contains(API_KEY_STOP), "got: {err}");
    assert_mentions(&err, SPENT_BEFORE_CANCEL);
}

/// Queued calls can hold every Lua slot, and a Lua timer needs one. The
/// startup limit must still stop a run that never sends its init, long
/// before the fake would stop, and leave no process behind.
#[test]
fn the_startup_limit_holds_with_every_slot_taken() {
    let fake = FakeClaude::new();
    let (reg, host) = fake.host(ONE_SLOT);
    let slot_holder = SLOT_HOLDER.replace("@HANG_MS@", &(HANG_SECS * 1000).to_string());
    host.load_source(SLOT_HOLDER_TOOL, &slot_holder).unwrap();
    let markers = tempdir().unwrap();
    let holders: Vec<_> = (1..MAX_INFLIGHT_TOOLS)
        .map(|holder| {
            let mut ctx = fake.ctx(None, None);
            let (trigger, token) = CancelToken::new();
            ctx.cancel = token;
            let marker = markers.path().join(holder.to_string());
            let inv = reg
                .get(SLOT_HOLDER_TOOL)
                .expect("the registry has the tool that holds a slot")
                .tool
                .parse(&json!({ "marker": marker }))
                .expect("input parses");
            let held = thread::spawn(move || {
                let _ = smol::block_on(inv.execute(&ctx));
            });
            (trigger, held)
        })
        .collect();
    assert!(
        wait_until(DEADLINE, || {
            fs::read_dir(markers.path()).unwrap().count() == MAX_INFLIGHT_TOOLS - 1
        }),
        "the calls that hold slots did not get them"
    );
    let ctx = fake.ctx(None, None);
    let result = smol::block_on(within_deadline(call(&reg, &ctx, NO_INIT)));
    let held: Vec<_> = holders
        .into_iter()
        .map(|(trigger, held)| {
            trigger.cancel();
            held
        })
        .collect();
    for holder in held {
        holder.join().unwrap();
    }
    let err = result.unwrap_err();
    assert!(err.contains(NO_INIT_STOP), "got: {err}");
    assert!(
        group_gone(fake.pid(), DEADLINE),
        "no process of the stopped child must stay"
    );
}

/// Every line is checked until the process exits. A line that is not an
/// event is an error even after the result. An empty keep-alive is not.
#[test_case(ANSWER_THEN_GARBAGE, Some(NOT_AN_EVENT) ; "a_line_that_is_no_event")]
#[test_case(ANSWER_THEN_KEEP_ALIVE, None ; "a_keep_alive")]
#[test_case(ANSWER_THEN_BAD_BYTE, Some(NOT_AN_EVENT_BYTE) ; "a_line_that_is_not_utf8")]
fn output_after_the_result_is_checked(scenario: &str, want: Option<&str>) {
    let result = FakeClaude::new().ask(scenario);
    match want {
        Some(want) => {
            let err = result.unwrap_err();
            assert!(err.contains(want), "got: {err}");
        }
        None => assert!(result.unwrap().contains(ANSWER)),
    }
}

/// One callback shows the view line and records the usage, so seeing the
/// line proves the usage was counted before the cancel.
#[test]
fn a_cancelled_run_keeps_what_it_spent() {
    let (scenario, progress, spent) = (USAGE_THEN_HANG, USAGE_PROGRESS, SPENT_BEFORE_CANCEL);
    let fake = FakeClaude::new();
    let (reg, _host) = fake.host(ONE_SLOT);
    let (tx, rx) = flume::unbounded::<Envelope>();
    let mut ctx = fake.ctx(Some(&EventSender::new(tx, 0)), Some(TOOL_USE_ID));
    let (trigger, token) = CancelToken::new();
    ctx.cancel = token;
    let canceller = thread::spawn(move || {
        if let Err(views) = wait_for_view_line(&rx, progress) {
            panic!("the usage message did not go to the view, which showed {views:?}");
        }
        trigger.cancel();
    });

    let err = smol::block_on(within_deadline(call(&reg, &ctx, scenario))).unwrap_err();
    canceller.join().unwrap();
    assert!(err.contains(CANCEL_MARKER), "got: {err}");
    assert_mentions(&err, spent);
    assert!(
        group_gone(fake.pid(), DEADLINE),
        "no process of the cancelled child must stay"
    );
}

/// A cancel must not free the slot while the process it killed still runs,
/// and must not leave the probe directory. With one slot, the next call is
/// queued when the cancel lands, so an early release would start it at once.
/// Each start checks that no earlier start of this fake is still running.
#[test_case(stall_the_version_check, ANSWERS, "version_stalled" ; "while_the_version_check_stalls")]
#[test_case(stall_every_start, ANSWERS, "stalled" ; "while_the_probe_stalls")]
#[test_case(change_nothing, USAGE_THEN_HANG, "prompt" ; "while_the_run_hangs")]
fn a_cancelled_call_lets_go_only_once_its_processes_end(
    setup: fn(&FakeClaude),
    prompt: &str,
    busy: &str,
) {
    let fake = FakeClaude::new();
    let project = fake.project.path();
    setup(&fake);
    let (reg, _host) = fake.host(ONE_SLOT);
    let (tx, rx) = flume::unbounded::<Envelope>();
    let events = EventSender::new(tx, 0);
    let mut cancelled = fake.ctx(Some(&events), Some(FIRST_ID));
    let (trigger, token) = CancelToken::new();
    cancelled.cancel = token;
    let next = fake.ctx(Some(&events), Some(SECOND_ID));

    let (first, second) = thread::scope(|scope| {
        let first = scope.spawn(|| smol::block_on(within_deadline(call(&reg, &cancelled, prompt))));
        assert!(
            wait_until(DEADLINE, || fake.path(busy).exists()),
            "the call did not start its work"
        );
        let second = scope.spawn(|| smol::block_on(within_deadline(call(&reg, &next, ANSWERS))));
        if let Err(views) = wait_for_view_line(&rx, WAITING) {
            panic!("the next call did not show that it waits, and the views were {views:?}");
        }
        for stall in [STALL, STALL_VERSION] {
            let _ = fs::remove_file(fake.path(stall));
        }
        trigger.cancel();
        (first.join().unwrap(), second.join().unwrap())
    });
    // When the cancel lands decides the text: the runtime's cancel note, or
    // the error of a process the cancel stopped.
    assert!(first.is_err(), "the call was cancelled");
    assert_eq!(second.unwrap(), ANSWER);
    assert_eq!(
        fake.log("overlaps"),
        "",
        "a start found a cancelled process that continues to run"
    );
    assert!(
        wait_until(DEADLINE, || fake.probe_dirs_left(&project).is_empty()),
        "probe directories stayed"
    );
}

/// With one slot, a path that fails to release its permit leaves the next
/// call waiting forever. A call can be rejected before the start and during
/// the handshake.
#[test]
fn every_way_a_call_ends_gives_its_slot_back() {
    let failures = [
        (CRASH, CRASH_REPORT),
        (API_KEY, &[API_KEY_STOP]),
        (ANSWER_THEN_CRASH, LATE_CRASH_REPORT),
    ];
    let fake = FakeClaude::new();
    let (reg, _host) = fake.host(ONE_SLOT);
    let ctx = fake.ctx(None, None);
    let ask = |prompt| smol::block_on(within_deadline(call(&reg, &ctx, prompt)));

    move_to_an_older_version(&fake);
    let refused = ask(ANSWERS).unwrap_err();
    assert!(refused.contains(TOO_OLD), "got: {refused}");
    fake.answer_version(MINIMUM_VERSION);
    log_out(&fake);
    let refused = ask(ANSWERS).unwrap_err();
    assert!(refused.contains(LOGIN_HINT), "got: {refused}");
    fake.write("account.json", ACCOUNT_SUBSCRIPTION);
    for (scenario, stop) in failures {
        assert_mentions(&ask(scenario).unwrap_err(), stop);
    }
    assert_eq!(ask(ANSWERS).unwrap(), ANSWER);
}

/// The completed body keeps its click handler, so a click expands the whole
/// reply in place. Without it, the view would be rebuilt from the saved
/// text, which was cut for the model.
#[test]
fn a_finished_answer_expands_in_place() {
    let fake = FakeClaude::new();
    let (reg, host) = fake.host(ONE_SLOT);
    let (tx, events) = flume::unbounded::<Envelope>();
    let ctx = fake.ctx(Some(&EventSender::new(tx, 0)), Some(TOOL_USE_ID));
    assert_eq!(
        smol::block_on(within_deadline(call(&reg, &ctx, LONG_ANSWERS))).unwrap(),
        LONG_ANSWER
    );
    let view = events
        .drain()
        .filter_map(|env| match env.event {
            AgentEvent::LiveToolBuf { body, .. } => Some(body),
            _ => None,
        })
        .last()
        .expect("the call shows a view");
    let shown = view.take().text();
    assert!(
        !shown.contains(LAST_ANSWER_LINE),
        "the view is not closed: {shown}"
    );

    let (fallback_tx, fallback_rx) = flume::unbounded::<Envelope>();
    host.event_handle().request_click_with_fallback(
        TOOL_USE_ID.to_owned(),
        0,
        restored(LONG_ANSWERS, None, vec![0]),
        EventSender::new(fallback_tx, 0),
    );
    // An empty load drains the host's queue, so the click has landed.
    host.load_source("barrier", "").unwrap();
    let shown = view.take().text();
    assert!(shown.contains(LAST_ANSWER_LINE), "not expanded: {shown}");
    assert!(
        !fallback_rx
            .drain()
            .any(|env| matches!(env.event, AgentEvent::ToolSnapshot { .. })),
        "the click made the view again from the saved text"
    );
}

/// Returns the header lines a completed call saved in its state.
fn saved_header(output: &ToolOutput) -> Vec<String> {
    output
        .state()
        .and_then(|state| state["header"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|line| line.as_str().map(str::to_owned))
        .collect()
}

/// A version above the minimum runs with the same checks. Every other test
/// runs the minimum.
#[test]
fn a_newer_version_runs() {
    let fake = FakeClaude::new();
    fake.answer_version(NEWER_VERSION);
    let (reg, _host) = fake.host(ONE_SLOT);
    let output = smol::block_on(within_deadline(execute(
        &reg,
        &fake.ctx(None, None),
        ANSWERS,
    )))
    .unwrap();

    let header = saved_header(&output);
    assert!(
        header.iter().any(|line| line.contains(ROUTE_CLAIM)),
        "header: {header:?}"
    );
}

/// A reloaded session keeps only the text and the saved state, so the live
/// call's route and usage header must be rebuilt from that state.
#[test]
fn a_restored_answer_rebuilds_its_header() {
    let fake = FakeClaude::new();
    let (reg, host) = fake.host(ONE_SLOT);
    let output = smol::block_on(within_deadline(execute(
        &reg,
        &fake.ctx(None, None),
        ANSWERS,
    )))
    .unwrap();
    let state = output.state().cloned();
    let header = saved_header(&output);
    assert!(
        !header.is_empty(),
        "test condition: the call saved its header"
    );

    let handle = host.event_handle();
    let (tx, rx) = flume::unbounded::<Envelope>();
    handle.request_restore(
        restored(ANSWERS, state, Vec::new()),
        EventSender::new(tx, 0),
    );
    handle.wait_restore_complete_for_test();
    // An empty load drains the host's queue, so the snapshot has been sent.
    host.load_source("barrier", "").unwrap();
    let body = rx
        .drain()
        .find_map(|env| match env.event {
            AgentEvent::ToolSnapshot { snapshot, .. } => Some(snapshot.text()),
            _ => None,
        })
        .expect("the session that maki loaded again showed a body");
    for line in header.iter().map(String::as_str).chain([ANSWER]) {
        assert!(
            body.contains(line),
            "{line:?} is missing from the view that maki made again:\n{body}"
        );
    }
}

/// A descendant that closed the run's output holds no stream the job waits
/// on, so only killing the whole process group stops it once the run exits,
/// whether the run replied or was stopped.
#[test_case(STRAY_THEN_ANSWER, Ok(ANSWER) ; "from_a_run_that_answered")]
#[test_case(STRAY_THEN_API_KEY, Err(API_KEY_STOP) ; "from_a_run_stopped_at_init")]
fn a_stray_descendant_dies_with_its_run(scenario: &str, want: Result<&str, &str>) {
    let fake = FakeClaude::new();
    match (fake.ask(scenario), want) {
        (Ok(answer), Ok(expected)) => assert_eq!(answer, expected),
        (Err(err), Err(expected)) => assert!(err.contains(expected), "got: {err}"),
        (got, _) => panic!("incorrect result: {got:?}"),
    }
    let stray = fake.log("stray").trim().to_owned();
    assert!(
        !stray.is_empty(),
        "test condition: the fake started a stray process"
    );
    assert!(
        wait_until(DEADLINE, || !running(&stray)),
        "the stray descendant continued after its run"
    );
}

/// The probe runs from a temporary directory, so a relative `executable`
/// must be resolved before the probe, or only the version check would find
/// it.
#[test]
fn a_relative_executable_is_found_from_every_directory() {
    let fake = FakeClaude::new();
    let _in_project = WorkingDir::enter(&fake.project.path());
    executable(
        &fake.project.path(),
        RELATIVE_EXECUTABLE,
        &WRAPPER.replace("@TARGET@", &fake.path("claude").to_string_lossy()),
    );
    let mut opts = fake.opts(ONE_SLOT);
    opts.insert("executable".into(), json!(RELATIVE_EXECUTABLE));
    let (reg, _host) = load(opts);
    let answer = smol::block_on(within_deadline(call(&reg, &fake.ctx(None, None), ANSWERS)));
    assert_eq!(answer.unwrap(), ANSWER);
}

fn leave_the_session_alone(_: &Path) {}

fn add_a_local_api_key_helper(session: &Path) {
    let settings = session.join(".claude");
    fs::create_dir(&settings).unwrap();
    fs::write(settings.join("settings.local.json"), HELPER_SETTINGS).unwrap();
}

/// Under ACP a session can work in a different checkout from maki's. The call
/// must check that checkout's settings and run Claude Code there, never in
/// the process directory, which here is the fake's clean project.
#[test_case(leave_the_session_alone, Ok(ANSWER) ; "runs_in_the_session_dir")]
#[test_case(add_a_local_api_key_helper, Err(LOCAL_HELPER_CONFLICT) ; "checks_the_session_dirs_settings")]
fn a_session_in_another_checkout_is_served_there(setup: fn(&Path), want: Result<&str, &str>) {
    let fake = FakeClaude::new();
    let _in_project = WorkingDir::enter(&fake.project.path());
    let session = tempdir().unwrap();
    fs::create_dir(session.path().join(".git")).unwrap();
    let session_dir = session.path().canonicalize().unwrap();
    setup(&session_dir);
    let (reg, _host) = fake.host(ONE_SLOT);
    let ctx = stub_ctx_in(&session_dir, None, None);
    let result = smol::block_on(within_deadline(call(&reg, &ctx, ANSWERS)));

    let started: Vec<PathBuf> = fake.log("started_in").lines().map(PathBuf::from).collect();
    assert!(
        !started.contains(&fake.project.path()),
        "Claude Code started in the directory of the process"
    );
    match (result, want) {
        (Ok(answer), Ok(expected)) => {
            assert_eq!(answer, expected);
            assert!(started.contains(&session_dir), "started in {started:?}");
        }
        (Err(err), Err(expected)) => {
            assert!(err.contains(expected), "got: {err}");
            assert!(
                err.contains(&session_dir.display().to_string()),
                "got: {err}"
            );
        }
        (got, _) => panic!("incorrect result: {got:?}"),
    }
}

/// The checks resolve the config directory from maki's directory and Claude
/// Code from its own, so only an absolute path means the same to both. The
/// plugin refuses the option when it loads.
#[test]
fn a_relative_config_dir_is_refused_before_anything_runs() {
    let fake = FakeClaude::new();
    let mut opts = fake.opts(ONE_SLOT);
    opts.insert("config_dir".into(), json!(RELATIVE_CONFIG_DIR));
    let err = try_load(opts)
        .err()
        .expect("the plugin must refuse the option");

    assert!(err.contains(NOT_ABSOLUTE), "got: {err}");
    assert_eq!(fake.log("calls"), "", "no process must run");
}
