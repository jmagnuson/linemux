//! Everything related to reading lines for a given event.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task;

use futures_util::ready;
use futures_util::stream::Stream;
use pin_project_lite::pin_project;
use tokio::fs::{metadata, File};
use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader, Lines};
use tokio_ as tokio;

type LineReader = Lines<BufReader<File>>;

async fn new_linereader(path: impl AsRef<Path>, seek_pos: Option<u64>) -> io::Result<LineReader> {
    let path = path.as_ref();
    let mut reader = File::open(path).await?;
    if let Some(pos) = seek_pos {
        reader.seek(io::SeekFrom::Start(pos)).await?;
    }
    let reader = BufReader::new(reader).lines();

    Ok(reader)
}

macro_rules! unwrap_or {
    ($opt:expr, $or:expr) => {
        if let Some(val) = $opt.into_iter().next() {
            val
        } else {
            $or;
        }
    };
}

macro_rules! unwrap_or_continue {
    ($opt:expr) => {
        unwrap_or!($opt, continue)
    };
}

/// Line captured for a given source path.
///
/// Also provides the caller extra context, such as the source path.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Line {
    /// The path from where the line was read.
    source: PathBuf,
    /// The received line.
    line: String,
}

impl Line {
    /// Returns a reference to the path from where the line was read.
    pub fn source(&self) -> &Path {
        self.source.as_path()
    }

    /// Returns a reference to the line.
    pub fn line(&self) -> &str {
        self.line.as_str()
    }

    /// Returns the internal components that make up a `Line`. Hidden as the
    /// return signature may change.
    #[doc(hidden)]
    pub fn into_inner(self) -> (PathBuf, String) {
        let Line { source, line } = self;

        (source, line)
    }
}

#[derive(Debug)]
struct Inner {
    reader_positions: HashMap<PathBuf, u64>,
    readers: HashMap<PathBuf, LineReader>,
    pending_readers: HashSet<PathBuf>,
}

impl Inner {
    pub fn new() -> Self {
        Inner {
            reader_positions: HashMap::new(),
            readers: HashMap::new(),
            pending_readers: HashSet::new(),
        }
    }

    pub fn reader_exists(&self, path: &Path) -> bool {
        // Make sure there isn't already a reader for the file
        self.readers.contains_key(path) || self.pending_readers.contains(path)
    }

    pub fn insert_pending(&mut self, path: PathBuf) -> bool {
        self.pending_readers.insert(path)
    }

    pub fn remove_pending(&mut self, path: &Path) -> bool {
        self.pending_readers.remove(path)
    }

    pub fn insert_reader(&mut self, path: PathBuf, reader: LineReader) -> Option<LineReader> {
        self.readers.insert(path, reader)
    }

    pub fn insert_reader_position(&mut self, path: PathBuf, pos: u64) -> Option<u64> {
        self.reader_positions.insert(path, pos)
    }

    pub fn is_empty(&self) -> bool {
        self.readers.is_empty() && self.pending_readers.is_empty()
    }
}

pin_project! {
/// Manages file watches, and can be polled to receive new lines.
///
/// ## Streaming multiplexed lines
///
/// `MuxedLines` implements [`futures::Stream`] which internally:
///   1. Receives a new event from [`MuxedEvents`].
///   2. Performs housekeeping for the event, such as moving pending file readers
///      to active, handling file rotation, etc.
///   3. Reads an active file reader if the event suggests that the file was
///      modified.
///   4. Returns a `Poll::Ready` for each line that could be read, via [`Line`]
///
/// [`futures::Stream`]: https://docs.rs/futures/0.3/futures/stream/trait.Stream.html
/// [`MuxedEvents`]: struct.MuxedEvents.html
/// [`Line`]: struct.Line.html
#[derive(Debug)]
pub struct MuxedLines {
    #[pin]
    events: crate::MuxedEvents,
    inner: Inner,
    stream_state: StreamState,
}
}

impl MuxedLines {
    pub fn new() -> io::Result<Self> {
        Ok(MuxedLines {
            events: crate::MuxedEvents::new()?,
            inner: Inner::new(),
            stream_state: StreamState::default(),
        })
    }

    fn reader_exists(&self, path: &Path) -> bool {
        // Make sure there isn't already a reader for the file
        self.inner.reader_exists(path)
    }

    fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Adds a given file to the lines watch, allowing for files which do not
    /// yet exist.
    ///
    /// Returns the canonicalized version of the path originally supplied, to
    /// match against the one contained in each `Line` received. Otherwise
    /// returns `io::Error` for a given registration failure.
    pub async fn add_file(&mut self, path: impl Into<PathBuf>, from_start: bool) -> io::Result<PathBuf> {
        let source = path.into();

        let source = self.events.add_file(&source).await?;

        if self.reader_exists(&source) {
            return Ok(source);
        }

        if !source.exists() {
            let didnt_exist = self.inner.insert_pending(source.clone());

            // If this fails it's a bug
            assert!(didnt_exist);
        } else {
            let size = match from_start {
                true => 0,
                false => metadata(&source).await?.len(),
            };

            let reader = new_linereader(&source, Some(size)).await?;

            let inner_mut = &mut self.inner;
            inner_mut.insert_reader_position(source.clone(), size);
            let last = inner_mut.insert_reader(source.clone(), reader);

            // If this fails it's a bug
            assert!(last.is_none());
        }
        // TODO: prob need 'pending' for non-existent files like Events

        Ok(source)
    }

    #[doc(hidden)]
    pub fn poll_next_line(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<io::Result<Option<Line>>> {
        if self.is_empty() && !self.stream_state.is_transient() {
            return task::Poll::Ready(Ok(None));
        }

        let this = self.project();

        let mut events = this.events;
        let inner = this.inner;
        let stream_state = this.stream_state;

        loop {
            let (new_state, maybe_line) = match stream_state {
                StreamState::Events => {
                    let event = unwrap_or_continue!(unwrap_or_continue!(ready!(events
                        .as_mut()
                        .poll_next(cx))));
                    (
                        StreamState::HandleEvent(event, HandleEventState::new()),
                        None,
                    )
                }
                StreamState::HandleEvent(ref mut event, ref mut state) => {
                    let res = ready!(poll_handle_event(inner, event, state, cx));
                    match res {
                        Ok(()) => {
                            if event.paths.is_empty() {
                                (StreamState::Events, None)
                            } else {
                                let paths = std::mem::take(&mut event.paths);
                                (StreamState::ReadLines(paths, 0), None)
                            }
                        }
                        _ => (StreamState::Events, None),
                    }
                }
                StreamState::ReadLines(paths, ref mut path_index) => {
                    if let Some(path) = paths.get(*path_index) {
                        if let Some(reader) = inner.readers.get_mut(path) {
                            let res = ready!(Pin::new(reader).poll_next_line(cx));

                            match res {
                                Ok(Some(line)) => {
                                    let line = Line {
                                        source: path.clone(),
                                        line,
                                    };
                                    return task::Poll::Ready(Some(Ok(line)).transpose());
                                }
                                Err(e) => (StreamState::Events, Some(Err(e))),
                                Ok(None) => {
                                    // Increase index whether line or not
                                    *path_index += 1;
                                    continue;
                                }
                            }
                        } else {
                            // Same state, fewer paths
                            *path_index += 1;

                            // TODO: this should work but is a bit ambiguous
                            continue;
                        }
                    } else {
                        (StreamState::Events, None)
                    }
                }
            };

            stream_state.replace(new_state);

            if let Some(line) = maybe_line {
                return task::Poll::Ready(Some(line).transpose());
            }
        }
    }

    /// Returns the next line in the stream.
    ///
    /// Waits for the next line from the set of watched files, otherwise
    /// returns `Ok(None)` if no files were ever added, or `Err` for a given
    /// error.
    pub async fn next_line(&mut self) -> io::Result<Option<Line>> {
        use futures_util::future::poll_fn;

        poll_fn(|cx| Pin::new(&mut *self).poll_next_line(cx)).await
    }
}

enum StreamState {
    Events,
    HandleEvent(notify::Event, HandleEventState),
    ReadLines(Vec<PathBuf>, usize),
}

impl StreamState {
    pub fn replace(&mut self, new_state: Self) -> StreamState {
        let mut old_state = new_state;

        std::mem::swap(self, &mut old_state);

        old_state
    }

    #[allow(clippy::match_like_matches_macro)] // otherwise bumps MSRV
    pub fn is_transient(&self) -> bool {
        if let StreamState::Events = self {
            false
        } else {
            true
        }
    }
}

impl fmt::Debug for StreamState {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        match self {
            StreamState::Events => write!(f, "Events"),
            StreamState::HandleEvent(ref event, _) => {
                write!(f, "HandleEvent({:?}, <elided>)", event)
            }
            StreamState::ReadLines(ref paths, path_index) => {
                write!(f, "ReadLines({:?})", &paths[*path_index..])
            }
        }
    }
}

