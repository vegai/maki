#![cfg(target_os = "linux")]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::OwnedFd;
use std::process::Command;
use std::sync::mpsc::sync_channel;
use std::thread;
use std::time::Duration;

use maki_agent::session::StoredSession;
use maki_providers::ThinkingSupport::{No, Required, Yes};
use maki_providers::test_support::{Canned, Requests, serve};
use maki_providers::{ContentBlock, ThinkingSupport};
use maki_storage::StateDir;
use maki_storage::model::{persist_thinking, read_thinking};
use maki_storage::sessions::StoredThinking;
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::process::{Pid, PidfdFlags, Signal, kill_process, pidfd_open, pidfd_send_signal};
use serde_json::{Value, json};
use test_case::test_case;

#[path = "support/process.rs"]
#[allow(dead_code)]
mod process;
use process::{Process, finish, ndjson};

#[path = "support/isolation.rs"]
mod isolation;
#[path = "support/wire.rs"]
#[allow(dead_code)]
mod wire;
use isolation::Isolation;
use wire::{INIT, RESULT, STREAM_FORMAT, SYSTEM};

const CONFIG_DIR: &str = "config/maki";
const TOOL_USE_ID: &str = "call1";
const MODEL: &str = "thinking-fixture/fixture-model";
const PROMPT: &str = "Reply briefly.";
const REPLY: &str = "done";
const SAVED_THINKING: &str = "max";
const OUTPUT_CAP: u32 = 128_000;
const LOW_BUDGET: u64 = 12_800;
const MAX_BUDGET: u64 = 64_000;
const XHIGH_BUDGET: u64 = 51_200;
const MINIMAL_BUDGET: u64 = 6_400;
const SDK_BUDGET: u64 = 4096;
const INVALID_THINKING: &str = "unknown thinking value \"turbo\"";
const INVALID_EFFORT: &str = "invalid value 'turbo' for '--effort <EFFORT>'";
const INVALID_BUDGET: &str = "invalid value '-1' for '--max-thinking-tokens <MAX_THINKING_TOKENS>'";
const BUDGET_RANGE: &str = "not in 0..=4294967295";
const PLUGIN_PID: &str = "plugin-job.pid";
const PLUGIN_JOB: &str = r#"
local id = assert(maki.fn.jobstart({ "sleep", "300" }, { scope = "plugin" }))
assert(maki.fs.write(@PID@, tostring(maki.fn.jobinfo(id).pid)))
"#;
const DROPPED_PROMPT: &str = "fixture refused this prompt";
const CATALOG_CACHE_FILE: &str = "models-dev-catalog.json";
const CATALOG_REFRESH_FAILED: &str = "catalog refresh failed, keeping existing cache";
const REFUSED_PROXY: &str = "http://127.0.0.1:0";
const PROTOCOL_TIMEOUT: Duration = Duration::from_secs(30);
const INITIALIZE_ID: u64 = 1;
const NEW_SESSION_ID: u64 = 2;
const PROMPT_ID: u64 = 3;
const STREAM: &[Canned] = &[Canned::sse(concat!(
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\n",
    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"done\"}}\n\n",
    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
))];

struct PluginJob(OwnedFd);

impl Drop for PluginJob {
    fn drop(&mut self) {
        let _ = pidfd_send_signal(&self.0, Signal::KILL);
    }
}

struct Fixture {
    isolation: Isolation,
    requests: Requests,
}

impl Fixture {
    fn new(configured: Option<&str>, support: ThinkingSupport) -> Self {
        let (url, requests) = serve(STREAM);
        Self::at(configured, support, url, requests)
    }

    fn at(
        configured: Option<&str>,
        support: ThinkingSupport,
        url: String,
        requests: Requests,
    ) -> Self {
        let fixture = Self {
            isolation: Isolation::new(),
            requests,
        };
        let config = fixture.isolation.dir(CONFIG_DIR);
        let settings =
            configured.map_or_else(String::new, |value| format!("always_thinking = {value:?}"));
        fs::write(
            config.join("init.lua"),
            format!("maki.setup({{ {settings} }})"),
        )
        .unwrap();
        fs::write(
            config.join("providers.toml"),
            format!(
                r#"[thinking-fixture]
protocol = "anthropic"
base_url = "{url}"
api_key = "fixture-key"
models = [{{ id = "fixture-model", max_output_tokens = {OUTPUT_CAP}, supports_thinking = {supports}, requires_thinking = {requires} }}]
"#,
                supports = support != No,
                requires = support == Required,
            ),
        )
        .unwrap();
        persist_thinking(
            &fixture.storage(),
            StoredThinking::parse_setting(SAVED_THINKING).unwrap(),
        );
        fixture
    }

