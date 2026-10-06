use std::path::PathBuf;
use std::sync::Arc;

use flume::Sender;
use futures_lite::future;
use maki_providers::provider::{Provider, RequestScope};
use maki_providers::{Message, Model, ProviderEvent, RequestOptions};
use maki_storage::id::SessionRef;
use serde_json::Value;

use crate::components::btw_modal::BtwEvent;

use super::App;

const BTW_REMINDER: &str = "<system-reminder>\nThis is a side question. Answer it directly in a \
single response.\n- You have NO tools: you cannot read files, run commands, or take any action.\n\
- One-off response: there are no follow-up turns.\n- Answer ONLY from the existing conversation \
context.\n- Never say \"Let me...\", \"I'll now...\", or promise any action.\n- If you don't know, \
say so; do not offer to look it up.\n</system-reminder>";

const BTW_FALLBACK_SYSTEM: &str = "You are a helpful coding assistant. Answer concisely \
from the conversation context.";

/// The reminder leads so the model treats the question as a quick aside, not a task to act on.
pub(crate) fn btw_question(question: &str) -> Message {
    Message::user(format!("{BTW_REMINDER}\n\n{question}"))
}

impl App {
    pub(crate) fn start_btw(
        &mut self,
        question: String,
        provider: Arc<dyn Provider>,
        model: Model,
    ) {
        // The mirror is verbatim, so mid-turn it can end on an open tool call.
        // Providers reject that, so close them off on our own copy.
        let snapshot = self.shared_history.as_ref().map(|h| h.load_full());
        let mut messages = snapshot
            .as_ref()
            .map(|s| Vec::clone(&s.messages))
            .unwrap_or_default();
        maki_agent::close_dangling_tool_calls(&mut messages, maki_agent::UNAVAILABLE_RESULT);
        // The session's own prompt, so the side question reads the transcript
        // under the instructions it was written under.
        let system = snapshot
            .as_ref()
            .and_then(|s| s.frame.as_ref())
            .map_or_else(|| BTW_FALLBACK_SYSTEM.to_string(), |f| f.system.clone());
        messages.push(btw_question(&question));

        let (tx, rx) = flume::bounded(64);
        self.btw_modal.open(&question, rx);

        let session_id = SessionRef::from(self.state.session.id);
        smol::spawn(run_btw(
            provider,
            model,
            system,
            messages,
            tx,
            Some(session_id),
            self.permissions.cwd().to_owned(),
        ))
        .detach();
    }
}

async fn run_btw(
    provider: Arc<dyn Provider>,
    model: Model,
    system: String,
    messages: Vec<Message>,
    btw_tx: Sender<BtwEvent>,
    session_id: Option<SessionRef>,
    cwd: PathBuf,
) {
    let (event_tx, event_rx) = flume::unbounded();
    let tools = Value::Array(vec![]);
    let messages = maki_providers::adapt_images_for_model(&model, &messages).await;

    // The request must own the sender so its completion closes the event channel.
    let stream_fut = async move {
        provider
            .stream_message_in(
                &model,
                &messages,
                &system,
                &tools,
                &event_tx,
                RequestOptions::default(),
                RequestScope {
                    session_id: session_id.as_ref(),
                    cwd: &cwd,
                },
            )
            .await
    };

    let forward_fut = async {
        while let Ok(event) = event_rx.recv_async().await {
            let event = match event {
                ProviderEvent::TextDelta { text } => BtwEvent::TextDelta(text),
                ProviderEvent::ThinkingDelta { text } => BtwEvent::ThinkingDelta(text),
                _ => continue,
            };
            if btw_tx.send(event).is_err() {
                return;
            }
        }
    };

    let (result, _) = future::zip(stream_fut, forward_fut).await;

    match result {
        Ok(_) => {
            let _ = btw_tx.send(BtwEvent::Done);
        }
        Err(e) => {
            let _ = btw_tx.send(BtwEvent::Error(e.to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use flume::Sender;
    use futures_lite::FutureExt;
    use maki_agent::AgentError;
    use maki_providers::provider::{BoxFuture, Provider, RequestScope};
    use maki_providers::{
        ContentBlock, Message, Model, ModelInfo, ProviderEvent, RequestOptions, StreamResponse,
        TokenUsage,
    };
    use maki_storage::id::SessionRef;
    use serde_json::Value;
    use smol::Timer;

    use super::{BTW_REMINDER, BtwEvent, btw_question, run_btw};

    const Q: &str = "why sqlite?";
    const SESSION_DIR: &str = "/session/dir";
    const NO_DIR: &str = "the side question came without the directory of the session";
    const NEVER_ENDED: &str = "the side question did not stop";
    const END_LIMIT: Duration = Duration::from_secs(10);

    #[derive(Default)]
    struct DirOnlyProvider {
        asked_in: Mutex<Option<PathBuf>>,
    }

    impl Provider for DirOnlyProvider {
        fn stream_message<'a>(
            &'a self,
            _model: &'a Model,
            _messages: &'a [Message],
            _system: &'a str,
            _tools: &'a Value,
            _event_tx: &'a Sender<ProviderEvent>,
            _opts: RequestOptions,
            _session_id: Option<&'a SessionRef>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            Box::pin(async {
                Err(AgentError::Config {
                    message: NO_DIR.into(),
                })
            })
        }

        fn stream_message_in<'a>(
            &'a self,
            _model: &'a Model,
            _messages: &'a [Message],
            _system: &'a str,
            _tools: &'a Value,
            _event_tx: &'a Sender<ProviderEvent>,
            _opts: RequestOptions,
            scope: RequestScope<'a>,
        ) -> BoxFuture<'a, Result<StreamResponse, AgentError>> {
            *self.asked_in.lock().unwrap() = Some(scope.cwd.to_owned());
            Box::pin(async {
                Ok(StreamResponse {
                    input_transformations: Vec::new(),
                    message: Message::default(),
                    usage: TokenUsage::default(),
                    stop_reason: None,
                })
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AgentError>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    #[test]
    fn a_side_question_runs_in_the_session_directory_and_ends() {
        let provider = Arc::new(DirOnlyProvider::default());
        let (tx, rx) = flume::unbounded();

        let run = run_btw(
            provider.clone(),
            crate::components::test_model(),
            String::new(),
            vec![btw_question(Q)],
            tx,
            None,
            PathBuf::from(SESSION_DIR),
        );
        smol::block_on(run.or(async {
            Timer::after(END_LIMIT).await;
            panic!("{NEVER_ENDED}");
        }));

        assert!(matches!(rx.try_recv(), Ok(BtwEvent::Done)));
        assert_eq!(
            provider.asked_in.lock().unwrap().as_deref(),
            Some(Path::new(SESSION_DIR))
        );
    }

    fn user_text(msg: &Message) -> String {
        msg.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn injects_reminder_before_question() {
        let text = user_text(&btw_question(Q));
        assert!(text.starts_with(BTW_REMINDER), "reminder leads the message");
        assert!(text.ends_with(Q), "question trails the message");
    }
}
