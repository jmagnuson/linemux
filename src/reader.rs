//! Everything related to reading lines for a given event.

use std::collections::{HashMap, HashSet};
use std::fs::metadata as std_metadata;
use std::io;
use std::iter::IntoIterator;
use std::path::{Path, PathBuf};
use std::slice::Iter;

use std::future::Future;
use std::task;

use futures_util::stream::{Stream as FuturesStream, StreamExt};
use futures_util::{pin_mut, ready};
use pin_project_lite::pin_project;
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, BufReader, Lines};

use std::pin::Pin;

type LineReader = Lines<BufReader<File>>;

fn new_linereader(path: impl AsRef<Path>, seek_pos: Option<u64>) -> io::Result<LineReader> {
    use std::fs::File as StdFile;
    use std::io::{Seek, SeekFrom};

    let path = path.as_ref();
    let mut reader = StdFile::open(path)?;
    if let Some(pos) = seek_pos {
        reader.seek(SeekFrom::Start(pos)).unwrap();
    }
    let reader = File::from_std(reader);
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

/// Batch of lines captured for a given source path.
///
/// This is structured with performance in mind, and to provide the caller extra
/// context about the set. For now, only the source path is included.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct LineSet {
    /// The path from where the lines were read.
    source: PathBuf,
    /// The batched list of lines.
    lines: Vec<String>,
}

impl LineSet {
    /// Returns a reference to the file from where the lines were read.
    pub fn source(&self) -> &Path {
        self.source.as_path()
    }

    /// Returns a slice to the vec of lines.
    #[doc(hidden)]
    pub fn lines(&self) -> &[String] {
        self.lines.as_slice()
    }

    /// Returns an iterator over the slice of lines.
    pub fn iter(&self) -> Iter<String> {
        self.lines().iter()
    }

    /// Returns the number of lines in the set.
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// Returns `true` if the number of lines in the set is zero.
    pub fn is_empty(&self) -> bool {
        self.lines.len() == 0
    }

    /// Returns the internal components that make up a `LineSet`. Hidden as the
    /// return signature may change.
    #[doc(hidden)]
    pub fn into_inner(self) -> (PathBuf, Vec<String>) {
        let LineSet { source, lines } = self;

        (source, lines)
    }
}

impl IntoIterator for LineSet {
    type Item = String;
    type IntoIter = std::vec::IntoIter<Self::Item>;

    fn into_iter(self) -> Self::IntoIter {
        self.lines.into_iter()
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
///   4. Returns a `Poll::Ready` with the set of lines that could be read, via
///      [`LineSet`].
///
/// [`futures::Stream`]: https://docs.rs/futures/0.3/futures/stream/trait.Stream.html
/// [`MuxedEvents`]: struct.MuxedEvents.html
/// [`LineSet`]: struct.LineSet.html
pub struct MuxedLines {
    #[pin]
    events: crate::MuxedEvents,
    reader_positions: HashMap<PathBuf, u64>,
    readers: HashMap<PathBuf, LineReader>,
    pending_readers: HashSet<PathBuf>,
    stream_state: StreamState,
}
}

impl MuxedLines {
    pub fn new() -> Self {
        MuxedLines {
            events: crate::MuxedEvents::new(),
            reader_positions: HashMap::new(),
            readers: HashMap::new(),
            pending_readers: HashSet::new(),
            stream_state: StreamState::default(),
        }
    }

    fn reader_exists(&self, path: &PathBuf) -> bool {
        // Make sure there isn't already a reader for the file
        self.readers.contains_key(path) || self.pending_readers.contains(path)
    }

    /// Adds a given file to the lines watch, allowing for files which do not
    /// yet exist.
    ///
    /// Returns the canonicalized version of the path originally supplied, to
    /// match against the one contained in each `LineSet` received. Otherwise
    /// returns `Error` for a given registration failure.
    pub async fn add_file(&mut self, path: impl Into<PathBuf>) -> io::Result<PathBuf> {
        let source = path.into();

        let source = self
            .events
            .add_file(&source)
            .map_err(|e| io::Error::new(io::ErrorKind::AlreadyExists, format!("{:?}", e)))?;

        if self.reader_exists(&source) {
            return Ok(source);
        }

        if !source.exists() {
            let didnt_exist = self.pending_readers.insert(source.clone());

            // If this fails it's a bug
            assert!(didnt_exist);
        } else {
            let size = std_metadata(&source)?.len();

            let reader = new_linereader(&source, Some(size))?;

            self.reader_positions.insert(source.clone(), size);

            let last = self.readers.insert(source.clone(), reader);

            // If this fails it's a bug
            assert!(last.is_none());
        }
        // TODO: prob need 'pending' for non-existent files like Events

        Ok(source)
    }
}

impl Default for MuxedLines {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug)]
enum StreamState {
    Events,
    HandleEvent(notify::Event),
    ReadLineSets(Vec<PathBuf>, Vec<String>),
}

