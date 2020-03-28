//! Demonstrates file event stream for a given set of files.
//!
//! Usage:
//!     events /path/to/file1 /path/to/file2 ...
//!
//! The files could be present or not, but assume some filesystem operations
//! will eventually be applied to them in order to generate events.

use futures_util::stream::StreamExt;
use tokio;

use linemux::MuxedEvents;

#[tokio::main(threaded_scheduler)]
pub async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let mut events = MuxedEvents::new().unwrap();

    for f in args {
        events.add_file(&f).unwrap();
    }

    while let Some(Ok(event)) = events.next().await {
        println!("event: {:?}", event)
    }
}
