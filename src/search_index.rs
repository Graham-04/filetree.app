//! App-owned, names-only fsearch index. No daemon or content indexing is started.
use anyhow::{Context, Result};
use fsearch::{
    fsevents,
    index::Index,
    live::Live,
    query::{Query, Searcher},
    walk::{self, FLAG_MOUNT, KIND_DIR, Listing, NONE, RawEnt},
};
use std::{
    collections::{HashMap, VecDeque},
    fs,
    os::unix::ffi::{OsStrExt, OsStringExt},
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    sync::{
        Arc, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard, Weak, mpsc,
    },
    time::{Duration, Instant},
};

/// Saving rebuilds the whole index (seconds of CPU and hundreds of MB of
/// transient memory), so fold the overlay into the saved index only once it
/// grows large or has been pending for hours. Startup replays FSEvents from
/// the saved cursor, so changes since the last save are not lost.
const CHECKPOINT_PENDING: usize = 50_000;
const CHECKPOINT_EVERY: Duration = Duration::from_secs(12 * 3600);

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Progress {
    pub percent: f32,
    pub label: String,
    pub revision: u64,
    pub ready: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct IndexUsage {
    pub total: u64,
    /// Label and estimated bytes, largest folders first, then "Other".
    pub parts: Vec<(String, u64)>,
}

/// Folders in Home by name; others, such as /Applications, by full path so
/// they stay distinct from their Home namesakes.
fn root_label(root: &Path, home: &Path) -> String {
    match root.strip_prefix(home) {
        Ok(relative) => relative.to_string_lossy().into_owned(),
        Err(_) => root.to_string_lossy().into_owned(),
    }
}

/// Locks that outlive a panic on another thread. The index worker and the
/// UI share these; if one panics while holding a lock, the other keeps the
/// data instead of panicking in turn, so an index failure cannot crash the app.
trait Unpoisoned<T> {
    fn locked(&self) -> MutexGuard<'_, T>;
}

impl<T> Unpoisoned<T> for Mutex<T> {
    fn locked(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

trait UnpoisonedRw<T> {
    fn reading(&self) -> RwLockReadGuard<'_, T>;
    fn writing(&self) -> RwLockWriteGuard<'_, T>;
}

impl<T> UnpoisonedRw<T> for RwLock<T> {
    fn reading(&self) -> RwLockReadGuard<'_, T> {
        self.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn writing(&self) -> RwLockWriteGuard<'_, T> {
        self.write().unwrap_or_else(PoisonError::into_inner)
    }
}

struct State {
    generation: u64,
    index: Option<Arc<RwLock<Live>>>,
    progress: Progress,
    /// Wakes the current worker from its wait so a cancelled generation exits.
    wake: Option<mpsc::Sender<Vec<fsevents::Event>>>,
    /// Tells the UI that `progress` changed. Bounded, so bursts coalesce.
    changed: Option<futures::channel::mpsc::Sender<()>>,
}

impl State {
    /// Cancels the running generation and wakes its worker to exit.
    fn advance(&mut self) {
        self.generation += 1;
        if let Some(wake) = self.wake.take() {
            let _ = wake.send(Vec::new());
        }
    }
}

/// State locked for mutation; unlocking notifies the UI if progress changed.
struct Locked<'a> {
    state: MutexGuard<'a, State>,
    before: Progress,
}

impl std::ops::Deref for Locked<'_> {
    type Target = State;
    fn deref(&self) -> &State {
        &self.state
    }
}

impl std::ops::DerefMut for Locked<'_> {
    fn deref_mut(&mut self) -> &mut State {
        &mut self.state
    }
}

impl Drop for Locked<'_> {
    fn drop(&mut self) {
        if self.state.progress != self.before
            && let Some(changed) = &mut self.state.changed
        {
            let _ = changed.try_send(());
        }
    }
}

#[derive(Clone)]
pub(super) struct SearchIndex {
    state: Arc<Mutex<State>>,
    home: PathBuf,
    roots: Arc<[PathBuf]>,
    cache: PathBuf,
    _owner: Option<Arc<CancelOnDrop>>,
}

