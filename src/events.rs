//! Everything related to watching files for a creations, modifications,
//! deletions, etc.

use std::collections::{HashMap, HashSet};
use std::fmt::{self, Debug, Formatter};
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task;

use futures_util::ready;
use notify::Watcher as NotifyWatcher;
use tokio::stream::Stream;
use tokio::sync::mpsc;

type EventStream = mpsc::UnboundedReceiver<Result<notify::Event, notify::Error>>;

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
    inner: notify::RecommendedWatcher,
    watched_directories: HashMap<PathBuf, usize>,
    /// Files that are successfully being watched
    watched_files: HashSet<PathBuf>,
    /// Files that don't exist yet, but will start once a create event comes
    /// in for the watched parent directory.
    pending_watched_files: HashSet<PathBuf>,
    event_stream: EventStream,
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

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn new_watcher(tx: mpsc::UnboundedSender<notify::Result<notify::Event>>) -> io::Result<notify::RecommendedWatcher> {
    println!("using PollingWatcher");

    let event_fn = std::sync::Arc::new(std::sync::Mutex::new(move |res| { let _ = tx.send(res); }));
    let delay = std::time::Duration::from_millis(100);
    let inner = notify::RecommendedWatcher::with_delay(event_fn, delay)
        .map_err(notify_to_io_error)?;

    Ok(inner)
}
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
fn new_watcher(tx: mpsc::UnboundedSender<notify::Result<notify::Event>>) -> io::Result<notify::RecommendedWatcher> {
    println!("using RecommendedWatcher");

    let inner: notify::RecommendedWatcher = NotifyWatcher::new_immediate(move |res| {
        // The only way `send` can fail is if the receiver is dropped,
        // and `MuxedEvents` controls both. `unwrap` is not used,
        // however, since `Drop` idiosyncrasies could otherwise result
        // in a panic.
        let _ = tx.send(res);
    }).map_err(notify_to_io_error)?;

    Ok(inner)
}

impl MuxedEvents {
    /// Constructs a new `MuxedEvents` instance.
    pub fn new() -> io::Result<Self> {
        let (tx, rx) = mpsc::unbounded_channel();

        let inner = new_watcher(tx)?;

        Ok(MuxedEvents {
            inner,
            watched_directories: HashMap::new(),
            watched_files: HashSet::new(),
            pending_watched_files: HashSet::new(),
            event_stream: rx,
        })
    }

    fn watch_exists(&self, path: impl AsRef<Path>) -> bool {
        let path = path.as_ref();

        // Make sure we aren't already watching the directory
        self.watched_files.contains(&path.to_path_buf())
            || self.pending_watched_files.contains(&path.to_path_buf())
            || self.watched_directories.contains_key(&path.to_path_buf())
    }

    fn watch(watcher: &mut notify::RecommendedWatcher, path: impl AsRef<Path>) -> io::Result<()> {
        watcher
            .watch(path, notify::RecursiveMode::NonRecursive)
            .map_err(notify_to_io_error)
    }

    fn unwatch(watcher: &mut notify::RecommendedWatcher, path: impl AsRef<Path>) -> io::Result<()> {
        watcher.unwatch(path).map_err(notify_to_io_error)
    }