    fn storage(&self) -> StateDir {
        StateDir::from_path(self.isolation.dir("state/maki"))
    }

    fn command(&self) -> Command {
        self.isolation.command()
    }

    fn assert_budget(&self, expected: Option<u64>) {
        let requests = self.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let request = requests[0].json();
        assert_eq!(
            request["thinking"]["budget_tokens"].as_u64(),
            expected,
            "{request}"
        );
        assert_eq!(
            read_thinking(&self.storage()),
            Some(StoredThinking::parse_setting(SAVED_THINKING).unwrap())
        );
    }
}

#[test_case(false, Some("low"), &["--thinking", "xhigh"], Yes, "xhigh", Some(XHIGH_BUDGET) ; "print_thinking_overrides_config_and_state")]
#[test_case(false, Some("xhigh"), &[], Yes, "xhigh", Some(XHIGH_BUDGET) ; "configured_fallback")]
#[test_case(false, None, &[], Yes, "low", Some(LOW_BUDGET) ; "headless_default_ignores_tui_state")]
#[test_case(false, None, &["--thinking", "max"], No, "off", None ; "unsupported_model_is_reported_as_off")]
#[test_case(false, None, &["--thinking", "off"], Required, "minimal", Some(MINIMAL_BUDGET) ; "required_thinking_is_reported_as_minimal")]
#[test_case(true, Some("low"), &["--effort", "xhigh"], Yes, "xhigh", Some(XHIGH_BUDGET) ; "sdk_effort")]
#[test_case(true, None, &["--max-thinking-tokens", "4096", "--effort", "high"], Yes, "4096", Some(SDK_BUDGET) ; "sdk_budget_with_effort")]
fn headless_thinking_reaches_the_request_and_init(
    sdk: bool,
    configured: Option<&str>,
    flags: &[&str],
    support: ThinkingSupport,
    expected: &str,
    budget: Option<u64>,
) {
    let fixture = Fixture::new(configured, support);
    let mut command = fixture.command();
    command.args(["-p", "--model", MODEL, "--output-format", STREAM_FORMAT]);
    command.args(flags);
    if sdk {
        command.args(["--input-format", STREAM_FORMAT]);
    } else {
        command.arg(PROMPT);
    }
    let mut process = Process::start(command);
    let init = process.until(|event| event["type"] == SYSTEM && event["subtype"] == INIT);
    assert_eq!(init["thinking"], expected);
    if sdk {
        process.send(json!({"type": "user", "message": {"role": "user", "content": PROMPT}}));
    }
    let result = process.until(|event| event["type"] == RESULT);
    assert_eq!(result["is_error"], false, "{result}");
    assert_eq!(result[RESULT], REPLY);
    process.finish();
    fixture.assert_budget(budget);
}

#[test_case(&[], Some(MAX_BUDGET) ; "saved_acp_default")]
#[test_case(&["--max-thinking-tokens", "0"], None ; "zero_budget")]
fn acp_launch_thinking_reaches_the_request(flags: &[&str], budget: Option<u64>) {
    let fixture = Fixture::new(None, Yes);
    let mut command = fixture.command();
    command.args(["acp", "--model", MODEL]).args(flags);
    let mut process = Process::start(command);
    process.send(json!({"jsonrpc": "2.0", "id": INITIALIZE_ID, "method": "initialize", "params": {"protocolVersion": 1, "clientCapabilities": {}}}));
    process.response(INITIALIZE_ID);
    process.send(json!({"jsonrpc": "2.0", "id": NEW_SESSION_ID, "method": "session/new", "params": {"cwd": fixture.isolation.dir("project"), "mcpServers": []}}));
    let session = process.response(NEW_SESSION_ID);
    process.send(json!({"jsonrpc": "2.0", "id": PROMPT_ID, "method": "session/prompt", "params": {"sessionId": session["sessionId"], "prompt": [{"type": "text", "text": PROMPT}]}}));
    process.response(PROMPT_ID);
    process.finish();
    fixture.assert_budget(budget);
}

#[test_case(&["-p", "--thinking", "turbo"], INVALID_THINKING ; "print")]
#[test_case(&["-p", "--input-format", STREAM_FORMAT, "--effort", "turbo"], INVALID_EFFORT ; "sdk")]
#[test_case(&["acp", "--max-thinking-tokens=-1"], INVALID_BUDGET ; "acp")]
fn invalid_launch_thinking_exits_before_a_provider_request(flags: &[&str], expected: &str) {
    let fixture = Fixture::new(None, Yes);
    let output = finish(fixture.command().args(flags));
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains(expected), "{stderr}");
    if expected == INVALID_BUDGET {
        assert!(stderr.contains(BUDGET_RANGE), "{stderr}");
    }
    assert!(output.stdout.is_empty());
    assert!(fixture.requests.lock().unwrap().is_empty());
}

