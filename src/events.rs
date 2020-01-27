//! Everything related to watching files for a creations, modifications,
//! deletions, etc.

use std::collections::{HashMap, HashSet};
use std::fmt::{self, Debug, Formatter};
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task;

use futures_util::pin_mut;
use futures_util::stream::{Stream as FuturesStream, StreamExt};
use notify;
use thiserror::Error;
use tokio::sync::mpsc;

/// Manages filesystem event watches, and can be polled to receive new events.
///
/// Internally, `MuxedEvents` contains a [`notify::Watcher`] from where
/// filesystem events are proxied. Functionality such as async/await support,
/// and nonexistent file registration are added.
///
/// [`notify::Watcher`]: ../notify/trait.Watcher.html
pub struct MuxedEvents {
    inner: notify::RecommendedWatcher,
    watched_directories: HashMap<PathBuf, usize>,
    /// Files that are successfully being watched
    watched_files: HashSet<PathBuf>,
    /// Files that don't exist yet, but will start once a create event comes
    /// in for the watched parent directory.
    pending_watched_files: HashSet<PathBuf>,
    event_stream: mpsc::UnboundedReceiver<Result<notify::Event, notify::Error>>,
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

#[derive(Debug, Error)]
pub enum Error {
    #[error("Failed to add path to watch")]
    AddFailure,
    #[error("Failed to remove path from watch")]
    RemoveFailure,
    #[error("Error receiving event: {0}")]
    Event(#[from] std::io::Error),
}

impl MuxedEvents {
    /// Constructs a new `MuxedEvents` instance.
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let inner: notify::RecommendedWatcher = notify::Watcher::new_immediate(move |res| {
            // The only way `send` can fail is if the receiver is dropped,
            // and `MuxedEvents` controls both. `unwrap` is not used,
            // however, since `Drop` idiosyncrasies could otherwise result
            // in a panic.
            let _ = tx.send(res);
        })
        .expect("Failed to start watcher");

        MuxedEvents {
            inner,
            watched_directories: HashMap::new(),
            watched_files: HashSet::new(),
            pending_watched_files: HashSet::new(),
            event_stream: rx,
        }
    }

    fn watch_exists(&self, path: impl AsRef<Path>) -> bool {
        let path = path.as_ref();

        // Make sure we aren't already watching the directory
        self.watched_files.contains(&path.to_path_buf())
            || self.pending_watched_files.contains(&path.to_path_buf())
            || self.watched_directories.contains_key(&path.to_path_buf())
    }

    fn add_directory(&mut self, path: impl AsRef<Path> + Into<PathBuf>) -> Result<(), Error> {
        let path_ref = path.as_ref();

        // TODO: Check the count, but this is okay to call multiple times anyway
        notify::Watcher::watch(
            &mut self.inner,
            &path_ref,
            notify::RecursiveMode::NonRecursive,
        )
        .map_err(|_e| Error::AddFailure)?;

        let count = self.watched_directories.entry(path.into()).or_insert(0);
        *count += 1;

        Ok(())
    }