    fn add_directory(&mut self, path: impl AsRef<Path>) -> io::Result<()> {
        let path_ref = path.as_ref();

        // `watch` behavior is platform-specific, and on some (windows) can produce
        // duplicate events if called multiple times.
        if !self.watch_exists(path_ref) {
            NotifyWatcher::watch(
                &mut self.inner,
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
                    Self::unwatch(&mut self.inner, path_ref)?;
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

    /// Adds a given file to the event watch, allowing for files which do not
    /// yet exist.
    ///
    /// Returns the canonicalized version of the path originally supplied, to
    /// match against the one contained in each `notify::Event` received.
    /// Otherwise returns `Error` for a given registration failure.
    pub fn add_file(&mut self, path: impl AsRef<Path>) -> io::Result<PathBuf> {
        let path = absolutify(path.as_ref(), true)?;

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
            Self::watch(&mut self.inner, &path)?;

            self.watched_files.insert(path.clone());
        }

        Ok(path)
    }

    fn handle_event(&mut self, event: &mut notify::Event) {
        use std::mem;

        let mut paths = mem::replace(&mut event.paths, Vec::new());

        // TODO: properly handle any errors encountered adding/removing stuff
        paths.retain(|path| {
            let path_exists = path.exists();

            // TODO: could be more intelligent/performant by checking event types
            if path_exists && self.pending_watched_files.contains(path) {
                let parent = path.parent().expect("Pending watched file needs a parent");
                let _ = self.remove_directory(parent);
                self.pending_watched_files.remove(path);
                let _ = self.add_file(path);
            }

            if !path_exists && self.watched_files.contains(path) {
                self.watched_files.remove(path);
                let _ = self.add_file(path);
            }

            self.watched_files.contains(path)
        });

        let _ = mem::replace(&mut event.paths, paths);
    }

    fn poll_next_event(
        event_stream: Pin<&mut EventStream>,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<Option<io::Result<notify::Event>>> {
        task::Poll::Ready(
            ready!(event_stream.poll_next(cx)).map(|res| res.map_err(notify_to_io_error)),
        )
    }
}

impl Stream for MuxedEvents {
    type Item = io::Result<notify::Event>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<Option<Self::Item>> {
        let mut res = ready!(Self::poll_next_event(Pin::new(&mut self.event_stream), cx));

        if let Some(Ok(ref mut event)) = res {
            self.handle_event(dbg!(event));
        }

        task::Poll::Ready(res)
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
    use std::time::Duration;
    use tempdir::TempDir;
    use tokio::fs::File;
    use tokio::stream::StreamExt;
    use tokio::time::timeout;

    #[test]
    fn test_add_directory() {
        let tmp_dir = TempDir::new("justa-filedir").expect("Failed to create tempdir");
        let tmp_dir_path = tmp_dir.path();

        let mut watcher = MuxedEvents::new().unwrap();
        assert!(watcher.add_file(&tmp_dir_path).is_err());
    }

    #[test]
    fn test_add_bad_filename() {
        let tmp_dir = TempDir::new("justa-filedir").expect("Failed to create tempdir");
        let tmp_dir_path = tmp_dir.path();

        let mut watcher = MuxedEvents::new().unwrap();

        // This is not okay
        let file_path1 = tmp_dir_path.join("..");
        assert!(watcher.add_file(&file_path1).is_err());

        // Don't add dir as file either
        assert!(watcher.add_file(&tmp_dir_path).is_err());
    }

    #[tokio::test]
    async fn test_add_missing_files() {
        use tokio::io::AsyncWriteExt;

        let tmp_dir = TempDir::new("missing-filedir").expect("Failed to create tempdir");
        let tmp_dir_path = tmp_dir.path();
        let pathclone = absolutify(tmp_dir_path, false).unwrap();

        let file_path1 = tmp_dir_path.join("missing_file1.txt");
        let file_path2 = tmp_dir_path.join("missing_file2.txt");

        let mut watcher = MuxedEvents::new().unwrap();
        let _ = format!("{:?}", watcher);
        dbg!(&watcher);
        watcher.add_file(&file_path1).unwrap();
        watcher.add_file(&file_path2).unwrap();

        // Registering the same path again should be fine
        watcher.add_file(&file_path2).unwrap();

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

        let expected_event = if cfg!(any(target_os = "windows", target_os = "freebsd")) {
            notify::EventKind::Create(notify::event::CreateKind::Any)
        } else {
            notify::EventKind::Create(notify::event::CreateKind::File)
        };

        let event1 = watcher.next().await.unwrap().unwrap();
        assert_eq!(event1.kind, expected_event,);
        let event2 = watcher.next().await.unwrap().unwrap();
        assert_eq!(event2.kind, expected_event,);

        // Now the files should be watched properly
        assert_eq!(watcher.watched_files.len(), 2);
        assert!(!watcher.watched_directories.contains_key(&pathclone));

        // Explicitly close file to allow deletion event to propagate
        _file1.sync_all().await.unwrap();
        _file1.shutdown().await.unwrap();
        drop(_file1);
        tokio::time::delay_for(Duration::from_millis(100)).await;

        // Deleting a file should throw it back into pending
        tokio::fs::remove_file(&file_path1).await.unwrap();

        // Flush possible file deletion event
        let _res = timeout(Duration::from_millis(1000), watcher.next()).await;

        assert_eq!(watcher.watched_files.len(), 1);
        assert!(watcher.watched_directories.contains_key(&pathclone));

        drop(watcher);
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