struct CancelOnDrop(Weak<Mutex<State>>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if let Some(state) = self.0.upgrade() {
            state.locked().advance();
        }
    }
}

impl SearchIndex {
    pub(super) fn new(home: PathBuf) -> Self {
        let roots = common_roots(&home);
        let folder = home.join("Library/Application Support/Filetree");
        // v2 leaves out dependency, build, and bundle contents (`walk::pruned`).
        // The older, larger index is only a cache: remove it and rebuild.
        let _ = fs::remove_file(folder.join("common-index.bin"));
        let cache = folder.join("common-index-v2.bin");
        Self::with_paths(home, roots, cache)
    }

    fn with_paths(home: PathBuf, roots: Vec<PathBuf>, cache: PathBuf) -> Self {
        // Names inside node_modules, .git, Cargo target, and .app bundles are
        // not what anyone searches for by hand; their folders stay findable.
        walk::PRUNE.store(true, std::sync::atomic::Ordering::Relaxed);
        let state = Arc::new(Mutex::new(State {
            generation: 0,
            index: None,
            progress: Progress {
                percent: 0.,
                label: "Preparing".into(),
                revision: 0,
                ready: false,
            },
            wake: None,
            changed: None,
        }));
        Self {
            _owner: Some(Arc::new(CancelOnDrop(Arc::downgrade(&state)))),
            state,
            home,
            roots: roots.into(),
            cache,
        }
    }

    /// Signals each time progress changes, coalescing bursts into one wake.
    pub(super) fn changes(&self) -> futures::channel::mpsc::Receiver<()> {
        let (changed, changes) = futures::channel::mpsc::channel(0);
        self.state.locked().changed = Some(changed);
        changes
    }

