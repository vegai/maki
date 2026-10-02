//! These tests run the claude CLI, so they spend a little of the caller's
//! subscription. They run only on request: `cargo nextest run -p
//! maki-providers --run-ignored only -E 'test(/claude_code::live::/)'`.

use std::env;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use flume::Sender;
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};
use test_case::test_case;

use super::checks::child_env;
use super::error::Error;
use super::run::{Limits, Request, Thinking, models, request};
use super::{SLUG, agent_error, utf8_vars};
use crate::model::Model;
use crate::process::find_program;
use crate::providers::catalog::warm_catalog;
use crate::{ContentBlock, Message, ProviderEvent, Role, StopReason, StreamResponse};

const MODEL: &str = "claude-haiku-4-5";
const TOOL: &str = "lookup";
const WORD: &str = "alpha";
const OTHER_WORD: &str = "beta";
/// Only the tool results hold these values, so a reply that quotes them
/// proves the results reached the model through the transcript.
const MEANING: &str = "FIRST-LETTER-7";
const OTHER_MEANING: &str = "SECOND-LETTER-3";
const IDLE: Duration = Duration::from_secs(120);
const TINY_OUTPUT_CAP: u32 = 20;
const LONG_STORY: &str = "Write a 150-word story about a lighthouse.";
const ADAPTIVE_MODEL: &str = "claude-sonnet-5";
const LOW_EFFORT: &str = "low";
/// Has a 1M window, which only its `-1m` id requests.
const LONG_CONTEXT_MODEL: &str = "claude-sonnet-5-1m";
/// Refuses thinking off, and is Claude Code's default model.
const ALWAYS_THINKING_MODEL: &str = "claude-opus-5-5";
/// The minimum thinking budget is 1024.
const LIVE_BUDGET: u32 = 2048;
const REPLY_OK: &str = "Reply with only the word OK.";
const THINK_FIRST: &str =
    "Think it through step by step, then reply with only the result of 17 * 23.";
const MODEL_ID_START: &str = "claude-";
const SYSTEM: &str = "You are an agent that helps. Use your tools for the reply.";
/// Far beyond Haiku's 200K window.
const OVERSIZED_WORDS: usize = 300_000;
const OVERFLOW_OPT_IN: &str = "MAKI_LIVE_OVERFLOW";
/// Lines of filler in the first message, about 12 tokens each, so the
/// cache gain from the second request is easy to see.
const FILLER_LINES: usize = 400;
/// Less than the filler's size, so only a cache hit on the first message can
/// reach it.
const MIN_CACHE_GAIN: u32 = 3000;
const NOTE_TOOL: &str = "note";
const FIRST_NOTE: &str = "start";
/// Slow enough that a long second call keeps the first one held for more than
/// a minute.
const SLOW_MODEL: &str = "claude-sonnet-5";
/// The limit that Claude Code's MCP client puts on a call without a server
/// `timeout`.
const CLIENT_CALL_LIMIT: Duration = Duration::from_secs(60);

struct Live {
    claude: PathBuf,
    env: Vec<(String, String)>,
    _project: TempDir,
    cwd: PathBuf,
}

impl Live {
    fn new() -> Self {
        let here = env::current_dir().unwrap();
        let claude = find_program("claude", &here).expect("claude must be on PATH");
        let project = tempdir().unwrap();
        let cwd = project.path().canonicalize().unwrap();
        let (env, _) = child_env(utf8_vars(env::vars_os()));
        Self {
            claude,
            env,
            _project: project,
            cwd,
        }
    }

    fn ask(
        &self,
        model: &str,
        messages: &[Message],
        tools: &Value,
        max_output: Option<u32>,
        thinking: Thinking,
    ) -> Result<StreamResponse, Error> {
        let (events, _received) = flume::unbounded();
        self.ask_streaming(model, messages, tools, max_output, thinking, &events)
    }

    fn ask_streaming(
        &self,
        model: &str,
        messages: &[Message],
        tools: &Value,
        max_output: Option<u32>,
        thinking: Thinking,
        events: &Sender<ProviderEvent>,
    ) -> Result<StreamResponse, Error> {
        smol::block_on(request(Request {
            executable: &self.claude,
            env: &self.env,
            model,
            cwd: &self.cwd,
            system: SYSTEM,
            messages,
            tools,
            events,
            plan_usage: &Mutex::default(),
            temp_dir: &env::temp_dir(),
            max_output,
            thinking,
            limits: &Limits::new(IDLE),
            slot_wait: Duration::ZERO,
        }))
    }
}

fn lookup_tool() -> Value {
    json!([{
        "name": TOOL,
        "description": "Find a word in the glossary of this project, and give the text for it.",
        "input_schema": { "type": "object", "properties": { "word": { "type": "string" } }, "required": ["word"] },
    }])
}

