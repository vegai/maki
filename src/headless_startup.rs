use color_eyre::Result;
use color_eyre::eyre::eyre;
use flume::Receiver;
use maki_agent::headless::Ready;
use maki_agent::{AgentEvent, SessionEvents};
use maki_providers::ThinkingConfig;
use maki_storage::sessions::StoredThinking;

pub fn startup(
    receiver: &Receiver<Ready>,
    events: &mut SessionEvents,
    thinking: Option<StoredThinking>,
) -> Result<(Ready, String)> {
    let ready = match smol::block_on(receiver.recv_async()) {
        Ok(ready) => ready,
        Err(_) => {
            while let Some(envelope) = smol::block_on(events.next()) {
                if let AgentEvent::Error { message, .. } = envelope.event {
                    return Err(eyre!(message));
                }
            }
            return Err(eyre!("provider startup ended before it was ready"));
        }
    };
    let thinking = ThinkingConfig::from(thinking)
        .clamped(&ready.model)
        .to_string();
    Ok((ready, thinking))
}
