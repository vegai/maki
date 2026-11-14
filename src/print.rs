//! Non-interactive (headless) mode: `maki "prompt" --print`.
//!
//! Wire format intentionally matches Claude Code so existing scripts work
//! unchanged. Keep `PrintResult` fields a strict subset of theirs. `StreamJson`
//! is JSONL with the same shape, `Text` prints the raw response only.
//!
//! Keep Claude Code fields compatible with its wire format. Maki adds `thinking` to init
//! metadata so scripted callers can inspect the effective, model-clamped setting.

use std::io::{self, Read};
use std::mem;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::ValueEnum;
use color_eyre::Result;
use color_eyre::eyre::{Context, bail, eyre};
use maki_agent::headless::{self, BackgroundMode, InteractiveHandle, InteractiveParams, Ready};
use maki_agent::permissions::PluginRuleStore;
use maki_agent::session::Resumed;
use maki_agent::tools::QUESTION_TOOL_NAME;
use maki_agent::{
    AgentConfig, AgentEvent, AgentInput, AgentMode, DoneReason, Envelope, HostPrompt, ImageSource,
    InputSource, PermissionsConfig, SteerKind,
};
use maki_config::{ModelPolicy, ProjectConfig, SessionDefaults};
use maki_lua::EventHandle;
use maki_lua::session_snapshot::{HeadlessMeta, HeadlessSnapshot, MODE_BUILD};
use maki_providers::TokenUsage;
use maki_providers::model::Model;
use maki_storage::id::SessionRef;
use maki_storage::sessions::SessionClaim;
use serde::Serialize;
use serde_json::Value;

use crate::headless_startup::{shutdown, start_mcp, startup};
use crate::interrupt::Interrupt;
use crate::sdk_mode::NOTICE_SUBTYPE;
use crate::subagent_tasks::{SubagentTasks, TaskMessage};
use crate::wire::{
    API_RETRY, ERROR_DURING_EXECUTION, INIT, NOTIFICATION, ProviderNotice, SUCCESS,
    failure_subtype, system_event,
};

const SESSION_LINE_PREFIX: &str = "session: ";

// Fails fast: silently dropping an image the caller explicitly attached
// would be worse than erroring.
fn load_images(paths: &[PathBuf]) -> Result<Vec<ImageSource>> {
    paths
        .iter()
        .map(|path| {
            let media_type = maki_ui::image::media_type_for(path)
                .ok_or_else(|| eyre!("unsupported image type: {}", path.display()))?;
            maki_ui::image::load_file_image(path, media_type)
                .map_err(|e| eyre!("failed to load image: {e}"))
        })
        .collect()
}

#[derive(Clone, ValueEnum)]
pub enum OutputFormat {
    Text,
    Json,
    StreamJson,
}

#[derive(Serialize)]
struct PrintResult {
    #[serde(rename = "type")]
    result_type: &'static str,
    subtype: &'static str,
    is_error: bool,
    duration_ms: u128,
    num_turns: u32,
    result: String,
    stop_reason: Option<DoneReason>,
    session_id: SessionRef,
    total_cost_usd: f64,
    usage: TokenUsage,
}

impl PrintResult {
    fn error(session_id: SessionRef, duration_ms: u128, result: String) -> Self {
        Self {
            result_type: "result",
            subtype: ERROR_DURING_EXECUTION,
            is_error: true,
            duration_ms,
            num_turns: 0,
            result,
            stop_reason: None,
            session_id,
            total_cost_usd: 0.0,
            usage: TokenUsage::default(),
        }
    }
}

#[derive(Serialize)]
struct InitEvent<'a> {
    cwd: &'a str,
    tools: &'a [String],
    model: &'a str,
    thinking: String,
}

#[derive(Serialize)]
struct AssistantEvent<'a> {
    #[serde(rename = "type")]
    event_type: &'static str,
    message: AssistantMessage<'a>,
    session_id: &'a SessionRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_tool_use_id: Option<&'a str>,
}

#[derive(Serialize)]
struct AssistantMessage<'a> {
    model: &'a str,
    role: &'static str,
    content: &'a Value,
    usage: &'a TokenUsage,
}

