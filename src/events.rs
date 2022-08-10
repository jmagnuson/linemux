//! Everything related to watching files for a creations, modifications,
//! deletions, etc.

use std::collections::{HashMap, HashSet};
use std::fmt::{self, Debug, Formatter};
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task;

use futures_util::ready;
use futures_util::stream::Stream;
use notify::Watcher as NotifyWatcher;
use tokio::sync::mpsc;
use tokio_ as tokio;

type BoxedWatcher = Box<dyn notify::Watcher + Send + Sync + Unpin + 'static>;
type EventStream = mpsc::UnboundedReceiver<Result<notify::Event, notify::Error>>;
type EventStreamSender = mpsc::UnboundedSender<Result<notify::Event, notify::Error>>;

fn notify_to_io_error(e: notify::Error) -> io::Error {
    match e.kind {
        notify::ErrorKind::Io(io_err) => io_err,
        _ => {
            // Runtime event errors should only be std::io, but
            // need to handle this case anyway.
            io::Error::new(io::ErrorKind::Other, e)
        }
    }
}

/// Manages filesystem event watches, and can be polled to receive new events.
///
/// Internally, `MuxedEvents` contains a [`notify::Watcher`] from where
/// filesystem events are proxied. Functionality such as async/await support,
/// and nonexistent file registration are added.
///
/// [`notify::Watcher`]: https://docs.rs/notify/5.0.0-pre.2/notify/trait.Watcher.html
pub struct MuxedEvents {
    inner: BoxedWatcher,
    watched_directories: HashMap<PathBuf, usize>,
    /// Files that are successfully being watched
    watched_files: HashSet<PathBuf>,
    /// Files that don't exist yet, but will start once a create event comes
    /// in for the watched parent directory.
    pending_watched_files: HashSet<PathBuf>,
    event_stream: EventStream,
    event_stream_sender: EventStreamSender,
}

impl Debug for MuxedEvents {
    fn fmt(&self, f: &mut Formatter) -> Result<(), fmt::Error> {
        f.debug_struct("MuxedEvents")
            .field("watched_directories", &self.watched_directories)
            .field("watched_files", &self.watched_files)
            .field("pending_watched_files", &self.pending_watched_files)
            .finish()
    }
}

impl MuxedEvents {
    /// Constructs a new `MuxedEvents` instance.
    pub fn new() -> io::Result<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        let sender = tx.clone();

        let inner: notify::RecommendedWatcher = notify::RecommendedWatcher::new(move |res| {
            // The only way `send` can fail is if the receiver is dropped,
            // and `MuxedEvents` controls both. `unwrap` is not used,
            // however, since `Drop` idiosyncrasies could otherwise result
            // in a panic.
            let _ = tx.send(res);
        })
        .map_err(notify_to_io_error)?;