    fn lock(&self) -> Locked<'_> {
        let state = self.state.locked();
        Locked {
            before: state.progress.clone(),
            state,
        }
    }

    pub(super) fn progress(&self) -> Progress {
        self.state.locked().progress.clone()
    }

    pub(super) fn start(&self) -> Result<()> {
        let generation = {
            let mut state = self.lock();
            state.advance();
            state.progress.ready = false;
            state.progress.percent = 0.;
            state.progress.label = "Indexing".into();
            state.generation
        };
        let mut service = self.clone();
        // The worker keeps shared data alive, but not the last client's lifetime.
        // Dropping the app's final handle cancels it and drops its native stream.
        service._owner = None;
        let worker = std::thread::Builder::new()
            .name("file-index".into())
            .spawn(move || {
                // SAFETY: adjusts only this newly-created worker thread's scheduling.
                unsafe {
                    libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_UTILITY, 0);
                }
                fsearch::no_materialize();
                // A panic ends indexing with an error instead of the app; the
                // locks below recover from it rather than re-raising it.
                let result = std::panic::catch_unwind(AssertUnwindSafe(|| service.run(generation)))
                    .unwrap_or_else(|_| {
                        Err(anyhow::anyhow!("the index worker stopped unexpectedly"))
                    });
                if let Err(error) = result {
                    let mut state = service.lock();
                    if state.generation == generation {
                        state.progress.label = format!("Index error: {error}");
                        state.progress.ready = state.index.is_some();
                        state.progress.revision += 1;
                        eprintln!("File index: {error:#}");
                    }
                }
            });
        if let Err(error) = worker {
            let mut state = self.lock();
            state.progress.label = format!("Could not start index: {error}");
            state.progress.revision += 1;
            return Err(error).context("Could not start file indexing");
        }
        Ok(())
    }

    fn load_cached(&self, generation: u64) -> bool {
        let Some(index) = Index::load(&self.cache) else {
            return false;
        };
        let mut state = self.lock();
        if state.generation != generation {
            return false;
        }
        state.index = Some(Arc::new(RwLock::new(Live::lazy(index))));
        state.progress.ready = true;
        state.progress.percent = 100.;
        state.progress.label = "Indexed".into();
        state.progress.revision += 1;
        true
    }

    fn live(&self) -> Result<Arc<RwLock<Live>>> {
        let state = self.state.locked();
        state
            .index
            .clone()
            .ok_or_else(|| anyhow::anyhow!(state.progress.label.clone()))
    }

    fn run(&self, generation: u64) -> Result<()> {
        let cached = self.load_cached(generation);
        if !self.current(generation) {
            return Ok(());
        }
        let saved_cursor = if cached {
            self.live()?.reading().event_id
        } else {
            0
        };
        let current_cursor = fsevents::current_id();
        let valid_cursor = saved_cursor > 0 && saved_cursor <= current_cursor;
        let since = if valid_cursor {
            saved_cursor
        } else {
            current_cursor
        };
        let (tx, rx) = mpsc::channel();
        {
            let mut state = self.state.locked();
            if state.generation != generation {
                return Ok(());
            }
            state.wake = Some(tx.clone());
        }
        // Subscribe before the initial scan/reconciliation, so changes made
        // during it remain queued and cannot slip between reading and watching.
        // Only the indexed folders: builds and other activity elsewhere on the
        // disk then cannot make fseventsd drop this stream's events. A root
        // missing now is picked up when the app next starts.
        let watched: Vec<&[u8]> = self
            .roots
            .iter()
            .filter(|root| root.exists())
            .map(|root| root.as_os_str().as_bytes())
            .collect();
        let _stream = fsevents::watch_paths(&watched, since, 0.2, tx, generation as u32);
        if !cached {
            self.build(generation, since)?;
        }
        if !self.current(generation) {
            return Ok(());
        }
        let mut dirty = cached && !valid_cursor;
        if dirty {
            // Upgrade old generated snapshots which predate saved event cursors
            // without discarding the searchable index or doing a new tree crawl.
            self.recover_gap(generation, &self.roots)?;
            self.live()?.writing().event_id = since;
        }
        let mut last_save = Instant::now();
        while self.current(generation) {
            let mut batch = Vec::new();
            // Sleep until FSEvents reports a change, cancellation wakes the
            // thread, or a pending checkpoint falls due. No periodic polling.
            let received = if dirty {
                rx.recv_timeout(CHECKPOINT_EVERY.saturating_sub(last_save.elapsed()))
            } else {
                rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected)
            };
            match received {
                Ok(events) => batch.extend(events),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            for events in rx.try_iter().take(127) {
                batch.extend(events);
            }
            if !batch.is_empty() {
                let mut targets = HashMap::new();
                let mut cursor = 0;
                // Indexed folders whose events were lost and must be rechecked.
                let mut gap: Vec<PathBuf> = Vec::new();
                for event in batch {
                    cursor = cursor.max(event.id);
                    let path = PathBuf::from(std::ffi::OsString::from_vec(event.path));
                    if event.flags & (fsevents::USER_DROPPED | fsevents::KERNEL_DROPPED) != 0
                        || path == Path::new("/") && event.flags & fsevents::MUST_SCAN_SUBDIRS != 0
                    {
                        for root in self.roots.iter() {
                            if (root.starts_with(&path) || path.starts_with(root))
                                && !gap.contains(root)
                            {
                                gap.push(root.clone());
                            }
                        }
                    }
                    if event.flags & fsevents::HISTORY_DONE != 0 || inside_pruned(&path) {
                        continue;
                    }
                    for (path, recursive) in affected_paths(
                        &self.roots,
                        &path,
                        event.flags & fsevents::MUST_SCAN_SUBDIRS != 0,
                    ) {
                        *targets.entry(path).or_insert(false) |= recursive;
                    }
                }
                if !gap.is_empty() {
                    let boundary = fsevents::current_id();
                    self.recover_gap(generation, &gap)?;
                    cursor = cursor.max(boundary);
                    dirty = true;
                }
                if !targets.is_empty() {
                    self.apply_changes(generation, targets.into_iter().collect())?;
                    dirty = true;
                }
                if self.current(generation) {
                    let live = self.live()?;
                    let mut live = live.writing();
                    live.event_id = live.event_id.max(cursor);
                }
            }
            // The old mmap stays searchable while saving.
            if dirty
                && (last_save.elapsed() >= CHECKPOINT_EVERY || {
                    let live = self.live()?;
                    let live = live.reading();
                    live.over.len() + live.dead_count >= CHECKPOINT_PENDING
                })
            {
                match self.checkpoint(generation) {
                    Ok(()) => dirty = false,
                    // A transient save failure must not stop live searches or
                    // the filesystem watcher. Keep the overlay and retry later.
                    Err(error) => eprintln!("File index checkpoint: {error:#}"),
                }
                last_save = Instant::now();
            }
        }
        Ok(())
    }

    fn apply_changes(&self, generation: u64, paths: Vec<(PathBuf, bool)>) -> Result<()> {
        for (mut path, mut recursive) in paths {
            if !self.current(generation) {
                return Ok(());
            }
            if !self.roots.iter().any(|root| path.starts_with(root)) {
                continue;
            }
            let live = self.live()?;
            // Slow filesystem reads use a shared lock, so searches can continue.
            let fetched = {
                let live = live.reading();
                // An event for a newly created nested directory can arrive
                // before its parent. Refresh the nearest indexed ancestor so
                // every new subtree stays attached in subsequent snapshots.
                while !known_path(&live, &path) {
                    if self.roots.contains(&path) {
                        recursive = true;
                        break;
                    }
                    if !path.pop() {
                        break;
                    }
                }
                live.fetch(path.as_os_str().as_bytes(), recursive)
            };
            if !self.current(generation) {
                return Ok(());
            }
            let mut live = live.writing();
            live.apply(fetched);
            live.trees.clear(); // no content index consumes these invalidations
        }
        let mut state = self.lock();
        if state.generation == generation {
            state.progress.revision += 1;
        }
        Ok(())
    }

    /// Recovers lost FSEvents history for the indexed folders in `scope`.
    /// Adding, removing, or renaming an entry updates its folder's mtime, so
    /// only folders are checked, in parallel, and only those changed since the
    /// last save are relisted. Files' own sizes and dates are re-read whenever
    /// they are shown, so they need no check here.
    fn recover_gap(&self, generation: u64, scope: &[PathBuf]) -> Result<()> {
        let changed = {
            let live = self.live()?;
            let live = live.reading();
            let since = live.synced_at.saturating_sub(120);
            live.changed_dirs_within(since, |path| {
                let path = Path::new(std::ffi::OsStr::from_bytes(path));
                scope.iter().any(|root| path.starts_with(root))
            })
        };
        if !self.current(generation) {
            return Ok(());
        }
        let mut paths: Vec<_> = changed
            .into_iter()
            .map(|path| (PathBuf::from(std::ffi::OsString::from_vec(path)), false))
            .collect();
        paths.extend(scope.iter().map(|root| (root.clone(), !root.exists())));
        self.apply_changes(generation, paths)
    }

    fn checkpoint(&self, generation: u64) -> Result<()> {
        if !self.current(generation) {
            return Ok(());
        }
        let live = self.live()?;
        let (listings, cursor) = {
            let live = live.reading();
            (live.to_listings(), live.event_id)
        };
        let built = Index::build(
            listings,
            cursor,
            fsearch::query::now_secs(),
            self.home.as_os_str().as_bytes(),
        );
        self.publish(built, generation)
    }

    pub(super) fn clear_and_rebuild(&self) -> Result<()> {
        // Serialize removal with the final index commit. An old worker cannot
        // restore a cleared cache, even if it was already building its mmap.
        {
            let mut state = self.lock();
            state.advance();
            state.index = None;
            state.progress.ready = false;
            state.progress.percent = 0.;
            state.progress.revision += 1;
            match fs::remove_file(&self.cache) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    state.progress.label = format!("Could not clear index: {error}");
                    return Err(error.into());
                }
            }
        }
        self.start()
    }

    fn current(&self, generation: u64) -> bool {
        self.state.locked().generation == generation
    }

    fn build(&self, generation: u64, event_id: u64) -> Result<()> {
        let listings = crawl(&self.roots, |completed, discovered, _| {
            let mut state = self.lock();
            if state.generation != generation {
                return false;
            }
            state.progress.percent = 95. * completed as f32 / discovered.max(1) as f32;
            state.progress.label = "Indexing".into();
            true
        });
        if !self.current(generation) {
            return Ok(());
        }
        {
            let mut state = self.lock();
            if state.generation != generation {
                return Ok(());
            }
            state.progress.percent = 95.;
            state.progress.label = "Finishing".into();
        }
        let built = Index::build(
            listings,
            event_id,
            fsearch::query::now_secs(),
            self.home.as_os_str().as_bytes(),
        );
        self.publish(built, generation)
    }

    fn publish(&self, built: Index, generation: u64) -> Result<()> {
        if !self.current(generation) {
            return Ok(());
        }
        fs::create_dir_all(self.cache.parent().context("Index cache has no parent")?)?;
        let staging = self
            .cache
            .with_file_name(format!("index-{}-{generation}.bin", std::process::id()));
        let _cleanup = StagedFile(staging.clone());
        built.save(&staging).context("Could not save index")?;
        let mut state = self.lock();
        if state.generation != generation {
            let _ = fs::remove_file(staging);
            return Ok(());
        }
        fs::rename(&staging, &self.cache).context("Could not publish index")?;
        let mapped = Index::load(&self.cache).context("Could not map saved index")?;
        state.index = Some(Arc::new(RwLock::new(Live::lazy(mapped))));
        state.progress = Progress {
            percent: 100.,
            label: "Indexed".into(),
            revision: state.progress.revision + 1,
            ready: true,
        };
        Ok(())
    }

    /// What the saved index occupies on disk, split by indexed folder: the
    /// largest few, biggest first, then everything else as "Other". Each
    /// folder's subtree is one contiguous entry range, so this reads a few
    /// lookups per folder, not every entry. A folder's bytes are its share of
    /// entries; names are shared between folders, so they are an estimate.
    pub(super) fn usage(&self) -> Option<IndexUsage> {
        const PARTS: usize = 5;
        let live = self.live().ok()?;
        let live = live.reading();
        let index = &live.base;
        let total = index.bytes() as u64;
        let entries = index.n.max(1) as u64;
        let mut parts: Vec<(String, u64)> = self
            .roots
            .iter()
            .filter_map(|root| {
                let dir = index.dir_of(index.lookup(root.as_os_str().as_bytes())?)?;
                let count = index.dir_end(dir as usize) - index.dir_start(dir as usize);
                let bytes = total * u64::from(count) / entries;
                (bytes > 0).then(|| (root_label(root, &self.home), bytes))
            })
            .collect();
        parts.sort_by(|a, b| b.1.cmp(&a.1));
        parts.truncate(PARTS);
        let shown: u64 = parts.iter().map(|(_, bytes)| bytes).sum();
        parts.push(("Other".into(), total.saturating_sub(shown)));
        Some(IndexUsage { total, parts })
    }

    /// Frees the scored name table and spare search buffers (tens of MB
    /// after a broad query) once no search has run for `idle`.
    pub(super) fn trim_if_idle(&self, idle: Duration) {
        if let Ok(live) = self.live() {
            live.reading().names_cache.trim_if_idle(idle);
        }
    }

    /// A panic inside the search engine becomes an error for this query
    /// rather than taking down the app.
    /// Searches the index, within `scope`'s subtree when given.
    pub(super) fn search(&self, text: &str, scope: Option<&Path>) -> Result<Vec<PathBuf>> {
        std::panic::catch_unwind(AssertUnwindSafe(|| self.search_unguarded(text, scope)))
            .unwrap_or_else(|_| Err(anyhow::anyhow!("Search failed unexpectedly")))
    }

    fn search_unguarded(&self, text: &str, scope: Option<&Path>) -> Result<Vec<PathBuf>> {
        let live = self.live()?;
        let live = live.reading();
        search_live(&live, &self.home, text, scope, &self.roots)
    }

    /// Whether the saved index holds `folder`'s contents: it lies in an
    /// indexed root and not inside a pruned folder.
    pub(super) fn covers(&self, folder: &Path) -> bool {
        self.roots.iter().any(|root| folder.starts_with(root)) && !inside_pruned(folder)
    }
}

