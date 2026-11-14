use std::path::Path;

use color_eyre::Result;
use color_eyre::eyre::eyre;
use maki_agent::headless::{Ready, SessionTask, await_shutdown};
use maki_agent::mcp::{self, McpHandle};
use maki_agent::session::SessionTrack;
use maki_agent::{AgentEvent, SessionEvents};
use maki_config::ProjectConfig;
use maki_lua::{EventHandle, SessionEndReason};
use maki_storage::id::SessionRef;
use tokio::sync::oneshot;

pub async fn start_mcp(cwd: &Path, project_config: ProjectConfig) -> Option<McpHandle> {
    let (handle, errors) = mcp::start_connected(cwd, project_config).await;
    if !errors.is_empty() {
        eprintln!("MCP config error: {errors}");
    }
    handle
}

pub async fn startup(
    ready: &mut oneshot::Receiver<Ready>,
    events: &mut SessionEvents,
) -> Result<Ready> {
    if let Ok(ready) = ready.await {
        return Ok(ready);
    }
    while let Some(envelope) = events.next().await {
        if let AgentEvent::Error { message, .. } = envelope.event {
            return Err(eyre!(message));
        }
    }
    Err(eyre!("provider startup ended before it was ready"))
}

pub async fn shutdown(
    task: SessionTask,
    lua: &EventHandle,
    session: &SessionRef,
) -> Option<SessionTrack> {
    let track = await_shutdown(task).await;
    lua.end_sessions([session.id()], SessionEndReason::Completed)
        .await;
    track
}
