//! Demonstrates file event stream for a given set of files.
//!
//! Usage:
//!     events /path/to/file1 /path/to/file2 ...
//!
//! The files could be present or not, but assume some filesystem operations
//! will eventually be applied to them in order to generate events.

use linemux::MuxedEvents;

#[tokio::main]
pub async fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let mut events = MuxedEvents::new()?;

    for f in args {
        events.add_file(&f).await?;
    }

    while let Ok(Some(event)) = events.next_event().await {
        println!("event: {:?}", event)
    }

    Ok(())
}

// Ignore this (not necessary for normal application use)
// Ref: https://github.com/tokio-rs/tokio/issues/2312
use tokio_ as tokio;