    fn remove_directory(&mut self, path: impl AsRef<Path>) -> Result<(), Error> {
        let path_ref = path.as_ref();

        if let Some(count) = self.watched_directories.get(path_ref).copied() {
            match count {
                0 => unreachable!(), // watch is removed if count == 1
                1 => {
                    // Remove from map first in case `unwatch` fails.
                    self.watched_directories.remove(path_ref);
                    notify::Watcher::unwatch(&mut self.inner, &path_ref)
                        .map_err(|_e| Error::RemoveFailure)?;
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
    pub fn add_file(&mut self, path: impl AsRef<Path> + Into<PathBuf>) -> Result<PathBuf, Error> {
        let path = absolutify(path, true).map_err(Error::from)?;

        // TODO: non-existent file that later gets created as a dir?
        if path.is_dir() {
            return Err(Error::AddFailure);
        }

        // Make sure we aren't already watching the directory
        if self.watch_exists(&path) {
            return Ok(path);
        }

        if !path.exists() {
            let parent = path.parent().expect("File needs a parent directory");

            self.add_directory(&parent)
                .map_err(|_e| Error::AddFailure)?;
            self.pending_watched_files.insert(path.clone());
        } else {
            notify::Watcher::watch(&mut self.inner, &path, notify::RecursiveMode::NonRecursive)
                .map_err(|_e| Error::AddFailure)?;

            self.watched_files.insert(path.clone());
        }

        Ok(path)
    }

    fn handle_event(&mut self, event: &mut notify::Event) {
        use std::mem;

        let mut paths = mem::replace(&mut event.paths, Vec::new());

        // TODO: properly handle any errors encountered adding/removing stuff
        paths.retain(|path| {
            if path.exists() && self.pending_watched_files.contains(path) {
                // TODO: should only do on events that imply file exists
                let parent = path.parent().expect("Pending watched file needs a parent");
                let _ = self.remove_directory(parent);
                self.pending_watched_files.remove(path);
                let _ = self.add_file(path);
            }

            self.watched_files.contains(path)
        });

        let _ = mem::replace(&mut event.paths, paths);
    }

    async fn next_event(&mut self) -> Option<Result<notify::Event, Error>> {
        self.event_stream.next().await.map(|res| {
            res.map_err(|e| {
                match e.kind {
                    notify::ErrorKind::Io(io_err) => io_err.into(),
                    _ => {
                        // Runtime event errors should only be std::io, but
                        // need to handle this case anyway.
                        io::Error::new(io::ErrorKind::Other, format!("Event error: {:?}", e)).into()
                    }
                }
            })
            .map(|mut event| {
                self.handle_event(&mut event);
                event
            })
        })
    }
}

impl Default for MuxedEvents {
    fn default() -> Self {
        Self::new()
    }
}

impl FuturesStream for MuxedEvents {
    type Item = notify::Event;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<Option<Self::Item>> {
        use core::future::Future;
        use futures_util::future::FutureExt;

        let fut = self
            .next_event()
            .map(|val_opt| val_opt.map(|val_res| val_res.ok()).flatten());
        pin_mut!(fut);
        fut.poll(cx)
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
    use futures_util::stream::StreamExt;
    use notify;
    use std::time::Duration;
    use tempdir::TempDir;
    use tokio;
    use tokio::fs::File;

    #[test]
    fn test_add_directory() {
        let tmp_dir = TempDir::new("justa-filedir").expect("Failed to create tempdir");
        let tmp_dir_path = tmp_dir.path();

        let mut watcher = MuxedEvents::new();
        assert!(watcher.add_file(&tmp_dir_path).is_err());
    }

    #[test]
    fn test_add_bad_filename() {
        let tmp_dir = TempDir::new("justa-filedir").expect("Failed to create tempdir");
        let tmp_dir_path = tmp_dir.path();

        let mut watcher = MuxedEvents::new();

        // This is not okay
        let file_path1 = tmp_dir_path.join("..");
        assert!(watcher.add_file(&file_path1).is_err());
    }

    #[tokio::test]
    async fn test_add_missing_files() {
        let tmp_dir = TempDir::new("missing-filedir").expect("Failed to create tempdir");
        let tmp_dir_path = tmp_dir.path();
        let pathclone = absolutify(tmp_dir_path, false).unwrap();

        let file_path1 = tmp_dir_path.join("missing_file1.txt");
        let file_path2 = tmp_dir_path.join("missing_file2.txt");

        let mut watcher = MuxedEvents::new();
        watcher.add_file(&file_path1).unwrap();
        watcher.add_file(&file_path2).unwrap();

        // Registering the same path again should be fine
        watcher.add_file(&file_path2).unwrap();

        assert_eq!(watcher.pending_watched_files.len(), 2);
        assert!(watcher.watched_directories.contains_key(&pathclone));

        tokio::select!(
            _event = watcher.next_event() => {
                println!("Got (hopefully directory) event");
            }
            _ = tokio::time::delay_for(Duration::from_secs(1)) => {
            }
        );

        let _file1 = File::create(&file_path1)
            .await
            .expect("Failed to create file");
        let _file2 = File::create(&file_path2)
            .await
            .expect("Failed to create file");

        let event1 = watcher.next().await.unwrap();
        assert_eq!(
            event1.kind,
            notify::EventKind::Create(notify::event::CreateKind::File)
        );
        let event2 = watcher.next_event().await.unwrap().unwrap();
        assert_eq!(
            event2.kind,
            notify::EventKind::Create(notify::event::CreateKind::File)
        );

        // Now the files should be watched properly
        assert_eq!(watcher.watched_files.len(), 2);
        assert!(!watcher.watched_directories.contains_key(&pathclone));

        drop(watcher);
    }
}