impl Default for StreamState {
    fn default() -> Self {
        StreamState::Events
    }
}

type MetadataFuture = Pin<Box<dyn Future<Output = io::Result<std::fs::Metadata>> + Send + Sync>>;
type NewLineReaderFuture = Pin<Box<dyn Future<Output = io::Result<LineReader>> + Send + Sync>>;

struct HandleEventState {
    path_index: usize,
    await_state: HandleEventAwaitState,
}

impl HandleEventState {
    pub fn new() -> Self {
        HandleEventState {
            path_index: 0,
            await_state: Default::default(),
        }
    }
}

enum HandleEventAwaitState {
    Idle,
    Metadata(MetadataFuture),
    NewLineReader(NewLineReaderFuture),
}

impl Default for HandleEventAwaitState {
    fn default() -> Self {
        HandleEventAwaitState::Idle
    }
}

impl HandleEventAwaitState {
    pub fn replace(&mut self, new_state: Self) -> HandleEventAwaitState {
        let mut old_state = new_state;

        std::mem::swap(self, &mut old_state);

        old_state
    }
}

fn poll_handle_event(
    inner: &mut Inner,
    event: &mut notify::Event,
    state: &mut HandleEventState,
    cx: &mut task::Context<'_>,
) -> task::Poll<io::Result<()>> {
    loop {
        if state.path_index >= event.paths.len() {
            // Done
            return task::Poll::Ready(Ok(()));
        }

        let maybe_new_state = match &event.kind {
            // Assumes starting tail position of 0
            notify::EventKind::Create(create_event) => {
                match state.await_state {
                    HandleEventAwaitState::Idle => {
                        // Windows returns `Any` for file creation, so handle that
                        match (cfg!(target_os = "windows"), create_event) {
                            (_, notify::event::CreateKind::File) => {}
                            (true, notify::event::CreateKind::Any) => {}
                            (_, _) => {
                                state.path_index += 1;
                                continue;
                            }
                        }

                        let path = event.paths.get(state.path_index).expect("Got None Path");

                        let _preset = inner.remove_pending(path);

                        let reader_fut = Box::pin(new_linereader(path.clone(), None));

                        Some(HandleEventAwaitState::NewLineReader(reader_fut))
                    }
                    HandleEventAwaitState::NewLineReader(ref mut reader_fut) => {
                        let reader_res = ready!(reader_fut.as_mut().poll(cx));
                        if let Ok(reader) = reader_res {
                            let path = event.paths.get(state.path_index).expect("Got None Path");

                            // Don't really care about old values, we got create
                            let _previous_reader = inner.insert_reader(path.clone(), reader);
                            let _previous_pos = inner.insert_reader_position(path.clone(), 0);
                        }
                        state.path_index += 1;
                        Some(HandleEventAwaitState::Idle)
                    }
                    _ => unreachable!(),
                }
            }
            notify::EventKind::Modify(modify_event) => {
                match state.await_state {
                    HandleEventAwaitState::Idle => {
                        // Windows returns `Any` for file modification, so handle that
                        match (
                            cfg!(target_os = "windows"),
                            cfg!(target_os = "macos"),
                            modify_event,
                        ) {
                            (_, _, notify::event::ModifyKind::Data(_)) => {}
                            (
                                _,
                                _,
                                notify::event::ModifyKind::Name(notify::event::RenameMode::To),
                            ) => {}
                            (
                                _,
                                true,
                                notify::event::ModifyKind::Name(notify::event::RenameMode::From),
                            ) => {}
                            (true, _, notify::event::ModifyKind::Any) => {}
                            (_, _, _) => {
                                state.path_index += 1;
                                continue;
                            }
                        }

                        let path = event.paths.get(state.path_index).expect("Got None Path");
                        let metadata_fut = Box::pin(metadata(path.clone()));
                        Some(HandleEventAwaitState::Metadata(metadata_fut))
                    }
                    HandleEventAwaitState::Metadata(ref mut metadata_fut) => {
                        let metadata_res = ready!(metadata_fut.as_mut().poll(cx));
                        if let Ok(metadata) = metadata_res {
                            let path = event.paths.get(state.path_index).expect("Got None Path");
                            let maybe_pos = inner.reader_positions.get_mut(path);

                            let size = metadata.len();

                            if let Some(pos) = maybe_pos {
                                if size < *pos {
                                    // rolled
                                    *pos = 0;

                                    let reader_fut = Box::pin(new_linereader(path.clone(), None));

                                    Some(HandleEventAwaitState::NewLineReader(reader_fut))
                                } else {
                                    // didn't roll, just update size
                                    *pos = size;

                                    state.path_index += 1;
                                    Some(HandleEventAwaitState::Idle)
                                }
                            } else {
                                let _preset = inner.remove_pending(path);

                                let _previous_pos =
                                    inner.insert_reader_position(path.clone(), size);

                                // A Modify without a Create, so we never got a reader
                                let reader_fut = Box::pin(new_linereader(path.clone(), Some(size)));

                                Some(HandleEventAwaitState::NewLineReader(reader_fut))
                            }
                        } else {
                            state.path_index += 1;
                            Some(HandleEventAwaitState::Idle)
                        }
                    }
                    HandleEventAwaitState::NewLineReader(ref mut reader_fut) => {
                        let reader_res = ready!(reader_fut.as_mut().poll(cx));
                        if let Ok(reader) = reader_res {
                            let path = event.paths.get(state.path_index).expect("Got None Path");

                            // Don't really care about old values, we got create
                            let _previous_reader = inner.insert_reader(path.clone(), reader);
                        }
                        state.path_index += 1;
                        Some(HandleEventAwaitState::Idle)
                    }
                }
            }
            _ => {
                state.path_index += 1;
                None
            }
        };
        if let Some(new_state) = maybe_new_state {
            let _ = state.await_state.replace(new_state);
        }
    }
}

