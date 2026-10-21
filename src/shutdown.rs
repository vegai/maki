use std::io;

#[cfg(unix)]
use async_signal::{Signal, Signals};
use maki_agent::CancelTrigger;
#[cfg(unix)]
use smol::stream::StreamExt;

pub fn watch(cancel: CancelTrigger) -> io::Result<smol::Task<()>> {
    #[cfg(unix)]
    let mut signals = Signals::new([Signal::Int, Signal::Term])?;
    Ok(smol::spawn(async move {
        #[cfg(unix)]
        let _ = signals.next().await;
        #[cfg(not(unix))]
        smol::future::pending::<()>().await;
        cancel.cancel();
    }))
}
