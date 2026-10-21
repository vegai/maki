#![cfg(target_os = "linux")]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

use flume::Receiver;
use maki_agent::session::StoredSession;
use maki_providers::ThinkingSupport::{No, Required, Yes};
use maki_providers::test_support::{Canned, Requests, serve};
use maki_providers::{ContentBlock, ThinkingSupport};
use maki_storage::StateDir;
use maki_storage::model::{persist_thinking, read_thinking};
use maki_storage::sessions::StoredThinking;
use rustix::process::{Pid, Signal, kill_process};
use serde_json::{Value, json};
use test_case::test_case;
use wait_timeout::ChildExt;

#[path = "support/isolation.rs"]
mod isolation;
#[path = "support/wire.rs"]
#[allow(dead_code)]
mod wire;
use isolation::Isolation;
use wire::{INIT, RESULT, STREAM_FORMAT, SYSTEM};

const MODEL: &str = "thinking-fixture/fixture-model";
const PROMPT: &str = "Reply briefly.";
const REPLY: &str = "done";
const SAVED_THINKING: &str = "max";
const OUTPUT_CAP: u32 = 128_000;
const LOW_BUDGET: u64 = 12_800;
const MAX_BUDGET: u64 = 64_000;
const HIGH_BUDGET: u64 = 38_400;
const XHIGH_BUDGET: u64 = 51_200;
const MINIMAL_BUDGET: u64 = 6_400;
const EXPLICIT_BUDGET: u64 = 2048;
const SDK_BUDGET: u64 = 4096;
const INVALID_THINKING: &str = "unknown thinking value \"turbo\"";
const INVALID_EFFORT: &str = "invalid value 'turbo' for '--effort <EFFORT>'";
const INVALID_BUDGET: &str = "invalid value '-1' for '--max-thinking-tokens <MAX_THINKING_TOKENS>'";
const BUDGET_RANGE: &str = "not in 0..=4294967295";
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
        let config = fixture.isolation.dir("config/maki");
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

struct Process {
    child: Child,
    events: Receiver<Value>,
}