impl StreamState {
    pub fn replace(&mut self, new_state: Self) -> StreamState {
        let mut old_state = new_state;

        std::mem::swap(self, &mut old_state);

        old_state
    }
}

impl Default for StreamState {
    fn default() -> Self {
        StreamState::Events
    }
}

fn handle_event_internal(
    event: &notify::Event,
    readers: &mut HashMap<PathBuf, LineReader>,
    reader_positions: &mut HashMap<PathBuf, u64>,
    pending_readers: &mut HashSet<PathBuf>,
) -> Option<io::Result<()>> {
    // TODO: This should return a PathBuf

    match &event.kind {
        // Assumes starting tail position of 0
        notify::EventKind::Create(create_event) => {
            // Windows returns `Any` for file creation, so handle that
            match (cfg!(target_os = "windows"), create_event) {
                (_, notify::event::CreateKind::File) => {}
                (true, notify::event::CreateKind::Any) => {}
                (_, _) => {
                    return None;
                }
            }

            for path in &event.paths {
                let _present = pending_readers.remove(path);

                // TODO: Handle error for each failed path
                let reader = unwrap_res_or_continue!(new_linereader(path, None));

                // Don't really care about old values, we got create
                let _previous_reader = readers.insert(path.clone(), reader);
                let _previous_pos = reader_positions.insert(path.clone(), 0);
            }
        }

        // Resets the reader to the beginning of the file if rotated (size < pos)
        notify::EventKind::Modify(modify_event) => {
            // Windows returns `Any` for file modification, so handle that
            match (cfg!(target_os = "windows"), modify_event) {
                (_, notify::event::ModifyKind::Data(_)) => {}
                (true, notify::event::ModifyKind::Any) => {}
                (_, _) => {
                    return None;
                }
            }

            // TODO: Currently assumes entry exists in `readers` for given path

            for path in &event.paths {
                let size = unwrap_res_or_continue!(std_metadata(path)).len();

                let pos = reader_positions
                    .get_mut(path)
                    .expect("missing reader position");

                if size < *pos {
                    // rolled
                    *pos = 0;

                    let reader = unwrap_res_or_continue!(new_linereader(path, None));

                    let _previous_reader = readers.insert(path.clone(), reader);
                } else {
                    // didn't roll, just update size
                    *pos = size;
                }
            }
        }

        // Ignored event that doesn't warrant reading files
        _ => return None,
    }

    Some(Ok(()))
}

async fn next_line_internal(reader: &mut LineReader) -> Option<String> {
    reader.next().await.map(|res| res.unwrap())
}

