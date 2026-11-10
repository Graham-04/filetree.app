//! Live folder changes: one FSEvents stream over exactly the folders the app
//! shows, reporting which of them changed. macOS reports changes anywhere
//! below a watched folder; only the folder itself counts here, so watching
//! Home ignores churn deep inside ~/Library. Changes collect in one set while
//! the UI catches up, so a burst of thousands of events wakes it once.

use std::{
    collections::HashSet,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError, mpsc},
};

use fsearch::fsevents::{self, Event};

/// How long macOS gathers events before reporting them.
const LATENCY: f64 = 0.3;

/// What changed since the last `take`.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Changes {
    /// Watched folders, as the app named them, whose contents changed or
    /// which were moved or deleted.
    pub folders: HashSet<PathBuf>,
    /// macOS dropped events; every watched folder may have changed.
    pub everything: bool,
}

#[derive(Default)]
struct Pending {
    /// The real paths watched now. Events for anything else are dropped on
    /// the watch thread, so activity deep inside a watched folder never wakes
    /// the UI and the set below stays as small as the watched set.
    interest: HashSet<PathBuf>,
    /// Real paths of watched folders that changed.
    folders: HashSet<PathBuf>,
    everything: bool,
}

pub(super) struct FolderWatcher {
    /// Each watched folder's real path and the paths the app uses for it:
    /// FSEvents reports real paths, so `/tmp` arrives as `/private/tmp`.
    watched: Vec<(PathBuf, Vec<PathBuf>)>,
    events: mpsc::Sender<Vec<Event>>,
    stream: Option<fsevents::Stream>,
    pending: Arc<Mutex<Pending>>,
}

impl FolderWatcher {
    /// A watcher with nothing watched yet, and a channel that wakes once
    /// whenever changes are waiting to be taken.
    pub(super) fn new() -> (Self, futures::channel::mpsc::Receiver<()>) {
        let (events, received) = mpsc::channel::<Vec<Event>>();
        let (mut wake, woken) = futures::channel::mpsc::channel(0);
        let pending = Arc::new(Mutex::new(Pending::default()));
        let collected = pending.clone();
        std::thread::Builder::new()
            .name("folder-watch".into())
            .spawn(move || {
                // Ends when the watcher, which owns the last sender, is dropped.
                while let Ok(batch) = received.recv() {
                    let relevant = {
                        let mut pending = collected.lock().unwrap_or_else(PoisonError::into_inner);
                        let mut relevant = false;
                        for event in batch {
                            relevant |= record(&mut pending, event);
                        }
                        relevant
                    };
                    // A full channel already means "look again"; that is enough.
                    if relevant {
                        let _ = wake.try_send(());
                    }
                }
            })
            .ok();
        let watcher = Self {
            watched: Vec::new(),
            events,
            stream: None,
            pending,
        };
        (watcher, woken)
    }

    /// Watches exactly `folders`, rebuilding the stream only when the set
    /// changes. Folders that do not exist are skipped.
    pub(super) fn watch(&mut self, folders: impl IntoIterator<Item = PathBuf>) {
        let mut watched: Vec<(PathBuf, Vec<PathBuf>)> = Vec::new();
        for folder in folders {
            let Ok(real) = std::fs::canonicalize(&folder) else {
                continue;
            };
            match watched.iter_mut().find(|(known, _)| *known == real) {
                Some((_, names)) if !names.contains(&folder) => names.push(folder),
                Some(_) => {}
                None => watched.push((real, vec![folder])),
            }
        }
        watched.sort();
        if watched == self.watched {
            return;
        }
        self.watched = watched;
        {
            let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
            pending.interest = self.watched.iter().map(|(real, _)| real.clone()).collect();
            // Changes to folders no longer shown are moot.
            let interest = pending.interest.clone();
            pending.folders.retain(|folder| interest.contains(folder));
        }
        let paths: Vec<&[u8]> = self
            .watched
            .iter()
            .map(|(real, _)| real.as_os_str().as_bytes())
            .collect();
        // Replace the stream; the old one stops when dropped.
        self.stream = (!paths.is_empty()).then(|| {
            fsevents::watch_folders(
                &paths,
                fsevents::current_id(),
                LATENCY,
                self.events.clone(),
                0,
            )
        });
    }

    /// The changes collected since the last call, among the folders
    /// watched now.
    pub(super) fn take(&self) -> Changes {
        let pending = {
            let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
            Pending {
                interest: HashSet::new(),
                folders: std::mem::take(&mut pending.folders),
                everything: std::mem::take(&mut pending.everything),
            }
        };
        let folders = self
            .watched
            .iter()
            .filter(|(real, _)| pending.everything || pending.folders.contains(real))
            .flat_map(|(_, names)| names.iter().cloned())
            .collect();
        Changes {
            folders,
            everything: pending.everything,
        }
    }
}

/// Notes `event` if it concerns a watched folder (or dropped events), and
/// returns whether it did.
fn record(pending: &mut Pending, event: Event) -> bool {
    if event.flags & (fsevents::USER_DROPPED | fsevents::KERNEL_DROPPED) != 0 {
        pending.everything = true;
        return true;
    }
    if event.flags & fsevents::HISTORY_DONE != 0 {
        return false;
    }
    // FSEvents names folders with a trailing slash.
    let mut path = event.path;
    while path.len() > 1 && path.ends_with(b"/") {
        path.pop();
    }
    let path = PathBuf::from(std::ffi::OsString::from_vec(path));
    if !pending.interest.contains(&path) {
        return false;
    }
    pending.folders.insert(path);
    true
}

/// The nearest folder at or above `path` that still exists, for a tab whose
/// folder was moved or deleted.
pub(super) fn nearest_existing(path: &Path) -> PathBuf {
    path.ancestors()
        .find(|ancestor| ancestor.is_dir())
        .unwrap_or(Path::new("/"))
        .to_path_buf()
}