impl Process {
    fn start(mut command: Command) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        let (tx, events) = flume::unbounded();
        thread::spawn(move || {
            for line in output.lines() {
                let line = line.expect("cannot read a protocol event from maki");
                let event = serde_json::from_str(&line)
                    .unwrap_or_else(|error| panic!("invalid protocol event: {error}: {line}"));
                if tx.send(event).is_err() {
                    break;
                }
            }
        });
        Self { child, events }
    }

    fn send(&mut self, message: Value) {
        let input = self.child.stdin.as_mut().unwrap();
        serde_json::to_writer(&mut *input, &message).unwrap();
        writeln!(input).unwrap();
        input.flush().unwrap();
    }

    fn until(&self, matches: impl Fn(&Value) -> bool) -> Value {
        loop {
            let event = self.events.recv_timeout(PROTOCOL_TIMEOUT).unwrap();
            if matches(&event) {
                return event;
            }
        }
    }

    fn response(&self, id: u64) -> Value {
        let response = self.until(|event| event["id"] == id);
        assert!(response.get("error").is_none(), "{response}");
        response[RESULT].clone()
    }

    fn finish(&mut self) {
        self.finish_status(true);
    }

    fn finish_status(&mut self, success: bool) {
        self.child.stdin.take();
        let status = self
            .child
            .wait_timeout(PROTOCOL_TIMEOUT)
            .unwrap()
            .unwrap_or_else(|| panic!("maki did not exit in {PROTOCOL_TIMEOUT:?}"));
        assert_eq!(status.success(), success, "{status}");
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test_case(false, Some("low"), &["--thinking", "xhigh"], Yes, "xhigh", Some(XHIGH_BUDGET) ; "print_thinking_overrides_config_and_state")]
#[test_case(false, Some("low"), &["--effort", "high"], Yes, "high", Some(HIGH_BUDGET) ; "print_effort")]
#[test_case(false, Some("low"), &["--max-thinking-tokens", "2048"], Yes, "2048", Some(EXPLICIT_BUDGET) ; "print_budget")]
#[test_case(false, Some("high"), &["--max-thinking-tokens", "0"], Yes, "off", None ; "print_zero_budget")]
#[test_case(false, Some("high"), &["--thinking", "off"], Yes, "off", None ; "explicit_off")]
#[test_case(false, Some("xhigh"), &[], Yes, "xhigh", Some(XHIGH_BUDGET) ; "configured_fallback")]
#[test_case(false, None, &[], Yes, "low", Some(LOW_BUDGET) ; "headless_default_ignores_tui_state")]
#[test_case(false, None, &["--thinking", "max"], No, "off", None ; "unsupported_model_is_reported_as_off")]
#[test_case(false, None, &["--thinking", "off"], Required, "minimal", Some(MINIMAL_BUDGET) ; "required_thinking_is_reported_as_minimal")]
#[test_case(true, Some("low"), &["--thinking", "xhigh"], Yes, "xhigh", Some(XHIGH_BUDGET) ; "sdk_thinking")]
#[test_case(true, Some("low"), &["--effort", "xhigh"], Yes, "xhigh", Some(XHIGH_BUDGET) ; "sdk_effort")]
#[test_case(true, Some("low"), &["--max-thinking-tokens", "2048"], Yes, "2048", Some(EXPLICIT_BUDGET) ; "sdk_budget")]
#[test_case(true, Some("high"), &["--max-thinking-tokens", "0"], Yes, "off", None ; "sdk_zero_budget")]
#[test_case(true, None, &["--thinking", "max"], No, "off", None ; "sdk_model_clamping")]
#[test_case(true, None, &["--thinking", "disabled"], Yes, "off", None ; "sdk_disabled")]
#[test_case(true, None, &["--thinking", "enabled", "--effort", "high"], Yes, "high", Some(HIGH_BUDGET) ; "sdk_enabled_with_effort")]
#[test_case(true, None, &["--thinking=adaptive", "--effort", "high"], Yes, "high", Some(HIGH_BUDGET) ; "sdk_adaptive_with_effort")]
#[test_case(true, None, &["--max-thinking-tokens", "4096", "--effort", "high"], Yes, "4096", Some(SDK_BUDGET) ; "sdk_budget_with_effort")]
#[test_case(false, None, &["--thinking", "disabled", "--max-thinking-tokens", "4096", "--effort", "high"], Yes, "off", None ; "disabled_overrides_other_flags")]
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
#[test_case(&["--thinking", "xhigh"], Some(XHIGH_BUDGET) ; "thinking")]
#[test_case(&["--effort", "high"], Some(HIGH_BUDGET) ; "effort")]
#[test_case(&["--max-thinking-tokens", "2048"], Some(EXPLICIT_BUDGET) ; "budget")]
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
    let output = fixture.command().args(flags).output().unwrap();
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
    const EXPECTED: &str = "thinking flags apply only to TUI, --print, SDK and ACP sessions";
    let fixture = Fixture::new(None, Yes);
    let output = fixture.command().args(flags).output().unwrap();
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
    let (waiting_tx, waiting_rx) = flume::bounded(1);
    let (release_tx, release_rx) = flume::bounded::<()>(1);
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
    let mut command = fixture.command();
    command.args(["-p", "--model", MODEL, "--output-format", STREAM_FORMAT]);
    if sdk {
        command.args(["--input-format", STREAM_FORMAT]);
    } else {
        command.arg(PROMPT);
    }
    let mut process = Process::start(command);
    let init = process.until(|event| event["subtype"] == INIT);
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
        assert_eq!(result["is_error"], true);
    }
    process.finish_status(signal.is_none());
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
    let output = fixture
        .command()
        .args([
            "-p",
            "-r",
            &id.to_string(),
            "--max-turns",
            "0",
            "--output-format",
            STREAM_FORMAT,
            PROMPT,
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let events: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
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
fn provider_failure_is_nonzero_and_persists_an_error_outside_the_transcript() {
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
    assert_eq!(
        stored
            .errors()
            .iter()
            .map(|error| error.message.as_str())
            .collect::<Vec<_>>(),
        &[result[RESULT].as_str().unwrap()]
    );
    assert!(stored.messages().iter().all(|message| {
        message
            .first_text_content()
            .is_none_or(|text| !text.contains(PROVIDER_FAILURE))
    }));
}