/// Most entries an on-the-spot folder index takes, nearest levels first.
/// Larger trees, such as `/`, are indexed up to here and quietly no further.
const FOLDER_LIMIT: usize = 200_000;

/// A temporary in-memory index of one folder outside the indexed roots, so
/// search there ranks and tolerates typos like everywhere else. Built when
/// that folder is first searched, kept while it is being searched, never
/// saved, and following the same pruning rules as the saved index.
pub(super) struct FolderIndex {
    /// The folder as the tab names it.
    folder: PathBuf,
    /// Its real path: crawling does not follow symlinks such as `/tmp`.
    root: PathBuf,
    home: PathBuf,
    live: Live,
}

impl FolderIndex {
    /// Lists `folder` breadth-first; returns `None` once `cancelled` is set.
    pub(super) fn build(folder: &Path, home: &Path, cancelled: &AtomicBool) -> Option<Self> {
        fsearch::no_materialize();
        let root = fs::canonicalize(folder).unwrap_or_else(|_| folder.to_path_buf());
        let roots = [root.clone()];
        let listings = crawl(&roots, |_, _, entries| {
            entries < FOLDER_LIMIT && !cancelled.load(Ordering::Relaxed)
        });
        if cancelled.load(Ordering::Relaxed) {
            return None;
        }
        let index = Index::build(
            listings,
            0,
            fsearch::query::now_secs(),
            home.as_os_str().as_bytes(),
        );
        Some(Self {
            folder: folder.to_path_buf(),
            root,
            home: home.to_path_buf(),
            live: Live::new(index),
        })
    }

