//! Model text must wait for the handshake and the validated init event. Tool-call replies
//! need both the complete generation and a held call.
//!
//! The held call establishes the handoff to maki. Text replies complete at their result event.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};

use super::checks::{self, InitExpect, RULES};
use super::error::{Error, TOO_MANY_REQUESTS};
use super::mcp::Handoff;
use super::transcript::Catalog;
use crate::model::is_same_model;
use crate::providers::anthropic::{LABEL_SESSION, LABEL_WEEK_ALL};
use crate::{
    ContentBlock, Message, ProviderEvent, ProviderUsage, Role, StopReason, StreamResponse,
    TokenUsage, UsageLimit,
};

pub(crate) const INITIALIZE: &str = "initialize";
pub(crate) const CONTROL_RESPONSE: &str = "control_response";
pub(crate) const SUCCESS: &str = "success";
/// The handshake ids in the shared rules. A test checks the rules name
/// exactly these.
pub(crate) const ACCOUNT_ANSWER: &str = "account";
pub(crate) const SETTINGS_ANSWER: &str = "settings";
pub(crate) const HOOKS_ANSWER: &str = "hooks";
const TOOL_USE_STOP: &str = "tool_use";
const TOOL_USE_BLOCK: &str = "tool_use";
const TEXT_BLOCK: &str = "text";
/// Claude Code's own placeholders. Its output filter drops a message whose
/// only block is one of these, as it drops blank text.
const DROPPED_TEXTS: [&str; 2] = ["(no content)", "[Request interrupted by user for tool use]"];
const MESSAGE_START: &str = "message_start";
const BLOCK_START: &str = "content_block_start";
const BLOCK_DELTA: &str = "content_block_delta";
const BLOCK_STOP: &str = "content_block_stop";
const MESSAGE_DELTA: &str = "message_delta";
const MESSAGE_STOP: &str = "message_stop";
/// Events a generation sends after its start has named the model.
const GENERATION_EVENTS: [&str; 5] = [
    BLOCK_START,
    BLOCK_DELTA,
    BLOCK_STOP,
    MESSAGE_DELTA,
    MESSAGE_STOP,
];
const MAX_TOKENS_STOP: &str = "max_tokens";
const REFUSAL_STOP: &str = "refusal";
const WINDOW_FULL_STOP: &str = "model_context_window_exceeded";
/// Stop reasons of an incomplete reply.
const CUT_STOPS: [&str; 2] = [MAX_TOKENS_STOP, WINDOW_FULL_STOP];
const TOOL_RESULT: &str = "tool_result";
const BATCH_TOOL: &str = "batch";
const BATCH_CALLS: &str = "tool_calls";
const BATCH_CALL_TOOL: &str = "tool";
const API_ERROR_MARK: &str = "is_api_error_message";
const RATE_LIMITED: &str = "rate_limit";
/// The plan status of a `rate_limit_event` once the plan's limit is used up.
const PLAN_REJECTED: &str = "rejected";
/// As 2.1.280 words it.
const INPUT_REJECTED: &str = "<tool_use_error>InputValidationError:";
/// As 2.1.284 words it.
const NO_SUCH_TOOL: &str = "<tool_use_error>Error: No such tool available:";
/// How Claude Code rejects a call it never hands to maki.
const REJECTIONS: [&str; 2] = [INPUT_REJECTED, NO_SUCH_TOOL];
const SHOWN_LINE_CHARS: usize = 80;
/// How many event kinds the trace keeps for a failed request's debug log.
const TRACE_LEN: usize = 32;
const UNNAMED_HOOK: &str = "startup";
const NO_MESSAGE: &str = "no message";
/// The Anthropic provider's names for the same windows.
const WINDOW_LABELS: [(&str, &str); 2] =
    [("five_hour", LABEL_SESSION), ("seven_day", LABEL_WEEK_ALL)];
const EXTRA_USAGE: &str = "Extra usage";
const EXTRA_USAGE_IN_USE: &str = "on, with charges more than the plan";
const PERCENT: f64 = 100.0;
const MS_PER_SEC: u64 = 1000;

#[derive(Debug)]
pub(crate) enum Step {
    /// Send the prompt.
    Ready,
    Event(ProviderEvent),
    Done,
    /// Claude Code sends it on a timer, so it says nothing about progress.
    Alive,
    Nothing,
}

/// `None` until Claude Code measures it.
#[derive(Default)]
struct Usage {
    input: Option<u32>,
    cache_creation: Option<u32>,
    cache_read: Option<u32>,
    output: Option<u32>,
}

#[derive(Debug)]
struct Parked {
    tool_use_id: String,
    name: String,
    arguments: Value,
}

pub(crate) struct Turn<'a> {
    catalog: &'a Catalog,
    expect: InitExpect<'a>,
    /// Each report is stored as it arrives, so a request that fails still
    /// leaves its usage behind.
    plan_usage: &'a Mutex<Option<ProviderUsage>>,
    answers: HashMap<String, Value>,
    prompted: bool,
    accepted: bool,
    message_id: Option<String>,
    /// Each block arrives once, between its start and its stop, so a block
    /// without a start is either a repeat or came from nowhere. A text block
    /// keeps its streamed text, because Claude Code never sends a blank one.
    started: Vec<Option<String>>,
    blocks: Vec<ContentBlock>,
    usage: Usage,
    stop_reason: Option<String>,
    complete: bool,
    result: bool,
    parked: Vec<Parked>,
    /// Claude Code answered a call itself because the call broke its schema.
    /// maki runs the call anyway, it fails the same way, and the model gets
    /// an error it can retry.
    rejected: bool,
    /// The last plan report said the plan's limit is used up.
    plan_rejected: bool,
    /// An API error can precede the result. Wait for the result because only it has the status.
    refusal: Option<Refusal>,
    /// A reply that stopped with a block missing waits for the result too,
    /// which can name an API error as the cause.
    lost: Option<Error>,
    /// Kinds of the latest events, logged when the request fails.
    trace: VecDeque<String>,
}

/// A name `--model` accepts, and the model it runs.
pub(crate) struct Offered {
    pub name: String,
    pub model: String,
}

fn answered(message: &Value) -> String {
    message_text(message)
        .chars()
        .take(SHOWN_LINE_CHARS)
        .collect()
}

fn message_text(message: &Value) -> String {
    let text = |value: &Value| -> Vec<String> {
        match value {
            Value::String(text) => vec![text.clone()],
            Value::Array(parts) => parts
                .iter()
                .filter_map(|part| part["text"].as_str().map(str::to_owned))
                .collect(),
            _ => Vec::new(),
        }
    };
    let content = &message["content"];
    let parts = match content.as_array() {
        Some(blocks) => blocks
            .iter()
            .flat_map(|block| [text(&block["content"]), text(&block["text"])].concat())
            .collect(),
        None => text(content),
    };
    parts.join(" ")
}

/// Returns the line that sends `request` (which carries its subtype), to be
/// answered under `id`.
pub(crate) fn control_request(id: &str, request: &Value) -> String {
    let line = json!({ "type": "control_request", "request_id": id, "request": request });
    format!("{line}\n")
}

/// One text block per transcript block, so the prompt cache can reuse the
/// blocks that an earlier request already sent.
pub(crate) fn user_message(blocks: &[String]) -> String {
    let content: Vec<Value> = blocks
        .iter()
        .map(|text| json!({ "type": "text", "text": text }))
        .collect();
    let message = json!({ "type": "user", "message": { "role": "user", "content": content } });
    format!("{message}\n")
}

fn count(usage: &Value, key: &str) -> Option<u32> {
    usage[key].as_u64().and_then(|n| u32::try_from(n).ok())
}

impl<'a> Turn<'a> {
    pub fn new(
        catalog: &'a Catalog,
        expect: InitExpect<'a>,
        plan_usage: &'a Mutex<Option<ProviderUsage>>,
    ) -> Self {
        Self {
            catalog,
            expect,
            plan_usage,
            answers: HashMap::new(),
            prompted: false,
            accepted: false,
            message_id: None,
            started: Vec::new(),
            blocks: Vec::new(),
            usage: Usage::default(),
            stop_reason: None,
            complete: false,
            result: false,
            parked: Vec::new(),
            rejected: false,
            plan_rejected: false,
            refusal: None,
            lost: None,
            trace: VecDeque::with_capacity(TRACE_LEN),
        }
    }

    pub fn prompted(&mut self) {
        self.prompted = true;
    }

    pub fn offered(&self) -> Vec<Offered> {
        self.answers
            .get(ACCOUNT_ANSWER)
            .and_then(|account| account["models"].as_array())
            .into_iter()
            .flatten()
            .filter_map(|offer| {
                Some(Offered {
                    name: offer["value"].as_str()?.to_owned(),
                    model: offer["resolvedModel"].as_str()?.to_owned(),
                })
            })
            .collect()
    }