#[test_case(&["models", "--thinking", "low"] ; "thinking")]
#[test_case(&["models", "--effort", "high"] ; "effort")]
#[test_case(&["models", "--max-thinking-tokens", "0"] ; "budget")]
fn thinking_flags_require_a_session(flags: &[&str]) {
    const EXPECTED: &str = "has no effect with this subcommand. It applies to: the TUI, --print, SDK mode (--print --input-format stream-json), maki acp";
    let fixture = Fixture::new(None, Yes);
    let output = finish(fixture.command().args(flags));
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains(EXPECTED), "{stderr}");
    assert!(fixture.requests.lock().unwrap().is_empty());
}

const TOOL_REPLY: &str = concat!(
    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\n",
    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call1\",\"name\":\"glob\",\"input\":{}}}\n\n",
    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"pattern\\\":\\\"*.txt\\\"}\"}}\n\n",
    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":1}}\n\n",
    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
);
const TOOL_STREAM: &[Canned] = &[Canned::sse(TOOL_REPLY)];
const MAX_TURNS_ERROR: &str = "error_max_turns";
const INTERRUPTED: &str = "error_during_execution";
const CANCELLED: &str = "cancelled";

#[test_case(false ; "print")]
#[test_case(true ; "sdk")]
fn max_turns_limits_requests_and_returns_a_failure(sdk: bool) {
    let (url, requests) = serve(TOOL_STREAM);
    let fixture = Fixture::at(None, Yes, url, requests);
    let mut command = fixture.command();
    command.args([
        "-p",
        "--model",
        MODEL,
        "--max-turns",
        "1",
        "--output-format",
        STREAM_FORMAT,
    ]);
    if sdk {
        command.args(["--input-format", STREAM_FORMAT]);
    } else {
        command.arg(PROMPT);
    }
    let mut process = Process::start(command);
    if sdk {
        process.send(json!({"type":"user", "message":{"role":"user", "content":PROMPT}}));
    }
    let result = process.until(|event| event["type"] == RESULT);
    assert_eq!(result["subtype"], MAX_TURNS_ERROR, "{result}");
    assert_eq!(result["is_error"], true);
    assert_eq!(result["num_turns"], 1);
    process.finish_status(false);
    assert_eq!(fixture.requests.lock().unwrap().len(), 1);
    let session = StoredSession::load(
        result["session_id"].as_str().unwrap().parse().unwrap(),
        &fixture.storage(),
    )
    .unwrap();
    assert!(
        session
            .messages()
            .iter()
            .flat_map(|message| &message.content)
            .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
    );
}

fn read_http_request(stream: &TcpStream) {
    stream.set_read_timeout(Some(PROTOCOL_TIMEOUT)).unwrap();
    let mut reader = BufReader::new(stream);
    let mut size = 0;
    loop {
        let mut line = String::new();
        assert_ne!(reader.read_line(&mut line).unwrap(), 0);
        if line == "\r\n" {
            break;
        }
        if let Some((key, value)) = line.split_once(':')
            && key.eq_ignore_ascii_case("content-length")
        {
            size = value.trim().parse().unwrap();
        }
    }
    let mut body = vec![0; size];
    reader.read_exact(&mut body).unwrap();
}