/// The model's `lookup` call for `word`, answered with `meaning`.
fn lookup_round(reply: &StreamResponse, word: &str, meaning: &str) -> Message {
    assert_eq!(reply.stop_reason, Some(StopReason::ToolUse));
    let (id, name, input) = reply
        .message
        .tool_uses()
        .next()
        .expect("the model called a tool");
    assert_eq!(name, TOOL);
    assert_eq!(input["word"], Value::from(word));
    Message {
        role: Role::User,
        content: vec![ContentBlock::tool_result(
            id,
            format!("{word}: {meaning}"),
            false,
        )],
        ..Default::default()
    }
}

fn answer(reply: &StreamResponse) -> String {
    assert_eq!(reply.stop_reason, Some(StopReason::EndTurn));
    reply
        .message
        .first_text_content()
        .unwrap_or_default()
        .to_owned()
}

/// maki compacts after an overflow, as with the anthropic provider. The
/// request is about 1.5 MB, so the test runs only with `MAKI_LIVE_OVERFLOW`
/// set.
#[test]
#[ignore = "runs the claude CLI on the subscription of the caller"]
fn live_a_conversation_past_the_window_overflows() {
    if env::var_os(OVERFLOW_OPT_IN).is_none() {
        eprintln!("skipped: set {OVERFLOW_OPT_IN}=1 to send about 1.5 MB");
        return;
    }
    let messages = [Message::user("word ".repeat(OVERSIZED_WORDS))];
    let err = Live::new()
        .ask(MODEL, &messages, &json!([]), None, Thinking::Default)
        .unwrap_err();
    assert!(agent_error(err).is_context_overflow());
}

/// Each listed id gets a price from the anthropic provider's rows, or from
/// models.dev for a release not in them. Listing sends no prompt.
#[test]
#[ignore = "runs the claude CLI"]
fn live_the_listed_models_are_full_ids_with_a_price() {
    warm_catalog();
    let live = Live::new();
    let listed = smol::block_on(models(
        &live.claude,
        &live.env,
        &live.cwd,
        &env::temp_dir(),
        &Mutex::default(),
        Limits::new(IDLE).startup,
    ))
    .unwrap();

    assert!(!listed.is_empty());
    assert!(
        listed
            .iter()
            .all(|model| model.id.starts_with(MODEL_ID_START)),
        "{listed:?}"
    );
    let unpriced: Vec<&str> = listed
        .iter()
        .map(|listed| listed.id.as_str())
        .filter(|id| {
            let model = Model::from_spec(&format!("{SLUG}/{id}")).unwrap();
            model.pricing.input <= 0.0 || model.pricing.output <= 0.0
        })
        .collect();
    assert!(unpriced.is_empty(), "no price for {unpriced:?}");
    assert!(
        listed.iter().all(|model| model.window.is_some()),
        "Claude Code gave no window for some of {listed:?}"
    );
}

/// Every maki thinking setting, with maki's flags and no tools, as for
/// `/btw`.
#[test_case(ADAPTIVE_MODEL, Thinking::Effort(LOW_EFFORT) => ignore["runs the claude CLI on the subscription of the caller"] ; "an_effort_on_an_adaptive_model")]
#[test_case(MODEL, Thinking::Budget(LIVE_BUDGET) => ignore["runs the claude CLI on the subscription of the caller"] ; "a_budget")]
#[test_case(MODEL, Thinking::Off => ignore["runs the claude CLI on the subscription of the caller"] ; "off")]
#[test_case(ALWAYS_THINKING_MODEL, Thinking::Off => ignore["runs the claude CLI on the subscription of the caller"] ; "off_on_a_model_that_always_thinks")]
fn live_thinking_settings_are_accepted(model: &str, thinking: Thinking) {
    let messages = [Message::user(REPLY_OK.to_owned())];
    let reply = Live::new()
        .ask(model, &messages, &json!([]), None, thinking)
        .unwrap();
    answer(&reply);
}

/// Claude Code shows the thinking text only with a flag that its help leaves
/// out, so a version that drops or ignores the flag shows no thinking.
#[test]
#[ignore = "runs the claude CLI on the subscription of the caller"]
fn live_thinking_reaches_maki_as_text() {
    let messages = [Message::user(THINK_FIRST.to_owned())];
    let (events, received) = flume::unbounded();
    let reply = Live::new()
        .ask_streaming(
            MODEL,
            &messages,
            &json!([]),
            None,
            Thinking::Budget(LIVE_BUDGET),
            &events,
        )
        .unwrap();
    answer(&reply);
    let thought: String = received
        .drain()
        .filter_map(|event| match event {
            ProviderEvent::ThinkingDelta { text } => Some(text),
            _ => None,
        })
        .collect();
    assert!(!thought.trim().is_empty(), "no thinking text reached maki");
}

/// A `-1m` id runs its model, which Claude Code opens with the 1M window.
#[test]
#[ignore = "runs the claude CLI on the subscription of the caller"]
fn live_a_1m_id_runs_its_model() {
    let messages = [Message::user(REPLY_OK.to_owned())];
    let reply = Live::new()
        .ask(
            LONG_CONTEXT_MODEL,
            &messages,
            &json!([]),
            None,
            Thinking::Default,
        )
        .unwrap();
    answer(&reply);
}