impl FuturesStream for MuxedLines {
    type Item = LineSet;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<Option<Self::Item>> {
        let this = self.project();

        let mut events = this.events;
        let reader_positions = this.reader_positions;
        let readers = this.readers;
        let pending_readers = this.pending_readers;
        let stream_state = this.stream_state;

        loop {
            let (new_state, maybe_lineset) = match stream_state {
                StreamState::Events => {
                    let event = unwrap_or_continue!(ready!(events.poll_next_unpin(cx)));
                    (StreamState::HandleEvent(event), None)
                }
                StreamState::HandleEvent(ref mut event) => {
                    let res =
                        handle_event_internal(&event, readers, reader_positions, pending_readers);

                    match res {
                        Some(Ok(())) => {
                            if event.paths.is_empty() {
                                (StreamState::Events, None)
                            } else {
                                let paths = std::mem::replace(&mut event.paths, Vec::new());
                                (StreamState::ReadLineSets(paths, Vec::new()), None)
                            }
                        }
                        _ => (StreamState::Events, None),
                    }
                }
                StreamState::ReadLineSets(paths, ref mut lines) => {
                    if let Some(path) = paths.get(0).cloned() {
                        if let Some(reader) = readers.get_mut(&path.clone()) {
                            let fut = next_line_internal(reader);
                            pin_mut!(fut);
                            let res = ready!(fut.poll(cx));

                            if let Some(line) = res {
                                lines.push(line);
                                continue;
                            } else {
                                // End of line stream, see if we have any to return as LineSet
                                let maybe_lineset = if !lines.is_empty() {
                                    let ret_lines = std::mem::replace(lines, Vec::new());
                                    let lineset = LineSet {
                                        source: path,
                                        lines: ret_lines,
                                    };
                                    Some(lineset)
                                } else {
                                    None
                                };
                                (StreamState::Events, maybe_lineset)
                            }
                        } else {
                            // Same state, fewer paths
                            paths.remove(0);

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
    use futures_util::stream::StreamExt;
    use std::time::Duration;
    use tempdir::TempDir;
    use tokio;
    use tokio::fs::File;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn test_lineset_fns() {
        let source_path = "/some/path";
        let lines = vec!["foo".to_string(), "bar".to_string(), "baz".to_string()];

        let lineset = LineSet {
            source: PathBuf::from(&source_path),
            lines: lines.clone(),
        };

        assert_eq!(lineset.source().to_str().unwrap(), source_path);

        let line_slice = lineset.lines();
        assert_eq!(line_slice, lines.as_slice());

        assert_eq!(lineset.len(), lines.len());
        assert_eq!(lineset.iter().collect::<Vec<&String>>().len(), lines.len());
        assert!(!lineset.is_empty());

        let (source_de, lines_de) = lineset.into_inner();
        assert_eq!(source_de, PathBuf::from(source_path));
        assert_eq!(lines_de, lines);
    }

    #[tokio::test]
    async fn test_add_directory() {
        let tmp_dir = TempDir::new("justa-filedir").expect("Failed to create tempdir");
        let tmp_dir_path = tmp_dir.path();

        let mut lines = MuxedLines::new();
        assert!(lines.add_file(&tmp_dir_path).await.is_err());
    }

    #[tokio::test]
    async fn test_add_bad_filename() {
        let tmp_dir = TempDir::new("justa-filedir").expect("Failed to create tempdir");
        let tmp_dir_path = tmp_dir.path();

        let mut lines = MuxedLines::new();

        // This is not okay
        let file_path1 = tmp_dir_path.join("..");
        assert!(lines.add_file(&file_path1).await.is_err());
    }

    #[tokio::test]
    async fn test_add_missing_files() {
        use tokio::time::timeout;

        let tmp_dir = TempDir::new("missing-filedir").expect("Failed to create tempdir");
        let tmp_dir_path = tmp_dir.path();

        let file_path1 = tmp_dir_path.join("missing_file1.txt");
        let file_path2 = tmp_dir_path.join("missing_file2.txt");

        let mut lines = MuxedLines::new();
        lines.add_file(&file_path1).await.unwrap();
        lines.add_file(&file_path2).await.unwrap();

        // Registering the same path again should be fine
        lines.add_file(&file_path2).await.unwrap();

        assert_eq!(lines.pending_readers.len(), 2);

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
        assert_eq!(lines.readers.len(), 2);
        //assert!(!lines.watched_directories.contains_key(&pathclone));

        _file1.write_all(b"foo\n").await.unwrap();
        _file1.sync_all().await.unwrap();
        _file1.shutdown().await.unwrap();
        drop(_file1);
        tokio::time::delay_for(Duration::from_millis(100)).await;
        let lineset1 = timeout(Duration::from_millis(100), lines.next())
            .await
            .unwrap()
            .unwrap();
        assert!(lineset1
            .source()
            .to_str()
            .unwrap()
            .contains("missing_file1.txt"));
        assert_eq!(lineset1.lines(), &["foo".to_string()]);

        _file2.write_all(b"bar\nbaz\n").await.unwrap();
        _file2.sync_all().await.unwrap();
        _file2.shutdown().await.unwrap();
        drop(_file2);
        tokio::time::delay_for(Duration::from_millis(100)).await;
        let lineset2 = timeout(Duration::from_millis(100), lines.next())
            .await
            .unwrap()
            .unwrap();
        assert!(lineset2
            .source()
            .to_str()
            .unwrap()
            .contains("missing_file2.txt"));
        assert_eq!(lineset2.lines(), &["bar".to_string(), "baz".to_string()]);

        drop(lines);
    }
}
