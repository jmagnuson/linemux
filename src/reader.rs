//! Everything related to reading lines for a given event.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task;

use futures_util::ready;
use pin_project_lite::pin_project;
use tokio::fs::{metadata, File};
use tokio::io::{AsyncBufReadExt, BufReader, Lines};
use tokio::stream::Stream;

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
        if let Some(val) = $opt {
            val
        } else {
            $or;
        }
    };
}

macro_rules! unwrap_res_or {
    ($res:expr, $or:expr) => {
        if let Ok(val) = $res {
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

macro_rules! unwrap_res_or_continue {
    ($res:expr) => {
        unwrap_res_or!($res, continue)
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

    pub fn reader_exists(&self, path: &PathBuf) -> bool {
        // Make sure there isn't already a reader for the file
        self.readers.contains_key(path) || self.pending_readers.contains(path)
    }

    pub fn insert_pending(&mut self, path: PathBuf) -> bool {
        self.pending_readers.insert(path)
    }

    pub fn remove_pending(&mut self, path: &PathBuf) -> bool {
        self.pending_readers.remove(path)
    }

    pub fn insert_reader(&mut self, path: PathBuf, reader: LineReader) -> Option<LineReader> {
        self.readers.insert(path, reader)
    }

    pub fn insert_reader_position(&mut self, path: PathBuf, pos: u64) -> Option<u64> {
        self.reader_positions.insert(path, pos)
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
    inner: Option<Inner>,
    stream_state: StreamState,
}
}

impl MuxedLines {
    pub fn new() -> io::Result<Self> {
        Ok(MuxedLines {
            events: crate::MuxedEvents::new()?,
            inner: Some(Inner::new()),
            stream_state: StreamState::default(),
        })
    }

    fn reader_exists(&self, path: &PathBuf) -> bool {
        // Make sure there isn't already a reader for the file
        self.inner.as_ref().unwrap().reader_exists(path)
    }

    /// Adds a given file to the lines watch, allowing for files which do not
    /// yet exist.
    ///
    /// Returns the canonicalized version of the path originally supplied, to
    /// match against the one contained in each `Line` received. Otherwise
    /// returns `io::Error` for a given registration failure.
    pub async fn add_file(&mut self, path: impl Into<PathBuf>) -> io::Result<PathBuf> {
        let source = path.into();

        let source = self.events.add_file(&source)?;

        if self.reader_exists(&source) {
            return Ok(source);
        }

        if !source.exists() {
            let didnt_exist = self.inner.as_mut().unwrap().insert_pending(source.clone());

            // If this fails it's a bug
            assert!(didnt_exist);
        } else {
            let size = metadata(&source).await?.len();

            let reader = new_linereader(&source, Some(size)).await?;

            let inner_mut = self.inner.as_mut().unwrap();
            inner_mut.insert_reader_position(source.clone(), size);
            let last = inner_mut.insert_reader(source.clone(), reader);

            // If this fails it's a bug
            assert!(last.is_none());
        }
        // TODO: prob need 'pending' for non-existent files like Events

        Ok(source)
    }
}

type HandleEventFuture = Pin<Box<dyn Future<Output = (Inner, Option<io::Result<()>>)>>>;

enum StreamState {
    Events,
    HandleEvent(notify::Event, HandleEventFuture),
    ReadLineSets(Vec<PathBuf>, usize),
}

impl StreamState {
    pub fn replace(&mut self, new_state: Self) -> StreamState {
        let mut old_state = new_state;

        std::mem::swap(self, &mut old_state);

        old_state
    }
}

impl fmt::Debug for StreamState {
    fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        match self {
            StreamState::Events => write!(f, "Events"),
            StreamState::HandleEvent(ref event, _) => {
                write!(f, "HandleEvent({:?}, <elided>)", event)
            }
            StreamState::ReadLineSets(ref paths, path_index) => {
                write!(f, "ReadLineSets({:?})", &paths[*path_index..])
            }
        }
    }
}

impl Default for StreamState {
    fn default() -> Self {
        StreamState::Events
    }
}

async fn handle_event(event: notify::Event, mut inner: Inner) -> (Inner, Option<io::Result<()>>) {
    // TODO: This should return a PathBuf

    match &event.kind {
        // Assumes starting tail position of 0
        notify::EventKind::Create(create_event) => {
            // Windows returns `Any` for file creation, so handle that
            match (cfg!(target_os = "windows"), create_event) {
                (_, notify::event::CreateKind::File) => {}
                (true, notify::event::CreateKind::Any) => {}
                (_, _) => {
                    return (inner, None);
                }
            }

            for path in &event.paths {
                let _preset = inner.remove_pending(path);

                // TODO: Handle error for each failed path
                let reader = unwrap_res_or_continue!(new_linereader(path, None).await);

                // Don't really care about old values, we got create
                let _previous_reader = inner.insert_reader(path.clone(), reader);
                let _previous_pos = inner.insert_reader_position(path.clone(), 0);
            }
        }

        // Resets the reader to the beginning of the file if rotated (size < pos)
        notify::EventKind::Modify(modify_event) => {
            // Windows returns `Any` for file modification, so handle that
            match (cfg!(target_os = "windows"), modify_event) {
                (_, notify::event::ModifyKind::Data(_)) => {}
                (true, notify::event::ModifyKind::Any) => {}
                (_, _) => {
                    return (inner, None);
                }
            }

            // TODO: Currently assumes entry exists in `readers` for given path

            for path in &event.paths {
                let size = unwrap_res_or_continue!(metadata(path).await).len();

                let pos = inner
                    .reader_positions
                    .get_mut(path)
                    .expect("missing reader position");

                if size < *pos {
                    // rolled
                    *pos = 0;

                    let reader = unwrap_res_or_continue!(new_linereader(path, None).await);

                    let _previous_reader = inner.insert_reader(path.clone(), reader);
                } else {
                    // didn't roll, just update size
                    *pos = size;
                }
            }
        }

        // Ignored event that doesn't warrant reading files
        _ => return (inner, None),
    }

    (inner, Some(Ok(())))
}

impl Stream for MuxedLines {
    type Item = io::Result<Line>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<Option<Self::Item>> {
        let this = self.project();

        let mut events = this.events;
        let inner = this.inner;
        let stream_state = this.stream_state;

        loop {
            let (new_state, maybe_lineset) = match stream_state {
                StreamState::Events => {
                    let event = unwrap_res_or_continue!(unwrap_or_continue!(ready!(Pin::new(
                        &mut *events
                    )
                    .poll_next(cx))));
                    // Temporarily take the inner reader context so that it can
                    // be modified within async/await Future.
                    let inner_taken = inner.take().expect("There was no inner to take");
                    let fut = Box::pin(handle_event(event.clone(), inner_taken));
                    (StreamState::HandleEvent(event, fut), None)
                }
                StreamState::HandleEvent(ref mut event, ref mut fut) => {
                    let (inner_taken, res) = ready!(Pin::new(&mut *fut).poll(cx));
                    let _isnone = inner.replace(inner_taken);
                    assert!(_isnone.is_none());

                    match res {
                        Some(Ok(())) => {
                            if event.paths.is_empty() {
                                (StreamState::Events, None)
                            } else {
                                let paths = std::mem::replace(&mut event.paths, Vec::new());
                                (StreamState::ReadLineSets(paths, 0), None)
                            }
                        }
                        _ => (StreamState::Events, None),
                    }
                }
                StreamState::ReadLineSets(paths, ref mut path_index) => {
                    if let Some(path) = paths.get(*path_index).cloned() {
                        if let Some(reader) = inner.as_mut().unwrap().readers.get_mut(&path.clone())
                        {
                            let res = ready!(Pin::new(reader).poll_next(cx));

                            match res {
                                Some(Ok(line)) => {
                                    let lineset = Line { source: path, line };
                                    return task::Poll::Ready(Some(Ok(lineset)));
                                }
                                Some(Err(e)) => (StreamState::Events, Some(Err(e))),
                                None => {
                                    // Increase index whether lineset or not
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

            if let Some(lineset) = maybe_lineset {
                return task::Poll::Ready(Some(lineset));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempdir::TempDir;
    use tokio;
    use tokio::fs::File;
    use tokio::io::AsyncWriteExt;
    use tokio::stream::StreamExt;

    #[test]
    fn test_lineset_fns() {
        let source_path = "/some/path";
        let line = "foo".to_string();

        let lineset = Line {
            source: PathBuf::from(&source_path),
            line: line.clone(),
        };

        assert_eq!(lineset.source().to_str().unwrap(), source_path);

        let line_slice = lineset.line();
        assert_eq!(line_slice, line.as_str());

        let (source_de, lines_de) = lineset.into_inner();
        assert_eq!(source_de, PathBuf::from(source_path));
        assert_eq!(lines_de, line);
    }

    #[tokio::test]
    async fn test_inner_fns() {
        let dir = TempDir::new("some-inner-filedir").unwrap();
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

        let inner = Inner::new();
        let event = notify::Event::new(notify::EventKind::Other);
        let fut = Box::pin(handle_event(event.clone(), inner));
        state = StreamState::HandleEvent(event, fut);
        let _ = format!("{:?}", state);

        state = StreamState::ReadLineSets(vec![], 0);
        let _ = format!("{:?}", state);
    }

    #[tokio::test]
    async fn test_add_directory() {
        let tmp_dir = TempDir::new("justa-filedir").expect("Failed to create tempdir");
        let tmp_dir_path = tmp_dir.path();

        let mut lines = MuxedLines::new().unwrap();
        assert!(lines.add_file(&tmp_dir_path).await.is_err());
    }

    #[tokio::test]
    async fn test_add_bad_filename() {
        let tmp_dir = TempDir::new("justa-filedir").expect("Failed to create tempdir");
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

        let tmp_dir = TempDir::new("missing-filedir").expect("Failed to create tempdir");
        let tmp_dir_path = tmp_dir.path();

        let file_path1 = tmp_dir_path.join("missing_file1.txt");
        let file_path2 = tmp_dir_path.join("missing_file2.txt");

        let mut lines = MuxedLines::new().unwrap();
        lines.add_file(&file_path1).await.unwrap();
        lines.add_file(&file_path2).await.unwrap();

        // Registering the same path again should be fine
        lines.add_file(&file_path2).await.unwrap();

        assert_eq!(lines.inner.as_ref().unwrap().pending_readers.len(), 2);

        let mut _file1 = File::create(&file_path1)
            .await
            .expect("Failed to create file");
        let mut _file2 = File::create(&file_path2)
            .await
            .expect("Failed to create file");

        tokio::select!(
            _event = lines.next() => {
                panic!("Should not be any lines yet");
            }
            _ = tokio::time::delay_for(Duration::from_millis(100)) => {
            }
        );

        // Now the files should be readable
        assert_eq!(lines.inner.as_ref().unwrap().readers.len(), 2);

        _file1.write_all(b"foo\n").await.unwrap();
        _file1.sync_all().await.unwrap();
        _file1.shutdown().await.unwrap();
        drop(_file1);
        tokio::time::delay_for(Duration::from_millis(100)).await;
        let lineset1 = timeout(Duration::from_millis(100), lines.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(lineset1
            .source()
            .to_str()
            .unwrap()
            .contains("missing_file1.txt"));
        assert_eq!(lineset1.line(), "foo");

        _file2.write_all(b"bar\nbaz\n").await.unwrap();
        _file2.sync_all().await.unwrap();
        _file2.shutdown().await.unwrap();
        drop(_file2);
        tokio::time::delay_for(Duration::from_millis(100)).await;
        {
            let lineset2 = timeout(Duration::from_millis(100), lines.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert!(lineset2
                .source()
                .to_str()
                .unwrap()
                .contains("missing_file2.txt"));
            assert_eq!(lineset2.line(), "bar");
        }
        {
            let lineset2 = timeout(Duration::from_millis(100), lines.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert!(lineset2
                .source()
                .to_str()
                .unwrap()
                .contains("missing_file2.txt"));
            assert_eq!(lineset2.line(), "baz");
        }

        drop(lines);
    }
}