/// The reply stops at the limit and maki continues it. Whatever Claude Code
/// prints to continue does not matter.
#[test]
#[ignore = "runs the claude CLI on the subscription of the caller"]
fn live_a_reply_at_its_output_cap_comes_back_cut() {
    let messages = [Message::user(LONG_STORY.to_owned())];
    let reply = Live::new()
        .ask(
            MODEL,
            &messages,
            &json!([]),
            Some(TINY_OUTPUT_CAP),
            Thinking::Default,
        )
        .unwrap();
    assert_eq!(reply.stop_reason, Some(StopReason::MaxTokens));
    assert_eq!(reply.usage.output, TINY_OUTPUT_CAP);
}

/// Two tool rounds. The history must show the first call's tool under the
/// name the model uses, or the model could copy a name it cannot call.
#[test]
#[ignore = "runs the claude CLI on the subscription of the caller"]
fn live_tool_calls_round_trip_through_maki() {
    let live = Live::new();
    let tools = lookup_tool();
    let ask = |messages: &[Message]| {
        live.ask(MODEL, messages, &tools, None, Thinking::Default)
            .unwrap()
    };
    let mut messages = vec![Message::user(format!(
        "Use the {TOOL} tool on the word {WORD}. Only after you have its result, use it on \
         the word {OTHER_WORD}. Then tell me the results of the two calls, without a change."
    ))];

    let first = ask(&messages);
    let first_result = lookup_round(&first, WORD, MEANING);
    messages.extend([first.message, first_result]);
    let second = ask(&messages);
    let second_result = lookup_round(&second, OTHER_WORD, OTHER_MEANING);
    messages.extend([second.message, second_result]);
    let text = answer(&ask(&messages));

    for meaning in [MEANING, OTHER_MEANING] {
        assert!(
            text.contains(meaning),
            "the reply must use the results of maki: {text}"
        );
    }
}

/// Each request resends the whole conversation. The second must read the
/// first message from the prompt cache, beyond the system prompt and tools.
/// Claude 5 models cache differently, and only with a tool offered.
#[test_case(MODEL ; "an_older_model")]
#[test_case(ADAPTIVE_MODEL ; "a_claude_5_model")]
#[ignore = "runs the claude CLI on the subscription of the caller"]
fn live_a_second_request_reads_the_first_from_the_cache(model: &str) {
    let live = Live::new();
    let filler: String = (0..FILLER_LINES)
        .map(|i| format!("Line {i}: the lighthouse keeper writes the weather in the log.\n"))
        .collect();
    let tools = lookup_tool();
    let ask = |messages: &[Message]| {
        live.ask(model, messages, &tools, None, Thinking::Default)
            .unwrap()
    };
    let mut messages = vec![Message::user(format!("{filler}{REPLY_OK}"))];
    let first = ask(&messages);
    messages.extend([first.message, Message::user(REPLY_OK.to_owned())]);
    let second = ask(&messages);

    let gain = second
        .usage
        .cache_read
        .saturating_sub(first.usage.cache_read);
    assert!(
        gain >= MIN_CACHE_GAIN,
        "the second request read only {gain} more tokens from the cache: first {:?}, second {:?}",
        first.usage,
        second.usage
    );
}

/// Claude Code starts each call as soon as its block ends, and maki holds the
/// call until the whole reply is done. A long second call keeps the first one
/// held for more than the minute after which Claude Code's MCP client would
/// otherwise give up on it and answer it itself.
#[test]
#[ignore = "runs the claude CLI on the subscription of the caller"]
fn live_a_call_held_past_a_minute_comes_back_to_maki() {
    let live = Live::new();
    let tools = json!([{
        "name": NOTE_TOOL,
        "description": "Save a note.",
        "input_schema": { "type": "object", "properties": { "text": { "type": "string" } }, "required": ["text"] },
    }]);
    let messages = [Message::user(format!(
        "Make two {NOTE_TOOL} calls in parallel, in this one reply, without waiting for the \
         result of the first. The first call gets the text `{FIRST_NOTE}`. The second gets an \
         essay of at least 5000 words about lighthouses. Write no other text."
    ))];

    let started = Instant::now();
    let reply = live
        .ask(SLOW_MODEL, &messages, &tools, None, Thinking::Default)
        .unwrap();
    let took = started.elapsed();

    assert_eq!(reply.stop_reason, Some(StopReason::ToolUse));
    let calls: Vec<_> = reply.message.tool_uses().collect();
    assert_eq!(calls.len(), 2, "the model made {} calls", calls.len());
    assert_eq!(calls[0].2["text"], Value::from(FIRST_NOTE));
    assert!(
        took > CLIENT_CALL_LIMIT,
        "the reply took only {took:?}, so no call was held long enough to test. Run the test again."
    );
}