    pub(super) fn folder(&self) -> &Path {
        &self.folder
    }

    pub(super) fn search(&self, text: &str) -> Result<Vec<PathBuf>> {
        std::panic::catch_unwind(AssertUnwindSafe(|| {
            search_live(
                &self.live,
                &self.home,
                text,
                Some(&self.root),
                std::slice::from_ref(&self.root),
            )
        }))
        .unwrap_or_else(|_| Err(anyhow::anyhow!("Search failed unexpectedly")))
    }
}

/// Lists `roots` breadth-first into listings for `Index::build`, never
/// crossing mounts, following symlinks, or entering pruned folders. After each
/// folder, `step` sees the folders listed and discovered and the entries so
/// far; returning false stops the crawl, which keeps what it listed.
fn crawl(roots: &[PathBuf], mut step: impl FnMut(usize, usize, usize) -> bool) -> Vec<Listing> {
    let (mut listings, roots) = scaffold(roots);
    let mut queue: VecDeque<_> = roots.into();
    let mut discovered = queue.len();
    let mut completed = 0usize;
    let mut entries = 0usize;
    while let Some((id, path)) = queue.pop_front() {
        let mut listing = walk::list_one(path.as_os_str().as_bytes()).unwrap_or_default();
        listing.id = id;
        for entry in &mut listing.ents {
            // Never cross mounts or follow symlinks. The bulk enumerator
            // also declines dataless iCloud content on this worker thread.
            if entry.kind & 3 == KIND_DIR && entry.kind & FLAG_MOUNT == 0 {
                let name = &listing.names
                    [entry.name_off as usize..entry.name_off as usize + entry.name_len as usize];
                let child = path.join(std::ffi::OsStr::from_bytes(name));
                // Pruned folders are indexed as entries without contents.
                if walk::pruned(child.as_os_str().as_bytes()) {
                    entry.child = NONE;
                    continue;
                }
                entry.child = listings.len() as u32;
                listings.push(Listing {
                    id: entry.child,
                    ..Default::default()
                });
                queue.push_back((entry.child, child));
                discovered += 1;
            } else {
                entry.child = NONE;
            }
        }
        entries += listing.ents.len();
        listings[id as usize] = listing;
        completed += 1;
        if !step(completed, discovered, entries) {
            break;
        }
    }
    listings
}

