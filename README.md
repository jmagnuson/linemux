
# linemux

A library providing asynchronous, multiplexed tailing for (namely log) files.

Also available is the underlying file event-stream (driven by [`notify`](https://crates.io/crates/notify))
that can register non-existent files.

## Usage

Add linemux to your `Cargo.toml` with:

```toml
[dependencies]
linemux = "0.1"
```

## Example

```rust,no_run
use linemux::MuxedLines;
use tokio::{self, stream::StreamExt};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let mut lines = MuxedLines::new();

    // Register some files to be tailed, whether they currently exist or not.
    lines.add_file("some/file.log").await?;
    lines.add_file("/some/other/file.log").await?;

    // Wait for `LineSet` event, which contains a batch of lines captured for a
    // given source path.
    while let Some(lineset) = lines.next().await {
        let source = lineset.source().display();

        for line in lineset.iter() {
            println!("source: {}, line: {}", source, line);
        }
    }
}
```

## Caveats

Currently, linemux assumes that if a nonexistent file is added, its parent does
at least exist to register a directory watch with `notify`. This is done for
performance reasons and to simplify the pending-watch complexity (such as
limiting recursion and fs event spam). However, this may change if a need
presents itself.