#[test_case(Some(Signal::TERM), false ; "sigterm")]
#[test_case(Some(Signal::INT), false ; "sigint")]
#[test_case(Some(Signal::KILL), false ; "sigkill_keeps_checkpoint")]
#[test_case(Some(Signal::TERM), true ; "sdk_sigterm")]
#[test_case(None, true ; "sdk_control_interrupt_exits_successfully")]
fn interrupted_print_leaves_a_resumable_prefix(signal: Option<Signal>, sdk: bool) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let (waiting_tx, waiting_rx) = sync_channel(1);
    let (release_tx, release_rx) = sync_channel::<()>(1);
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        read_http_request(&stream);
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{TOOL_REPLY}", TOOL_REPLY.len()).unwrap();
        drop(stream);
        let (stream, _) = listener.accept().unwrap();
        read_http_request(&stream);
        waiting_tx.send(()).unwrap();
        let _ = release_rx.recv_timeout(PROTOCOL_TIMEOUT);
    });
    let fixture = Fixture::at(None, Yes, url, Requests::default());
    let config = fixture.isolation.dir(CONFIG_DIR);
    let pid_path = config.join(PLUGIN_PID);
    if signal != Some(Signal::KILL) {
        let init_path = config.join("init.lua");
        let init = fs::read_to_string(&init_path).unwrap();
        fs::write(
            config.join("plugin.toml"),
            "[permissions]\nrun = true\nfs_read = true\nfs_write = true\n",
        )
        .unwrap();
        fs::write(
            init_path,
            init + &PLUGIN_JOB.replace("@PID@", &serde_json::to_string(&pid_path).unwrap()),
        )
        .unwrap();
    }
    let mut command = fixture.command();
    command.args(["-p", "--model", MODEL, "--output-format", STREAM_FORMAT]);
    if sdk {
        command.args(["--input-format", STREAM_FORMAT]);
    } else {
        command.arg(PROMPT);
    }
    let mut process = Process::start(command);
    let init = process.until(|event| event["subtype"] == INIT);
    let plugin_job = (signal != Some(Signal::KILL)).then(|| {
        let pid = Pid::from_raw(fs::read_to_string(&pid_path).unwrap().parse().unwrap()).unwrap();
        PluginJob(pidfd_open(pid, PidfdFlags::empty()).unwrap())
    });
    if sdk {
        process.send(json!({"type":"user", "message":{"role":"user", "content":PROMPT}}));
    }
    waiting_rx.recv_timeout(PROTOCOL_TIMEOUT).unwrap();
    let id = init["session_id"].as_str().unwrap().parse().unwrap();
    let stored = StoredSession::load(id, &fixture.storage()).unwrap();
    assert!(
        stored
            .messages()
            .iter()
            .flat_map(|message| &message.content)
            .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
    );
    if let Some(signal) = signal {
        kill_process(
            Pid::from_raw(process.child.id().try_into().unwrap()).unwrap(),
            signal,
        )
        .unwrap();
    } else {
        process.send(json!({"type":"control_request", "request_id":"fixture-interrupt", "request":{"subtype":"interrupt"}}));
    }
    if signal != Some(Signal::KILL) {
        let result = process.until(|event| event["type"] == RESULT);
        assert_eq!(result["subtype"], INTERRUPTED, "{result}");
        if !sdk {
            assert_eq!(result["stop_reason"], CANCELLED, "{result}");
        }
        assert_eq!(result["is_error"], true);
    }
    process.finish_status(signal.is_none());
    if let Some(job) = plugin_job {
        let mut fds = [PollFd::new(&job.0, PollFlags::IN)];
        let timeout = Timespec::try_from(PROTOCOL_TIMEOUT).unwrap();
        let exited = poll(&mut fds, Some(&timeout)).unwrap() > 0;
        assert!(exited, "plugin job outlived the plugin host");
    }
    let resumed = StoredSession::load(id, &fixture.storage()).unwrap();
    let saved: Vec<Value> = stored
        .messages()
        .iter()
        .map(|message| serde_json::to_value(message).unwrap())
        .collect();
    let restored: Vec<Value> = resumed
        .messages()
        .iter()
        .map(|message| serde_json::to_value(message).unwrap())
        .collect();
    assert!(restored.starts_with(&saved));
    drop(release_tx);
    server.join().unwrap();
    let output = finish(fixture.command().args([
        "-p",
        "-r",
        &id.to_string(),
        "--max-turns",
        "0",
        "--output-format",
        STREAM_FORMAT,
        PROMPT,
    ]));
    assert!(!output.status.success());
    let events = ndjson(&output.stdout);
    assert_eq!(
        events.last().unwrap()["subtype"],
        MAX_TURNS_ERROR,
        "{events:?}"
    );
}

const PROVIDER_FAILURE: &str = "fixture request failed";
const ERROR_STREAM: &[Canned] = &[Canned::json(
    400,
    r#"{"error":{"type":"invalid_request_error","message":"fixture request failed"}}"#,
)];

#[test]
fn provider_failure_is_nonzero_and_stays_outside_the_transcript() {
    let (url, requests) = serve(ERROR_STREAM);
    let fixture = Fixture::at(None, Yes, url, requests);
    let mut command = fixture.command();
    command.args([
        "-p",
        "--model",
        MODEL,
        "--output-format",
        STREAM_FORMAT,
        PROMPT,
    ]);
    let mut process = Process::start(command);
    let result = process.until(|event| event["type"] == RESULT);
    assert_eq!(result["is_error"], true);
    assert!(result[RESULT].as_str().unwrap().contains(PROVIDER_FAILURE));
    process.finish_status(false);
    let stored = StoredSession::load(
        result["session_id"].as_str().unwrap().parse().unwrap(),
        &fixture.storage(),
    )
    .unwrap();
    assert!(stored.messages().iter().all(|message| {
        message
            .first_text_content()
            .is_none_or(|text| !text.contains(PROVIDER_FAILURE))
    }));
}