#[derive(Serialize)]
struct UserEvent<'a> {
    #[serde(rename = "type")]
    event_type: &'static str,
    message: UserMessage<'a>,
    session_id: &'a SessionRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_tool_use_id: Option<&'a str>,
}

#[derive(Serialize)]
struct UserMessage<'a> {
    role: &'static str,
    content: &'a Value,
}

/// Claude Code has no event for this, so it takes the SDK's shape. Without it
/// a woken turn's assistant messages follow the last turn's with nothing
/// between them that says what woke it.
#[derive(Serialize)]
struct NoticeEvent<'a> {
    text: &'a str,
}

#[derive(Serialize)]
struct TaskEvent<'a> {
    #[serde(rename = "type")]
    event_type: &'static str,
    subtype: &'static str,
    #[serde(flatten)]
    task: TaskMessage,
    session_id: &'a SessionRef,
}

impl<'a> TaskEvent<'a> {
    fn new(task: TaskMessage, session_id: &'a SessionRef) -> Self {
        Self {
            event_type: "system",
            subtype: task.subtype(),
            task,
            session_id,
        }
    }
}

#[derive(Serialize)]
struct RetryEvent<'a> {
    attempt: u32,
    retry_delay_ms: u64,
    error: &'a str,
}

/// What the result reports besides the spend. A turn woken by a background
/// result belongs to the same invocation, so turns add up over every run,
/// and the answer is the last one a run gave. A woken run that says nothing
/// keeps the answer before it.
#[derive(Default)]
struct Totals {
    result_text: String,
    /// The answer of the run still streaming.
    run_text: String,
    is_error: bool,
    num_turns: u32,
    stop_reason: Option<DoneReason>,
}

impl Totals {
    /// `false` once the invocation failed, which ends it.
    fn fold(&mut self, event: &AgentEvent, parent_tool_use_id: Option<&str>) -> bool {
        match event {
            AgentEvent::TextDelta { text } if parent_tool_use_id.is_none() => {
                self.run_text.push_str(text);
            }
            AgentEvent::Steered { .. } => {
                if let Some(reason) = dropped_prompt(event, parent_tool_use_id) {
                    self.is_error = true;
                    self.run_text = reason.to_owned();
                }
            }
            AgentEvent::Done {
                num_turns, reason, ..
            } if parent_tool_use_id.is_none() => {
                self.num_turns += *num_turns;
                self.stop_reason = Some(*reason);
                // Only a stop cancels a run here, and its answer is cut short.
                self.is_error |= failure_subtype(*reason).is_some();
                let run_text = mem::take(&mut self.run_text);
                if !run_text.is_empty() {
                    self.result_text = run_text;
                }
            }
            AgentEvent::Error { message, .. } if parent_tool_use_id.is_none() => {
                self.is_error = true;
                self.result_text = message.clone();
                return false;
            }
            _ => {}
        }
        true
    }
}

enum VerboseOutput {
    StreamJson,
    Json(Vec<Value>),
}

impl VerboseOutput {
    fn emit(&mut self, value: &impl Serialize) -> Result<()> {
        match self {
            Self::StreamJson => println!("{}", serde_json::to_string(value)?),
            Self::Json(events) => events.push(serde_json::to_value(value)?),
        }
        Ok(())
    }

    fn emit_tasks(&mut self, tasks: Vec<TaskMessage>, session_id: &SessionRef) -> Result<()> {
        tasks
            .into_iter()
            .try_for_each(|task| self.emit(&TaskEvent::new(task, session_id)))
    }
}

pub struct PrintParams {
    pub dry_run: bool,
    pub model: Model,
    pub prompt: String,
    pub image_paths: Vec<PathBuf>,
    pub format: OutputFormat,
    pub verbose: bool,
    pub config: AgentConfig,
    pub permissions_config: PermissionsConfig,
    pub timeouts: maki_providers::Timeouts,
    pub lua_handle: EventHandle,
    pub defaults: SessionDefaults,
    pub model_policy: Arc<ModelPolicy>,
    pub plugin_rules: Arc<PluginRuleStore>,
    pub project_config: ProjectConfig,
    pub host_prompt: HostPrompt,
    /// Which session this run continues and writes under, and where. Resolved
    /// by the caller from the same flags every other entry point reads.
    pub resumed: Resumed,
    /// The right to write [`Self::resumed`]'s session, taken when it was
    /// resolved and held for the whole run.
    pub claim: SessionClaim,
    /// How long to wait for background work once the prompt's run ends.
    /// `None` runs every call blocking instead.
    pub background_wait: Option<Duration>,
}