    /// Only a held call is missing.
    pub fn awaits_handoff(&self) -> bool {
        self.complete && self.calls_tools() && self.parked.is_empty() && !self.rejected
    }

    pub fn calls_tools(&self) -> bool {
        self.calls().next().is_some()
    }

    /// Only the result is missing.
    pub fn awaits_result(&self) -> bool {
        self.complete && !self.result && !self.calls_tools()
    }

    /// The stream has given every usage count, the final output count
    /// included, so a complete reply no longer needs the result.
    pub fn has_stream_usage(&self) -> bool {
        let Usage {
            input,
            cache_creation,
            cache_read,
            output,
        } = &self.usage;
        input.is_some() && cache_creation.is_some() && cache_read.is_some() && output.is_some()
    }

    /// Claude Code then hands maki no call and tells the model to continue.
    /// A reply that filled the context window counts as cut too.
    pub fn truncated(&self) -> bool {
        self.complete
            && self
                .stop_reason
                .as_deref()
                .is_some_and(|reason| CUT_STOPS.contains(&reason))
    }

    pub fn accepted(&self) -> bool {
        self.accepted
    }

    /// Kinds of the latest events, oldest first, as `type/subtype` or the API
    /// event's type.
    pub fn trace(&self) -> String {
        self.trace
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn record(&mut self, event: &Value) {
        let kind = match (event["type"].as_str(), event["subtype"].as_str()) {
            (Some("stream_event"), _) => event["event"]["type"].as_str().unwrap_or("stream_event"),
            (Some(kind), _) => kind,
            (None, _) => "?",
        };
        if self.trace.len() == TRACE_LEN {
            self.trace.pop_front();
        }
        self.trace.push_back(match event["subtype"].as_str() {
            Some(subtype) if kind == event["type"] => format!("{kind}/{subtype}"),
            _ => kind.to_owned(),
        });
    }

    /// A line that is not a Claude Code event stops the request. Anthropic
    /// can add API events to `stream_event`, so an unknown API event is
    /// ignored until the reply is complete.
    pub fn feed(&mut self, line: &str) -> Result<Step, Error> {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            return Err(Error::NotAnEvent(
                line.chars().take(SHOWN_LINE_CHARS).collect(),
            ));
        };
        self.record(&event);
        match (event["type"].as_str(), event["subtype"].as_str()) {
            (Some(CONTROL_RESPONSE), _) => self.answer(&event["response"]),
            (Some("control_request"), _) => Err(Error::Asked(event["request"]["subtype"].clone())),
            (Some("system"), Some("init")) => self.init(&event),
            (Some("system"), Some("hook_started")) => Err(Error::HookStarted(
                event["hook_event"]
                    .as_str()
                    .unwrap_or(UNNAMED_HOOK)
                    .to_owned(),
            )),
            (Some("rate_limit_event"), _) => {
                self.plan_rejected = event["rate_limit_info"]["status"] == PLAN_REJECTED;
                *self
                    .plan_usage
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) =
                    Some(usage_of(&event["rate_limit_info"]));
                Ok(Step::Nothing)
            }
            (Some("keep_alive"), _) => Ok(Step::Alive),
            (Some("system"), Some("api_retry")) => Err(Error::CliRetry {
                kind: event["error"].as_str().unwrap_or_default().to_owned(),
                status: event["error_status"]
                    .as_u64()
                    .and_then(|s| u16::try_from(s).ok()),
                delay: event["retry_delay_ms"].as_u64().map(Duration::from_millis),
            }),
            (Some("system"), Some("compact_boundary")) => Err(Error::Compacted),
            (Some("tool_progress"), _) => Ok(Step::Alive),
            (Some("system"), _) => Ok(Step::Nothing),
            (Some("stream_event" | "assistant" | "user" | "result"), _) if !self.accepted => {
                Err(Error::UncheckedStart)
            }
            (Some("assistant"), _) if event[API_ERROR_MARK] == true => {
                self.refusal = Some(Refusal {
                    kind: event["error"].as_str().unwrap_or_default().to_owned(),
                    text: message_text(&event["message"]),
                    status: status_of(&event["api_error_status"]),
                });
                Ok(Step::Nothing)
            }
            (Some("stream_event"), _) => self.stream_event(&event["event"]),
            (Some("assistant"), _) => self.assistant(&event["message"]),
            (Some("user"), _) if self.truncated() && continuation(&event) => Ok(Step::Nothing),
            (Some("user"), _) if self.rejects_calls(&event["message"]) => {
                self.rejected = true;
                self.settle()
            }
            (Some("user"), _) => Err(Error::AnsweredItself(answered(&event["message"]))),
            (Some("result"), _) => self.result(&event),
            _ => Err(Error::UnknownEvent(event["type"].clone())),
        }
    }

    fn answer(&mut self, response: &Value) -> Result<Step, Error> {
        let id = response["request_id"].as_str().unwrap_or_default();
        if !RULES.handshake.iter().any(|step| step.id == id) || self.answers.contains_key(id) {
            return Err(Error::StrayAnswer(id.to_owned()));
        }
        if response["subtype"] != SUCCESS {
            return Err(Error::RequestRefused {
                id: id.to_owned(),
                error: response["error"].clone(),
            });
        }
        self.answers
            .insert(id.to_owned(), response["response"].clone());
        if RULES
            .handshake
            .iter()
            .any(|step| !self.answers.contains_key(&step.id))
        {
            return Ok(Step::Nothing);
        }
        match checks::account_problem(&self.answers[ACCOUNT_ANSWER]).or_else(|| {
            checks::policy_problem(&self.answers[SETTINGS_ANSWER], &self.answers[HOOKS_ANSWER])
        }) {
            Some(problem) => Err(Error::Check(problem)),
            None => Ok(Step::Ready),
        }
    }

    fn init(&mut self, event: &Value) -> Result<Step, Error> {
        if !self.prompted {
            return Err(Error::UncheckedStart);
        }
        if let Some(problem) = checks::init_problem(event, &self.expect) {
            return Err(Error::Check(problem));
        }
        self.accepted = true;
        Ok(Step::Nothing)
    }

    fn stream_event(&mut self, event: &Value) -> Result<Step, Error> {
        let kind = event["type"].as_str();
        if self.complete && kind != Some(MESSAGE_START) {
            return Err(Error::LateContent);
        }
        // Ignore the whole generation, and show the user nothing, until its
        // start names a model that was checked.
        if self.message_id.is_none() && kind.is_some_and(|kind| GENERATION_EVENTS.contains(&kind)) {
            return Err(Error::OutsideGeneration);
        }
        match kind {
            Some(MESSAGE_START) => {
                let message = &event["message"];
                if self.message_id.is_some() {
                    return Err(Error::SecondGeneration);
                }
                let Some(id) = message["id"].as_str().filter(|id| !id.is_empty()) else {
                    return Err(Error::NoGenerationId);
                };
                let ran = message["model"].as_str().unwrap_or_default();
                if !is_same_model(ran, self.expect.model) {
                    return Err(Error::OtherModel {
                        asked: self.expect.model.to_owned(),
                        ran: ran.to_owned(),
                    });
                }
                self.message_id = Some(id.to_owned());
                self.take_usage(&message["usage"], false);
                Ok(Step::Nothing)
            }
            Some(BLOCK_START) => {
                let block = &event["content_block"];
                self.started.push(
                    (block["type"] == TEXT_BLOCK)
                        .then(|| block["text"].as_str().unwrap_or_default().to_owned()),
                );
                if block["type"] != TOOL_USE_BLOCK {
                    return Ok(Step::Nothing);
                }
                let Some(id) = block["id"].as_str().filter(|id| !id.is_empty()) else {
                    return Err(Error::NoCallId);
                };
                let name = self.maki_name(&block["name"])?;
                Ok(Step::Event(ProviderEvent::ToolUseStart {
                    id: id.to_owned(),
                    name,
                }))
            }
            Some(BLOCK_DELTA) => {
                let delta = &event["delta"];
                let text = |key: &str| delta[key].as_str().unwrap_or_default().to_owned();
                Ok(match delta["type"].as_str() {
                    Some("text_delta") => {
                        let text = text("text");
                        if let Some(Some(streamed)) = self.started.last_mut() {
                            streamed.push_str(&text);
                        }
                        Step::Event(ProviderEvent::TextDelta { text })
                    }
                    Some("thinking_delta") => Step::Event(ProviderEvent::ThinkingDelta {
                        text: text("thinking"),
                    }),
                    _ => Step::Nothing,
                })
            }
            Some(MESSAGE_DELTA) => {
                let delta = &event["delta"];
                if delta["stop_reason"] == REFUSAL_STOP {
                    let explanation = delta["stop_details"]["explanation"].as_str();
                    return Err(Error::Refused(explanation.unwrap_or(NO_MESSAGE).to_owned()));
                }
                self.stop_reason = delta["stop_reason"].as_str().map(str::to_owned);
                self.take_usage(&event["usage"], true);
                Ok(Step::Nothing)
            }
            Some(MESSAGE_STOP) => {
                self.complete = true;
                self.settle()
            }
            _ => Ok(Step::Nothing),
        }
    }

    /// Only the last `message_delta` has the real output count.
    fn take_usage(&mut self, usage: &Value, is_final: bool) {
        let fields = [
            ("input_tokens", &mut self.usage.input),
            (
                "cache_creation_input_tokens",
                &mut self.usage.cache_creation,
            ),
            ("cache_read_input_tokens", &mut self.usage.cache_read),
        ];
        for (key, field) in fields {
            if let Some(n) = count(usage, key) {
                *field = Some(n);
            }
        }
        if is_final {
            self.usage.output = count(usage, "output_tokens");
        }
    }

    /// Claude Code rejects a call to a tool maki did not offer, and the call
    /// goes to maki, which answers it as an unknown tool.
    fn maki_name(&self, exposed: &Value) -> Result<String, Error> {
        exposed
            .as_str()
            .filter(|name| !name.is_empty())
            .map(|name| self.catalog.maki_name_or_made_up(name).to_owned())
            .ok_or_else(|| Error::NotOffered(exposed.clone()))
    }

    /// Each block comes in its own event, tagged with the generation's id. A
    /// completed reply is frozen, because a later block could add a call that
    /// Claude Code never hands to maki.
    fn assistant(&mut self, message: &Value) -> Result<Step, Error> {
        if self.complete {
            return Err(Error::LateContent);
        }
        if self
            .message_id
            .as_deref()
            .is_none_or(|streaming| message["id"].as_str() != Some(streaming))
        {
            return Err(Error::OutsideGeneration);
        }
        for block in message["content"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            let text = |key: &str| block[key].as_str().unwrap_or_default().to_owned();
            let parsed = match block["type"].as_str() {
                Some(TEXT_BLOCK) => ContentBlock::Text { text: text("text") },
                Some("thinking") => ContentBlock::Thinking {
                    thinking: text("thinking"),
                    signature: block["signature"].as_str().map(str::to_owned),
                },
                Some("redacted_thinking") => ContentBlock::RedactedThinking { data: text("data") },
                Some(TOOL_USE_BLOCK) => {
                    let Some(id) = block["id"].as_str().filter(|id| !id.is_empty()) else {
                        return Err(Error::NoCallId);
                    };
                    let name = self.maki_name(&block["name"])?;
                    let input = &block["input"];
                    if !input.is_object() {
                        return Err(Error::NotArguments {
                            name,
                            input: input.clone(),
                        });
                    }
                    if self.calls().any(|(known, _, _)| known == id) {
                        return Err(Error::RepeatedCall(id.to_owned()));
                    }
                    ContentBlock::tool_use(id, name, input.clone())
                }
                _ => return Err(Error::UnreadableBlock(block["type"].clone())),
            };
            if self.blocks.len() == self.started.len() {
                return Err(Error::BlockCount {
                    started: self.started.len(),
                    sent: self.blocks.len() + 1,
                });
            }
            self.blocks.push(parsed);
        }
        Ok(Step::Nothing)
    }

    fn result(&mut self, event: &Value) -> Result<Step, Error> {
        if let Some(refusal) = self.refusal.take() {
            return Err(self.refused(refusal, status_of(&event["api_error_status"])));
        }
        if event["subtype"] != SUCCESS || event["is_error"] == true {
            return Err(Error::Failed(
                event["result"].as_str().unwrap_or(NO_MESSAGE).to_owned(),
            ));
        }
        if let Some(lost) = self.lost.take() {
            return Err(lost);
        }
        self.result = true;
        self.settle()
    }

    /// A used-up plan cannot pass before its window resets, so a rate limit
    /// after the plan report says so is no temporary error.
    fn refused(&self, refusal: Refusal, result_status: Option<u16>) -> Error {
        let Refusal { kind, text, status } = refusal;
        let status = status.or(result_status);
        if self.plan_rejected && (kind == RATE_LIMITED || status == Some(TOO_MANY_REQUESTS)) {
            Error::PlanLimit(text)
        } else {
            Error::ApiRefused { kind, text, status }
        }
    }

    /// The error a reply broke off with, for a request whose output ended
    /// before its result.
    pub fn take_broken(&mut self) -> Option<Error> {
        match self.refusal.take() {
            Some(refusal) => Some(self.refused(refusal, None)),
            None => self.lost.take(),
        }
    }

    pub fn is_broken(&self) -> bool {
        self.refusal.is_some() || self.lost.is_some()
    }

    /// The handoff must be a call from the reply, with the same tool and
    /// arguments.
    pub fn park(&mut self, handoff: Handoff) -> Result<Step, Error> {
        let (tool_use_id, name, arguments) = match handoff {
            Handoff::Parked {
                tool_use_id: Some(id),
                name,
                arguments,
            } => (id, name, arguments),
            Handoff::Parked { .. } => return Err(Error::NoCallId),
            Handoff::Invalid(error) => return Err(error),
        };
        let name = self
            .catalog
            .maki_name_of_server(&name)
            .ok_or_else(|| Error::NotOffered(Value::from(name.as_str())))?
            .to_owned();
        self.parked.push(Parked {
            tool_use_id,
            name,
            arguments,
        });
        self.settle()
    }

    fn calls(&self) -> impl Iterator<Item = (&str, &str, &Value)> {
        self.blocks.iter().filter_map(|block| match block {
            ContentBlock::ToolUse {
                id, name, input, ..
            } => Some((id.as_str(), name.as_str(), input)),
            _ => None,
        })
    }

    fn settle(&mut self) -> Result<Step, Error> {
        if !self.complete {
            return Ok(Step::Nothing);
        }
        // Claude Code sends blocks before `message_stop`, except those its output filter
        // removes. Wait for the result to explain any other absent block.
        let sendable = self
            .started
            .iter()
            .filter(|text| !text.as_deref().is_some_and(dropped_by_claude_code))
            .count();
        if self.blocks.len() != sendable {
            self.lost.get_or_insert(Error::BlockCount {
                started: self.started.len(),
                sent: self.blocks.len(),
            });
            return Ok(Step::Nothing);
        }
        let Some(stop_reason) = self.stop_reason.clone() else {
            return Err(Error::NoStopReason);
        };
        let calls: Vec<_> = self.calls().collect();
        if calls.is_empty() {
            if let Some(parked) = self.parked.first() {
                return Err(Error::NotInReply(parked.name.clone()));
            }
            // Otherwise maki would get a tool-call turn with nothing to run.
            if stop_reason == TOOL_USE_STOP {
                return Err(Error::NoCallsToRun);
            }
            return Ok(if self.result || self.truncated() {
                Step::Done
            } else {
                Step::Nothing
            });
        }
        for parked in &self.parked {
            let Some((_, name, input)) = calls.iter().find(|(id, _, _)| *id == parked.tool_use_id)
            else {
                return Err(Error::NotInReply(parked.name.clone()));
            };
            if *name != parked.name || **input != parked.arguments {
                return Err(Error::CallDiffers(parked.name.clone()));
            }
        }
        // Claude Code hands over no calls from a cut reply. Its last call can be incomplete,
        // so return only the text.
        if self.truncated() {
            return Ok(Step::Done);
        }
        if stop_reason != TOOL_USE_STOP {
            return Err(Error::StoppedFor(stop_reason));
        }
        Ok(if self.parked.is_empty() && !self.rejected {
            Step::Nothing
        } else {
            Step::Done
        })
    }

    /// The only answers Claude Code may give to maki's calls: a rejection over
    /// the schema, or over a tool maki did not offer.
    fn rejects_calls(&self, message: &Value) -> bool {
        message["content"].as_array().is_some_and(|blocks| {
            !blocks.is_empty()
                && blocks.iter().all(|block| {
                    block["type"] == TOOL_RESULT
                        && block["is_error"] == true
                        && block["tool_use_id"]
                            .as_str()
                            .is_some_and(|id| self.calls().any(|(call, _, _)| call == id))
                        && block["content"].as_str().is_some_and(|text| {
                            REJECTIONS.iter().any(|mark| text.starts_with(mark))
                        })
                })
        })
    }

    /// An absent usage count must fail the request. Zero would conceal incomplete protocol
    /// data.
    pub fn response(self) -> Result<StreamResponse, Error> {
        let stop_reason = if self.truncated() {
            StopReason::MaxTokens
        } else if self.calls_tools() {
            StopReason::ToolUse
        } else {
            StopReason::from_anthropic(self.stop_reason.as_deref().unwrap_or_default())
        };
        let Usage {
            input: Some(input),
            cache_creation: Some(cache_creation),
            cache_read: Some(cache_read),
            output,
        } = self.usage
        else {
            return Err(Error::NoUsage);
        };
        let output = output.ok_or(Error::NoFinalOutput)?;
        let mut blocks = self.blocks;
        if stop_reason == StopReason::MaxTokens {
            blocks.retain(|block| !matches!(block, ContentBlock::ToolUse { .. }));
        }
        for block in &mut blocks {
            if let ContentBlock::ToolUse { name, input, .. } = block
                && name == BATCH_TOOL
            {
                unexpose_batch(self.catalog, input);
            }
        }
        Ok(StreamResponse {
            message: Message {
                role: Role::Assistant,
                content: blocks,
                ..Default::default()
            },
            usage: TokenUsage {
                input,
                output,
                cache_creation,
                cache_read,
                cost: None,
            },
            stop_reason: Some(stop_reason),
        })
    }
}

