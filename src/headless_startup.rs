use flume::Receiver;
use maki_agent::headless::Ready;
use maki_providers::{Model, ThinkingConfig};
use maki_storage::sessions::StoredThinking;

pub fn startup(
    receiver: &Receiver<Ready>,
    model: Model,
    thinking: Option<StoredThinking>,
) -> (Ready, String) {
    let ready = smol::block_on(receiver.recv_async()).unwrap_or(Ready {
        model,
        tool_names: Vec::new(),
    });
    let thinking = ThinkingConfig::from(thinking)
        .clamped(&ready.model)
        .to_string();
    (ready, thinking)
}