pub fn read_prompt(prompt: Option<String>, dry_run: bool) -> Result<String> {
    match prompt {
        Some(p) => Ok(p),
        None if dry_run => Ok(String::new()),
        None => {
            let mut buf = String::new();
            io::stdin().read_to_string(&mut buf).context("read stdin")?;
            Ok(buf)
        }
    }
}

pub async fn run(params: PrintParams, interrupt: &Interrupt) -> Result<()> {
    let PrintParams {
        dry_run,
        model,
        prompt,
        image_paths,
        format,
        verbose,
        config,
        permissions_config,
        timeouts,
        lua_handle,
        defaults,
        model_policy,
        plugin_rules,
        project_config,
        host_prompt,
        resumed,
        claim,
        background_wait,
    } = params;

    let images = load_images(&image_paths)?;

    let prompt_slots = lua_handle
        .collect_prompt_slots(Some(resumed.id.as_str()))
        .await;

    let initial_wd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let cwd = initial_wd.to_string_lossy().into_owned();
    let mcp_handle = start_mcp(&initial_wd, project_config.clone()).await;

    let (handle, mut events) = headless::spawn_interactive(InteractiveParams {
        model: model.clone(),
        config,
        permissions_config,
        timeouts,
        prompt_slots: Arc::new(prompt_slots),
        excluded_tools: vec![QUESTION_TOOL_NAME],
        mcp_handle,
        initial_wd,
        resumed,
        claim,
        yolo: false,
        unattended: true,
        host_prompt,
        defaults,
        model_policy,
        plugin_rules,
        project_config,
        local_tools: Default::default(),
        background: BackgroundMode::from_wait(background_wait),
        stop: interrupt.stop(),
    });

    let InteractiveHandle {
        mut ready,
        input_tx,
        session_id,
        task,
        spend,
        ..
    } = handle;
    let start = Instant::now();
    let mut verbose_out = match format {
        OutputFormat::StreamJson => Some(VerboseOutput::StreamJson),
        _ if verbose => Some(VerboseOutput::Json(Vec::new())),
        _ => None,
    };

    let Ready {
        model,
        tool_names,
        thinking,
    } = match startup(&mut ready, &mut events).await {
        Ok(started) => started,
        Err(error) => {
            drop(shutdown(task, &lua_handle, &session_id).await);
            if !matches!(format, OutputFormat::Text) {
                write_result(
                    format,
                    verbose_out,
                    PrintResult::error(session_id, start.elapsed().as_millis(), error.to_string()),
                )?;
            }
            return Err(error);
        }
    };

    let init = system_event(
        &session_id,
        INIT,
        InitEvent {
            cwd: &cwd,
            tools: &tool_names,
            model: &model.id,
            thinking,
        },
    );
    if dry_run {
        drop(input_tx);
        drop(shutdown(task, &lua_handle, &session_id).await);
        println!("{}", serde_json::to_string(&init)?);
        return Ok(());
    }
    match &mut verbose_out {
        Some(out) => out.emit(&init)?,
        None => eprintln!("{SESSION_LINE_PREFIX}{session_id}"),
    }
    drop(init);
    let _ = input_tx.send(AgentInput::from_defaults(
        prompt,
        AgentMode::Build,
        images,
        defaults,
        InputSource::Headless,
    ));
    drop(input_tx);

    let mut totals = Totals::default();
    // The stream shows Maki's own tool names, so a task message does too.
    let mut tasks = SubagentTasks::new(|name| name);

    let snapshot = HeadlessSnapshot::default();
    snapshot.install(
        &lua_handle,
        HeadlessMeta {
            id: session_id.to_string(),
            cwd: cwd.clone(),
            model: model.spec(),
        },
        // `maki -p` sends its one prompt in build mode and never switches.
        || MODE_BUILD,
    );

    while let Some(envelope) = events.next().await {
        // Folded in first, so a plugin handling `TurnEnd` finds the finished
        // totals when it calls `maki.session.read()`.
        snapshot.observe(&envelope);
        maki_lua::agent_autocmd::dispatch(&lua_handle, &session_id, &envelope);
        let Envelope {
            ref event,
            ref subagent,
            ..
        } = envelope;
        let parent_tool_use_id = subagent.as_ref().map(|s| s.parent_tool_use_id.as_str());

        if !totals.fold(event, parent_tool_use_id) {
            break;
        }
        let task_messages = tasks.observe(&envelope);
        if let Some(out) = &mut verbose_out {
            out.emit_tasks(task_messages.before, &session_id)?;
        }
        match event {
            AgentEvent::TextDelta { .. }
            | AgentEvent::ThinkingDelta { .. }
            | AgentEvent::ToolPending { .. }
            | AgentEvent::ToolStart(_)
            | AgentEvent::ToolOutput { .. }
            | AgentEvent::ToolDone(_)
            | AgentEvent::ToolMovable { .. }
            | AgentEvent::ToolParts { .. }
            | AgentEvent::QueueItemConsumed { .. }
            | AgentEvent::QueueDrained
            | AgentEvent::AutoCompacting { .. }
            | AgentEvent::CompactionStarted
            | AgentEvent::CompactionDone { .. }
            | AgentEvent::AuthRequired
            | AgentEvent::AllowedByPlugin { .. }
            | AgentEvent::PermissionRequest { .. }
            | AgentEvent::SubagentHistory { .. }
            | AgentEvent::ToolSnapshot { .. }
            | AgentEvent::ToolHeaderSnapshot { .. }
            | AgentEvent::LiveToolBuf { .. }
            | AgentEvent::Nudge
            | AgentEvent::Notice { .. }
            | AgentEvent::PromptProgress { .. }
            | AgentEvent::StreamClosed
            | AgentEvent::Steered { .. }
            | AgentEvent::Done { .. }
            | AgentEvent::Error { .. } => {}
            AgentEvent::ObservationLine { text, .. } => {
                if let Some(out) = verbose_out
                    .as_mut()
                    .filter(|_| parent_tool_use_id.is_none())
                {
                    out.emit(&system_event(
                        &session_id,
                        NOTICE_SUBTYPE,
                        NoticeEvent { text },
                    ))?;
                }
            }
            AgentEvent::ProviderWaiting { message } => {
                if let Some(out) = &mut verbose_out {
                    out.emit(&system_event(
                        &session_id,
                        NOTIFICATION,
                        ProviderNotice::new(message.as_deref(), parent_tool_use_id),
                    ))?;
                } else if let Some(message) = message {
                    match &subagent {
                        Some(info) => eprintln!("{}: {message}", info.name),
                        None => eprintln!("{message}"),
                    }
                }
            }
            AgentEvent::Retry {
                attempt,
                message,
                delay_ms,
            } => {
                if let Some(out) = &mut verbose_out {
                    out.emit(&system_event(
                        &session_id,
                        API_RETRY,
                        RetryEvent {
                            attempt: *attempt,
                            retry_delay_ms: *delay_ms,
                            error: message,
                        },
                    ))?;
                }
            }
            AgentEvent::TurnComplete(tc) => {
                if let Some(out) = &mut verbose_out {
                    let content_value = serde_json::to_value(&tc.message.content)?;
                    out.emit(&AssistantEvent {
                        event_type: "assistant",
                        message: AssistantMessage {
                            model: &tc.model,
                            role: "assistant",
                            content: &content_value,
                            usage: &tc.usage,
                        },
                        session_id: &session_id,
                        parent_tool_use_id,
                    })?;
                }
            }
            AgentEvent::ToolResultsSubmitted { message } => {
                if let Some(out) = &mut verbose_out {
                    let content_value = serde_json::to_value(&message.content)?;
                    out.emit(&UserEvent {
                        event_type: "user",
                        message: UserMessage {
                            role: "user",
                            content: &content_value,
                        },
                        session_id: &session_id,
                        parent_tool_use_id,
                    })?;
                }
            }
        }
        if let Some(out) = &mut verbose_out {
            out.emit_tasks(task_messages.after, &session_id)?;
        }
    }
    let track = shutdown(task, &lua_handle, &session_id).await;
    if let Some(out) = &mut verbose_out {
        out.emit_tasks(tasks.finish(), &session_id)?;
    }
    // Background work and plugin jobs are stopped by now, so nothing spends
    // any more. Dropping the track saves the full bill, also what no turn
    // was left to report.
    drop(track);
    let spent = spend.totals();

    let duration_ms = start.elapsed().as_millis();
    let Totals {
        result_text,
        is_error,
        num_turns,
        stop_reason,
        ..
    } = totals;
    // A stop before the session read the prompt runs nothing at all.
    let is_error = is_error || stop_reason.is_none();
    // Zero on an unpriced model, which is what its turns reported too.
    let total_cost_usd = spent.cost.unwrap_or_default();

    let subtype = stop_reason
        .and_then(failure_subtype)
        .unwrap_or(if is_error {
            ERROR_DURING_EXECUTION
        } else {
            SUCCESS
        });
    write_result(
        format,
        verbose_out,
        PrintResult {
            result_type: "result",
            subtype,
            is_error,
            duration_ms,
            num_turns,
            result: result_text,
            stop_reason,
            session_id,
            total_cost_usd,
            usage: spent.usage,
        },
    )?;
    if is_error {
        bail!("headless run ended with {subtype}");
    }

    Ok(())
}