#[test]
fn model_refresh_failure_still_lists_configured_models() {
    let fixture = Fixture::new(None, No);
    fs::write(
        fixture.isolation.dir("cache/maki").join(CATALOG_CACHE_FILE),
        "{}",
    )
    .unwrap();
    let output = finish(
        fixture
            .command()
            .args(["models", "--refresh"])
            .env("HTTPS_PROXY", REFUSED_PROXY),
    );
    assert!(!output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stdout.contains(MODEL), "{stdout}");
    assert!(stderr.contains(CATALOG_REFRESH_FAILED), "{stderr}");
}

#[test_case(false ; "print")]
#[test_case(true ; "sdk")]
fn a_dropped_prompt_fails_the_process(sdk: bool) {
    let fixture = Fixture::new(None, No);
    let config = fixture.isolation.dir(CONFIG_DIR);
    fs::write(config.join("plugin.toml"), "[permissions]\n").unwrap();
    fs::write(config.join("init.lua"), format!(
        "maki.setup({{}})\nmaki.api.set_slot('agent.user_message', function() return nil, {DROPPED_PROMPT:?} end)"
    )).unwrap();
    let mut command = fixture.command();
    command.args(["-p", "--model", MODEL, "--output-format", STREAM_FORMAT]);
    if sdk {
        command.args(["--input-format", STREAM_FORMAT]);
    } else {
        command.arg(PROMPT);
    }
    let mut process = Process::start(command);
    if sdk {
        process.send(json!({"type":"user", "message":{"role":"user", "content":PROMPT}}));
    }
    let result = process.until(|event| event["type"] == RESULT);
    assert_eq!(result["is_error"], true, "{result}");
    assert!(result.to_string().contains(DROPPED_PROMPT), "{result}");
    process.finish_status(false);
    assert!(fixture.requests.lock().unwrap().is_empty());
}

#[test]
fn checkpoints_precede_provider_requests_and_tool_dispatch() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let (waiting_tx, waiting_rx) = sync_channel(1);
    let (release_tx, release_rx) = sync_channel::<()>(1);
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        read_http_request(&stream);
        waiting_tx.send(()).unwrap();
        release_rx.recv_timeout(PROTOCOL_TIMEOUT).unwrap();
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{TOOL_REPLY}", TOOL_REPLY.len()).unwrap();
    });
    let fixture = Fixture::at(None, No, url, Requests::default());
    let config = fixture.isolation.dir(CONFIG_DIR);
    fs::write(config.join("plugin.toml"), "[permissions]\n").unwrap();
    let sessions = fixture.isolation.dir("state/maki/sessions");
    let observed = config.join("checkpoint.jsonl");
    fs::write(
        config.join("init.lua"),
        format!(
            r#"
maki.setup({{}})
maki.api.set_slot("tool.glob.input", function(prev, input, ctx)
  local path = maki.fs.joinpath({sessions}, ctx.session_id .. ".jsonl")
  assert(maki.fs.write({observed}, assert(maki.fs.read(path))))
  return prev(input, ctx)
end)
"#,
            sessions = serde_json::to_string(&sessions).unwrap(),
            observed = serde_json::to_string(&observed).unwrap()
        ),
    )
    .unwrap();
    let mut command = fixture.command();
    command.args([
        "-p",
        "--model",
        MODEL,
        "--output-format",
        STREAM_FORMAT,
        "--max-turns",
        "1",
        PROMPT,
    ]);
    let mut process = Process::start(command);
    let init = process.until(|event| event["subtype"] == INIT);
    waiting_rx.recv_timeout(PROTOCOL_TIMEOUT).unwrap();
    let id = init["session_id"].as_str().unwrap().parse().unwrap();
    let stored = StoredSession::load(id, &fixture.storage()).unwrap();
    assert!(
        stored
            .messages()
            .iter()
            .any(|message| message.first_text_content() == Some(PROMPT))
    );
    release_tx.send(()).unwrap();
    process.until(|event| event["type"] == RESULT);
    process.finish_status(false);
    let before_dispatch = fs::read_to_string(observed).unwrap();
    assert!(before_dispatch.contains(TOOL_USE_ID), "{before_dispatch}");
    assert!(before_dispatch.contains("tool_use"), "{before_dispatch}");
    server.join().unwrap();
}
