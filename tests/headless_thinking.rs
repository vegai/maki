#![cfg(target_os = "linux")]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

use flume::Receiver;
use maki_providers::ThinkingSupport;
use maki_providers::ThinkingSupport::{No, Required, Yes};
use maki_providers::test_support::{Canned, Requests, serve};
use maki_storage::StateDir;
use maki_storage::model::{persist_thinking, read_thinking};
use maki_storage::sessions::StoredThinking;
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
        self.child.stdin.take();
        let status = self
            .child
            .wait_timeout(PROTOCOL_TIMEOUT)
            .unwrap()
            .unwrap_or_else(|| panic!("maki did not exit in {PROTOCOL_TIMEOUT:?}"));
        assert!(status.success(), "{status}");
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
#[test_case(false, None, &[], Yes, "max", Some(MAX_BUDGET) ; "saved_fallback")]
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

#[test_case(&["--thinking", "xhigh"], Some(XHIGH_BUDGET) ; "thinking")]
#[test_case(&["--effort", "high"], Some(HIGH_BUDGET) ; "effort")]
#[test_case(&["--max-thinking-tokens", "2048"], Some(EXPLICIT_BUDGET) ; "budget")]
#[test_case(&["--max-thinking-tokens", "0"], None ; "zero_budget")]
fn acp_launch_thinking_reaches_the_request(flags: &[&str], budget: Option<u64>) {
    let fixture = Fixture::new(Some("low"), Yes);
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