        Ok(MuxedEvents {
            inner: Box::new(inner),
            watched_directories: HashMap::new(),
            watched_files: HashSet::new(),
            pending_watched_files: HashSet::new(),
            event_stream: rx,
            event_stream_sender: sender,
        })
    }

    fn watch_exists(&self, path: impl AsRef<Path>) -> bool {
        let path = path.as_ref();

        // Make sure we aren't already watching the directory
        self.watched_files.contains(&path.to_path_buf())
            || self.pending_watched_files.contains(&path.to_path_buf())
            || self.watched_directories.contains_key(&path.to_path_buf())
    }

    fn watch(watcher: &mut dyn notify::Watcher, path: &Path) -> io::Result<()> {
        watcher
            .watch(path, notify::RecursiveMode::NonRecursive)
            .map_err(notify_to_io_error)
    }

    fn unwatch(watcher: &mut dyn notify::Watcher, path: &Path) -> io::Result<()> {
        watcher.unwatch(path).map_err(notify_to_io_error)
    }

    fn add_directory(&mut self, path: impl AsRef<Path>) -> io::Result<()> {
        let path_ref = path.as_ref();

        // `watch` behavior is platform-specific, and on some (windows) can produce
        // duplicate events if called multiple times.
        if !self.watch_exists(path_ref) {
            NotifyWatcher::watch(
                self.inner.as_mut(),
                path_ref,
                notify::RecursiveMode::NonRecursive,
            )
            .map_err(notify_to_io_error)?;
        }

        let count = self
            .watched_directories
            .entry(path_ref.to_owned())
            .or_insert(0);
        *count += 1;

        Ok(())
    }

    fn remove_directory(&mut self, path: impl AsRef<Path>) -> io::Result<()> {
        let path_ref = path.as_ref();

        if let Some(count) = self.watched_directories.get(path_ref).copied() {
            match count {
                0 => unreachable!(), // watch is removed if count == 1
                1 => {
                    // Remove from map first in case `unwatch` fails.
                    self.watched_directories.remove(path_ref);
                    Self::unwatch(self.inner.as_mut(), path_ref)?;
                }
                _ => {
                    let new_count = self
                        .watched_directories
                        .get_mut(path_ref)
                        .expect("path was not present but count > 1");
                    *new_count -= 1;
                }
            }
        }

        Ok(())
    }

    /*fn len(&self) -> usize {
        self.watched_files.len() + self.pending_watched_files.len()
    }*/

    fn is_empty(&self) -> bool {
        self.watched_files.is_empty() && self.pending_watched_files.is_empty()
    }

    /// Adds a given file to the event watch, allowing for files which do not
    /// yet exist.
    ///
    /// Returns the canonicalized version of the path originally supplied, to
    /// match against the one contained in each `notify::Event` received.
    /// Otherwise returns `Error` for a given registration failure.
    pub async fn add_file(&mut self, path: impl Into<PathBuf>) -> io::Result<PathBuf> {
        self._add_file(path, false)
    }

    /// Adds a given file to the event watch, allowing for files which do not
    /// yet exist. Once the file is added, an event is immediately created for
    /// the file to trigger reading it as soon as events are being read.
    ///
    /// Returns the canonicalized version of the path originally supplied, to
    /// match against the one contained in each `notify::Event` received.
    /// Otherwise returns `Error` for a given registration failure.
    pub(crate) async fn add_file_initial_event(
        &mut self,
        path: impl Into<PathBuf>,
    ) -> io::Result<PathBuf> {
        self._add_file(path, true)
    }

    fn _add_file(&mut self, path: impl Into<PathBuf>, initial_event: bool) -> io::Result<PathBuf> {
        let path = absolutify(path, true)?;

        // TODO: non-existent file that later gets created as a dir?
        if path.is_dir() {
            // on Linux this would be `EISDIR` (21) and maybe
            // `ERROR_DIRECTORY_NOT_SUPPORTED` (336) for windows?
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Is a directory",
            ));
        }

        // Make sure we aren't already watching the directory
        if self.watch_exists(&path) {
            return Ok(path);
        }

        if !path.exists() {
            let parent = path.parent().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "File needs a parent directory")
            })?;

            self.add_directory(&parent)?;
            self.pending_watched_files.insert(path.clone());
        } else {
            Self::watch(self.inner.as_mut(), &path)?;

            self.watched_files.insert(path.clone());

            if initial_event {
                // Send an initial event for this file when requested.
                // This is useful if we wanted earlier lines in the file than
                // where it is up to now, and we want those events before the
                // next time this file is modified.
                self.event_stream_sender
                    .send(Ok(notify::Event {
                        attrs: notify::event::EventAttributes::new(),
                        kind: notify::EventKind::Create(notify::event::CreateKind::File),
                        paths: vec![path.clone()],
                    }))
                    .ok();
                // Errors here are not anything to worry about, so we .ok();
                // An error would just mean no one is listening.
            }
        }

        Ok(path)
    }

    fn handle_event(&mut self, event: &mut notify::Event) {
        let paths = &mut event.paths;
        let event_kind = &event.kind;

        // TODO: properly handle any errors encountered adding/removing stuff
        paths.retain(|path| {
            // Fixes a potential race when detecting file rotations.
            let path_exists =
                if let notify::EventKind::Remove(notify::event::RemoveKind::File) = &event_kind {
                    false
                } else if let notify::EventKind::Modify(notify::event::ModifyKind::Name(
                    notify::event::RenameMode::From,
                )) = &event_kind
                {
                    if cfg!(target_os = "macos") {
                        path.exists()
                    } else {
                        false
                    }
                } else {
                    path.exists()
                };

            // TODO: could be more intelligent/performant by checking event types
            if path_exists && self.pending_watched_files.contains(path) {
                let parent = path.parent().expect("Pending watched file needs a parent");
                let _ = self.remove_directory(parent);
                self.pending_watched_files.remove(path);
                let _ = self._add_file(path, false);
            }

            if !path_exists && self.watched_files.contains(path) {
                self.watched_files.remove(path);
                let _ = self._add_file(path, false);
            }

            if event_kind.is_remove() {
                self.pending_watched_files.contains(path)
            } else {
                self.watched_files.contains(path)
            }
        });
    }

    fn __poll_next_event(
        mut event_stream: Pin<&mut EventStream>,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<Option<io::Result<notify::Event>>> {
        task::Poll::Ready(
            ready!(event_stream.poll_recv(cx)).map(|res| res.map_err(notify_to_io_error)),
        )
    }

    #[doc(hidden)]
    pub fn poll_next_event(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<io::Result<Option<notify::Event>>> {
        if self.is_empty() {
            return task::Poll::Ready(Ok(None));
        }

        let mut res = ready!(Self::__poll_next_event(
            Pin::new(&mut self.event_stream),
            cx
        ));

        if let Some(Ok(ref mut event)) = res {
            self.handle_event(event);
        }

        task::Poll::Ready(res.transpose())
    }

    /// Returns the next event in the stream.
    ///
    /// Waits for the next event from the set of watched files, otherwise
    /// returns `Ok(None)` if no files were ever added, or `Err` for a given
    /// error.
    pub async fn next_event(&mut self) -> io::Result<Option<notify::Event>> {
        use futures_util::future::poll_fn;

        poll_fn(|cx| Pin::new(&mut *self).poll_next_event(cx)).await
    }
}

impl Stream for MuxedEvents {
    type Item = io::Result<notify::Event>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<Option<Self::Item>> {
        self.poll_next_event(cx).map(Result::transpose)
    }
}

// TODO: maybe use with crate `path-absolutize`
fn absolutify(path: impl Into<PathBuf>, is_file: bool) -> io::Result<PathBuf> {
    let path = path.into();

    let (dir, maybe_filename) = if is_file {
        let parent = match path.parent() {
            None => std::env::current_dir()?,
            Some(path) => {
                if path == Path::new("") {
                    std::env::current_dir()?
                } else {
                    path.to_path_buf()
                }
            }
        };
        let filename = path
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Filename not found in path"))?
            .to_os_string();

        (parent, Some(filename))
    } else {
        (path, None)
    };

    let dir = if let Ok(linked_dir) = dir.read_link() {
        linked_dir
    } else {
        dir
    };

    let dir = if let Ok(abs_dir) = dir.canonicalize() {
        abs_dir
    } else {
        dir
    };

    let path = if let Some(filename) = maybe_filename {
        dir.join(filename)
    } else {
        dir
    };

    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::absolutify;
    use super::MuxedEvents;
    use crate::events::notify_to_io_error;
    use futures_util::stream::StreamExt;
    use std::time::Duration;
    use tempfile::tempdir;
    use tokio::fs::File;
    use tokio::time::timeout;
    use tokio_ as tokio;

    #[tokio::test]
    async fn test_add_directory() {
        let tmp_dir = tempdir().unwrap();
        let tmp_dir_path = tmp_dir.path();

        let mut watcher = MuxedEvents::new().unwrap();
        assert!(watcher.add_file(&tmp_dir_path).await.is_err());
    }

    #[tokio::test]
    async fn test_add_bad_filename() {
        let tmp_dir = tempdir().unwrap();
        let tmp_dir_path = tmp_dir.path();

        let mut watcher = MuxedEvents::new().unwrap();

        // This is not okay
        let file_path1 = tmp_dir_path.join("..");
        assert!(watcher.add_file(&file_path1).await.is_err());

        // Don't add dir as file either
        assert!(watcher.add_file(&tmp_dir_path).await.is_err());
    }

    #[tokio::test]
    async fn test_add_missing_files() {
        use tokio::io::AsyncWriteExt;

        let tmp_dir = tempdir().unwrap();
        let tmp_dir_path = tmp_dir.path();
        let pathclone = absolutify(tmp_dir_path, false).unwrap();

        let file_path1 = tmp_dir_path.join("missing_file1.txt");
        let file_path2 = tmp_dir_path.join("missing_file2.txt");

        let mut watcher = MuxedEvents::new().unwrap();
        let _ = format!("{:?}", watcher);
        watcher.add_file(&file_path1).await.unwrap();
        watcher.add_file(&file_path2).await.unwrap();

        // Registering the same path again should be fine
        watcher.add_file(&file_path2).await.unwrap();

        assert_eq!(watcher.pending_watched_files.len(), 2);
        assert!(watcher.watched_directories.contains_key(&pathclone));

        // Flush possible directory creation event
        let _res = timeout(Duration::from_secs(1), watcher.next()).await;

        let mut _file1 = File::create(&file_path1)
            .await
            .expect("Failed to create file");
        let _file2 = File::create(&file_path2)
            .await
            .expect("Failed to create file");

        let expected_event = if cfg!(target_os = "windows") {
            notify::EventKind::Create(notify::event::CreateKind::Any)
        } else {
            notify::EventKind::Create(notify::event::CreateKind::File)
        };

        let event1 = watcher.next().await.unwrap().unwrap();
        assert_eq!(event1.kind, expected_event,);
        let event2 = watcher.next_event().await.unwrap().unwrap();
        assert_eq!(event2.kind, expected_event,);

        // Now the files should be watched properly
        assert_eq!(watcher.watched_files.len(), 2);
        assert!(!watcher.watched_directories.contains_key(&pathclone));

        // Explicitly close file to allow deletion event to propagate
        _file1.sync_all().await.unwrap();
        _file1.shutdown().await.unwrap();
        drop(_file1);
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Deleting a file should throw it back into pending
        tokio::fs::remove_file(&file_path1).await.unwrap();

        // Flush possible file deletion event
        let expected_event = {
            let remove_kind = if cfg!(target_os = "windows") {
                notify::event::RemoveKind::Any
            } else {
                notify::event::RemoveKind::File
            };
            notify::Event::new(notify::EventKind::Remove(remove_kind))
                .add_path(absolutify(file_path1, true).unwrap())
        };

        let mut events = vec![];
        tokio::time::timeout(tokio::time::Duration::from_millis(2000), async {
            loop {
                let event = watcher.next_event().await.unwrap().unwrap();
                if event == expected_event {
                    break;
                }
                events.push(event);
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "Did not receive expected event, events received: {:?}",
                events
            );
        });

        assert_eq!(watcher.watched_files.len(), 1);
        assert!(watcher.watched_directories.contains_key(&pathclone));

        drop(watcher);
    }

    #[tokio::test]
    async fn test_empty_next_event() {
        let mut watcher = MuxedEvents::new().unwrap();

        // No files added, expect None
        assert!(watcher.next_event().await.unwrap().is_none());
        assert!(watcher.next().await.is_none());
    }

    #[test]
    fn test_notify_error() {
        use std::io;

        let notify_io_error = notify::Error::io(io::Error::new(io::ErrorKind::AddrInUse, "foobar"));
        let io_error = notify_to_io_error(notify_io_error);
        assert_eq!(io_error.kind(), io::ErrorKind::AddrInUse);

        let notify_custom_error = notify::Error::path_not_found();
        let io_error = notify_to_io_error(notify_custom_error);
        assert_eq!(io_error.kind(), io::ErrorKind::Other);
    }
}