/// Runs `text` against `live`, within `scope` when given, keeping results
/// under `within` (the scaffold above the roots holds other folders' names).
fn search_live(
    live: &Live,
    home: &Path,
    text: &str,
    scope: Option<&Path>,
    within: &[PathBuf],
) -> Result<Vec<PathBuf>> {
    let mut query = Query::parse(text, &home.to_string_lossy()).map_err(anyhow::Error::msg)?;
    anyhow::ensure!(
        query.grep.is_none(),
        "Search file names and paths; content search is not enabled"
    );
    query.limit = query.limit.clamp(1, 1000);
    // Plain searches default to a useful first page rather than only 50.
    if !text
        .split_whitespace()
        .any(|word| word.starts_with("limit:"))
    {
        query.limit = 1000;
    }
    if let Some(scope) = scope {
        query.scope = Some(scope.as_os_str().as_bytes().to_vec());
    }
    let hits = Searcher { live }.search(&query);
    let mut paths = Vec::with_capacity(hits.len());
    let mut bytes = Vec::new();
    for hit in hits {
        let path = match hit.over {
            Some(path) => PathBuf::from(std::ffi::OsString::from_vec(path)),
            None => {
                live.base.path(hit.idx as usize, &mut bytes);
                PathBuf::from(std::ffi::OsString::from_vec(bytes.clone()))
            }
        };
        if within.iter().any(|root| path.starts_with(root)) {
            paths.push(path);
        }
    }
    Ok(paths)
}