fn write_result(
    format: OutputFormat,
    verbose_out: Option<VerboseOutput>,
    result: PrintResult,
) -> Result<()> {
    match format {
        OutputFormat::Text => print!("{}", result.result),
        OutputFormat::Json | OutputFormat::StreamJson => match verbose_out {
            Some(VerboseOutput::Json(mut events)) => {
                events.push(serde_json::to_value(&result)?);
                println!("{}", serde_json::to_string(&events)?);
            }
            _ => println!("{}", serde_json::to_string(&result)?),
        },
    }
    Ok(())
}

/// Nothing reached the model, so the reason is the only answer there is. A
/// script checking the exit status should see that the prompt never ran.
fn dropped_prompt<'a>(event: &'a AgentEvent, parent_tool_use_id: Option<&str>) -> Option<&'a str> {
    match event {
        AgentEvent::Steered {
            kind: SteerKind::MessageDropped,
            text,
        } if parent_tool_use_id.is_none() => Some(text),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use maki_agent::RunTotals;
    use maki_providers::{ThinkingConfig, TokenUsage};
    use test_case::test_case;

    const DROP_REASON: &str = "blocked by a plugin";
    const PARENT_TOOL_USE_ID: &str = "toolu_parent";

    #[test_case(SteerKind::MessageDropped, None, Some(DROP_REASON) ; "top_level_drop_is_the_result")]
    #[test_case(SteerKind::MessageDropped, Some(PARENT_TOOL_USE_ID), None ; "subagent_drop_is_ignored")]
    #[test_case(SteerKind::MessageRewritten, None, None ; "other_steers_are_ignored")]
    fn dropped_prompt_only_counts_a_top_level_drop(
        kind: SteerKind,
        parent: Option<&str>,
        expected: Option<&str>,
    ) {
        let event = AgentEvent::Steered {
            kind,
            text: DROP_REASON.into(),
        };
        assert_eq!(dropped_prompt(&event, parent), expected);
    }

    const FIRST_ANSWER: &str = "started the build";
    const WOKEN_ANSWER: &str = "the build passed";
    const SUBAGENT_TEXT: &str = "child chatter";
    const ERROR_MESSAGE: &str = "provider error";
    const RUN_TURNS: u32 = 2;
    const RUNS: u32 = 2;

    fn text_delta(text: &str) -> AgentEvent {
        AgentEvent::TextDelta { text: text.into() }
    }

    fn run_done(reason: DoneReason) -> AgentEvent {
        AgentEvent::Done {
            usage: TokenUsage::default(),
            cost: None,
            list_cost: None,
            context_size: 0,
            context_window: 0,
            num_turns: RUN_TURNS,
            reason,
        }
    }

    #[test]
    fn a_run_cut_short_by_a_stop_is_an_error() {
        let mut totals = Totals::default();
        assert!(totals.fold(&text_delta(FIRST_ANSWER), None));
        assert!(totals.fold(&run_done(DoneReason::Cancelled), None));

        assert!(totals.is_error);
        assert_eq!(totals.result_text, FIRST_ANSWER);
    }

    /// A woken turn is part of the same invocation: its turns add up, and its
    /// answer, if it gave one, is the one the caller gets.
    #[test_case(WOKEN_ANSWER, WOKEN_ANSWER ; "woken_answer_replaces_the_first")]
    #[test_case("", FIRST_ANSWER ; "silent_woken_run_keeps_the_first")]
    fn totals_add_up_every_run_and_answer_with_the_last_text(woken: &str, expected: &str) {
        let mut totals = Totals::default();
        for answer in [FIRST_ANSWER, woken] {
            assert!(totals.fold(&text_delta(answer), None));
            assert!(totals.fold(&text_delta(SUBAGENT_TEXT), Some(PARENT_TOOL_USE_ID)));
            assert!(totals.fold(&run_done(DoneReason::EndTurn), None));
        }

        assert_eq!(totals.result_text, expected);
        assert_eq!(totals.num_turns, RUN_TURNS * RUNS);
        assert_eq!(totals.stop_reason, Some(DoneReason::EndTurn));
        assert!(!totals.is_error);
    }

    #[test]
    fn an_error_ends_the_invocation_with_its_message() {
        let mut totals = Totals::default();
        assert!(totals.fold(&text_delta(FIRST_ANSWER), None));

        let error = AgentEvent::Error {
            message: ERROR_MESSAGE.into(),
            auth: false,
            totals: RunTotals::default(),
        };
        assert!(!totals.fold(&error, None));

        assert!(totals.is_error);
        assert_eq!(totals.result_text, ERROR_MESSAGE);
    }

    const PRINT_RESULT_FIELDS: &[&str] = &[
        "type",
        "subtype",
        "is_error",
        "num_turns",
        "result",
        "stop_reason",
        "session_id",
        "total_cost_usd",
        "usage",
        "duration_ms",
    ];
    const INIT_EVENT_FIELDS: &[&str] = &[
        "uuid",
        "type",
        "subtype",
        "cwd",
        "session_id",
        "tools",
        "model",
        "thinking",
    ];
    const NOTICE_EVENT_FIELDS: &[&str] = &["type", "subtype", "text", "session_id"];
    const NOTICE_TEXT: &str = "Background bash finished: cargo test";
    const RETRY_EVENT_FIELDS: &[&str] = &[
        "uuid",
        "type",
        "subtype",
        "attempt",
        "retry_delay_ms",
        "error",
        "session_id",
    ];

    #[test]
    fn wire_format_required_fields() {
        let result = PrintResult {
            result_type: "result",
            subtype: SUCCESS,
            is_error: false,
            duration_ms: 1234,
            num_turns: 2,
            result: "done".into(),
            stop_reason: Some(DoneReason::EndTurn),
            session_id: SessionRef::generate(),
            total_cost_usd: 0.003,
            usage: TokenUsage::default(),
        };
        let json: Value = serde_json::to_value(&result).unwrap();
        for field in PRINT_RESULT_FIELDS {
            assert!(json.get(field).is_some(), "PrintResult missing: {field}");
        }

        let sid = SessionRef::generate();
        let tools = ["bash".into(), "read".into()];
        let init = system_event(
            &sid,
            INIT,
            InitEvent {
                cwd: "/tmp",
                tools: &tools,
                model: "test-model",
                thinking: ThinkingConfig::Off.to_string(),
            },
        );
        let json: Value = serde_json::to_value(&init).unwrap();
        for field in INIT_EVENT_FIELDS {
            assert!(json.get(field).is_some(), "InitEvent missing: {field}");
        }

        let retry = system_event(
            &sid,
            API_RETRY,
            RetryEvent {
                attempt: 2,
                retry_delay_ms: 3000,
                error: "rate_limit",
            },
        );
        let json: Value = serde_json::to_value(&retry).unwrap();
        for field in RETRY_EVENT_FIELDS {
            assert!(json.get(field).is_some(), "RetryEvent missing: {field}");
        }

        let notice = system_event(&sid, NOTICE_SUBTYPE, NoticeEvent { text: NOTICE_TEXT });
        let json: Value = serde_json::to_value(&notice).unwrap();
        for field in NOTICE_EVENT_FIELDS {
            assert!(json.get(field).is_some(), "NoticeEvent missing: {field}");
        }
    }
}