/// The model names a batch's calls as Claude Code shows the tools, but maki
/// runs them by its own names. An unknown name stays, so maki reports it.
fn unexpose_batch(catalog: &Catalog, input: &mut Value) {
    let Some(calls) = input.get_mut(BATCH_CALLS).and_then(Value::as_array_mut) else {
        return;
    };
    for call in calls {
        if let Some(name) = call[BATCH_CALL_TOOL]
            .as_str()
            .and_then(|exposed| catalog.maki_name(exposed))
        {
            call[BATCH_CALL_TOOL] = Value::from(name);
        }
    }
}

struct Refusal {
    kind: String,
    text: String,
    status: Option<u16>,
}

fn status_of(status: &Value) -> Option<u16> {
    status.as_u64().and_then(|s| u16::try_from(s).ok())
}

/// The prompt Claude Code writes itself after a reply hits its limit. It
/// holds only text, so it answers no tool call.
fn continuation(event: &Value) -> bool {
    event["isSynthetic"] == true
        && event["message"]["content"]
            .as_array()
            .is_some_and(|blocks| blocks.iter().all(|block| block["type"] == TEXT_BLOCK))
}

/// Each block becomes a separate message. Claude Code omits messages that contain only blank
/// text or a placeholder.
fn dropped_by_claude_code(text: &str) -> bool {
    js_trim(text).is_empty() || DROPPED_TEXTS.contains(&text)
}