/// Whether `path` is a pruned folder or lies inside one, so its changes are
/// not indexed (see `walk::pruned`).
fn inside_pruned(path: &Path) -> bool {
    path.ancestors()
        .any(|ancestor| walk::pruned(ancestor.as_os_str().as_bytes()))
}

fn affected_paths(roots: &[PathBuf], path: &Path, recursive: bool) -> Vec<(PathBuf, bool)> {
    let mut result = Vec::new();
    for root in roots {
        if path.starts_with(root) {
            result.push((path.to_path_buf(), recursive));
            break;
        } else if root.parent() == Some(path) || recursive && root.starts_with(path) {
            // Never enumerate an ancestor (Home or /), which would widen scope.
            // Checking configured roots also handles their creation/removal.
            result.push((root.clone(), recursive || !root.exists()));
        }
    }
    result
}

fn known_path(live: &Live, path: &Path) -> bool {
    let bytes = path.as_os_str().as_bytes();
    live.over.contains_key(bytes)
        || live
            .base
            .lookup(bytes)
            .is_some_and(|entry| !live.is_dead(entry))
}

struct StagedFile(PathBuf);
impl Drop for StagedFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
        let _ = fs::remove_file(self.0.with_extension("tmp"));
    }
}

pub(super) fn common_roots(home: &Path) -> Vec<PathBuf> {
    [
        "Desktop",
        "Documents",
        "Downloads",
        "Pictures",
        "Music",
        "Movies",
        "Applications",
    ]
    .map(|name| home.join(name))
    .into_iter()
    .chain([PathBuf::from("/Applications")])
    .collect()
}

// Index::build expects directory id 0 to be /. Create just the ancestor
// listings needed to attach disjoint roots, without enumerating other folders.
fn scaffold(roots: &[PathBuf]) -> (Vec<Listing>, Vec<(u32, PathBuf)>) {
    let mut listings = vec![Listing::default()];
    let mut ids = HashMap::from([(PathBuf::from("/"), 0u32)]);
    let mut scan = Vec::new();
    for root in roots {
        let exists = root.is_dir();
        let mut parent = PathBuf::from("/");
        for component in root.components().filter_map(|component| match component {
            std::path::Component::Normal(name) => Some(name),
            _ => None,
        }) {
            let child = parent.join(component);
            // Keep the ancestors of absent roots so a later creation can be
            // attached without enumerating or indexing the rest of Home.
            if child == *root && !exists {
                break;
            }
            if !ids.contains_key(&child) {
                let id = listings.len() as u32;
                let listing = &mut listings[ids[&parent] as usize];
                let name = component.as_bytes();
                listing.ents.push(RawEnt {
                    name_off: listing.names.len() as u32,
                    name_len: name.len() as u16,
                    kind: KIND_DIR,
                    size: 0,
                    mtime: 0,
                    child: id,
                });
                listing.names.extend_from_slice(name);
                ids.insert(child.clone(), id);
                listings.push(Listing {
                    id,
                    ..Default::default()
                });
            }
            parent = child;
        }
        if exists {
            scan.push((ids[root], root.clone()));
        }
    }
    (listings, scan)
}
