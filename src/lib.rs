//! A library providing asynchronous, multiplexed tailing for (namely log) files.
//!
//! Also available is the underlying file event-stream (driven by [`notify`](https://crates.io/crates/notify))
//! that can register non-existent files.
//!
//! ## Example
//!
//! ```no_run
//! use linemux::MuxedLines;
//!
//! #[tokio::main]
//! async fn main() -> std::io::Result<()> {
//!     let mut lines = MuxedLines::new()?;
//!
//!     // Register some files to be tailed, whether they currently exist or not.
//!     lines.add_file("some/file.log").await?;
//!     lines.add_file("/some/other/file.log").await?;
//!
//!     // Wait for `Line` event, which contains the line captured for a given
//!     // source path.
//!     while let Ok(Some(line)) = lines.next_line().await {
//!         println!("source: {}, line: {}", line.source().display(), line.line());
//!     }
//!     Ok(())
//! }
//! ```
//!
//! ## Caveats
//!
//! Currently, linemux assumes that if a nonexistent file is added, its parent does
//! at least exist to register a directory watch with `notify`. This is done for
//! performance reasons and to simplify the pending-watch complexity (such as
//! limiting recursion and fs event spam). However, this may change if a need
//! presents itself.

mod events;
mod reader;

pub use events::MuxedEvents;
pub use notify::event;
pub use reader::{Line, MuxedLines};

#[cfg(doctest)]
doc_comment::doctest!("../README.md");
