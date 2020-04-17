//! Demonstrates file event stream for a given set of files.
//!
//! Usage:
//!     lines /path/to/file1 /path/to/file2 ...
//!
//! The files could be present or not, but assume some data will eventually be
//! be written to them in order to generate lines.

use linemux::MuxedLines;
use tokio::stream::StreamExt;

#[tokio::main]
pub async fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let mut lines = MuxedLines::new()?;

    for f in args {
        lines.add_file(&f).await?;
    }

    while let Some(Ok(line)) = lines.next().await {
        println!("({}) {}", line.source().display(), line.line());
    }

    Ok(())
}