/// JavaScript's `trim`, which Claude Code's filter uses. Unlike Rust's, it
/// trims U+FEFF and keeps U+0085.
fn js_trim(text: &str) -> &str {
    text.trim_matches(|c: char| c == '\u{feff}' || (c.is_whitespace() && c != '\u{85}'))
}

fn usage_of(info: &Value) -> ProviderUsage {
    let mut limits: Vec<UsageLimit> = info["unifiedWindows"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(name, window)| UsageLimit {
            label: WINDOW_LABELS
                .iter()
                .find(|(known, _)| known == name)
                .map_or(name.as_str(), |(_, label)| label)
                .to_owned(),
            percentage: window["utilization"]
                .as_f64()
                .map(|used| (used * PERCENT).round() as u32),
            reset_at: window["resetsAt"].as_u64().map(|secs| secs * MS_PER_SEC),
            detail: None,
        })
        .collect();
    if info["isUsingOverage"] == true {
        limits.push(UsageLimit {
            label: EXTRA_USAGE.to_owned(),
            percentage: None,
            reset_at: None,
            detail: Some(EXTRA_USAGE_IN_USE.to_owned()),
        });
    }
    ProviderUsage {
        plan: None,
        limits,
        by_model_today: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::iter;
    use std::path::Path;
    use std::sync::Mutex;

    use test_case::test_case;

    use serde_json::{Value, json};

    use super::super::checks::{CONNECTED, DEFAULT_MODE, InitExpect, RULES, profile};
    use super::super::error::Error;
    use super::super::mcp::Handoff;
    use super::super::transcript::Catalog;
    use super::{
        ACCOUNT_ANSWER, API_ERROR_MARK, BATCH_CALL_TOOL, BATCH_CALLS, BATCH_TOOL, EXTRA_USAGE,
        EXTRA_USAGE_IN_USE, HOOKS_ANSWER, INPUT_REJECTED, MAX_TOKENS_STOP, NO_SUCH_TOOL,
        PLAN_REJECTED, RATE_LIMITED, REFUSAL_STOP, SETTINGS_ANSWER, SHOWN_LINE_CHARS, SUCCESS,
        Step, TOOL_RESULT, TOOL_USE_STOP, Turn, WINDOW_FULL_STOP, dropped_by_claude_code, usage_of,
        user_message,
    };
    use crate::{StopReason, StreamResponse};

    const CWD: &str = "/work";
    const VERSION: &str = "2.1.284";
    const LINUX: &str = "linux";
    const SERVER: &str = "maki";
    const END_TURN: &str = "end_turn";
    /// The model maki asks for, and the dated snapshot the generation
    /// reports.
    const ASKED: &str = "claude-haiku-4-5";
    const RAN: &str = "claude-haiku-4-5-20251001";
    const OPUS: &str = "claude-opus-5-5";
    const MESSAGE_ID: &str = "msg_1";
    const OTHER_MESSAGE_ID: &str = "msg_9";
    const TOO_LONG: &str = "Prompt is too long";
    const TOO_MANY: u16 = 429;
    const BAD_REQUEST: u16 = 400;
    /// As 2.1.284 words a used-up plan.
    const SESSION_LIMIT: &str = "You've hit your session limit · resets 12:10pm (Europe/Helsinki)";
    /// As 2.1.284 and the API word them.
    const REFUSAL_EXPLANATION: &str = "This request triggered restrictions on violative cyber content and was blocked under Anthropic's Usage Policy.";
    const CLASSIFIER_NOTE: &str = "Your response above was stopped by a safety classifier. Do not produce that content again, even reworded.";
    const UNASKED: &str = "unasked";
    /// Longer than an error shows, so only its start appears.
    const ANSWERED: &str = "Claude Code sent the request again without a stream after the stream error, then gave the result of the tool call that it made";
    const FIRST_CALL: &str = "toolu_1";
    const SECOND_CALL: &str = "toolu_2";
    const EXPOSED: &str = "mcp__maki__read";
    const TOOL: &str = "read";
    const EXPOSED_BATCH: &str = "mcp__maki__batch";
    const UNKNOWN_TOOL: &str = "mcp__maki__unknown";
    /// As a model wrote it in a live run.
    const GARBLED_TOOL: &str = "mcp__maki__grmc__grep";
    const GARBLED_MAKI_NAME: &str = "grmc__grep";
    const CLAUDE_CODE_TOOL: &str = "Bash";
    /// Claude Code's placeholders, as its output filter spells them.
    const NO_CONTENT: &str = "(no content)";
    const INTERRUPTED: &str = "[Request interrupted by user for tool use]";

    fn catalog() -> Catalog {
        Catalog::new(&json!([
            { "name": TOOL, "description": "d", "input_schema": {} },
            { "name": BATCH_TOOL, "description": "d", "input_schema": {} },
        ]))
        .unwrap()
    }

    fn control(id: &str, response: Value) -> String {
        json!({ "type": "control_response", "response": { "subtype": SUCCESS, "request_id": id, "response": response } }).to_string()
    }

    fn handshake() -> Vec<String> {
        vec![
            control(
                ACCOUNT_ANSWER,
                json!({
                    "current_permission_mode": DEFAULT_MODE,
                    "account": { "apiProvider": RULES.first_party, "subscriptionType": "Claude Pro" },
                    "models": [
                        { "value": "default", "resolvedModel": OPUS },
                        { "value": "opus", "resolvedModel": OPUS },
                        { "value": "haiku", "resolvedModel": RAN },
                    ],
                }),
            ),
            control(
                SETTINGS_ANSWER,
                json!({ "effective": { "autoCompactEnabled": false }, "sources": [{ "source": RULES.flag_source, "settings": {} }] }),
            ),
            control(
                HOOKS_ANSWER,
                json!({ "hooks": [], "policy": { "allDisabled": true } }),
            ),
        ]
    }

    fn init() -> String {
        json!({
            "type": "system", "subtype": "init", "apiKeySource": RULES.no_key_source, "claude_code_version": VERSION,
            "permissionMode": DEFAULT_MODE, "tools": [EXPOSED, EXPOSED_BATCH], "mcp_servers": [{ "name": SERVER, "status": CONNECTED }],
            "plugins": [], "cwd": CWD,
        })
        .to_string()
    }

    fn stream(event: Value) -> String {
        json!({ "type": "stream_event", "event": event }).to_string()
    }

    fn assistant(block: Value) -> String {
        json!({ "type": "assistant", "message": { "id": MESSAGE_ID, "content": [block] } })
            .to_string()
    }

    /// A block as Claude Code sends it: its start, the whole block, and its
    /// stop.
    fn streamed(block: Value) -> Vec<String> {
        vec![
            stream(json!({ "type": "content_block_start", "content_block": block })),
            assistant(block),
            stream(json!({ "type": "content_block_stop" })),
        ]
    }

    /// A full generation of `blocks` that stops for `reason`.
    fn reply(blocks: &[Value], reason: &str) -> Vec<String> {
        let mut lines = vec![start()];
        lines.extend(blocks.iter().cloned().flat_map(streamed));
        lines.extend(stop(reason));
        lines
    }

    fn call(id: &str, path: &str) -> Value {
        json!({ "type": "tool_use", "id": id, "name": EXPOSED, "input": { "path": path } })
    }

    fn start() -> String {
        start_as(RAN)
    }

    fn start_as(model: &str) -> String {
        stream(
            json!({ "type": "message_start", "message": { "id": MESSAGE_ID, "model": model, "usage": { "input_tokens": 10, "cache_read_input_tokens": 200, "cache_creation_input_tokens": 30, "output_tokens": 1 } } }),
        )
    }

    fn stop(reason: &str) -> Vec<String> {
        vec![
            stream(
                json!({ "type": "message_delta", "delta": { "stop_reason": reason }, "usage": { "output_tokens": 42 } }),
            ),
            stream(json!({ "type": "message_stop" })),
        ]
    }

    fn parked(id: Option<&str>, path: &str) -> Handoff {
        Handoff::Parked {
            tool_use_id: id.map(str::to_owned),
            name: TOOL.into(),
            arguments: json!({ "path": path }),
        }
    }

    enum Input {
        Line(String),
        Park(Handoff),
    }

    /// Returns the first error, `Done` once the reply is done, or else the
    /// last step, plus the response once the reply is done.
    fn run(
        lines: &[String],
        handoffs: Vec<Handoff>,
        handoff_first: bool,
    ) -> (Result<Step, Error>, Option<Result<StreamResponse, Error>>) {
        with_turn(|mut turn| {
            let mut last = Ok(Step::Nothing);
            for line in handshake() {
                last = turn.feed(&line);
            }
            assert!(matches!(last, Ok(Step::Ready)), "{last:?}");
            turn.prompted();
            let parks = handoffs.into_iter().map(Input::Park);
            let feeds = iter::once(init())
                .chain(lines.iter().cloned())
                .map(Input::Line);
            let inputs: Vec<Input> = if handoff_first {
                parks.chain(feeds).collect()
            } else {
                feeds.chain(parks).collect()
            };
            // A late protocol error must invalidate the reply even after its completion.
            let mut done = false;
            for input in inputs {
                let step = match input {
                    Input::Line(line) => turn.feed(&line),
                    Input::Park(handoff) => turn.park(handoff),
                };
                done |= matches!(step, Ok(Step::Done));
                if step.is_err() {
                    return (step, None);
                }
                last = if done { Ok(Step::Done) } else { step };
            }
            (last, done.then(|| turn.response()))
        })
    }

    /// Runs `test` on a turn with the test catalog, as a request for `ASKED`
    /// in `CWD` would build it.
    fn with_turn<T>(test: impl FnOnce(Turn<'_>) -> T) -> T {
        let profile = profile(VERSION, LINUX).unwrap();
        let catalog = catalog();
        let tools = catalog.exposed();
        let expect = InitExpect {
            profile: &profile,
            model: ASKED,
            cwd: Path::new(CWD),
            server: SERVER,
            tools: &tools,
        };
        let plan_usage = Mutex::default();
        test(Turn::new(&catalog, expect, &plan_usage))
    }

    fn batch() -> Vec<String> {
        reply(
            &[
                json!({ "type": "text", "text": "Reading both." }),
                call(FIRST_CALL, "a"),
                call(SECOND_CALL, "b"),
            ],
            TOOL_USE_STOP,
        )
    }

    /// Unexpected protocol data must stop the request before an unvalidated reply reaches the
    /// user.
    #[test_case(json!({ "type": "control_request", "request": { "subtype": "can_use_tool" } }).to_string() => matches Err(Error::Asked(_)) ; "a_question")]
    #[test_case(json!({ "type": "system", "subtype": "hook_started", "hook_event": "Stop" }).to_string() => matches Err(Error::HookStarted(_)) ; "a_hook")]
    #[test_case(json!({ "type": "system", "subtype": "compact_boundary" }).to_string() => matches Err(Error::Compacted) ; "a_compaction")]
    #[test_case(start() => matches Err(Error::SecondGeneration) ; "a_second_start")]
    #[test_case(assistant(json!({ "type": "image", "source": {} })) => matches Err(Error::UnreadableBlock(_)) ; "an_image_block")]
    #[test_case(assistant(json!({ "type": "tool_use", "id": "toolu_x", "name": "", "input": {} })) => matches Err(Error::NotOffered(_)) ; "a_call_without_a_name")]
    #[test_case(json!({ "type": "result", "subtype": "error_during_execution", "is_error": true, "result": "usage limit reached" }).to_string() => matches Err(Error::Failed(_)) ; "an_error_result")]
    #[test_case(json!({ "type": "surprise" }).to_string() => matches Err(Error::UnknownEvent(_)) ; "an_unknown_event")]
    #[test_case("Update available".to_owned() => matches Err(Error::NotAnEvent(_)) ; "no_event")]
    fn a_line_maki_cannot_accept_stops_the_generation(line: String) -> Result<Step, Error> {
        run(&[start(), line], Vec::new(), false).0
    }

    /// The start names the model to check, so content before it never
    /// reaches the user. A start must carry its id.
    #[test_case(stream(json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "early" } })) => matches Err(Error::OutsideGeneration) ; "content_before_the_start")]
    #[test_case(stream(json!({ "type": "message_start", "message": { "id": "", "model": RAN } })) => matches Err(Error::NoGenerationId) ; "a_start_without_an_id")]
    fn a_generation_starts_with_its_id_and_model(line: String) -> Result<Step, Error> {
        run(&[line], Vec::new(), false).0
    }

    /// Anthropic can add API events, so an unknown one is ignored while the
    /// reply streams and stops the request once the reply is complete.
    #[test_case(false => matches Ok(_) ; "while_the_reply_streams")]
    #[test_case(true => matches Err(Error::LateContent) ; "after_the_reply")]
    fn an_unknown_api_event_is_skipped_until_the_reply_is_complete(
        also_after: bool,
    ) -> Result<Step, Error> {
        let unknown = stream(json!({ "type": "future_event" }));
        let mut lines = reply(&[json!({ "type": "text", "text": "Done." })], END_TURN);
        lines.insert(1, unknown.clone());
        if also_after {
            lines.push(unknown);
        }
        run(&lines, Vec::new(), false).0
    }

    /// A complete reply must agree with its stop reason and with the call
    /// Claude Code holds.
    #[test_case(json!({ "type": "text", "text": "Done." }), END_TURN, true => matches Err(Error::NotInReply(_)) ; "a_parked_call_the_reply_never_made")]
    #[test_case(call(FIRST_CALL, "a"), END_TURN, false => matches Err(Error::StoppedFor(_)) ; "a_batch_that_stops_for_another_reason")]
    #[test_case(json!({ "type": "text", "text": "Reading both." }), TOOL_USE_STOP, false => matches Err(Error::NoCallsToRun) ; "a_reply_that_stops_for_tools_it_never_called")]
    fn a_reply_that_contradicts_itself_is_refused(
        block: Value,
        reason: &str,
        held: bool,
    ) -> Result<Step, Error> {
        let handoffs = held.then(|| parked(Some(FIRST_CALL), "a"));
        run(
            &reply(&[block], reason),
            handoffs.into_iter().collect(),
            false,
        )
        .0
    }

    /// Claude Code hands over no calls from a cut reply. Return only text so maki can request
    /// a continuation.
    #[test]
    fn a_tool_batch_cut_at_the_output_cap_comes_back_without_its_calls() {
        let lines = reply(
            &[
                json!({ "type": "text", "text": "Reading both." }),
                call(FIRST_CALL, "a"),
                json!({ "type": "tool_use", "id": SECOND_CALL, "name": EXPOSED, "input": {} }),
            ],
            MAX_TOKENS_STOP,
        );
        let (step, response) = run(&lines, Vec::new(), false);
        assert!(matches!(step, Ok(Step::Done)), "{step:?}");
        let response = response.unwrap().unwrap();
        assert_eq!(response.stop_reason, Some(StopReason::MaxTokens));
        assert_eq!(response.message.tool_uses().count(), 0);
        assert!(response.message.first_text_content().is_some());
    }

    /// Claude Code holds only the first call, and only that one is compared,
    /// so every other call must be complete and appear once.
    #[test_case(json!({ "type": "tool_use", "id": SECOND_CALL, "name": EXPOSED, "input": null }) => matches Err(Error::NotArguments { .. }) ; "null_arguments")]
    #[test_case(call(FIRST_CALL, "elsewhere") => matches Err(Error::RepeatedCall(_)) ; "a_changed_repeat")]
    #[test_case(call(FIRST_CALL, "a") => matches Err(Error::RepeatedCall(_)) ; "an_unchanged_repeat")]
    fn a_malformed_call_in_a_batch_is_refused(bad: Value) -> Result<Step, Error> {
        let lines = reply(&[call(FIRST_CALL, "a"), bad], TOOL_USE_STOP);
        run(&lines, vec![parked(Some(FIRST_CALL), "a")], false).0
    }

    /// Claude Code sends each block once, after its start, so a repeated or
    /// unstarted block is not the stream that was checked.
    #[test_case(true ; "sent_twice")]
    #[test_case(false ; "never_started")]
    fn a_block_beyond_those_started_is_refused(started_once: bool) {
        let text = json!({ "type": "text", "text": "Reading." });
        let mut lines = vec![start()];
        if started_once {
            lines.extend(streamed(text.clone()));
        }
        lines.push(assistant(text));
        let (step, _) = run(&lines, Vec::new(), false);
        assert!(matches!(step, Err(Error::BlockCount { .. })), "{step:?}");
    }

    /// The user sees a call start before the call is complete, so the start
    /// must carry the call's id.
    #[test_case(json!({ "type": "tool_use", "name": EXPOSED }) ; "no_id")]
    #[test_case(json!({ "type": "tool_use", "id": "", "name": EXPOSED }) ; "an_empty_id")]
    fn a_call_that_starts_without_an_id_is_refused(block: Value) {
        let started =
            stream(json!({ "type": "content_block_start", "index": 0, "content_block": block }));
        let (step, _) = run(&[start(), started], Vec::new(), false);
        assert!(matches!(step, Err(Error::NoCallId)), "{step:?}");
    }

    /// The id the start showed must be the id of the whole call.
    #[test_case(json!({ "type": "tool_use", "name": EXPOSED, "input": {} }) ; "no_id")]
    #[test_case(json!({ "type": "tool_use", "id": "", "name": EXPOSED, "input": {} }) ; "an_empty_id")]
    fn a_call_that_loses_its_id_is_refused(whole: Value) {
        let started = stream(
            json!({ "type": "content_block_start", "index": 0, "content_block": call(FIRST_CALL, "a") }),
        );
        let (step, _) = run(&[start(), started, assistant(whole)], Vec::new(), false);
        assert!(matches!(step, Err(Error::NoCallId)), "{step:?}");
    }

    /// The model may call a tool twice with the same arguments, and maki runs
    /// both calls.
    #[test]
    fn calls_with_equal_arguments_both_reach_maki() {
        let lines = reply(
            &[call(FIRST_CALL, "a"), call(SECOND_CALL, "a")],
            TOOL_USE_STOP,
        );
        let (step, response) = run(&lines, vec![parked(Some(FIRST_CALL), "a")], false);
        assert!(matches!(step, Ok(Step::Done)), "{step:?}");
        let response = response.unwrap().unwrap();
        let ids: Vec<_> = response.message.tool_uses().map(|(id, _, _)| id).collect();
        assert_eq!(ids, [FIRST_CALL, SECOND_CALL]);
    }

    /// Each transcript block must retain its bytes and position so later requests can reuse
    /// the cached prefix.
    #[test]
    fn each_transcript_block_is_a_text_block_of_its_own() {
        let blocks = ["first".to_owned(), "second\nline".to_owned()];
        let sent = user_message(&blocks);
        let line = sent.strip_suffix('\n').unwrap();
        assert!(!line.contains('\n'), "{sent}");
        let message: Value = serde_json::from_str(line).unwrap();
        assert_eq!(
            message["message"]["content"],
            json!([
                { "type": "text", "text": blocks[0] },
                { "type": "text", "text": blocks[1] },
            ])
        );
    }

    fn no_such_tool(id: &str, name: &str) -> String {
        json!({ "type": "user", "message": { "role": "user", "content": [{
            "type": TOOL_RESULT, "tool_use_id": id, "is_error": true,
            "content": format!("{NO_SUCH_TOOL} {name}</tool_use_error>"),
        }] } })
        .to_string()
    }

    /// A tool the model made up reaches maki, which answers it as an unknown
    /// tool, after Claude Code rejects the call itself. The model reads the
    /// name it wrote, because the transcript adds the prefix back.
    #[test_case(GARBLED_TOOL, GARBLED_MAKI_NAME ; "a_garbled_name")]
    #[test_case(CLAUDE_CODE_TOOL, CLAUDE_CODE_TOOL ; "a_claude_code_tool")]
    fn a_call_to_a_tool_maki_did_not_offer_comes_back_to_maki(name: &str, maki_name: &str) {
        let call = json!({ "type": "tool_use", "id": FIRST_CALL, "name": name, "input": {} });
        let mut lines = reply(&[call], TOOL_USE_STOP);
        lines.push(no_such_tool(FIRST_CALL, name));
        let (step, response) = run(&lines, Vec::new(), false);
        assert!(matches!(step, Ok(Step::Done)), "{step:?}");
        let response = response.unwrap().unwrap();
        let names: Vec<_> = response
            .message
            .tool_uses()
            .map(|(_, name, _)| name)
            .collect();
        assert_eq!(names, [maki_name]);
    }

    fn rejected(id: &str) -> String {
        json!({ "type": "user", "message": { "role": "user", "content": [{
            "type": TOOL_RESULT, "tool_use_id": id, "is_error": true,
            "content": format!("{INPUT_REJECTED} {TOOL} was called with input that does not match its schema</tool_use_error>"),
        }] } })
        .to_string()
    }

    /// When Claude Code rejects a call against maki's schema, maki runs it
    /// anyway, it fails the same way, and the model can retry. Claude Code
    /// rejects at the end of the call's block, before the reply is complete.
    #[test_case(false ; "before_the_reply_ends")]
    #[test_case(true ; "after_the_reply_ends")]
    fn a_call_claude_code_rejects_is_handed_to_maki(after_the_reply: bool) {
        let mut lines = vec![start()];
        lines.extend(streamed(call(FIRST_CALL, "a")));
        if after_the_reply {
            lines.extend(stop(TOOL_USE_STOP));
            lines.push(rejected(FIRST_CALL));
        } else {
            lines.push(rejected(FIRST_CALL));
            lines.extend(stop(TOOL_USE_STOP));
        }
        let (step, response) = run(&lines, Vec::new(), false);
        assert!(matches!(step, Ok(Step::Done)), "{step:?}");
        let response = response.unwrap().unwrap();
        assert_eq!(response.stop_reason, Some(StopReason::ToolUse));
        assert_eq!(response.message.tool_uses().count(), 1);
    }

    /// Any other answer, or an answer to a call the reply did not make, means
    /// Claude Code runs tools itself.
    #[test_case(rejected(SECOND_CALL) ; "a_call_not_in_the_reply")]
    #[test_case(json!({ "type": "user", "message": { "content": [{ "type": TOOL_RESULT, "tool_use_id": FIRST_CALL, "content": ANSWERED }] } }).to_string() ; "an_answer")]
    #[test_case(classifier_note() ; "a_note_without_a_refusal")]
    fn only_a_refused_call_of_the_reply_hands_off(answer: String) {
        let mut lines = reply(&[call(FIRST_CALL, "a")], TOOL_USE_STOP);
        lines.push(answer);
        let (step, _) = run(&lines, Vec::new(), false);
        assert!(matches!(step, Err(Error::AnsweredItself(_))), "{step:?}");
    }

    fn classifier_note() -> String {
        json!({ "type": "user", "isSynthetic": true, "message": { "role": "user", "content": [{ "type": "text", "text": CLASSIFIER_NOTE }] } }).to_string()
    }

    /// The safety classifier's stop ends the request before Claude Code adds
    /// its note and runs the model again, and says why.
    #[test]
    fn a_reply_the_safety_classifier_stopped_says_why() {
        let lines = [
            start(),
            stream(
                json!({ "type": "message_delta", "delta": { "stop_reason": REFUSAL_STOP, "stop_details": { "type": REFUSAL_STOP, "category": "cyber", "explanation": REFUSAL_EXPLANATION } }, "usage": { "output_tokens": 0 } }),
            ),
            stream(json!({ "type": "message_stop" })),
            classifier_note(),
        ];
        let (step, _) = run(&lines, Vec::new(), false);
        assert!(
            matches!(&step, Err(Error::Refused(text)) if text == REFUSAL_EXPLANATION),
            "{step:?}"
        );
    }

    fn plan_report(status: &str) -> String {
        json!({ "type": "rate_limit_event", "rate_limit_info": { "status": status, "unifiedWindows": {} } }).to_string()
    }

    /// An API error as Claude Code reports it: a reply outside any
    /// generation, and a result that carries the status. An error in the
    /// middle of a reply has kind "unknown" and no status on the reply.
    fn refused(kind: &str, status: Option<u16>, text: &str) -> [String; 2] {
        let reply = json!({
            "type": "assistant", "error": kind, API_ERROR_MARK: true, "api_error_status": status,
            "message": { "model": "<synthetic>", "content": [{ "type": "text", "text": text }] },
        });
        let result = json!({
            "type": "result", "subtype": SUCCESS, "is_error": true, "api_error_status": status.unwrap_or(TOO_MANY), "result": text,
        });
        [reply.to_string(), result.to_string()]
    }

    /// A used-up plan cannot serve another request before its window resets. Its rate limit
    /// must not enter the temporary-error retry path.
    #[test_case(PLAN_REJECTED, true ; "a_used_up_plan")]
    #[test_case("allowed", false ; "a_plan_with_room_left")]
    fn a_used_up_plan_is_not_retried(status: &str, used_up: bool) {
        let mut lines = vec![plan_report(status)];
        lines.extend(refused(RATE_LIMITED, Some(TOO_MANY), SESSION_LIMIT));
        let (step, _) = run(&lines, Vec::new(), false);
        let error = step.unwrap_err();
        assert_eq!(
            matches!(&error, Error::PlanLimit(text) if text == SESSION_LIMIT),
            used_up,
            "{error}"
        );
        assert_eq!(error.temporary().is_none(), used_up, "{error}");
    }

    /// Claude Code writes an API error as a reply outside any generation.
    #[test]
    fn a_reply_the_api_refused_says_why() {
        let (step, _) = run(
            &refused("invalid_request", Some(BAD_REQUEST), TOO_LONG),
            Vec::new(),
            false,
        );
        assert!(
            matches!(&step, Err(Error::ApiRefused { text, .. }) if text == TOO_LONG),
            "{step:?}"
        );
    }

    /// Only the result supplies the status of a mid-reply API error. A missing block alone
    /// cannot identify the retry path.
    #[test_case(None, false ; "a_rate_limit")]
    #[test_case(Some(PLAN_REJECTED), true ; "a_used_up_plan")]
    fn an_api_error_in_the_middle_of_a_reply_names_the_cause(plan: Option<&str>, used_up: bool) {
        let mut lines: Vec<String> = plan.map(plan_report).into_iter().collect();
        lines.extend([
            start(),
            stream(json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } })),
            stream(json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "Hel" } })),
        ]);
        lines.extend(stop(END_TURN));
        lines.extend(refused("unknown", None, SESSION_LIMIT));
        let (step, _) = run(&lines, Vec::new(), false);
        let error = step.unwrap_err();
        assert_eq!(matches!(&error, Error::PlanLimit(_)), used_up, "{error}");
        assert_eq!(error.temporary().is_some(), !used_up, "{error}");
    }

    #[test_case(false ; "the_generation_before_the_parked_call")]
    #[test_case(true ; "the_parked_call_before_the_generation")]
    fn a_batch_hands_off_whole_in_either_order(handoff_first: bool) {
        let (step, response) = run(&batch(), vec![parked(Some(FIRST_CALL), "a")], handoff_first);
        assert!(matches!(step, Ok(Step::Done)), "{step:?}");
        let response = response.unwrap().unwrap();
        let calls: Vec<_> = response
            .message
            .tool_uses()
            .map(|(id, name, _)| (id.to_owned(), name.to_owned()))
            .collect();
        assert_eq!(
            calls,
            [
                (FIRST_CALL.to_owned(), TOOL.to_owned()),
                (SECOND_CALL.to_owned(), TOOL.to_owned())
            ]
        );
        assert_eq!(response.stop_reason, Some(StopReason::ToolUse));
        assert_eq!(response.usage.input, 10);
        assert_eq!(response.usage.cache_read, 200);
        assert_eq!(
            response.usage.output, 42,
            "the last count, and not the placeholder"
        );
    }

    /// The model names a batch's calls as it sees the tools. The held call
    /// still matches the reply, and maki gets its own names back.
    #[test_case(EXPOSED, TOOL ; "an_offered_tool_gets_its_maki_name")]
    #[test_case(UNKNOWN_TOOL, UNKNOWN_TOOL ; "an_unknown_tool_keeps_its_name")]
    fn a_batch_names_its_calls_by_maki_names(inner: &str, expected: &str) {
        let input =
            json!({ BATCH_CALLS: [{ BATCH_CALL_TOOL: inner, "parameters": { "path": "a" } }] });
        let call =
            json!({ "type": "tool_use", "id": FIRST_CALL, "name": EXPOSED_BATCH, "input": input });
        let handoff = Handoff::Parked {
            tool_use_id: Some(FIRST_CALL.to_owned()),
            name: BATCH_TOOL.into(),
            arguments: input,
        };
        let (step, response) = run(&reply(&[call], TOOL_USE_STOP), vec![handoff], false);
        assert!(matches!(step, Ok(Step::Done)), "{step:?}");
        let response = response.unwrap().unwrap();
        let (_, _, input) = response.message.tool_uses().next().unwrap();
        assert_eq!(input[BATCH_CALLS][0][BATCH_CALL_TOOL], expected);
    }

    /// A batch reaches maki once its generation is complete and its call is
    /// held, in either order.
    #[test_case(None, Vec::new() ; "a_generation_waits_for_its_parked_call")]
    #[test_case(Some(5), vec![parked(Some(FIRST_CALL), "a")] ; "a_parked_call_waits_for_the_whole_generation")]
    fn a_batch_waits_for_its_generation_and_its_call(
        streamed: Option<usize>,
        handoffs: Vec<Handoff>,
    ) {
        let lines = batch();
        let (step, response) = run(&lines[..streamed.unwrap_or(lines.len())], handoffs, false);
        assert!(matches!(step, Ok(Step::Nothing)), "{step:?}");
        assert!(response.is_none());
    }

    #[test_case(parked(Some("toolu_other"), "a") => matches Err(Error::NotInReply(_)) ; "a_call_the_reply_lacks")]
    #[test_case(parked(Some(FIRST_CALL), "changed") => matches Err(Error::CallDiffers(_)) ; "other_arguments")]
    #[test_case(parked(None, "a") => matches Err(Error::NoCallId) ; "no_tool_use_id")]
    #[test_case(Handoff::Invalid(Error::NotOffered(json!("bash"))) => matches Err(Error::NotOffered(_)) ; "an_invalid_call")]
    fn a_handoff_that_does_not_match_the_reply_stops_the_request(
        handoff: Handoff,
    ) -> Result<Step, Error> {
        run(&batch(), vec![handoff], false).0
    }

    /// A text reply that ends at its result.
    fn finished(mut lines: Vec<String>) -> Vec<String> {
        lines.push(
            json!({ "type": "result", "subtype": SUCCESS, "is_error": false, "result": "Done." })
                .to_string(),
        );
        lines
    }

    /// A reply that filled the context window is cut, as one at the output
    /// cap is.
    #[test_case(MAX_TOKENS_STOP ; "at_the_output_cap")]
    #[test_case(WINDOW_FULL_STOP ; "at_a_full_window")]
    fn a_cut_reply_comes_back_as_cut(reason: &str) {
        let lines = reply(&[json!({ "type": "text", "text": "Half" })], reason);
        let (step, response) = run(&lines, Vec::new(), false);
        assert!(matches!(step, Ok(Step::Done)), "{step:?}");
        assert_eq!(
            response.unwrap().unwrap().stop_reason,
            Some(StopReason::MaxTokens)
        );
    }

    /// Claude Code sends each block before `message_stop`, and the last
    /// `message_delta` carries the stop reason. Without either, the reply is
    /// not the one the model sent.
    #[test_case(Vec::new(), json!({ "stop_reason": END_TURN }) => matches Err(Error::BlockCount { .. }) ; "a_started_block_that_never_came")]
    #[test_case(vec![json!({ "type": "text", "text": "Done." })], json!({}) => matches Err(Error::NoStopReason) ; "no_stop_reason")]
    fn an_incomplete_reply_is_refused(sent: Vec<Value>, delta: Value) -> Result<Step, Error> {
        let mut lines = vec![
            start(),
            stream(
                json!({ "type": "content_block_start", "content_block": { "type": "text", "text": "" } }),
            ),
            text_delta("Done."),
        ];
        lines.extend(sent.into_iter().map(assistant));
        lines.push(stream(
            json!({ "type": "message_delta", "delta": delta, "usage": { "output_tokens": 42 } }),
        ));
        lines.push(stream(json!({ "type": "message_stop" })));
        run(&finished(lines), Vec::new(), false).0
    }

    fn text_delta(text: &str) -> String {
        stream(
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": text } }),
        )
    }

    /// Claude Code can omit an output message after its block start event. Blank text and
    /// placeholder blocks must pass this exception.
    #[test_case("\n\n" ; "blank_text")]
    #[test_case(NO_CONTENT ; "no_content")]
    #[test_case(INTERRUPTED ; "an_interruption")]
    fn a_text_block_claude_code_leaves_out_is_not_lost(text: &str) {
        let mut lines = vec![
            start(),
            stream(
                json!({ "type": "content_block_start", "content_block": { "type": "text", "text": "" } }),
            ),
            text_delta(text),
        ];
        lines.extend(streamed(call(FIRST_CALL, "a")));
        lines.extend(stop(TOOL_USE_STOP));

        let (step, response) = run(&lines, vec![parked(Some(FIRST_CALL), "a")], false);

        assert!(matches!(step, Ok(Step::Done)), "{step:?}");
        let response = response.unwrap().unwrap();
        assert_eq!(response.message.tool_uses().count(), 1);
    }

    /// Claude Code's filter trims with JavaScript's rules, where a byte order
    /// mark is space and a next-line character is text.
    #[test_case("\u{feff}\n" => true ; "a_byte_order_mark")]
    #[test_case("\u{85}" => false ; "a_next_line_character")]
    fn claude_code_trims_like_javascript(text: &str) -> bool {
        dropped_by_claude_code(text)
    }

    /// maki needs every count of the reply, and takes the output count only
    /// from the last `message_delta`.
    #[test_case(json!({}), json!({ "output_tokens": 42 }) => matches Err(Error::NoUsage) ; "a_start_without_usage")]
    #[test_case(json!({ "input_tokens": 10, "cache_read_input_tokens": 200, "cache_creation_input_tokens": 30 }), json!({}) => matches Err(Error::NoFinalOutput) ; "a_last_delta_without_output")]
    fn a_reply_without_all_its_counts_is_refused(
        start_usage: Value,
        last_usage: Value,
    ) -> Result<StreamResponse, Error> {
        let text = json!({ "type": "text", "text": "Done." });
        let mut lines = vec![stream(
            json!({ "type": "message_start", "message": { "id": MESSAGE_ID, "model": RAN, "usage": start_usage } }),
        )];
        lines.extend(streamed(text));
        for usage in [json!({ "output_tokens": 7 }), last_usage] {
            lines.push(stream(
                json!({ "type": "message_delta", "delta": { "stop_reason": END_TURN }, "usage": usage }),
            ));
        }
        lines.push(stream(json!({ "type": "message_stop" })));
        let (step, response) = run(&finished(lines), Vec::new(), false);
        assert!(matches!(step, Ok(Step::Done)), "{step:?}");
        response.unwrap()
    }

    #[test]
    fn a_reply_without_calls_is_done_at_its_result() {
        let mut lines = reply(&[json!({ "type": "text", "text": "Done." })], END_TURN);
        let (step, _) = run(&lines, Vec::new(), false);
        assert!(
            matches!(step, Ok(Step::Nothing)),
            "not before the result: {step:?}"
        );
        lines.push(
            json!({ "type": "result", "subtype": SUCCESS, "is_error": false, "result": "Done." })
                .to_string(),
        );
        let (step, response) = run(&lines, Vec::new(), false);
        assert!(matches!(step, Ok(Step::Done)), "{step:?}");
        assert_eq!(
            response.unwrap().unwrap().stop_reason,
            Some(StopReason::EndTurn)
        );
    }

    /// The last answer sends the prompt, so a second answer could send it
    /// again.
    #[test_case(ACCOUNT_ANSWER => matches Err(Error::StrayAnswer(_)) ; "a_second_answer")]
    #[test_case(UNASKED => matches Err(Error::StrayAnswer(_)) ; "an_answer_to_no_request")]
    fn a_stray_answer_stops_the_request(id: &str) -> Result<Step, Error> {
        run(&[control(id, json!({}))], Vec::new(), false).0
    }

    /// A message before the first generation, or with a different id, stops
    /// the request.
    #[test_case(Vec::new(), json!({ "content": [] }) => matches Err(Error::OutsideGeneration) ; "no_id_before_any_generation")]
    #[test_case(vec![start()], json!({ "id": OTHER_MESSAGE_ID, "content": [] }) => matches Err(Error::OutsideGeneration) ; "another_generations_id")]
    fn an_assistant_message_outside_its_generation_stops_the_request(
        before: Vec<String>,
        message: Value,
    ) -> Result<Step, Error> {
        let line = json!({ "type": "assistant", "message": message }).to_string();
        let lines: Vec<String> = before.into_iter().chain([line]).collect();
        run(&lines, Vec::new(), false).0
    }

    /// Organization policy can select a different model.
    #[test_case(RAN => matches Ok(Step::Nothing) ; "its_dated_snapshot")]
    #[test_case(ASKED => matches Ok(Step::Nothing) ; "the_model_itself")]
    #[test_case(OPUS => matches Err(Error::OtherModel { .. }) ; "another_model")]
    #[test_case("claude-haiku-4-5-1" => matches Err(Error::OtherModel { .. }) ; "a_point_release")]
    #[test_case("" => matches Err(Error::OtherModel { .. }) ; "no_model")]
    fn a_generation_must_run_the_model_asked_for(model: &str) -> Result<Step, Error> {
        run(&[start_as(model)], Vec::new(), false).0
    }

    /// The error keeps the start of Claude Code's answer, which explains it.
    #[test_case(json!([{ "type": "tool_result", "content": ANSWERED }]) ; "a_text_result")]
    #[test_case(json!([{ "type": "tool_result", "content": [{ "type": "text", "text": ANSWERED }] }]) ; "a_result_in_parts")]
    #[test_case(json!(ANSWERED) ; "plain_text")]
    fn a_tool_answered_inside_stops_the_request_and_says_why(content: Value) {
        let line = json!({ "type": "user", "message": { "content": content } }).to_string();
        let (step, _) = run(&[start(), line], Vec::new(), false);

        let shown: String = ANSWERED.chars().take(SHOWN_LINE_CHARS).collect();
        assert!(
            matches!(&step, Err(Error::AnsweredItself(text)) if *text == shown),
            "{step:?}"
        );
    }

    /// A block after the reply could add a call that Claude Code never hands
    /// to maki.
    #[test_case(json!({ "type": "result", "subtype": SUCCESS, "is_error": false, "result": "Done." }).to_string() ; "after_the_text_result")]
    #[test_case(stream(json!({ "type": "message_stop" })) ; "after_the_generation_ended")]
    fn content_after_a_finished_reply_is_refused(last_of_reply: String) {
        let mut lines = reply(&[json!({ "type": "text", "text": "Done." })], END_TURN);
        lines.push(last_of_reply);
        lines.push(assistant(call(FIRST_CALL, "late")));
        let (step, _) = run(&lines, Vec::new(), false);
        assert!(matches!(step, Err(Error::LateContent)), "{step:?}");
    }

    #[test]
    fn nothing_counts_before_init() {
        with_turn(|mut turn| {
            for line in handshake() {
                turn.feed(&line).unwrap();
            }
            assert!(
                matches!(turn.feed(&init()), Err(Error::UncheckedStart)),
                "an init before the prompt shows that Claude Code did not do the checks"
            );
            turn.prompted();
            assert!(matches!(turn.feed(&start()), Err(Error::UncheckedStart)));
        });
    }

    /// A rejected handshake request stops the request, and the error names
    /// it.
    #[test]
    fn a_refused_handshake_request_stops_the_request() {
        let refused = json!({ "type": "control_response", "response": { "subtype": "error", "request_id": SETTINGS_ANSWER, "error": "no" } });
        let step = with_turn(|mut turn| turn.feed(&refused.to_string()));
        assert!(
            matches!(&step, Err(Error::RequestRefused { id, .. }) if id == SETTINGS_ANSWER),
            "{step:?}"
        );
    }

    /// Usage beyond the plan is billed, so it shows even without the window
    /// values.
    #[test]
    fn extra_usage_in_use_is_shown_on_its_own() {
        let usage = usage_of(&json!({ "isUsingOverage": true }));
        let shown: Vec<(&str, Option<&str>)> = usage
            .limits
            .iter()
            .map(|limit| (limit.label.as_str(), limit.detail.as_deref()))
            .collect();
        assert_eq!(shown, [(EXTRA_USAGE, Some(EXTRA_USAGE_IN_USE))]);
    }

    /// The account answer maps each alias to its model, in order.
    #[test]
    fn the_account_answer_offers_its_models() {
        let offered: Vec<(String, String)> = with_turn(|mut turn| {
            for line in handshake() {
                turn.feed(&line).unwrap();
            }
            turn.offered()
                .into_iter()
                .map(|offer| (offer.name, offer.model))
                .collect()
        });
        let want = [("default", OPUS), ("opus", OPUS), ("haiku", RAN)]
            .map(|(name, model)| (name.to_owned(), model.to_owned()));
        assert_eq!(offered, want);
    }
}