impl Stream for MuxedLines {
    type Item = io::Result<Line>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<Option<Self::Item>> {
        self.poll_next_line(cx).map(Result::transpose)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream::StreamExt;
    use std::time::Duration;
    use tempfile::tempdir;
    use tokio::fs::File;
    use tokio::io::AsyncWriteExt;
    use tokio_ as tokio;

    #[tokio::test]
    async fn test_is_send() {
        fn is_send<T: Send>() {}
        is_send::<MuxedLines>();

        tokio::spawn(async move {
            let mut lines = MuxedLines::new().unwrap();
            let _ = lines.add_file("foo").await.unwrap();
        });
    }

    #[test]
    fn test_is_sync() {
        fn is_sync<T: Sync>() {}
        is_sync::<MuxedLines>();
    }

    #[test]
    fn test_line_fns() {
        let source_path = "/some/path";
        let line_expected = "foo".to_string();

        let line = Line {
            source: PathBuf::from(&source_path),
            line: line_expected.clone(),
        };

        assert_eq!(line.source().to_str().unwrap(), source_path);

        let line_ref = line.line();
        assert_eq!(line_ref, line_expected.as_str());

        let (source_de, lines_de) = line.into_inner();
        assert_eq!(source_de, PathBuf::from(source_path));
        assert_eq!(lines_de, line_expected);
    }

    #[tokio::test]
    async fn test_inner_fns() {
        let dir = tempdir().unwrap();
        let source_path = dir.path().join("foo.txt");

        let mut inner = Inner::new();

        assert!(!inner.reader_exists(&source_path));
        assert!(inner.insert_pending(source_path.clone()));
        assert!(inner.reader_exists(&source_path));
        assert!(!inner.insert_pending(source_path.clone()));

        {
            let mut f = File::create(&source_path).await.unwrap();
            f.write_all(b"Hello, world!\nasdf\n").await.unwrap();
            f.sync_all().await.unwrap();
            f.shutdown().await.unwrap();
        }

        let linereader = new_linereader(&source_path, None).await.unwrap();
        assert!(inner
            .insert_reader(source_path.clone(), linereader)
            .is_none());
        assert!(inner
            .insert_reader_position(source_path.clone(), 0)
            .is_none());
        assert!(inner.remove_pending(&source_path));

        let linereader = new_linereader(&source_path, Some(3)).await.unwrap();
        assert!(inner
            .insert_reader(source_path.clone(), linereader)
            .is_some());
        assert_eq!(
            inner.insert_reader_position(source_path.clone(), 3),
            Some(0)
        );
    }

    #[tokio::test]
    async fn test_streamstate_debug() {
        let mut state = StreamState::default();
        let _ = format!("{:?}", state);

        let event = notify::Event::new(notify::EventKind::Other);
        state = StreamState::HandleEvent(event, HandleEventState::new());
        let _ = format!("{:?}", state);

        state = StreamState::ReadLines(vec![], 0);
        let _ = format!("{:?}", state);
    }

    #[tokio::test]
    async fn test_add_directory() {
        let tmp_dir = tempdir().unwrap();
        let tmp_dir_path = tmp_dir.path();

        let mut lines = MuxedLines::new().unwrap();
        assert!(lines.add_file(&tmp_dir_path).await.is_err());
    }

    #[tokio::test]
    async fn test_add_bad_filename() {
        let tmp_dir = tempdir().unwrap();
        let tmp_dir_path = tmp_dir.path();

        let mut lines = MuxedLines::new().unwrap();

        // This is not okay
        let file_path1 = tmp_dir_path.join("..");
        assert!(lines.add_file(&file_path1).await.is_err());

        // Don't add dir as file either
        assert!(lines.add_file(&tmp_dir_path).await.is_err());
    }

    #[tokio::test]
    async fn test_add_missing_files() {
        use tokio::time::timeout;

        let tmp_dir = tempdir().unwrap();
        let tmp_dir_path = tmp_dir.path();

        let file_path1 = tmp_dir_path.join("missing_file1.txt");
        let file_path2 = tmp_dir_path.join("missing_file2.txt");

        let mut lines = MuxedLines::new().unwrap();
        lines.add_file(&file_path1).await.unwrap();
        lines.add_file(&file_path2).await.unwrap();

        // Registering the same path again should be fine
        lines.add_file(&file_path2).await.unwrap();

        assert_eq!(lines.inner.pending_readers.len(), 2);

        let mut _file1 = File::create(&file_path1)
            .await
            .expect("Failed to create file");

        if cfg!(target_os = "macos") {
            // XXX: OSX sometimes fails `readers.len() == 2` if no delay in between file creates.
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let mut _file2 = File::create(&file_path2)
            .await
            .expect("Failed to create file");

        assert!(
            timeout(Duration::from_millis(100), lines.next())
                .await
                .is_err(),
            "Should not be any lines yet",
        );

        // Now the files should be readable
        assert_eq!(lines.inner.readers.len(), 2);

        _file1.write_all(b"foo\n").await.unwrap();
        _file1.sync_all().await.unwrap();
        _file1.shutdown().await.unwrap();
        drop(_file1);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let line1 = timeout(Duration::from_millis(100), lines.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(line1
            .source()
            .to_str()
            .unwrap()
            .contains("missing_file1.txt"));
        assert_eq!(line1.line(), "foo");

        _file2.write_all(b"bar\nbaz\n").await.unwrap();
        _file2.sync_all().await.unwrap();
        _file2.shutdown().await.unwrap();
        drop(_file2);
        tokio::time::sleep(Duration::from_millis(100)).await;
        {
            let line2 = timeout(Duration::from_millis(100), lines.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert!(line2
                .source()
                .to_str()
                .unwrap()
                .contains("missing_file2.txt"));
            assert_eq!(line2.line(), "bar");
        }
        {
            let line2 = timeout(Duration::from_millis(100), lines.next_line())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert!(line2
                .source()
                .to_str()
                .unwrap()
                .contains("missing_file2.txt"));
            assert_eq!(line2.line(), "baz");
        }

        drop(lines);
    }

    #[tokio::test]
    async fn test_file_rollover() {
        use tokio::time::timeout;

        let tmp_dir = tempdir().unwrap();
        let tmp_dir_path = tmp_dir.path();

        let file_path1 = tmp_dir_path.join("missing_file1.txt");

        let mut lines = MuxedLines::new().unwrap();
        lines.add_file(&file_path1).await.unwrap();
        assert!(!lines.is_empty());

        let mut _file1 = File::create(&file_path1)
            .await
            .expect("Failed to create file");
        _file1.write_all(b"bar\nbaz\n").await.unwrap();
        _file1.sync_all().await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        {
            let line1 = timeout(Duration::from_millis(100), lines.next_line())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert!(line1
                .source()
                .to_str()
                .unwrap()
                .contains("missing_file1.txt"));
            assert_eq!(line1.line(), "bar");
        }
        {
            let line1 = timeout(Duration::from_millis(100), lines.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert!(line1
                .source()
                .to_str()
                .unwrap()
                .contains("missing_file1.txt"));
            assert_eq!(line1.line(), "baz");
        }

        // Reset cursor
        _file1.seek(io::SeekFrom::Start(0)).await.unwrap();
        let _ = timeout(Duration::from_millis(100), lines.next()).await;

        // Roll over
        _file1.set_len(0).await.unwrap();

        // TODO: Can we still catch roll without flushing?
        let _ = timeout(Duration::from_millis(100), lines.next()).await;

        _file1.write_all(b"qux").await.unwrap();
        _file1.sync_all().await.unwrap();
        _file1.shutdown().await.unwrap();
        drop(_file1);
        tokio::time::sleep(Duration::from_millis(100)).await;
        {
            let line1 = timeout(Duration::from_millis(100), lines.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert!(line1
                .source()
                .to_str()
                .unwrap()
                .contains("missing_file1.txt"));
            assert_eq!(line1.line(), "qux");
        }
    }

    #[tokio::test]
    async fn test_ops_in_transient_state() {
        use futures_util::future::poll_fn;
        use futures_util::stream::Stream;
        use tokio::time::timeout;

        let tmp_dir = tempdir().unwrap();
        let tmp_dir_path = tmp_dir.path();

        let file_path1 = tmp_dir_path.join("missing_file1.txt");

        let mut lines = MuxedLines::new().unwrap();
        lines.add_file(&file_path1).await.unwrap();

        let mut _file1 = File::create(&file_path1)
            .await
            .expect("Failed to create file");
        _file1.write_all(b"bar\n").await.unwrap();
        _file1.sync_all().await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let maybe_pending =
            poll_fn(|cx| task::Poll::Ready(Pin::new(&mut lines).poll_next(cx))).await;
        assert!(maybe_pending.is_pending());

        // TODO: Deterministic state checking?
        //let maybe_pending = poll_fn(|cx| task::Poll::Ready(Pin::new(&mut lines).poll_next(cx))).await;
        //assert!(maybe_pending.is_pending());

        let file_path2 = tmp_dir_path.join("missing_file2.txt");
        lines.add_file(&file_path2).await.unwrap();

        // TODO: Find a way to guarantee this
        //assert_eq!(lines.inner.readers.len(), 1);

        // This should be guaranteed
        assert_eq!(lines.inner.pending_readers.len(), 1);

        {
            let line1 = timeout(Duration::from_millis(100), lines.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert!(line1
                .source()
                .to_str()
                .unwrap()
                .contains("missing_file1.txt"));
            assert_eq!(line1.line(), "bar");
        }
    }

    #[tokio::test]
    async fn test_empty_next_line() {
        let mut watcher = MuxedLines::new().unwrap();

        // No files added, expect None
        assert!(watcher.next_line().await.unwrap().is_none());
        assert!(watcher.next().await.is_none());
    }

    #[tokio::test]
    async fn test_add_existing_file() {
        use tokio::time::timeout;

        let tmp_dir = tempdir().unwrap();
        let tmp_dir_path = tmp_dir.path();

        let file_path1 = tmp_dir_path.join("foo.txt");
        let file_path2 = tmp_dir_path.join("bar.txt");

        let mut lines = MuxedLines::new().unwrap();
        lines.add_file(&file_path2).await.unwrap();

        assert_eq!(lines.inner.pending_readers.len(), 1);

        let mut _file1 = File::create(&file_path1)
            .await
            .expect("Failed to create file");

        if cfg!(target_os = "macos") {
            // XXX: OSX sometimes fails `readers.len() == 2` if no delay in between file creates.
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        _file1.write_all(b"foo\n").await.unwrap();
        _file1.sync_all().await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        tokio::fs::rename(&file_path1, &file_path2).await.unwrap();

        // Spin to handle the rename event
        let res = timeout(Duration::from_millis(100), lines.next_line()).await;
        assert!(res.is_err());

        // Now the files should be readable
        assert_eq!(lines.inner.readers.len(), 1);

        _file1.write_all(b"now bar\n").await.unwrap();
        _file1.sync_all().await.unwrap();
        _file1.shutdown().await.unwrap();
        drop(_file1);
        tokio::time::sleep(Duration::from_millis(100)).await;

        let line1 = timeout(Duration::from_millis(100), lines.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(line1.source().to_str().unwrap().contains("bar.txt"));
        assert_eq!(line1.line(), "now bar");
    }
}
