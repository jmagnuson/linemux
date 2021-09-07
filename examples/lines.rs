//! Demonstrates file event stream for a given set of files.
//!
//! Usage:
//!     lines /path/to/file1 /path/to/file2 ...
//!
//! The files could be present or not, but assume some data will eventually be
//! be written to them in order to generate lines.

use linemux::MuxedLines;

#[tokio::main]
pub async fn main() -> std::io::Result<()> {
    let mut show_source = true;

    let mut args: Vec<String> = std::env::args().skip(1).collect();

    if args.get(0).map(|s| s.as_str()) == Some("--no-source") {
        show_source = false;
        args.remove(0);
    }

    let mut lines = MuxedLines::new()?;

    for f in args {
        lines.add_file(&f).await?;
    }

    while let Ok(Some(line)) = lines.next_line().await {
        if show_source {
            println!("({}) {}", line.source().display(), line.line());
        } else {
            println!("{}", line.line());
        }
    }

    Ok(())
}

// Ignore this (not necessary for normal application use)
// Ref: https://github.com/tokio-rs/tokio/issues/2312
use tokio_ as tokio;
