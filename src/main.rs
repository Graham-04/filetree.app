use std::{
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, atomic::AtomicBool},
    time::{Duration, Instant},
};

use futures::StreamExt as _;
use gpui::{
    App, Application, Bounds, Context, Entity, ExternalPaths, FocusHandle, Focusable, IntoElement,
    KeyBinding, MouseButton, PromptLevel, Render, ScrollHandle, SharedString, TitlebarOptions,
    Window, WindowBounds, WindowOptions, actions, div, prelude::*, px, rgb, size,
};

mod drag_out;
mod file_list;
mod file_ops;
mod folder_cache;
mod history;
mod native_icons;
mod path_completion;
mod quick_look;
use file_list::{FileEntry, FileList};
mod search_index;
mod search_input;
mod settings;
mod sidebar;
mod storage;
mod system;
mod ui;
mod undo;
mod updater;
mod watcher;
mod window_chrome;

use ui::{dp, radius, row, text_size};

/// Titlebar search widths in unzoomed points.
/// Returns large blocks straight to the system. With macOS malloc, the
/// transient memory of an index save stayed mapped and dirty afterwards.
#[global_allocator]
static GLOBAL: fsearch::alloc::Alloc = fsearch::alloc::Alloc;

const SEARCH_CLOSED: f32 = 32.;
const SEARCH_OPEN: f32 = 260.;

actions!(
    filetree,
    [
        NewTab,
        CloseTab,
        ToggleSidebar,
        ToggleFavorites,
        ToggleRecent,
        ToggleLocations,
        ZoomIn,
        ZoomOut,
        ResetZoom,
        ToggleCommandMenu,
        ToggleSettings,
        ToggleHiddenFiles,
        ToggleSounds,
        CheckForUpdates,
        ArrowKeysHistory,
        ArrowKeysFolderPath,
        GoLeft,
        GoRight,
        EditPath,
        PathEditGo,
        PathEditCancel,
        PathEditPrevious,
        PathEditNext,
        PathEditComplete,
        ToggleSearchScope,
        RunMenuCommand,
        OpenSearch,
        CloseSearch,
        ClearFileIndex,
        PreviousCommand,
        NextCommand,
        CompleteMenuPath,
        ContextMenuPrevious,
        ContextMenuNext,
        ContextMenuRun,
        CloseContextMenu,
        MenuOpen,
        MenuQuickLook,
        MenuCopyPath,
        MenuTrash,
        MenuNewFolder,
        MenuRename,
        MenuDuplicate,
        MenuCopy,
        MenuPaste,
        CopyFiles,
        PasteFiles,
        PasteMoveFiles,
        DuplicateFiles,
        Undo,
        Redo,
        GoHome,
        GoDesktop,
        GoDocuments,
        GoDownloads,
        GoApplications,
        GoPictures,
        GoMusic,
        GoMovies,
        GoBack,
        GoForward,
        GoUp,
        SelectTab1,
        SelectTab2,
        SelectTab3,
        SelectTab4,
        SelectTab5,
        SelectTab6,
        SelectTab7,
        SelectTab8,
        SelectLastTab,
        Quit
    ]
);

struct BrowserTab {
    id: u64,
    location: PathBuf,
    query: String,
    request: u64,
    search_revision: u64,
    loading: bool,
    list: Entity<FileList>,
    history: history::History,
    live: LiveRefresh,
}

/// Re-reading a tab's folder after another app changed it.
#[derive(Default)]
struct LiveRefresh {
    /// Changed while in a background tab; re-read when it is shown.
    stale: bool,
    /// When the last live re-read started, to space them out.
    last: Option<Instant>,
    /// A re-read waiting out that spacing.
    waiting: Option<gpui::Task<()>>,
}

/// Live re-reads of one folder happen at most this often.
const LIVE_SPACING: Duration = Duration::from_millis(500);

#[derive(Clone, Copy)]
enum Command {
    Settings,
    HiddenFiles,
    Sounds,
    CheckForUpdates,
    ArrowsHistory,
    ArrowsFolderPath,
    ResetZoom,
    ClearIndex,
}
impl Command {
    fn label(self) -> &'static str {
        match self {
            Self::Settings => "Settings",
            Self::HiddenFiles => "Show Hidden Files",
            Self::Sounds => "Play Sounds",
            Self::CheckForUpdates => "Check for Updates",
            Self::ArrowsHistory => "⌘← ⌘→ Go Back and Forward",
            Self::ArrowsFolderPath => "⌘← ⌘→ Move Along the Folder Path",
            Self::ResetZoom => "Reset to Default Zoom",
            Self::ClearIndex => "Clear File Index",
        }
    }
    fn shortcut(self) -> Option<&'static str> {
        match self {
            Self::Settings => Some("⌘,"),
            Self::HiddenFiles => Some("⌘⇧."),
            Self::Sounds | Self::CheckForUpdates => None,
            Self::ArrowsHistory | Self::ArrowsFolderPath => None,
            Self::ResetZoom => Some("⌘0"),
            Self::ClearIndex => None,
        }
    }
    fn icon(self) -> &'static str {
        match self {
            Self::Settings => "gearshape",
            Self::HiddenFiles => "eye",
            Self::Sounds => "speaker.wave.2",
            Self::CheckForUpdates => "arrow.down.circle",
            Self::ArrowsHistory => "clock.arrow.circlepath",
            Self::ArrowsFolderPath => "arrow.up.and.down.text.horizontal",
            Self::ResetZoom => "arrow.counterclockwise",
            Self::ClearIndex => "trash",
        }
    }
    fn action(self) -> Box<dyn gpui::Action> {
        match self {
            Self::Settings => Box::new(ToggleSettings),
            Self::HiddenFiles => Box::new(ToggleHiddenFiles),
            Self::Sounds => Box::new(ToggleSounds),
            Self::CheckForUpdates => Box::new(CheckForUpdates),
            Self::ArrowsHistory => Box::new(ArrowKeysHistory),
            Self::ArrowsFolderPath => Box::new(ArrowKeysFolderPath),
            Self::ResetZoom => Box::new(ResetZoom),
            Self::ClearIndex => Box::new(ClearFileIndex),
        }
    }
}
#[derive(Clone)]
enum MenuItem {
    Command(Command),
    Path(path_completion::Completion),
}

/// The commands matching `query`; Check for Updates only in builds that
/// update themselves.
fn matching_commands(query: &str, updates: bool) -> Vec<Command> {
    [
        Command::Settings,
        Command::HiddenFiles,
        Command::Sounds,
        Command::CheckForUpdates,
        Command::ArrowsHistory,
        Command::ArrowsFolderPath,
        Command::ResetZoom,
        Command::ClearIndex,
    ]
    .into_iter()
    .filter(|command| updates || !matches!(command, Command::CheckForUpdates))
    .filter(|command| {
        let haystack = match command {
            Command::Settings => "settings preferences storage file index size disk",
            Command::HiddenFiles => "show hidden files dotfiles dot files invisible toggle",
            Command::Sounds => "play sounds sound effects audio mute volume toggle",
            Command::CheckForUpdates => "check for updates update upgrade new version release",
            Command::ArrowsHistory => "arrow keys left right back forward history navigation",
            Command::ArrowsFolderPath => {
                "arrow keys left right folder path parent up down navigation"
            }
            Command::ResetZoom => "reset to default zoom appearance 100%",
            Command::ClearIndex => "clear file index fsearch rebuild cache search",
        };
        query
            .split_whitespace()
            .all(|word| haystack.contains(&word.to_lowercase()))
    })
    .collect()
}

struct FileBrowser {
    home: PathBuf,
    index: search_index::SearchIndex,
    index_progress: search_index::Progress,
    _index_watch: gpui::Task<()>,
    /// Mounted volumes for Locations, with icons, kept current by watching
    /// /Volumes.
    volumes: Vec<(system::Volume, native_icons::FileIcon)>,
    watcher: watcher::FolderWatcher,
    _folder_watch: gpui::Task<()>,
    /// Releases search buffers a minute after the last search; replaced, and
    /// so restarted, by each search.
    search_trim: Option<gpui::Task<()>>,
    next_tab_id: u64,
    selected_command: usize,
    /// The one copy of the user's settings; the menu and panel read it here.
    settings: storage::UserSettings,
    settings_path: PathBuf,
    session_path: PathBuf,
    /// Recently opened files and folders, newest first, with their icons.
    recent: Vec<(PathBuf, native_icons::FileIcon)>,
    /// The window's latest frame, kept for saving at quit.
    window_frame: Option<storage::WindowFrame>,
    _session_hooks: Vec<gpui::Subscription>,
    /// The on-the-spot index of an unindexed folder being searched.
    folder_index: Option<Arc<search_index::FolderIndex>>,
    folder_build: Option<FolderBuild>,
    /// Recent items as ⌘K rows, captured when the menu opens.
    menu_recent: Vec<path_completion::Completion>,
    folders: folder_cache::FolderCache<FileEntry>,
    /// The folder listed for ⌘K path completion, and its children.
    path_children: Option<(PathBuf, Rc<[path_completion::Child]>)>,
    path_read: Option<(PathBuf, gpui::Task<()>)>,
    sidebar: sidebar::Sidebar,
    zoom: ui::Zoom,
    tabs: Vec<BrowserTab>,
    active_tab: usize,
    tab_scroll: ScrollHandle,
    focus: FocusHandle,
    command_menu_open: bool,
    context_menu: Option<ContextMenu>,
    /// A newer release on the titlebar's Update button, and its install.
    update: Option<UpdateState>,
    /// Whether this is an official build that updates itself.
    updates_supported: bool,
    _update_checks: Option<gpui::Task<()>>,
    _update_check_now: Option<gpui::Task<()>>,
    _update_install: Option<gpui::Task<()>>,
    /// The path bar's text field, shown while typing a path.
    path_input: Entity<search_input::SearchInput>,
    path_edit: Option<PathEdit>,
    /// Undo and redo for file changes, for the session.
    undo: undo::UndoHistory,
    /// An undo or redo is running; the next waits for it.
    reverting: bool,
    settings_open: bool,
    /// Measured when settings opens and refreshed while it stays open.
    index_usage: Option<search_index::IndexUsage>,
    command_search: Entity<search_input::SearchInput>,
    search: ui::Reveal,
    file_search: Entity<search_input::SearchInput>,
}

impl FileBrowser {
    fn new(
        home: PathBuf,
        session: storage::Session,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Saved tabs whose folders are gone are dropped; with none left, the
        // window opens on Desktop.
        let mut saved_tabs: Vec<PathBuf> = session
            .tabs
            .iter()
            .filter(|location| location.is_dir())
            .cloned()
            .collect();
        let first_location = if saved_tabs.is_empty() {
            home.join("Desktop")
        } else {
            saved_tabs.remove(0)
        };
        let command_search = cx.new(|cx| search_input::SearchInput::new("Search commands…", cx));
        let file_search = cx.new(|cx| search_input::SearchInput::new("Search", cx));
        let path_input = cx.new(|cx| search_input::SearchInput::new("Go to folder", cx));
        cx.subscribe(&path_input, |this, _, _: &search_input::Changed, cx| {
            if let Some(edit) = &mut this.path_edit {
                edit.typed = true;
                edit.selected = None;
                let query = this.path_input.read(cx).text().to_owned();
                this.read_children_for(&query, cx);
                cx.notify();
            }
        })
        .detach();
        let file_search_focus = file_search.read(cx).focus_handle(cx);
        cx.on_blur(&file_search_focus, window, |this, _, cx| {
            if this.file_search.read(cx).text().is_empty() {
                this.search.set(false, Instant::now());
                cx.notify();
            }
        })
        .detach();
        cx.subscribe(&command_search, |this, _, _: &search_input::Changed, cx| {
            this.selected_command = 0;
            let query = this.command_search.read(cx).text().to_owned();
            this.read_children_for(&query, cx);
            cx.notify();
        })
        .detach();
        cx.subscribe(&file_search, |this, _, _: &search_input::Changed, cx| {
            let query = this.file_search.read(cx).text().to_owned();
            if query != this.tabs[this.active_tab].query {
                this.tabs[this.active_tab].query = query;
                this.refresh_tab(this.active_tab, cx);
            }
        })
        .detach();
        let list = file_list_view(Rc::from([]), window, cx);
        let index = search_index::SearchIndex::new(home.clone());
        // Subscribe before starting, so no early progress change is missed.
        let mut changes = index.changes();
        if let Err(error) = index.start() {
            eprintln!("Index startup: {error:#}");
        }
        let progress = index.progress();
        // Live changes in shown folders, and in /Volumes for mounts.
        let (watcher, mut folders_changed) = watcher::FolderWatcher::new();
        let folder_watch = cx.spawn(async move |this, cx| {
            while folders_changed.next().await.is_some() {
                if this
                    .update(cx, |this, cx| this.folders_changed(cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        let watch = cx.spawn(async move |this, cx| {
            while changes.next().await.is_some() {
                let updated = this.update(cx, |this, cx| {
                    let progress = this.index.progress();
                    if progress != this.index_progress {
                        this.index_progress = progress;
                        if this.settings_open {
                            this.index_usage = this.index.usage();
                        }
                        cx.notify();
                    }
                    this.refresh_stale_searches(cx);
                });
                if updated.is_err() {
                    break;
                }
            }
        });
        let mut browser = Self {
            command_search,
            home: home.clone(),
            index,
            index_progress: progress,
            _index_watch: watch,
            volumes: listed_volumes(),
            watcher,
            _folder_watch: folder_watch,
            search_trim: None,
            next_tab_id: 1,
            selected_command: 0,
            settings: storage::load(&storage::settings_path(&home)),
            settings_path: storage::settings_path(&home),
            session_path: storage::session_path(&home),
            recent: classify_recent(session.recent),
            window_frame: session.window,
            _session_hooks: Vec::new(),
            folder_index: None,
            folder_build: None,
            menu_recent: Vec::new(),
            folders: folder_cache::FolderCache::new(),
            path_children: None,
            path_read: None,
            sidebar: sidebar::Sidebar::new(session.sidebar),
            zoom: ui::Zoom::restore(session.zoom),
            tabs: vec![BrowserTab {
                id: 0,
                location: first_location,
                query: String::new(),
                request: 0,
                search_revision: 0,
                loading: false,
                list,
                history: Default::default(),
                live: LiveRefresh::default(),
            }],
            active_tab: 0,
            tab_scroll: ScrollHandle::new(),
            focus: cx.focus_handle(),
            command_menu_open: false,
            context_menu: None,
            update: None,
            updates_supported: false,
            _update_checks: None,
            _update_check_now: None,
            _update_install: None,
            path_input,
            path_edit: None,
            undo: Default::default(),
            reverting: false,
            settings_open: false,
            index_usage: None,
            search: ui::Reveal::new(false),
            file_search,
        };
        browser.refresh_tab(0, cx);
        browser.sync_watched();
        browser.watch_for_updates(cx);
        for location in saved_tabs {
            let list = file_list_view(Rc::from([]), window, cx);
            browser.tabs.push(BrowserTab {
                id: browser.next_tab_id,
                location,
                query: String::new(),
                request: 0,
                search_revision: 0,
                loading: false,
                list,
                history: Default::default(),
                live: LiveRefresh::default(),
            });
            browser.next_tab_id += 1;
            browser.refresh_tab(browser.tabs.len() - 1, cx);
        }
        browser.active_tab = session.active_tab.min(browser.tabs.len() - 1);
        browser._session_hooks = vec![
            cx.observe_window_bounds(window, |this, window, _| {
                this.window_frame = window_frame(window);
            }),
            cx.on_app_quit(|this, _| {
                this.save_session();
                async {}
            }),
        ];
        browser.window_frame = window_frame(window).or(browser.window_frame);
        // Closing the window frees the browser before the app quits, so save
        // here too; quitting with ⌘Q is covered by `on_app_quit`.
        let this = cx.entity().downgrade();
        window.on_window_should_close(cx, move |_, cx| {
            let _ = this.update(cx, |this, _| this.save_session());
            true
        });
        browser
    }

    /// Saves the window, tabs, zoom, sidebar, and recent items for next launch.
    fn save_session(&self) {
        let session = storage::Session {
            window: self.window_frame,
            tabs: self.tabs.iter().map(|tab| tab.location.clone()).collect(),
            active_tab: self.active_tab,
            zoom: self.zoom.saved(),
            sidebar: self.sidebar.state(),
            recent: self.recent.iter().map(|(path, _)| path.clone()).collect(),
        };
        if let Err(error) = storage::save(&self.session_path, &session) {
            eprintln!("Could not save session: {error:#}");
        }
    }

    /// Records explicitly opened items as recent. Saved at once, so recent
    /// items survive even if the app does not quit normally.
    fn remember_recent(&mut self, paths: &[PathBuf]) {
        let mut recent: Vec<PathBuf> = self.recent.iter().map(|(path, _)| path.clone()).collect();
        for path in paths.iter().rev() {
            storage::Session::remember(&mut recent, path.clone());
        }
        self.recent = recent
            .into_iter()
            .map(|path| {
                let icon = self
                    .recent
                    .iter()
                    .find(|(known, _)| *known == path)
                    .map(|(_, icon)| icon.clone())
                    .unwrap_or_else(|| classify(&path));
                (path, icon)
            })
            .collect();
        self.save_session();
    }

    /// Opens a recent item, or drops it if it no longer exists.
    fn open_recent(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if path.exists() {
            // Opening from Recent keeps the list's order, so the item does not
            // jump away from under the pointer.
            self.open_without_remembering(&[path], window, cx);
        } else {
            self.recent.retain(|(item, _)| *item != path);
            self.save_session();
            cx.notify();
        }
    }

    fn refresh_tab(&mut self, tab_index: usize, cx: &mut Context<Self>) {
        self.refresh_tab_inner(tab_index, false, cx);
    }

    /// The rows to show for a listing under the current hidden-files setting.
    fn visible(&self, entries: Rc<[FileEntry]>, in_search: bool) -> Rc<[FileEntry]> {
        if self.settings.show_hidden_files {
            entries
        } else {
            file_list::without_hidden(entries, in_search)
        }
    }

    /// Flips hidden-file visibility, saves it, and updates every tab in place,
    /// keeping scroll and selection. The command menu stays open so its
    /// checkbox shows the new state.
    /// Plays a file-action sound unless the user muted them.
    fn play(&self, sound: ui::Sound) {
        if !self.settings.mute_sounds {
            ui::play(sound);
        }
    }

    /// Turns the app's sounds on or off, from ⌘K, Settings, or the sidebar.
    fn toggle_sounds(&mut self, _: &ToggleSounds, _: &mut Window, cx: &mut Context<Self>) {
        self.settings.mute_sounds = !self.settings.mute_sounds;
        if let Err(error) = storage::save(&self.settings_path, &self.settings) {
            eprintln!("Could not save settings: {error:#}");
        }
        cx.notify();
    }

    fn toggle_hidden_files(
        &mut self,
        _: &ToggleHiddenFiles,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings.show_hidden_files = !self.settings.show_hidden_files;
        if let Err(error) = storage::save(&self.settings_path, &self.settings) {
            eprintln!("Could not save settings: {error:#}");
        }
        for tab in 0..self.tabs.len() {
            self.refresh_tab_inner(tab, true, cx);
        }
        cx.notify();
    }

    /// Where a search from `location` runs: the saved index, limited to the
    /// folder unless searching every indexed folder, or an on-the-spot index
    /// of a folder the saved index does not cover. `None` while that one is
    /// still being built.
    fn search_source(&mut self, location: &Path, cx: &mut Context<Self>) -> Option<SearchSource> {
        if self.settings.search_scope == storage::SearchScope::Indexed {
            return Some(SearchSource::Index(self.index.clone(), None));
        }
        if self.index.covers(location) {
            return Some(SearchSource::Index(
                self.index.clone(),
                Some(location.to_path_buf()),
            ));
        }
        if let Some(index) = &self.folder_index
            && index.folder() == location
        {
            return Some(SearchSource::Folder(index.clone()));
        }
        if self
            .folder_build
            .as_ref()
            .is_none_or(|build| build.folder != location)
        {
            self.folder_build = Some(self.build_folder_index(location.to_path_buf(), cx));
        }
        None
    }

    fn build_folder_index(&mut self, folder: PathBuf, cx: &mut Context<Self>) -> FolderBuild {
        let cancelled = Arc::new(AtomicBool::new(false));
        let (flag, home, path) = (cancelled.clone(), self.home.clone(), folder.clone());
        let worker = cx.background_executor().clone();
        let task = cx.spawn(async move |this, cx| {
            let built = worker
                .spawn(async move { search_index::FolderIndex::build(&path, &home, &flag) })
                .await;
            let _ = this.update(cx, |this, cx| {
                let Some(built) = built else {
                    return;
                };
                let folder = built.folder().to_path_buf();
                this.folder_build = None;
                this.folder_index = Some(Arc::new(built));
                for tab in 0..this.tabs.len() {
                    if !this.tabs[tab].query.trim().is_empty() && this.tabs[tab].location == folder
                    {
                        this.refresh_tab(tab, cx);
                    }
                }
            });
        });
        FolderBuild {
            folder,
            cancelled,
            _task: task,
        }
    }

    /// Drops the on-the-spot index, or stops building it, once no tab is
    /// searching that folder.
    fn release_folder_index(&mut self) {
        let in_use = |folder: &Path| {
            self.settings.search_scope == storage::SearchScope::CurrentFolder
                && self
                    .tabs
                    .iter()
                    .any(|tab| !tab.query.trim().is_empty() && tab.location == folder)
        };
        if self
            .folder_index
            .as_ref()
            .is_some_and(|index| !in_use(index.folder()))
        {
            self.folder_index = None;
        }
        if self
            .folder_build
            .as_ref()
            .is_some_and(|build| !in_use(&build.folder))
        {
            self.folder_build = None;
        }
    }

    /// Switches between searching the current folder and every indexed folder.
    fn toggle_search_scope(
        &mut self,
        _: &ToggleSearchScope,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings.search_scope = match self.settings.search_scope {
            storage::SearchScope::CurrentFolder => storage::SearchScope::Indexed,
            storage::SearchScope::Indexed => storage::SearchScope::CurrentFolder,
        };
        if let Err(error) = storage::save(&self.settings_path, &self.settings) {
            eprintln!("Could not save settings: {error:#}");
        }
        for tab in 0..self.tabs.len() {
            if !self.tabs[tab].query.trim().is_empty() {
                self.refresh_tab(tab, cx);
            }
        }
        cx.notify();
    }

    /// Looks for a newer release at launch and every few hours after, in
    /// release builds running from an app bundle. A debug build with
    /// `FILETREE_PREVIEW_UPDATE` set shows a pretend update instead, whose
    /// download only counts up, to preview the button.
    fn watch_for_updates(&mut self, cx: &mut Context<Self>) {
        if cfg!(debug_assertions) && std::env::var_os("FILETREE_PREVIEW_UPDATE").is_some() {
            self.update = Some(UpdateState::Available(updater::Release::preview()));
            return;
        }
        let worker = cx.background_executor().clone();
        self._update_checks = Some(cx.spawn(async move |this, cx| {
            // Only official builds, signed by the release team, update.
            if !worker.spawn(async { updater::supported() }).await {
                return;
            }
            if this
                .update(cx, |this, _| this.updates_supported = true)
                .is_err()
            {
                return;
            }
            loop {
                match worker.spawn(async { updater::check() }).await {
                    Ok(release) => {
                        let offered = this.update(cx, |this, cx| {
                            // A download or a check in progress keeps its state.
                            if matches!(this.update, None | Some(UpdateState::Available(_))) {
                                this.update = release.map(UpdateState::Available);
                                cx.notify();
                            }
                        });
                        if offered.is_err() {
                            break;
                        }
                    }
                    Err(error) => eprintln!("Update check failed: {error:#}"),
                }
                worker.timer(updater::CHECK_EVERY).await;
            }
        }));
    }

    /// Looks for a new release now, as asked from the command menu: a
    /// spinner in the Update button's place while it looks, then the button
    /// if there is a newer release, or an alert saying there is none.
    fn check_for_updates(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.updates_supported || self.update.is_some() {
            return;
        }
        self.update = Some(UpdateState::Checking(Instant::now()));
        cx.notify();
        let worker = cx.background_executor().clone();
        self._update_check_now = Some(cx.spawn_in(window, async move |this, cx| {
            let result = worker.spawn(async { updater::check() }).await;
            let _ = this.update_in(cx, |this, window, cx| {
                if !matches!(this.update, Some(UpdateState::Checking(_))) {
                    return;
                }
                this.update = None;
                cx.notify();
                let (level, message, detail) = match result {
                    Ok(Some(release)) => {
                        this.update = Some(UpdateState::Available(release));
                        return;
                    }
                    Ok(None) => (
                        PromptLevel::Info,
                        "You’re up to date",
                        concat!(
                            "Filetree ",
                            env!("CARGO_PKG_VERSION"),
                            " is the latest version."
                        ),
                    ),
                    Err(error) => {
                        eprintln!("Update check failed: {error:#}");
                        (
                            PromptLevel::Warning,
                            "Couldn’t check for updates",
                            "Check your internet connection and try again.",
                        )
                    }
                };
                // Only OK to press; nothing waits on the answer.
                drop(window.prompt(level, message, Some(detail), &["OK"], cx));
            });
        }));
    }

    /// Downloads and installs the offered update, showing progress in the
    /// button, then restarts into it. A failure leaves the app as it was and
    /// offers the update again.
    fn start_update(&mut self, cx: &mut Context<Self>) {
        let Some(UpdateState::Available(release)) = self.update.clone() else {
            return;
        };
        self.update = Some(UpdateState::Downloading(0.));
        cx.notify();
        let (report, mut reported) = futures::channel::mpsc::unbounded::<f32>();
        let worker = cx.background_executor().clone();
        let preview = release.is_preview();
        let retry = release.clone();
        let installing = worker.spawn(async move {
            if preview {
                for step in 1..=30 {
                    std::thread::sleep(Duration::from_millis(100));
                    let _ = report.unbounded_send(step as f32 / 30.);
                }
                return Ok(());
            }
            updater::install(&release, |fraction| {
                let _ = report.unbounded_send(fraction);
            })
        });
        self._update_install = Some(cx.spawn(async move |this, cx| {
            // Progress ends when the install drops its sender.
            while let Some(fraction) = reported.next().await {
                let _ = this.update(cx, |this, cx| {
                    this.update = Some(if fraction >= 1. {
                        UpdateState::Installing
                    } else {
                        UpdateState::Downloading(fraction)
                    });
                    cx.notify();
                });
            }
            let result = installing.await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(()) if preview => {
                    this.update = None;
                    cx.notify();
                }
                // Quitting saves the session, which the new build restores.
                Ok(()) => match updater::relaunch_after_exit() {
                    Ok(()) => cx.quit(),
                    Err(error) => {
                        eprintln!("Could not restart: {error:#}");
                        this.update = None;
                        cx.notify();
                    }
                },
                Err(error) => {
                    eprintln!("Update failed: {error:#}");
                    ui::beep();
                    this.update = Some(UpdateState::Available(retry));
                    cx.notify();
                }
            });
        }));
    }

    /// Watches exactly the folders tabs show, and /Volumes for mounts.
    fn sync_watched(&mut self) {
        let folders: Vec<PathBuf> = self
            .tabs
            .iter()
            .filter(|tab| tab.query.trim().is_empty())
            .map(|tab| tab.location.clone())
            .chain([PathBuf::from("/Volumes")])
            .collect();
        self.watcher.watch(folders);
    }

    /// Another app changed shown folders: re-read the visible tab in place,
    /// and mark background tabs to re-read when shown.
    fn folders_changed(&mut self, cx: &mut Context<Self>) {
        let changes = self.watcher.take();
        if changes.everything || changes.folders.contains(Path::new("/Volumes")) {
            self.volumes = listed_volumes();
            cx.notify();
        }
        for index in 0..self.tabs.len() {
            let tab = &self.tabs[index];
            let changed = tab.query.trim().is_empty()
                && (changes.everything || changes.folders.contains(&tab.location));
            if !changed {
                continue;
            }
            self.folders.remove(&tab.location.clone());
            if index == self.active_tab {
                self.live_refresh(index, cx);
            } else {
                self.tabs[index].live.stale = true;
            }
        }
    }

    /// Re-reads a tab's folder in place, at most once per `LIVE_SPACING`;
    /// changes arriving sooner wait for one more re-read.
    fn live_refresh(&mut self, index: usize, cx: &mut Context<Self>) {
        let tab = &mut self.tabs[index];
        if tab.live.waiting.is_some() {
            return;
        }
        let since = tab.live.last.map_or(LIVE_SPACING, |last| last.elapsed());
        if since >= LIVE_SPACING {
            tab.live.last = Some(Instant::now());
            self.refresh_tab_inner(index, true, cx);
            return;
        }
        let id = tab.id;
        let worker = cx.background_executor().clone();
        tab.live.waiting = Some(cx.spawn(async move |this, cx| {
            worker.timer(LIVE_SPACING - since).await;
            let _ = this.update(cx, |this, cx| {
                let Some(index) = this.tabs.iter().position(|tab| tab.id == id) else {
                    return;
                };
                let tab = &mut this.tabs[index];
                tab.live.waiting = None;
                tab.live.last = Some(Instant::now());
                if tab.query.trim().is_empty() {
                    this.folders.remove(&tab.location.clone());
                    this.refresh_tab_inner(index, true, cx);
                }
            });
        }));
    }

    /// Re-runs searches made against an older index snapshot. A search still
    /// running is left alone and caught up when it finishes, so a busy folder
    /// cannot keep cancelling its own results.
    fn refresh_stale_searches(&mut self, cx: &mut Context<Self>) {
        for tab in 0..self.tabs.len() {
            let current = &self.tabs[tab];
            if !current.loading
                && !current.query.trim().is_empty()
                && current.search_revision != self.index_progress.revision
            {
                self.refresh_tab_inner(tab, true, cx);
            }
        }
    }

    /// Loads the tab's folder or search results. A cached folder shows at once
    /// and is re-read in the background; otherwise the previous rows stay up,
    /// under the loading bar, until the new ones arrive. `quiet` refreshes a
    /// search in place for a newer index snapshot.
    fn refresh_tab_inner(&mut self, tab_index: usize, quiet: bool, cx: &mut Context<Self>) {
        // Every change of a tab's folder or search passes through here.
        self.sync_watched();
        let tab = &mut self.tabs[tab_index];
        tab.request += 1;
        tab.search_revision = self.index_progress.revision;
        tab.loading = true;
        let id = tab.id;
        let request = tab.request;
        let query = tab.query.clone();
        let is_search = !query.trim().is_empty();
        let location = tab.location.clone();
        let mut in_place = quiet;
        if !is_search && let Some(entries) = self.folders.get(&location) {
            let entries = self.visible(entries, false);
            let tab = &mut self.tabs[tab_index];
            tab.list.update(cx, |list, cx| {
                if quiet {
                    list.reconcile_entries(entries, "This folder is empty", cx);
                } else {
                    list.set_entries(entries, cx);
                }
            });
            in_place = true;
        }
        if !in_place {
            self.tabs[tab_index]
                .list
                .update(cx, |list, cx| list.start_loading(cx));
        }
        self.release_folder_index();
        let source = if is_search {
            match self.search_source(&location, cx) {
                Some(source) => Some(source),
                // The folder is being indexed; its searches rerun when ready.
                None => return,
            }
        } else {
            None
        };
        let worker = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            if is_search {
                worker.timer(Duration::from_millis(120)).await;
            }
            let current = this
                .update(cx, |this, _| {
                    this.tabs
                        .iter()
                        .any(|tab| tab.id == id && tab.request == request)
                })
                .unwrap_or(false);
            if !current {
                return;
            }
            let folder = location.clone();
            let result = worker
                .spawn(async move {
                    match source {
                        Some(SearchSource::Index(index, scope)) => index
                            .search(&query, scope.as_deref())
                            .map(file_list::entries_for_paths),
                        Some(SearchSource::Folder(index)) => {
                            index.search(&query).map(file_list::entries_for_paths)
                        }
                        None => file_list::read_directory(&folder),
                    }
                })
                .await
                .map(Rc::<[FileEntry]>::from);
            let _ = this.update(cx, |this, cx| {
                if !is_search && let Ok(entries) = &result {
                    this.folders.insert(location, entries.clone());
                }
                // The cache keeps every entry, so toggling hidden files never re-reads.
                let result = result.map(|entries| this.visible(entries, is_search));
                let Some(index) = this
                    .tabs
                    .iter()
                    .position(|tab| tab.id == id && tab.request == request)
                else {
                    return;
                };
                // The folder was moved or deleted: go to the nearest folder
                // that still exists, as Finder does.
                if !is_search && result.is_err() && !this.tabs[index].location.is_dir() {
                    let tab = &mut this.tabs[index];
                    let nearest = watcher::nearest_existing(&tab.location);
                    tab.history.visit(&mut tab.location, nearest);
                    this.refresh_tab(index, cx);
                    return;
                }
                let tab = &mut this.tabs[index];
                tab.loading = false;
                let empty_message = if is_search {
                    "No matching files"
                } else {
                    "This folder is empty"
                };
                let stale = is_search && tab.search_revision != this.index_progress.revision;
                tab.list.update(cx, |list, cx| match result {
                    Ok(entries) if in_place => list.reconcile_entries(entries, empty_message, cx),
                    Ok(entries) => {
                        let empty = entries.is_empty();
                        if is_search {
                            list.set_search_entries(entries, cx);
                        } else {
                            list.set_entries(entries, cx);
                        }
                        if empty {
                            list.set_message(Some(empty_message.into()), cx);
                        }
                    }
                    Err(error) => {
                        if !(quiet && is_search) {
                            list.set_entries(Rc::from([]), cx);
                        }
                        list.set_message(Some(format!("{error:#}")), cx);
                    }
                });
                if stale {
                    // The index moved on while this search ran; catch up now.
                    this.refresh_stale_searches(cx);
                }
                if is_search {
                    let index = this.index.clone();
                    let worker = cx.background_executor().clone();
                    this.search_trim = Some(cx.background_executor().spawn(async move {
                        worker.timer(Duration::from_secs(60)).await;
                        index.trim_if_idle(Duration::from_secs(30));
                    }));
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn sync_search(&mut self, cx: &mut Context<Self>) {
        let query = self.tabs[self.active_tab].query.clone();
        self.search.set(!query.is_empty(), Instant::now());
        self.file_search
            .update(cx, |input, cx| input.set_text(query, cx));
    }

    fn select_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }
        self.tabs[self.active_tab]
            .list
            .update(cx, |list, cx| list.end_drag(cx));
        self.active_tab = index;
        self.focus_list(window, cx);
        self.sync_search(cx);
        self.tab_scroll.scroll_to_item(index);
        if std::mem::take(&mut self.tabs[index].live.stale) {
            self.refresh_tab_inner(index, true, cx);
        }
        cx.notify();
    }

    fn handle_list_request(
        &mut self,
        list: &Entity<FileList>,
        request: &file_list::Request,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use file_list::Request;
        match request {
            Request::Open(paths) => self.open_paths(paths, window, cx),
            Request::QuickLook(paths) => {
                quick_look::toggle(paths.iter().take(quick_look::LIMIT).cloned().collect())
            }
            Request::Trash(paths) => self.trash(paths.clone(), cx),
            Request::CopyPath(paths) => self.copy_paths(paths, cx),
            Request::NewFolder => self.new_folder(list.clone(), window, cx),
            Request::Rename { from, name } => self.rename(from, name, cx),
            Request::Drop { paths, into } => {
                let tab = self.tabs.iter().find(|tab| tab.list == *list);
                let into = match (into, tab) {
                    (Some(folder), _) => Some(folder.clone()),
                    // Search results are not a folder to drop into.
                    (None, Some(tab)) if tab.query.trim().is_empty() => Some(tab.location.clone()),
                    _ => None,
                };
                match into {
                    Some(folder) => self.drop_files(paths.clone(), folder, cx),
                    None => ui::beep(),
                }
            }
            Request::DragEnded(paths) => {
                // Another app may have moved them; it finishes just after the
                // drop, so look again a moment later.
                let folders: Vec<PathBuf> = paths
                    .iter()
                    .filter_map(|path| path.parent().map(Path::to_path_buf))
                    .collect();
                let worker = cx.background_executor().clone();
                cx.spawn(async move |this, cx| {
                    worker.timer(Duration::from_millis(400)).await;
                    let _ = this.update(cx, |this, cx| this.refresh_folders(&folders, cx));
                })
                .detach();
            }
            Request::ContextMenu { position, paths } => {
                self.context_menu = Some(ContextMenu {
                    position: *position,
                    paths: paths.clone(),
                    selected: None,
                    can_paste: !system::pasted_files().is_empty(),
                    open_with: None,
                });
                // The menu takes the keyboard until it closes.
                self.focus.focus(window);
                cx.notify();
            }
        }
    }

    fn run_file_action(&mut self, action: FileAction, window: &mut Window, cx: &mut Context<Self>) {
        let Some(menu) = self.context_menu.take() else {
            return;
        };
        self.focus_list(window, cx);
        let list = self.tabs[self.active_tab].list.clone();
        // Blank space acts on the folder being shown.
        let targets = if menu.paths.is_empty() {
            vec![self.tabs[self.active_tab].location.clone()]
        } else {
            menu.paths.clone()
        };
        match action {
            // Enter on Open With opens with the default app.
            FileAction::Open | FileAction::OpenWith => self.open_paths(&menu.paths, window, cx),
            FileAction::QuickLook => {
                quick_look::toggle(menu.paths.into_iter().take(quick_look::LIMIT).collect())
            }
            FileAction::Rename => {
                if let Some(path) = menu.paths.first() {
                    list.update(cx, |list, cx| list.begin_rename(path, window, cx));
                }
            }
            FileAction::Duplicate => self.duplicate(menu.paths, cx),
            FileAction::Copy => system::copy_files(&menu.paths),
            FileAction::CopyPath => self.copy_paths(&targets, cx),
            FileAction::Paste => self.paste(file_ops::Transfer::Copy, cx),
            FileAction::ShowInFinder => system::show_in_finder(&targets),
            FileAction::Trash => self.trash(menu.paths, cx),
            FileAction::NewFolder => self.new_folder(list, window, cx),
        }
        cx.notify();
    }

    /// Runs `action` from its shortcut if the open menu offers it.
    fn run_shortcut(&mut self, action: FileAction, window: &mut Window, cx: &mut Context<Self>) {
        let offered = self.context_menu.as_ref().is_some_and(|menu| {
            FileAction::menu(&menu.paths, menu.can_paste).contains(&Some(action))
        });
        if offered {
            self.run_file_action(action, window, cx);
        }
    }

    fn close_context_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.context_menu.take().is_some() {
            self.focus_list(window, cx);
            cx.notify();
        }
    }

    /// Shows the Open With submenu, finding the apps once per menu.
    fn show_open_with(&mut self, cx: &mut Context<Self>) {
        let Some(menu) = &mut self.context_menu else {
            return;
        };
        if menu.open_with.is_some() {
            return;
        }
        let apps = menu
            .paths
            .first()
            .map(|path| system::apps_for(path))
            .unwrap_or_default();
        menu.open_with = Some(
            apps.into_iter()
                .map(|app| {
                    let icon = classify(&app.path);
                    (app, icon)
                })
                .collect(),
        );
        cx.notify();
    }

    fn hide_open_with(&mut self, cx: &mut Context<Self>) {
        if let Some(menu) = &mut self.context_menu
            && menu.open_with.take().is_some()
        {
            cx.notify();
        }
    }

    fn open_with(&mut self, app: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let Some(menu) = self.context_menu.take() else {
            return;
        };
        self.focus_list(window, cx);
        self.remember_recent(&menu.paths);
        let worker = cx.background_executor().clone();
        cx.spawn(async move |_, _| {
            let result = worker
                .spawn(async move { system::open_with(&app, &menu.paths) })
                .await;
            if let Err(error) = result {
                eprintln!("{error:#}");
            }
        })
        .detach();
        cx.notify();
    }

    fn copy_paths(&self, paths: &[PathBuf], cx: &mut Context<Self>) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(file_ops::path_text(paths)));
    }

    /// The active list's selection, for keyboard commands.
    fn selection(&self, cx: &App) -> Vec<PathBuf> {
        self.tabs[self.active_tab]
            .list
            .read(cx)
            .selected_paths(usize::MAX)
    }

    /// The folder new items go into, or `None` while showing search results.
    fn current_folder(&self) -> Option<PathBuf> {
        let tab = &self.tabs[self.active_tab];
        tab.query.trim().is_empty().then(|| tab.location.clone())
    }

    /// Moves `paths` to the Trash. Their rows disappear at once; listings and
    /// searches that showed them refresh once macOS is done.
    fn trash(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        for tab in &self.tabs {
            tab.list
                .update(cx, |list, cx| list.remove_paths(&paths, cx));
        }
        let worker = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            let folders: Vec<PathBuf> = paths
                .iter()
                .filter_map(|path| path.parent().map(Path::to_path_buf))
                .collect();
            let (trashed, error) = worker.spawn(async move { file_ops::trash(&paths) }).await;
            let _ = this.update(cx, |this, cx| {
                if !trashed.is_empty() {
                    this.play(ui::Sound::Trash);
                }
                this.undo.record(undo::Change::trashed(trashed));
                if let Some(error) = error {
                    eprintln!("{error:#}");
                    ui::beep();
                }
                this.refresh_folders(&folders, cx);
            });
        })
        .detach();
    }

    fn rename(&mut self, from: &Path, name: &str, cx: &mut Context<Self>) {
        let to = match file_ops::rename(from, name) {
            Ok(to) => to,
            Err(error) => {
                eprintln!("{error:#}");
                ui::beep();
                return;
            }
        };
        for tab in &self.tabs {
            tab.list
                .update(cx, |list, cx| list.apply_rename(from, to.clone(), cx));
        }
        self.retarget(from, &to);
        self.undo.record(undo::Change::new(vec![undo::Step::Moved {
            from: from.to_path_buf(),
            to,
        }]));
        if let Some(folder) = from.parent() {
            self.refresh_folders(&[folder.to_path_buf()], cx);
        }
    }

    /// Points tabs, their history, and recent items inside `from` at `to`
    /// after a folder was renamed or moved.
    fn retarget(&mut self, from: &Path, to: &Path) {
        for tab in &mut self.tabs {
            if let Ok(rest) = tab.location.strip_prefix(from) {
                tab.location = to.join(rest);
            }
            tab.history.rename(from, to);
        }
        for (path, icon) in &mut self.recent {
            if let Ok(rest) = path.strip_prefix(from) {
                *path = to.join(rest);
                *icon = classify(path);
            }
        }
    }

    /// Creates a folder in the current folder and starts renaming it.
    fn new_folder(&mut self, list: Entity<FileList>, window: &mut Window, cx: &mut Context<Self>) {
        // Search results are not a folder to create in.
        let Some(folder) = self.current_folder() else {
            ui::beep();
            return;
        };
        match file_ops::new_folder(&folder) {
            Ok(path) => {
                list.update(cx, |list, cx| {
                    list.insert_and_rename(path.clone(), window, cx)
                });
                self.undo
                    .record(undo::Change::new(vec![undo::Step::Placed(path)]));
                self.refresh_folders(&[folder], cx);
            }
            Err(error) => {
                eprintln!("{error:#}");
                ui::beep();
            }
        }
    }

    /// Moves or copies dropped files into `folder`, as Finder would: a move on
    /// the same volume, a copy across volumes or with Option held.
    fn drop_files(&mut self, paths: Vec<PathBuf>, folder: PathBuf, cx: &mut Context<Self>) {
        let mode = if drag_out::option_held() {
            file_ops::Transfer::Copy
        } else {
            file_ops::Transfer::Drop
        };
        self.transfer(vec![(paths, folder)], mode, cx);
    }

    /// Pastes the clipboard's files into the current folder: copies with ⌘V,
    /// moves with ⌥⌘V.
    fn paste(&mut self, mode: file_ops::Transfer, cx: &mut Context<Self>) {
        let files = system::pasted_files();
        match self.current_folder() {
            Some(folder) if !files.is_empty() => self.transfer(vec![(files, folder)], mode, cx),
            _ => ui::beep(),
        }
    }

    /// Copies each item into its own folder as "name copy".
    fn duplicate(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let mut jobs: Vec<(Vec<PathBuf>, PathBuf)> = Vec::new();
        for path in paths {
            let Some(parent) = path.parent().map(Path::to_path_buf) else {
                continue;
            };
            match jobs.iter_mut().find(|(_, folder)| *folder == parent) {
                Some((group, _)) => group.push(path),
                None => jobs.push((vec![path], parent)),
            }
        }
        if jobs.is_empty() {
            ui::beep();
            return;
        }
        self.transfer(jobs, file_ops::Transfer::Copy, cx);
    }

    /// Moves or copies each group of paths into its folder off the UI thread,
    /// then records one undoable change, plays the move sound if anything
    /// moved, and refreshes the affected folders.
    fn transfer(
        &mut self,
        jobs: Vec<(Vec<PathBuf>, PathBuf)>,
        mode: file_ops::Transfer,
        cx: &mut Context<Self>,
    ) {
        let mut changed: Vec<PathBuf> = Vec::new();
        for (paths, folder) in &jobs {
            changed.extend(
                paths
                    .iter()
                    .filter_map(|path| path.parent().map(Path::to_path_buf)),
            );
            changed.push(folder.clone());
        }
        let worker = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            let (placed, error) = worker
                .spawn(async move {
                    let mut placed = Vec::new();
                    for (paths, folder) in jobs {
                        let (done, error) = file_ops::transfer(&paths, &folder, mode);
                        placed.extend(done);
                        if error.is_some() {
                            return (placed, error);
                        }
                    }
                    (placed, None)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                // Nothing moved, such as a drop where the items already are,
                // stays silent.
                if !placed.is_empty() {
                    this.play(ui::Sound::Move);
                }
                for item in placed.iter().filter(|item| !item.copied) {
                    this.retarget(&item.from, &item.to);
                }
                this.undo.record(undo::Change::transferred(placed));
                if let Some(error) = error {
                    eprintln!("{error:#}");
                    ui::beep();
                }
                this.refresh_folders(&changed, cx);
            });
        })
        .detach();
    }

    /// Undoes (or redoes) the latest file change off the UI thread. One runs
    /// at a time; with nothing to revert it plays the alert.
    fn revert(&mut self, undo: bool, cx: &mut Context<Self>) {
        if self.reverting {
            ui::beep();
            return;
        }
        let change = if undo {
            self.undo.take_undo()
        } else {
            self.undo.take_redo()
        };
        let Some(change) = change else {
            ui::beep();
            return;
        };
        self.reverting = true;
        let mut folders = change.folders();
        let worker = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            let (inverse, error) = worker.spawn(async move { change.revert() }).await;
            let _ = this.update(cx, |this, cx| {
                this.reverting = false;
                if let Some(inverse) = &inverse {
                    folders.extend(inverse.folders());
                    let mut trashed = false;
                    for step in inverse.steps() {
                        match step {
                            // The item now at `to` came from `from`.
                            undo::Step::Moved { from, to } => this.retarget(from, to),
                            undo::Step::Trashed(_) => trashed = true,
                            undo::Step::Placed(_) => {}
                        }
                    }
                    this.play(if trashed {
                        ui::Sound::Trash
                    } else {
                        ui::Sound::Move
                    });
                }
                if undo {
                    this.undo.undone(inverse);
                } else {
                    this.undo.redone(inverse);
                }
                if let Some(error) = error {
                    eprintln!("{error:#}");
                    ui::beep();
                }
                this.refresh_folders(&folders, cx);
            });
        })
        .detach();
    }

    /// Ejects a volume off the UI thread. Tabs on it move to Desktop, as
    /// Finder closes windows showing an ejected disk; if it is in use, it
    /// stays mounted and the alert sounds.
    fn eject(&mut self, volume: PathBuf, cx: &mut Context<Self>) {
        let worker = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            let path = volume.clone();
            let result = worker.spawn(async move { system::eject(&path) }).await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        let desktop = this.home.join("Desktop");
                        for tab in 0..this.tabs.len() {
                            if this.tabs[tab].location.starts_with(&volume) {
                                this.tabs[tab].location = desktop.clone();
                                this.tabs[tab].query.clear();
                                this.refresh_tab(tab, cx);
                            }
                        }
                    }
                    Err(error) => {
                        eprintln!("{error:#}");
                        ui::beep();
                    }
                }
                this.volumes = listed_volumes();
                cx.notify();
            });
        })
        .detach();
    }

    /// Re-reads `folders` and refreshes, in place, the tabs listing them.
    /// Searches of indexed folders already show the change and refresh when
    /// the index sees it; an on-the-spot folder index is not watched, so a
    /// change inside it rebuilds it.
    fn refresh_folders(&mut self, folders: &[PathBuf], cx: &mut Context<Self>) {
        for folder in folders {
            self.folders.remove(folder);
        }
        let stale_index = self.folder_index.as_ref().is_some_and(|index| {
            folders
                .iter()
                .any(|folder| folder.starts_with(index.folder()))
        });
        if stale_index {
            self.folder_index = None;
        }
        for tab in 0..self.tabs.len() {
            let current = &self.tabs[tab];
            let searching = !current.query.trim().is_empty();
            if (!searching && folders.contains(&current.location))
                || (searching && stale_index && !self.index.covers(&current.location))
            {
                self.refresh_tab_inner(tab, true, cx);
            }
        }
    }

    /// Keyboard focus belongs to the active file list so arrows and Enter work.
    fn focus_list(&self, window: &mut Window, cx: &App) {
        self.tabs[self.active_tab].list.read(cx).focus(window);
    }

    /// Goes to `location` in the active tab, recording it in the tab's history.
    fn navigate(&mut self, location: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let tab = &mut self.tabs[self.active_tab];
        tab.history.visit(&mut tab.location, location);
        self.show_location(window, cx);
    }

    fn go_back(&mut self, _: &GoBack, window: &mut Window, cx: &mut Context<Self>) {
        let tab = &mut self.tabs[self.active_tab];
        if tab.history.go_back(&mut tab.location) {
            self.show_location(window, cx);
        }
    }

    fn go_forward(&mut self, _: &GoForward, window: &mut Window, cx: &mut Context<Self>) {
        let tab = &mut self.tabs[self.active_tab];
        if tab.history.go_forward(&mut tab.location) {
            self.show_location(window, cx);
        }
    }

    /// ⌘←: back through history, or up to the parent, per the setting.
    fn go_left(&mut self, _: &GoLeft, window: &mut Window, cx: &mut Context<Self>) {
        match self.settings.arrow_keys {
            storage::ArrowKeys::History => self.go_back(&GoBack, window, cx),
            storage::ArrowKeys::FolderPath => self.go_up(&GoUp, window, cx),
        }
    }

    /// ⌘→: forward through history, or back down the folder path the tab
    /// came up, per the setting. With nowhere to go it plays the alert.
    fn go_right(&mut self, _: &GoRight, window: &mut Window, cx: &mut Context<Self>) {
        match self.settings.arrow_keys {
            storage::ArrowKeys::History => self.go_forward(&GoForward, window, cx),
            storage::ArrowKeys::FolderPath => {
                let tab = &self.tabs[self.active_tab];
                match tab.history.deeper(&tab.location).into_iter().next() {
                    Some(folder) => self.navigate(folder, window, cx),
                    None => ui::beep(),
                }
            }
        }
    }

    fn set_arrow_keys(&mut self, arrow_keys: storage::ArrowKeys, cx: &mut Context<Self>) {
        self.settings.arrow_keys = arrow_keys;
        if let Err(error) = storage::save(&self.settings_path, &self.settings) {
            eprintln!("Could not save settings: {error:#}");
        }
        cx.notify();
    }

    fn go_up(&mut self, _: &GoUp, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(parent) = self.tabs[self.active_tab].location.parent() {
            self.navigate(parent.to_path_buf(), window, cx);
        }
    }

    /// Shows the active tab's location, leaving any search.
    fn show_location(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let tab = &mut self.tabs[self.active_tab];
        tab.list.update(cx, |list, cx| list.end_drag(cx));
        tab.query.clear();
        self.sync_search(cx);
        self.refresh_tab(self.active_tab, cx);
        self.focus_list(window, cx);
        cx.notify();
    }

    /// Browses into the first folder and opens files and packages in their apps.
    fn open_paths(&mut self, paths: &[PathBuf], window: &mut Window, cx: &mut Context<Self>) {
        self.remember_recent(paths);
        self.open_without_remembering(paths, window, cx);
    }

    fn open_without_remembering(
        &mut self,
        paths: &[PathBuf],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut folder = None;
        for path in paths {
            if file_list::opens_as_folder(path) {
                folder.get_or_insert_with(|| path.clone());
            } else {
                cx.open_with_system(path);
            }
        }
        if let Some(folder) = folder {
            self.navigate(folder, window, cx);
        }
    }

    /// Turns the path bar into a text field holding the current path, all
    /// selected so typing replaces it.
    fn begin_path_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.command_menu_open || self.settings_open || self.path_edit.is_some() {
            return;
        }
        let text = path_completion::display(&self.tabs[self.active_tab].location, &self.home);
        let length = text.len();
        self.path_input
            .update(cx, |input, cx| input.edit(text, 0..length, window, cx));
        let focus = self.path_input.read(cx).focus_handle(cx);
        let blur = cx.on_blur(&focus, window, |this, window, cx| {
            this.end_path_edit(window, cx)
        });
        self.path_edit = Some(PathEdit {
            typed: false,
            selected: None,
            _blur: blur,
        });
        cx.notify();
    }

    fn end_path_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.path_edit.take().is_some() {
            self.focus_list(window, cx);
            cx.notify();
        }
    }

    /// Completions for the typed path, as in ⌘K.
    fn path_suggestions(&self, cx: &App) -> Vec<path_completion::Completion> {
        if !self.path_edit.as_ref().is_some_and(|edit| edit.typed) {
            return Vec::new();
        }
        let query = self.path_input.read(cx).text();
        let Some(folder) = path_completion::folder(query, &self.home) else {
            return Vec::new();
        };
        let children = match &self.path_children {
            Some((listed, children)) if *listed == folder => &children[..],
            _ => &[],
        };
        path_completion::completions(query, &self.home, children, self.settings.show_hidden_files)
    }

    /// Enter: the highlighted suggestion, else the typed path if it exists,
    /// else the first suggestion. Folders open in the tab, files in their app.
    fn commit_path_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.path_input.read(cx).composing() {
            return;
        }
        let suggestions = self.path_suggestions(cx);
        let selected = self.path_edit.as_ref().and_then(|edit| edit.selected);
        let typed = path_completion::resolve(self.path_input.read(cx).text(), &self.home)
            .filter(|path| path.exists());
        let target = match selected {
            Some(index) => suggestions.get(index).map(|item| item.path.clone()),
            None => typed.or_else(|| suggestions.first().map(|item| item.path.clone())),
        };
        let Some(target) = target else {
            ui::beep();
            return;
        };
        self.end_path_edit(window, cx);
        self.open_paths(&[target], window, cx);
    }

    fn move_path_selection(&mut self, step: isize, cx: &mut Context<Self>) {
        let count = self.path_suggestions(cx).len();
        let Some(edit) = &mut self.path_edit else {
            return;
        };
        if count == 0 {
            return;
        }
        edit.selected = Some(match edit.selected {
            None if step > 0 => 0,
            None => count - 1,
            Some(index) => index.saturating_add_signed(step).min(count - 1),
        });
        cx.notify();
    }

    /// Tab: fills in the highlighted (or first) suggestion and keeps typing.
    fn complete_path_edit(&mut self, cx: &mut Context<Self>) {
        let suggestions = self.path_suggestions(cx);
        let index = self
            .path_edit
            .as_ref()
            .and_then(|edit| edit.selected)
            .unwrap_or(0);
        let Some(item) = suggestions.get(index) else {
            return;
        };
        let query = self.path_input.read(cx).text().to_owned();
        let text = path_completion::completed_text(&query, &self.home, item);
        self.path_input
            .update(cx, |input, cx| input.set_text(text.clone(), cx));
        if let Some(edit) = &mut self.path_edit {
            edit.selected = None;
            edit.typed = true;
        }
        self.read_children_for(&text, cx);
        cx.notify();
    }

    /// The path bar's text field and, once typing, its suggestions below.
    fn path_editor(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let suggestions = self.path_suggestions(cx);
        let selected = self.path_edit.as_ref().and_then(|edit| edit.selected);
        div()
            .id("path-editor")
            .key_context("PathEdit")
            .relative()
            .flex_1()
            .min_w(dp(0.))
            .ml(dp(6.))
            .h(dp(26.))
            .px(dp(8.))
            .flex()
            .items_center()
            .rounded(dp(radius::SM))
            .bg(rgb(0x101010))
            .border_1()
            .border_color(rgb(0x0a84ff))
            .line_height(dp(20.))
            .child(self.path_input.clone())
            .when(!suggestions.is_empty(), |editor| {
                editor.child(gpui::deferred(
                    div()
                        .id("path-suggestions")
                        .absolute()
                        .top(dp(30.))
                        .left(dp(-1.))
                        .right(dp(-1.))
                        .p(dp(4.))
                        .rounded(dp(radius::MD))
                        .bg(rgb(0x222222))
                        .shadow_lg()
                        .occlude()
                        .children(suggestions.into_iter().enumerate().map(|(index, item)| {
                            let path = item.path.clone();
                            div()
                                .id(("path-suggestion", index))
                                .h(dp(row::HEIGHT))
                                .px(dp(8.))
                                .flex()
                                .items_center()
                                .gap(dp(row::GAP))
                                .rounded(dp(radius::SM))
                                .when(selected == Some(index), |row| row.bg(rgb(0x303840)))
                                .hover(|row| row.bg(rgb(0x303840)))
                                .cursor_pointer()
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.end_path_edit(window, cx);
                                    this.open_paths(std::slice::from_ref(&path), window, cx);
                                }))
                                .child(native_icons::file_icon(item.icon.clone(), row::ICON))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w(dp(0.))
                                        .text_ellipsis()
                                        .child(item.label.clone()),
                                )
                        })),
                ))
            })
    }

    fn path_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tab = &self.tabs[self.active_tab];
        let mut segments = path_segments(&tab.location, &self.home);
        let last = segments.len().saturating_sub(1);
        // With the arrows following the folder path, the folders ⌘→ returns
        // to show after the current one, dimmed.
        if self.settings.arrow_keys == storage::ArrowKeys::FolderPath {
            segments.extend(tab.history.deeper(&tab.location).into_iter().map(|path| {
                let label = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                (label, path)
            }));
        }
        let editing = self.path_edit.is_some();
        div()
            .id("path-bar")
            .flex()
            .items_center()
            .flex_shrink_0()
            .h(dp(36.))
            // Clicking the bar outside its segments types a path instead.
            .when(!editing, |bar| {
                bar.cursor_text()
                    .on_click(cx.listener(|this, _, window, cx| this.begin_path_edit(window, cx)))
            })
            // The back chevron's glyph starts on the file icons' column.
            .pl(dp(12.))
            .pr(dp(8.))
            .gap(dp(2.))
            .bg(rgb(0x181818))
            .border_b_1()
            .border_color(rgb(0x262626))
            .child(history_button(
                "go-back",
                "chevron.left",
                tab.history.can_go_back(),
                || Box::new(GoBack),
            ))
            .child(history_button(
                "go-forward",
                "chevron.right",
                tab.history.can_go_forward(),
                || Box::new(GoForward),
            ))
            .when(editing, |bar| bar.child(self.path_editor(cx)))
            .when(!editing, |bar| {
                bar.child(
                    div()
                        .flex()
                        .items_center()
                        .flex_1()
                        .min_w(dp(0.))
                        .ml(dp(6.))
                        .overflow_hidden()
                        .children(segments.into_iter().enumerate().flat_map(
                            |(index, (label, path))| {
                                let current = index == last;
                                let separator = (index > 0).then(|| {
                                    div()
                                        .flex_shrink_0()
                                        .px(dp(2.))
                                        .child(native_icons::symbol("chevron.right", 10.))
                                        .into_any_element()
                                });
                                let target = path.clone();
                                let segment = div()
                                    .id(("path-segment", index))
                                    .drag_over::<ExternalPaths>(|segment, _, _, _| {
                                        segment.bg(rgb(0x1d4f7d))
                                    })
                                    .on_drop(cx.listener(
                                        move |this, paths: &ExternalPaths, _, cx| {
                                            this.drop_files(
                                                paths.paths().to_vec(),
                                                target.clone(),
                                                cx,
                                            )
                                        },
                                    ))
                                    .flex()
                                    .items_center()
                                    .h(dp(24.))
                                    .px(dp(6.))
                                    .min_w(dp(0.))
                                    .rounded(dp(radius::SM))
                                    .text_color(rgb(if current {
                                        0xeeeeee
                                    } else if index > last {
                                        0x666666
                                    } else {
                                        0xaaaaaa
                                    }))
                                    .when(current, |segment| {
                                        segment
                                            .flex_shrink_0()
                                            .font_weight(gpui::FontWeight::MEDIUM)
                                    })
                                    // Short of room, the dimmed folders ahead
                                    // give way before the path that leads here.
                                    .when(index > last, |mut segment| {
                                        segment.style().flex_shrink = Some(8.);
                                        segment
                                    })
                                    .cursor_pointer()
                                    .hover(|segment| segment.bg(rgb(0x252525)))
                                    .cursor_pointer()
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.navigate(path.clone(), window, cx);
                                    }))
                                    .child(div().min_w(dp(0.)).truncate().child(label))
                                    .into_any_element();
                                separator.into_iter().chain([segment])
                            },
                        )),
                )
            })
    }

    fn open_search(&mut self, _: &OpenSearch, window: &mut Window, cx: &mut Context<Self>) {
        if self.command_menu_open || self.settings_open {
            return;
        }
        if !self.search.is_open() {
            self.search.set(true, Instant::now());
        }
        self.file_search.read(cx).focus_handle(cx).focus(window);
        cx.notify();
    }

    fn close_search(&mut self, _: &CloseSearch, window: &mut Window, cx: &mut Context<Self>) {
        self.tabs[self.active_tab].query.clear();
        self.sync_search(cx);
        self.refresh_tab(self.active_tab, cx);
        self.focus_list(window, cx);
        cx.notify();
    }

    /// Collapses to a 32-point icon button; expands into a field when opened.
    fn search_field(&self, amount: f32, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("file-search")
            .key_context("FileSearch")
            .flex_shrink_0()
            .w(dp(SEARCH_CLOSED + (SEARCH_OPEN - SEARCH_CLOSED) * amount))
            .h(dp(32.))
            .flex()
            .items_center()
            .overflow_hidden()
            .rounded(dp(radius::MD))
            .bg(gpui::Hsla::from(rgb(0x222222)).opacity(amount))
            .cursor_pointer()
            .when(!self.search.is_open(), |field| {
                field.hover(|field| field.bg(rgb(0x353535)))
            })
            .on_click(|_, window, cx| {
                cx.stop_propagation();
                window.dispatch_action(Box::new(OpenSearch), cx);
            })
            .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                if this.search.is_open() && this.file_search.read(cx).text().is_empty() {
                    this.close_search(&CloseSearch, window, cx);
                }
            }))
            .child(
                div()
                    .flex_shrink_0()
                    .size(dp(SEARCH_CLOSED))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(native_icons::symbol("magnifyingglass", row::ICON)),
            )
            .when(amount > 0., |field| {
                field
                    .child(
                        div()
                            .flex_1()
                            .min_w(dp(0.))
                            .pr(dp(8.))
                            .opacity(amount)
                            .line_height(dp(20.))
                            .child(self.file_search.clone()),
                    )
                    .child(self.search_scope_toggle(amount))
            })
    }

    /// The search field's scope: the current folder's name, or All for every
    /// indexed folder. Clicking switches between them.
    fn search_scope_toggle(&self, amount: f32) -> impl IntoElement {
        let label: SharedString = match self.settings.search_scope {
            storage::SearchScope::Indexed => "All".into(),
            storage::SearchScope::CurrentFolder => {
                let location = &self.tabs[self.active_tab].location;
                location
                    .file_name()
                    .map_or("Macintosh HD".into(), |name| {
                        name.to_string_lossy().into_owned()
                    })
                    .into()
            }
        };
        div()
            .id("search-scope")
            .flex_shrink_0()
            .max_w(dp(96.))
            .h(dp(22.))
            .mr(dp(5.))
            .px(dp(8.))
            .flex()
            .items_center()
            .rounded(dp(radius::SM))
            .bg(rgb(0x2e2e2e))
            .hover(|pill| pill.bg(rgb(0x383838)))
            .text_size(dp(text_size::DETAIL))
            .text_color(rgb(0xbbbbbb))
            .opacity(amount)
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(|_, window, cx| {
                cx.stop_propagation();
                window.dispatch_action(Box::new(ToggleSearchScope), cx);
            })
            .child(div().min_w(dp(0.)).text_ellipsis().child(label))
    }

    fn new_tab(&mut self, _: &NewTab, window: &mut Window, cx: &mut Context<Self>) {
        self.tabs[self.active_tab]
            .list
            .update(cx, |list, cx| list.end_drag(cx));
        let location = self.tabs[self.active_tab].location.clone();
        let entries = self.folders.get(&location).unwrap_or_else(|| Rc::from([]));
        let list = file_list_view(entries, window, cx);
        self.tabs.push(BrowserTab {
            id: self.next_tab_id,
            location,
            query: String::new(),
            request: 0,
            search_revision: 0,
            loading: false,
            list,
            history: Default::default(),
            live: LiveRefresh::default(),
        });
        self.next_tab_id += 1;
        self.active_tab = self.tabs.len() - 1;
        self.sync_search(cx);
        self.refresh_tab(self.active_tab, cx);
        self.focus_list(window, cx);
        self.tab_scroll.scroll_to_item(self.active_tab);
        cx.notify();
    }

    fn close_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.tabs[self.active_tab]
            .list
            .update(cx, |list, cx| list.end_drag(cx));
        if self.tabs.len() == 1 {
            self.save_session();
            window.remove_window();
            return;
        }
        // The focused list may belong to the tab being removed. Restore a
        // surviving focus target before detaching it from GPUI's dispatch tree,
        // otherwise subsequent window shortcuts can lose their action route.
        self.command_menu_open = false;
        self.settings_open = false;
        self.focus.focus(window);
        self.tabs.remove(index);
        self.sync_watched();
        if index < self.active_tab || self.active_tab == self.tabs.len() {
            self.active_tab -= 1;
        }
        self.focus_list(window, cx);
        self.sync_search(cx);
        self.tab_scroll.scroll_to_item(self.active_tab);
        cx.notify();
    }

    fn tab_row(
        &self,
        window: &Window,
        sidebar_width: f32,
        titlebar: &window_chrome::Titlebar,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let controls_width = f32::from(titlebar.controls_width);
        let scale = f32::from(dp(1.).to_pixels(window.rem_size()));
        let search_amount = self.search.amount(window);
        let search_width = SEARCH_CLOSED + (SEARCH_OPEN - SEARCH_CLOSED) * search_amount;
        let available = f32::from(window.viewport_size().width)
            - sidebar_width.max(controls_width)
            - (search_width + 10.) * scale;
        let width = ((available / scale - 4. * (self.tabs.len() - 1) as f32)
            / self.tabs.len() as f32)
            .max(36.);
        let tabs = div()
            .id("tabs")
            .text_size(dp(text_size::DETAIL))
            .flex()
            .flex_1()
            .min_w(dp(0.))
            .h_full()
            .gap(dp(4.))
            .py(dp(4.))
            .overflow_x_scroll()
            .track_scroll(&self.tab_scroll)
            .children(self.tabs.iter().enumerate().map(|(index, tab)| {
                let active = index == self.active_tab;
                let title = if tab.query.is_empty() {
                    tab.location
                        .file_name()
                        .unwrap_or(tab.location.as_os_str())
                        .to_string_lossy()
                        .into_owned()
                } else {
                    format!("Search: {}", tab.query)
                };
                let show_close = width >= 100. || active;
                let target = tab.location.clone();
                div()
                    .id(("tab", index))
                    .drag_over::<ExternalPaths>(|tab, _, _, _| tab.bg(rgb(0x1d4f7d)))
                    .on_drop(cx.listener(move |this, paths: &ExternalPaths, _, cx| {
                        this.drop_files(paths.paths().to_vec(), target.clone(), cx)
                    }))
                    .flex()
                    .flex_1()
                    .min_w(dp(36.))
                    .px(dp(4.))
                    .h_full()
                    .items_center()
                    .justify_center()
                    .rounded(dp(radius::MD))
                    .bg(rgb(if active { 0x353535 } else { 0x222222 }))
                    .text_color(rgb(if active { 0xeeeeee } else { 0xaaaaaa }))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select_tab(index, window, cx);
                    }))
                    .when(width >= 70., |tab| {
                        tab.child(
                            div()
                                .flex_1()
                                .min_w(dp(0.))
                                .text_center()
                                .text_ellipsis()
                                .child(title.clone()),
                        )
                    })
                    .when(width < 70. && !show_close, |tab| {
                        tab.child(div().size(dp(8.)).rounded_full().bg(rgb(0x0a84ff)))
                    })
                    .when(show_close, |tab| {
                        tab.child(
                            div()
                                .id(("close-tab", index))
                                .size(dp(24.))
                                .flex_shrink_0()
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(dp(radius::SM))
                                .hover(|button| button.bg(rgb(0x4a4a4a)))
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.close_tab(index, window, cx);
                                }))
                                .child(native_icons::symbol("xmark", 10.)),
                        )
                    })
            }));
        div()
            .ml(px((controls_width - sidebar_width).max(0.)))
            .flex()
            .items_center()
            .h(titlebar.height)
            .flex_shrink_0()
            .gap(dp(4.))
            .pr(dp(6.))
            .child(tabs)
            .child(self.search_field(search_amount, cx))
    }

    /// Commands, or path completions once the query starts with `/` or `~`.
    fn menu_items(&self, cx: &App) -> Vec<MenuItem> {
        let query = self.command_search.read(cx).text();
        match path_completion::folder(query, &self.home) {
            Some(folder) => {
                let children = match &self.path_children {
                    Some((listed, children)) if *listed == folder => &children[..],
                    _ => &[],
                };
                path_completion::completions(
                    query,
                    &self.home,
                    children,
                    self.settings.show_hidden_files,
                )
                .into_iter()
                .map(MenuItem::Path)
                .collect()
            }
            // Recent files and folders matching the query first, then commands.
            None => path_completion::matching_recent(query, &self.menu_recent)
                .into_iter()
                .map(MenuItem::Path)
                .chain(
                    matching_commands(query, self.updates_supported)
                        .into_iter()
                        .map(MenuItem::Command),
                )
                .collect(),
        }
    }

    /// Lists the folder being typed into off the UI thread, once per folder.
    /// Lists the folder `query` is typing into for completions, off the UI
    /// thread and once per folder; shared by ⌘K and the path bar.
    fn read_children_for(&mut self, query: &str, cx: &mut Context<Self>) {
        let Some(folder) = path_completion::folder(query, &self.home) else {
            return;
        };
        let listed = self
            .path_children
            .as_ref()
            .is_some_and(|(listed, _)| *listed == folder);
        let reading = self
            .path_read
            .as_ref()
            .is_some_and(|(reading, _)| *reading == folder);
        if listed || reading {
            return;
        }
        let worker = cx.background_executor().clone();
        let key = folder.clone();
        let task = cx.spawn(async move |this, cx| {
            let path = folder.clone();
            let children = worker
                .spawn(async move { path_completion::read_children(&path) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this
                    .path_read
                    .as_ref()
                    .is_some_and(|(reading, _)| *reading == folder)
                {
                    this.path_read = None;
                }
                this.path_children = Some((folder, children.into()));
                cx.notify();
            });
        });
        self.path_read = Some((key, task));
    }

    fn run_menu_item(&mut self, item: MenuItem, window: &mut Window, cx: &mut Context<Self>) {
        match item {
            MenuItem::Command(command) => window.dispatch_action(command.action(), cx),
            MenuItem::Path(completion) => {
                self.command_menu_open = false;
                self.focus_list(window, cx);
                self.open_paths(&[completion.path], window, cx);
                cx.notify();
            }
        }
    }

    fn move_context_selection(&mut self, step: isize, cx: &mut Context<Self>) {
        let Some(menu) = &mut self.context_menu else {
            return;
        };
        let count = FileAction::menu(&menu.paths, menu.can_paste)
            .into_iter()
            .flatten()
            .count();
        menu.selected = Some(match menu.selected {
            None if step > 0 => 0,
            None => count - 1,
            Some(index) => index.saturating_add_signed(step).min(count - 1),
        });
        cx.notify();
    }

    /// The right-click menu at the pointer, kept inside the window. A click
    /// anywhere else closes it.
    fn context_menu_view(
        &self,
        menu: &ContextMenu,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        const WIDTH: f32 = 220.;
        const PADDING: f32 = 4.;
        const SEPARATOR: f32 = 9.;
        const SUBMENU_WIDTH: f32 = 240.;
        let items = FileAction::menu(&menu.paths, menu.can_paste);
        let scale = f32::from(dp(1.).to_pixels(window.rem_size()));
        // Where the Open With row sits, for its submenu.
        let open_with_top = PADDING
            + items
                .iter()
                .take_while(|item| **item != Some(FileAction::OpenWith))
                .map(|item| {
                    if item.is_some() {
                        row::HEIGHT
                    } else {
                        SEPARATOR
                    }
                })
                .sum::<f32>();
        let height = PADDING * 2.
            + items
                .iter()
                .map(|item| {
                    if item.is_some() {
                        row::HEIGHT
                    } else {
                        SEPARATOR
                    }
                })
                .sum::<f32>();
        let viewport = window.viewport_size();
        let margin = 8. * scale;
        let left = f32::from(menu.position.x)
            .min(f32::from(viewport.width) - WIDTH * scale - margin)
            .max(margin);
        let top = f32::from(menu.position.y)
            .min(f32::from(viewport.height) - height * scale - margin)
            .max(margin);
        let mut action_index = 0;
        div()
            .id("context-menu-overlay")
            .absolute()
            .inset_0()
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.close_context_menu(window, cx)),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _, window, cx| this.close_context_menu(window, cx)),
            )
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .id("context-menu")
                    .absolute()
                    .left(px(left))
                    .top(px(top))
                    .w(dp(WIDTH))
                    .p(dp(PADDING))
                    .rounded(dp(radius::MD))
                    .bg(rgb(0x222222))
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                    .children(items.into_iter().map(|item| {
                        match item {
                            None => div()
                                .h(dp(SEPARATOR))
                                .flex()
                                .items_center()
                                .px(dp(6.))
                                .child(div().h(px(1.)).w_full().bg(rgb(0x333333)))
                                .into_any_element(),
                            Some(action) => {
                                let index = action_index;
                                action_index += 1;
                                div()
                                    .id(("context-action", index))
                                    .h(dp(row::HEIGHT))
                                    .px(dp(10.))
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .rounded(dp(radius::SM))
                                    .when(menu.selected == Some(index), |row| row.bg(rgb(0x303840)))
                                    .hover(|row| row.bg(rgb(0x303840)))
                                    .cursor_pointer()
                                    .when(
                                        action == FileAction::OpenWith && menu.open_with.is_some(),
                                        |row| row.bg(rgb(0x303840)),
                                    )
                                    .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                                        if *hovered {
                                            if action == FileAction::OpenWith {
                                                this.show_open_with(cx);
                                            } else {
                                                this.hide_open_with(cx);
                                            }
                                        }
                                    }))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        cx.stop_propagation();
                                        if action == FileAction::OpenWith {
                                            this.show_open_with(cx);
                                        } else {
                                            this.run_file_action(action, window, cx);
                                        }
                                    }))
                                    .child(action.label())
                                    .when(action == FileAction::OpenWith, |row| {
                                        row.child(native_icons::symbol("chevron.right", 10.))
                                    })
                                    .children(action.shortcut().map(|shortcut| {
                                        div()
                                            .text_size(dp(text_size::DETAIL))
                                            .text_color(rgb(0x999999))
                                            .child(shortcut)
                                    }))
                                    .into_any_element()
                            }
                        }
                    })),
            )
            .when_some(menu.open_with.as_ref(), |overlay, apps| {
                let rows = apps.len().max(1) as f32;
                let height = (PADDING * 2. + rows * row::HEIGHT) * scale;
                // To the menu's right, or its left when there is no room.
                let right = left + (WIDTH - PADDING) * scale;
                let sub_left =
                    if right + SUBMENU_WIDTH * scale + margin <= f32::from(viewport.width) {
                        right
                    } else {
                        (left - (SUBMENU_WIDTH - PADDING) * scale).max(margin)
                    };
                let sub_top = (top + (open_with_top - PADDING) * scale)
                    .min(f32::from(viewport.height) - height - margin)
                    .max(margin);
                overlay.child(
                    div()
                        .id("open-with-menu")
                        .absolute()
                        .left(px(sub_left))
                        .top(px(sub_top))
                        .w(dp(SUBMENU_WIDTH))
                        .p(dp(PADDING))
                        .rounded(dp(radius::MD))
                        .bg(rgb(0x222222))
                        .shadow_lg()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                        .when(apps.is_empty(), |submenu| {
                            submenu.child(
                                div()
                                    .h(dp(row::HEIGHT))
                                    .px(dp(10.))
                                    .flex()
                                    .items_center()
                                    .text_color(rgb(0x777777))
                                    .child("No apps"),
                            )
                        })
                        .children(apps.iter().enumerate().map(|(index, (app, icon))| {
                            let path = app.path.clone();
                            div()
                                .id(("open-with", index))
                                .h(dp(row::HEIGHT))
                                .px(dp(8.))
                                .flex()
                                .items_center()
                                .gap(dp(row::GAP))
                                .rounded(dp(radius::SM))
                                .hover(|row| row.bg(rgb(0x303840)))
                                .cursor_pointer()
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.open_with(path.clone(), window, cx);
                                }))
                                .child(native_icons::file_icon(icon.clone(), row::ICON))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w(dp(0.))
                                        .text_ellipsis()
                                        .child(app.name.clone()),
                                )
                                .when(app.default, |row| {
                                    row.child(
                                        div()
                                            .text_size(dp(text_size::DETAIL))
                                            .text_color(rgb(0x999999))
                                            .child("Default"),
                                    )
                                })
                        })),
                )
            })
    }

    fn command_menu(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let width = dp(560.)
            .to_pixels(window.rem_size())
            .min((window.viewport_size().width - dp(32.).to_pixels(window.rem_size())).max(px(0.)));
        let items = self.menu_items(cx);
        let is_path =
            path_completion::folder(self.command_search.read(cx).text(), &self.home).is_some();
        let has_match = !items.is_empty();
        div()
            .id("command-menu-overlay")
            .absolute()
            .inset_0()
            .occlude()
            .bg(gpui::rgba(0x00000055))
            .flex()
            .justify_center()
            .items_start()
            .pt(dp(64.))
            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                cx.stop_propagation();
                window.dispatch_action(Box::new(ToggleCommandMenu), cx);
            })
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .id("command-menu")
                    .w(width)
                    .rounded(dp(radius::LG))
                    .bg(rgb(0x222222))
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .h(dp(52.))
                            .px(dp(16.))
                            .flex()
                            .items_center()
                            .gap(dp(10.))
                            .child(native_icons::symbol("magnifyingglass", row::ICON))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(dp(0.))
                                    .text_size(dp(text_size::INPUT))
                                    .line_height(dp(22.))
                                    .child(self.command_search.clone()),
                            ),
                    )
                    .when(!has_match, |panel| {
                        panel.child(
                            div()
                                .px(dp(16.))
                                .pb(dp(16.))
                                .text_color(rgb(0x999999))
                                .child(if is_path {
                                    "No matching files"
                                } else {
                                    "No matching commands"
                                }),
                        )
                    })
                    .when(has_match, |panel| {
                        // A gap sets commands apart from the recent items above.
                        let commands_start = items
                            .iter()
                            .position(|item| matches!(item, MenuItem::Command(_)))
                            .filter(|&position| position > 0);
                        panel.child(div().p(dp(8.)).pt(dp(0.)).children(
                            items.into_iter().enumerate().map(move |(position, item)| {
                                div()
                                    .id(("command", position))
                                    .when(Some(position) == commands_start, |row| row.mt(dp(8.)))
                                    .h(dp(40.))
                                    .px(dp(8.))
                                    .flex()
                                    .items_center()
                                    .gap(dp(10.))
                                    .rounded(dp(radius::SM))
                                    // Hover stays quieter than the keyboard selection.
                                    .when(position == self.selected_command, |row| {
                                        row.bg(rgb(0x303840))
                                    })
                                    .when(position != self.selected_command, |row| {
                                        row.hover(|row| row.bg(rgb(0x2a2e33)))
                                    })
                                    .cursor_pointer()
                                    .on_click({
                                        let item = item.clone();
                                        cx.listener(move |this, _, window, cx| {
                                            cx.stop_propagation();
                                            this.run_menu_item(item.clone(), window, cx);
                                        })
                                    })
                                    .child(match &item {
                                        MenuItem::Command(command) => {
                                            native_icons::symbol(command.icon(), row::ICON)
                                        }
                                        MenuItem::Path(completion) => native_icons::file_icon(
                                            completion.icon.clone(),
                                            row::ICON,
                                        ),
                                    })
                                    .child(div().flex_1().min_w(dp(0.)).text_ellipsis().child(
                                        match &item {
                                            MenuItem::Command(command) => {
                                                command.label().to_owned()
                                            }
                                            MenuItem::Path(completion) => completion.label.clone(),
                                        },
                                    ))
                                    .children(
                                        match &item {
                                            MenuItem::Command(command) => command.shortcut(),
                                            MenuItem::Path(_) => None,
                                        }
                                        .map(|shortcut| {
                                            div()
                                                .text_size(dp(text_size::DETAIL))
                                                .text_color(rgb(0xaaaaaa))
                                                .child(shortcut)
                                        }),
                                    )
                                    .children(match &item {
                                        MenuItem::Command(Command::HiddenFiles) => {
                                            Some(ui::checkbox(self.settings.show_hidden_files))
                                        }
                                        MenuItem::Command(Command::Sounds) => {
                                            Some(ui::checkbox(!self.settings.mute_sounds))
                                        }
                                        MenuItem::Command(Command::ArrowsHistory) => {
                                            Some(ui::checkbox(
                                                self.settings.arrow_keys
                                                    == storage::ArrowKeys::History,
                                            ))
                                        }
                                        MenuItem::Command(Command::ArrowsFolderPath) => {
                                            Some(ui::checkbox(
                                                self.settings.arrow_keys
                                                    == storage::ArrowKeys::FolderPath,
                                            ))
                                        }
                                        _ => None,
                                    })
                            }),
                        ))
                    }),
            )
    }
}

impl Render for FileBrowser {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.zoom.apply(window);
        native_icons::release_stale_rasters(window, cx);
        let sidebar_width = self.sidebar.width(window);
        let index_amount = self
            .sidebar
            .index_amount(!self.index_progress.ready, window);
        let sidebar_reveal = (sidebar_width
            / f32::from(dp(sidebar::WIDTH).to_pixels(window.rem_size())))
        .clamp(0., 1.);
        let titlebar = window_chrome::Titlebar::layout(window, sidebar_width);
        div()
            .relative()
            .track_focus(&self.focus)
            .when(self.context_menu.is_some(), |view| {
                view.key_context("ContextMenu")
            })
            // The menu's own shortcuts work while it is open, as in a native
            // menu: they run that item and close the menu.
            .on_action(cx.listener(|this, _: &MenuOpen, window, cx| {
                this.run_shortcut(FileAction::Open, window, cx)
            }))
            .on_action(cx.listener(|this, _: &MenuQuickLook, window, cx| {
                this.run_shortcut(FileAction::QuickLook, window, cx)
            }))
            .on_action(cx.listener(|this, _: &MenuCopyPath, window, cx| {
                this.run_shortcut(FileAction::CopyPath, window, cx)
            }))
            .on_action(cx.listener(|this, _: &MenuTrash, window, cx| {
                this.run_shortcut(FileAction::Trash, window, cx)
            }))
            .on_action(cx.listener(|this, _: &MenuNewFolder, window, cx| {
                this.run_shortcut(FileAction::NewFolder, window, cx)
            }))
            .on_action(cx.listener(|this, _: &MenuRename, window, cx| {
                this.run_shortcut(FileAction::Rename, window, cx)
            }))
            .on_action(cx.listener(|this, _: &MenuDuplicate, window, cx| {
                this.run_shortcut(FileAction::Duplicate, window, cx)
            }))
            .on_action(cx.listener(|this, _: &MenuCopy, window, cx| {
                this.run_shortcut(FileAction::Copy, window, cx)
            }))
            .on_action(cx.listener(|this, _: &MenuPaste, window, cx| {
                this.run_shortcut(FileAction::Paste, window, cx)
            }))
            .on_action(cx.listener(|this, _: &CopyFiles, _, cx| {
                let paths = this.selection(cx);
                if paths.is_empty() {
                    ui::beep();
                } else {
                    system::copy_files(&paths);
                }
            }))
            .on_action(
                cx.listener(|this, _: &PasteFiles, _, cx| this.paste(file_ops::Transfer::Copy, cx)),
            )
            .on_action(cx.listener(|this, _: &PasteMoveFiles, _, cx| {
                this.paste(file_ops::Transfer::Move, cx)
            }))
            .on_action(cx.listener(|this, _: &DuplicateFiles, _, cx| {
                let paths = this.selection(cx);
                this.duplicate(paths, cx)
            }))
            .on_action(cx.listener(|this, _: &Undo, _, cx| this.revert(true, cx)))
            .on_action(cx.listener(|this, _: &Redo, _, cx| this.revert(false, cx)))
            .on_action(cx.listener(|this, _: &CloseContextMenu, window, cx| {
                this.close_context_menu(window, cx)
            }))
            .on_action(cx.listener(|this, _: &ContextMenuPrevious, _, cx| {
                this.move_context_selection(-1, cx)
            }))
            .on_action(
                cx.listener(|this, _: &ContextMenuNext, _, cx| this.move_context_selection(1, cx)),
            )
            .on_action(cx.listener(|this, _: &ContextMenuRun, window, cx| {
                let action = this.context_menu.as_ref().and_then(|menu| {
                    let actions: Vec<FileAction> = FileAction::menu(&menu.paths, menu.can_paste)
                        .into_iter()
                        .flatten()
                        .collect();
                    menu.selected.and_then(|index| actions.get(index).copied())
                });
                match action {
                    Some(action) => this.run_file_action(action, window, cx),
                    None => this.close_context_menu(window, cx),
                }
            }))
            .when(self.command_menu_open, |view| {
                view.key_context("CommandMenu")
            })
            .when(self.settings_open, |view| view.key_context("Settings"))
            .on_action(cx.listener(|this, _: &ToggleSettings, window, cx| {
                this.settings_open = !this.settings_open;
                if this.settings_open {
                    this.command_menu_open = false;
                    this.tabs[this.active_tab]
                        .list
                        .update(cx, |list, cx| list.end_drag(cx));
                    this.index_usage = this.index.usage();
                    // Keep keys away from the list behind the panel.
                    this.focus.focus(window);
                } else {
                    this.focus_list(window, cx);
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleCommandMenu, window, cx| {
                this.settings_open = false;
                this.command_menu_open = !this.command_menu_open;
                if this.command_menu_open {
                    this.tabs[this.active_tab]
                        .list
                        .update(cx, |list, cx| list.end_drag(cx));
                    this.menu_recent = path_completion::recent(&this.recent, &this.home);
                    this.command_search
                        .update(cx, |input, cx| input.open(window, cx));
                } else {
                    this.focus_list(window, cx);
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &RunMenuCommand, window, cx| {
                if this.command_search.read(cx).composing() {
                    return;
                }
                if let Some(item) = this.menu_items(cx).into_iter().nth(this.selected_command) {
                    this.run_menu_item(item, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &CompleteMenuPath, _, cx| {
                let Some(MenuItem::Path(completion)) =
                    this.menu_items(cx).into_iter().nth(this.selected_command)
                else {
                    return;
                };
                let query = this.command_search.read(cx).text().to_owned();
                let text = path_completion::completed_text(&query, &this.home, &completion);
                this.command_search
                    .update(cx, |input, cx| input.set_text(text, cx));
                this.selected_command = 0;
                let query = this.command_search.read(cx).text().to_owned();
                this.read_children_for(&query, cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &PreviousCommand, _, cx| {
                this.selected_command = this.selected_command.saturating_sub(1);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &NextCommand, _, cx| {
                let count = this.menu_items(cx).len();
                this.selected_command = (this.selected_command + 1).min(count.saturating_sub(1));
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &CheckForUpdates, window, cx| {
                this.command_menu_open = false;
                this.focus_list(window, cx);
                this.check_for_updates(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ClearFileIndex, window, cx| {
                this.command_menu_open = false;
                this.focus_list(window, cx);
                this.index_progress.label = "Clearing".into();
                this.index_progress.percent = 0.;
                this.index_progress.ready = false;
                for tab in &mut this.tabs {
                    if !tab.query.is_empty() {
                        tab.request += 1;
                        tab.loading = false;
                        tab.list.update(cx, |list, cx| {
                            list.set_entries(Rc::from([]), cx);
                            list.start_loading(cx);
                        });
                    }
                }
                let index = this.index.clone();
                cx.background_executor()
                    .spawn(async move {
                        if let Err(error) = index.clear_and_rebuild() {
                            eprintln!("Clear file index: {error:#}");
                        }
                    })
                    .detach();
                cx.notify();
            }))
            .on_action(
                cx.listener(|this, _: &SelectTab1, window, cx| this.select_tab(0, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectTab2, window, cx| this.select_tab(1, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectTab3, window, cx| this.select_tab(2, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectTab4, window, cx| this.select_tab(3, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectTab5, window, cx| this.select_tab(4, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectTab6, window, cx| this.select_tab(5, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectTab7, window, cx| this.select_tab(6, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectTab8, window, cx| this.select_tab(7, window, cx)),
            )
            .on_action(cx.listener(|this, _: &SelectLastTab, window, cx| {
                this.select_tab(this.tabs.len() - 1, window, cx)
            }))
            .on_action(cx.listener(Self::toggle_hidden_files))
            .on_action(cx.listener(Self::toggle_sounds))
            .on_action(
                cx.listener(|this, _: &EditPath, window, cx| this.begin_path_edit(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &PathEditGo, window, cx| this.commit_path_edit(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &PathEditCancel, window, cx| this.end_path_edit(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &PathEditPrevious, _, cx| this.move_path_selection(-1, cx)),
            )
            .on_action(cx.listener(|this, _: &PathEditNext, _, cx| this.move_path_selection(1, cx)))
            .on_action(cx.listener(|this, _: &PathEditComplete, _, cx| this.complete_path_edit(cx)))
            .on_action(cx.listener(Self::go_left))
            .on_action(cx.listener(Self::go_right))
            .on_action(cx.listener(|this, _: &ArrowKeysHistory, _, cx| {
                this.set_arrow_keys(storage::ArrowKeys::History, cx)
            }))
            .on_action(cx.listener(|this, _: &ArrowKeysFolderPath, _, cx| {
                this.set_arrow_keys(storage::ArrowKeys::FolderPath, cx)
            }))
            .on_action(cx.listener(Self::toggle_search_scope))
            .on_action(cx.listener(Self::go_back))
            .on_action(cx.listener(Self::go_forward))
            .on_action(cx.listener(Self::go_up))
            .on_action(cx.listener(|this, _: &GoHome, window, cx| {
                this.navigate(this.home.clone(), window, cx)
            }))
            .on_action(cx.listener(|this, _: &GoDesktop, window, cx| {
                this.navigate(this.home.join("Desktop"), window, cx)
            }))
            .on_action(cx.listener(|this, _: &GoDocuments, window, cx| {
                this.navigate(this.home.join("Documents"), window, cx)
            }))
            .on_action(cx.listener(|this, _: &GoDownloads, window, cx| {
                this.navigate(this.home.join("Downloads"), window, cx)
            }))
            .on_action(cx.listener(|this, _: &GoApplications, window, cx| {
                this.navigate(PathBuf::from("/Applications"), window, cx)
            }))
            .on_action(cx.listener(|this, _: &GoPictures, window, cx| {
                this.navigate(this.home.join("Pictures"), window, cx)
            }))
            .on_action(cx.listener(|this, _: &GoMusic, window, cx| {
                this.navigate(this.home.join("Music"), window, cx)
            }))
            .on_action(cx.listener(|this, _: &GoMovies, window, cx| {
                this.navigate(this.home.join("Movies"), window, cx)
            }))
            .on_action(cx.listener(Self::new_tab))
            .on_action(cx.listener(Self::open_search))
            .on_action(cx.listener(Self::close_search))
            .on_action(cx.listener(|this, _: &ZoomIn, _, cx| {
                this.zoom.change(1.);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ZoomOut, _, cx| {
                this.zoom.change(-1.);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ResetZoom, window, cx| {
                this.zoom.reset();
                this.command_menu_open = false;
                this.focus_list(window, cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleFavorites, _, cx| {
                this.sidebar.toggle_favorites();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleRecent, _, cx| {
                this.sidebar.toggle_recent();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleLocations, _, cx| {
                this.sidebar.toggle_locations();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| {
                this.sidebar.toggle(Instant::now());
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &CloseTab, window, cx| {
                this.close_tab(this.active_tab, window, cx);
            }))
            .size_full()
            .flex()
            .bg(rgb(0x101010))
            .text_color(rgb(0xeeeeee))
            .font_family(".SystemUIFont")
            .text_size(dp(text_size::BODY))
            .child(self.sidebar.render(sidebar::SidebarView {
                width: sidebar_width,
                header_height: titlebar.sidebar_top,
                progress: &self.index_progress,
                index_amount,
                home: &self.home,
                location: &self.tabs[self.active_tab].location,
                recent: &self.recent,
                open_recent: {
                    let browser = cx.entity().downgrade();
                    Rc::new(move |path, window: &mut Window, cx: &mut App| {
                        let _ = browser.update(cx, |this, cx| this.open_recent(path, window, cx));
                    })
                },
                sounds_muted: self.settings.mute_sounds,
                volumes: &self.volumes,
                trash: &self.home.join(".Trash"),
                go_to: {
                    let browser = cx.entity().downgrade();
                    Rc::new(move |path, window: &mut Window, cx: &mut App| {
                        let _ = browser.update(cx, |this, cx| this.navigate(path, window, cx));
                    })
                },
                eject: {
                    let browser = cx.entity().downgrade();
                    Rc::new(move |path, cx: &mut App| {
                        let _ = browser.update(cx, |this, cx| this.eject(path, cx));
                    })
                },
                trash_files: {
                    let browser = cx.entity().downgrade();
                    Rc::new(move |paths, cx: &mut App| {
                        let _ = browser.update(cx, |this, cx| this.trash(paths, cx));
                    })
                },
                drop_into: {
                    let browser = cx.entity().downgrade();
                    Rc::new(move |paths, folder, cx: &mut App| {
                        let _ = browser.update(cx, |this, cx| this.drop_files(paths, folder, cx));
                    })
                },
            }))
            .when(sidebar_reveal > 0., |view| {
                view.child(
                    div()
                        .absolute()
                        .top_0()
                        .left(px(sidebar_width))
                        .right_0()
                        .h(titlebar.height)
                        .bg(gpui::Hsla::from(rgb(0x181818)).opacity(sidebar_reveal)),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .h_full()
                    .min_w(dp(0.))
                    .min_h(dp(0.))
                    .child(self.tab_row(window, sidebar_width, &titlebar, cx))
                    .child(self.path_bar(cx))
                    .child(
                        div()
                            .flex_1()
                            .min_h(dp(0.))
                            .child(self.tabs[self.active_tab].list.clone()),
                    ),
            )
            .child(
                div()
                    .id("toggle-sidebar")
                    .absolute()
                    .left(titlebar.toggle_left)
                    .top(titlebar.toggle_top)
                    .size(titlebar.toggle_size)
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(dp(radius::SM))
                    .cursor_pointer()
                    .hover(|button| button.bg(rgb(0x353535)))
                    .on_click(|_, window, cx| {
                        cx.stop_propagation();
                        window.dispatch_action(Box::new(ToggleSidebar), cx);
                    })
                    .child(native_icons::symbol("square.leftthird.inset.filled", 22.)),
            )
            .when_some(
                titlebar.update_left.filter(|_| self.update.is_some()),
                |view, left| {
                    let progress = match &self.update {
                        Some(UpdateState::Downloading(fraction)) => Some(*fraction),
                        Some(UpdateState::Installing) => Some(1.),
                        _ => None,
                    };
                    view.child(
                        div()
                            .id("update")
                            .absolute()
                            .left(left)
                            .top(titlebar.update_top)
                            .w(titlebar.update_width)
                            .h(titlebar.update_height)
                            .flex()
                            .items_center()
                            .justify_center()
                            .gap(dp(4.))
                            .overflow_hidden()
                            .rounded(dp(radius::SM))
                            .text_size(dp(text_size::DETAIL))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(rgb(0xffffff))
                            .map(|button| match (&self.update, progress) {
                                // Checking: a spinner beside the sidebar toggle.
                                (Some(UpdateState::Checking(started)), _) => {
                                    button.justify_end().child(ui::spinner(*started, 14.))
                                }
                                // Offered: a solid accent button.
                                (_, None) => button
                                    .bg(rgb(0x0a84ff))
                                    .hover(|button| button.bg(rgb(0x2a94ff)))
                                    .cursor_pointer()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        cx.stop_propagation();
                                        this.start_update(cx);
                                    }))
                                    .child(native_icons::symbol_on_accent("arrow.down.circle", 12.))
                                    .child("Update"),
                                // Installing: the accent fills the button as the
                                // download arrives, behind the label.
                                (_, Some(fraction)) => button
                                    .bg(rgb(0x1d3a5c))
                                    .child(
                                        div()
                                            .absolute()
                                            .top_0()
                                            .bottom_0()
                                            .left_0()
                                            .w(gpui::relative(fraction.clamp(0., 1.)))
                                            .bg(rgb(0x0a84ff)),
                                    )
                                    .child(div().relative().child(if fraction >= 1. {
                                        "Installing"
                                    } else {
                                        "Updating"
                                    })),
                            }),
                    )
                },
            )
            .when(self.command_menu_open, |view| {
                view.child(self.command_menu(window, cx))
            })
            .when(self.settings_open, |view| {
                view.child(settings::render(self.index_usage.as_ref(), &self.settings))
            })
            .when_some(self.context_menu.as_ref(), |view, menu| {
                view.child(self.context_menu_view(menu, window, cx))
            })
    }
}

fn file_list_view(
    entries: Rc<[FileEntry]>,
    window: &mut Window,
    cx: &mut Context<FileBrowser>,
) -> Entity<FileList> {
    let list = cx.new(|cx| FileList::new(entries, cx));
    cx.subscribe_in(&list, window, |this, list, request, window, cx| {
        this.handle_list_request(list, request, window, cx)
    })
    .detach();
    list
}

/// One command in the right-click menu.
#[derive(Clone, Copy, PartialEq)]
enum FileAction {
    Open,
    OpenWith,
    QuickLook,
    Rename,
    Duplicate,
    Copy,
    CopyPath,
    Paste,
    ShowInFinder,
    Trash,
    NewFolder,
}

impl FileAction {
    fn label(self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::OpenWith => "Open With",
            Self::QuickLook => "Quick Look",
            Self::Rename => "Rename",
            Self::Duplicate => "Duplicate",
            Self::Copy => "Copy",
            Self::CopyPath => "Copy Path",
            Self::Paste => "Paste",
            Self::ShowInFinder => "Show in Finder",
            Self::Trash => "Move to Trash",
            Self::NewFolder => "New Folder",
        }
    }

    fn shortcut(self) -> Option<&'static str> {
        match self {
            Self::Open => Some("⌘↓"),
            Self::OpenWith | Self::ShowInFinder => None,
            Self::QuickLook => Some("Space"),
            Self::Rename => Some("R"),
            Self::Duplicate => Some("⌘D"),
            Self::Copy => Some("⌘C"),
            Self::CopyPath => Some("⌥⌘C"),
            Self::Paste => Some("⌘V"),
            Self::Trash => Some("⌘⌫"),
            Self::NewFolder => Some("⇧⌘N"),
        }
    }

    /// The menu for `paths`, `None` marking separators. Blank space (no
    /// paths) offers the current folder's actions. Paste appears only when
    /// the clipboard holds files.
    fn menu(paths: &[PathBuf], can_paste: bool) -> Vec<Option<Self>> {
        if paths.is_empty() {
            let mut items = vec![Some(Self::NewFolder)];
            if can_paste {
                items.push(Some(Self::Paste));
            }
            items.extend([None, Some(Self::CopyPath), Some(Self::ShowInFinder)]);
            return items;
        }
        let mut items = vec![
            Some(Self::Open),
            Some(Self::OpenWith),
            Some(Self::QuickLook),
            None,
        ];
        if paths.len() == 1 {
            items.push(Some(Self::Rename));
        }
        items.extend([
            Some(Self::Duplicate),
            Some(Self::Copy),
            Some(Self::CopyPath),
            None,
            Some(Self::ShowInFinder),
            None,
            Some(Self::Trash),
            None,
            Some(Self::NewFolder),
        ]);
        if can_paste {
            items.push(Some(Self::Paste));
        }
        items
    }
}

/// The Update button's state.
#[derive(Clone, Debug, PartialEq)]
enum UpdateState {
    Available(updater::Release),
    /// The fraction downloaded, from 0 to 1.
    Downloading(f32),
    /// Verifying and swapping in the new build, then restarting.
    Installing,
    /// Looking for a release because the user asked, since the instant
    /// given (the spinner's start).
    Checking(Instant),
}

/// Typing a path in the path bar.
struct PathEdit {
    /// Suggestions show once the user types, not for the path as it opened.
    typed: bool,
    /// The highlighted suggestion, once the keyboard has moved.
    selected: Option<usize>,
    _blur: gpui::Subscription,
}

/// An open right-click menu.
struct ContextMenu {
    /// Where it was opened, in window coordinates.
    position: gpui::Point<gpui::Pixels>,
    paths: Vec<PathBuf>,
    /// The highlighted action, once the keyboard has moved.
    selected: Option<usize>,
    can_paste: bool,
    /// The Open With submenu's apps, with icons, while it is showing.
    open_with: Option<Vec<(system::App, native_icons::FileIcon)>>,
}

/// Clickable path components: from the home folder when inside it,
/// otherwise from the startup disk.
fn path_segments(location: &std::path::Path, home: &std::path::Path) -> Vec<(String, PathBuf)> {
    let (mut path, rest, first) = match location.strip_prefix(home) {
        Ok(rest) => (
            home.to_path_buf(),
            rest,
            home.file_name()
                .unwrap_or(home.as_os_str())
                .to_string_lossy()
                .into_owned(),
        ),
        Err(_) => (
            PathBuf::from("/"),
            location.strip_prefix("/").unwrap_or(location),
            "Macintosh HD".to_owned(),
        ),
    };
    let mut segments = vec![(first, path.clone())];
    for component in rest.components() {
        path.push(component);
        segments.push((
            component.as_os_str().to_string_lossy().into_owned(),
            path.clone(),
        ));
    }
    segments
}

fn history_button(
    id: &'static str,
    symbol: &'static str,
    enabled: bool,
    action: fn() -> Box<dyn gpui::Action>,
) -> impl IntoElement {
    div()
        .id(id)
        .flex_shrink_0()
        .size(dp(28.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(dp(radius::SM))
        .when(!enabled, |button| button.opacity(0.35))
        .when(enabled, |button| {
            button
                .cursor_pointer()
                .hover(|button| button.bg(rgb(0x252525)))
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    window.dispatch_action(action(), cx);
                })
        })
        .child(native_icons::symbol(symbol, 14.))
}

/// Where one search runs (see `FileBrowser::search_source`).
enum SearchSource {
    Index(search_index::SearchIndex, Option<PathBuf>),
    Folder(Arc<search_index::FolderIndex>),
}

/// An on-the-spot folder index being built; dropping it stops the crawl.
struct FolderBuild {
    folder: PathBuf,
    cancelled: Arc<AtomicBool>,
    _task: gpui::Task<()>,
}

impl Drop for FolderBuild {
    fn drop(&mut self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Mounted volumes with their system icons, for Locations.
fn listed_volumes() -> Vec<(system::Volume, native_icons::FileIcon)> {
    system::volumes()
        .into_iter()
        .map(|volume| {
            let icon = classify(&volume.path);
            (volume, icon)
        })
        .collect()
}

fn classify(path: &std::path::Path) -> native_icons::FileIcon {
    native_icons::FileIcon::classify(path, std::fs::symlink_metadata(path).ok().as_ref())
}

/// Saved recent items that still exist, with their icons.
fn classify_recent(recent: Vec<PathBuf>) -> Vec<(PathBuf, native_icons::FileIcon)> {
    recent
        .into_iter()
        .filter(|path| path.exists())
        .take(storage::RECENT_LIMIT)
        .map(|path| {
            let icon = classify(&path);
            (path, icon)
        })
        .collect()
}

/// The window's restorable frame; fullscreen restores to its windowed frame.
fn window_frame(window: &Window) -> Option<storage::WindowFrame> {
    let (bounds, maximized) = match window.window_bounds() {
        WindowBounds::Windowed(bounds) | WindowBounds::Fullscreen(bounds) => (bounds, false),
        WindowBounds::Maximized(bounds) => (bounds, true),
    };
    (bounds.size.width > px(0.) && bounds.size.height > px(0.)).then(|| storage::WindowFrame {
        x: f32::from(bounds.origin.x),
        y: f32::from(bounds.origin.y),
        width: f32::from(bounds.size.width),
        height: f32::from(bounds.size.height),
        maximized,
    })
}

/// The saved frame if it still lands on a connected display, else centered.
fn initial_bounds(frame: Option<storage::WindowFrame>, cx: &App) -> WindowBounds {
    let default = WindowBounds::Windowed(Bounds::centered(None, size(px(960.), px(640.)), cx));
    let Some(frame) = frame else {
        return default;
    };
    let bounds = Bounds::new(
        gpui::point(px(frame.x), px(frame.y)),
        size(px(frame.width.max(480.)), px(frame.height.max(320.))),
    );
    let visible = cx.displays().iter().any(|display| {
        let overlap = display.bounds().intersect(&bounds);
        overlap.size.width >= px(100.) && overlap.size.height >= px(100.)
    });
    match (visible, frame.maximized) {
        (false, _) => default,
        (true, true) => WindowBounds::Maximized(bounds),
        (true, false) => WindowBounds::Windowed(bounds),
    }
}

fn main() {
    let Some(home) = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
    else {
        eprintln!("A valid HOME directory is required");
        return;
    };
    Application::new().run(move |cx: &mut App| {
        search_input::init(cx);
        file_list::init(cx);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-t", NewTab, None),
            KeyBinding::new("cmd-1", SelectTab1, None),
            KeyBinding::new("cmd-2", SelectTab2, None),
            KeyBinding::new("cmd-3", SelectTab3, None),
            KeyBinding::new("cmd-4", SelectTab4, None),
            KeyBinding::new("cmd-5", SelectTab5, None),
            KeyBinding::new("cmd-6", SelectTab6, None),
            KeyBinding::new("cmd-7", SelectTab7, None),
            KeyBinding::new("cmd-8", SelectTab8, None),
            KeyBinding::new("cmd-9", SelectLastTab, None),
            KeyBinding::new("cmd-r", ToggleSidebar, None),
            KeyBinding::new("cmd-w", CloseTab, None),
            KeyBinding::new("cmd--", ZoomOut, None),
            KeyBinding::new("cmd-=", ZoomIn, None),
            KeyBinding::new("cmd-+", ZoomIn, None),
            KeyBinding::new("cmd-0", ResetZoom, None),
            KeyBinding::new("cmd-k", ToggleCommandMenu, None),
            KeyBinding::new("cmd-,", ToggleSettings, None),
            // macOS reports shifted punctuation as its shifted character.
            KeyBinding::new("cmd->", ToggleHiddenFiles, None),
            KeyBinding::new("cmd-f", OpenSearch, None),
            KeyBinding::new("cmd-[", GoBack, None),
            KeyBinding::new("cmd-]", GoForward, None),
            // Scoped to the list so text fields keep ⌘←/⌘→ for line start and end.
            KeyBinding::new("shift-cmd-g", EditPath, None),
            KeyBinding::new("cmd-l", EditPath, None),
            KeyBinding::new("enter", PathEditGo, Some("PathEdit")),
            KeyBinding::new("escape", PathEditCancel, Some("PathEdit")),
            KeyBinding::new("up", PathEditPrevious, Some("PathEdit")),
            KeyBinding::new("down", PathEditNext, Some("PathEdit")),
            KeyBinding::new("tab", PathEditComplete, Some("PathEdit")),
            KeyBinding::new("cmd-left", GoLeft, Some("FileList")),
            KeyBinding::new("cmd-right", GoRight, Some("FileList")),
            KeyBinding::new("cmd-up", GoUp, None),
            KeyBinding::new("escape", CloseSearch, Some("FileSearch")),
            KeyBinding::new("escape", ToggleCommandMenu, Some("CommandMenu")),
            KeyBinding::new("escape", ToggleSettings, Some("Settings")),
            KeyBinding::new("enter", RunMenuCommand, Some("CommandMenu")),
            KeyBinding::new("up", PreviousCommand, Some("CommandMenu")),
            KeyBinding::new("down", NextCommand, Some("CommandMenu")),
            KeyBinding::new("tab", CompleteMenuPath, Some("CommandMenu")),
            KeyBinding::new("up", ContextMenuPrevious, Some("ContextMenu")),
            KeyBinding::new("down", ContextMenuNext, Some("ContextMenu")),
            KeyBinding::new("enter", ContextMenuRun, Some("ContextMenu")),
            KeyBinding::new("escape", CloseContextMenu, Some("ContextMenu")),
            KeyBinding::new("cmd-down", MenuOpen, Some("ContextMenu")),
            KeyBinding::new("space", MenuQuickLook, Some("ContextMenu")),
            KeyBinding::new("alt-cmd-c", MenuCopyPath, Some("ContextMenu")),
            KeyBinding::new("cmd-backspace", MenuTrash, Some("ContextMenu")),
            KeyBinding::new("shift-cmd-n", MenuNewFolder, Some("ContextMenu")),
            KeyBinding::new("r", MenuRename, Some("ContextMenu")),
            KeyBinding::new("cmd-d", MenuDuplicate, Some("ContextMenu")),
            KeyBinding::new("cmd-c", MenuCopy, Some("ContextMenu")),
            KeyBinding::new("cmd-v", MenuPaste, Some("ContextMenu")),
            // File commands act on the list, so text fields keep their own
            // copy, paste, and undo.
            KeyBinding::new("cmd-c", CopyFiles, Some("FileList")),
            KeyBinding::new("cmd-v", PasteFiles, Some("FileList")),
            KeyBinding::new("alt-cmd-v", PasteMoveFiles, Some("FileList")),
            KeyBinding::new("cmd-d", DuplicateFiles, Some("FileList")),
            KeyBinding::new("cmd-z", Undo, Some("FileList")),
            KeyBinding::new("shift-cmd-z", Redo, Some("FileList")),
        ]);
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let session: storage::Session = storage::load(&storage::session_path(&home));
        let bounds = initial_bounds(session.window, cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(bounds),
                // Extend the content underneath the native title bar and traffic lights.
                titlebar: Some(TitlebarOptions {
                    appears_transparent: true,
                    ..Default::default()
                }),
                ..Default::default()
            },
            move |window, cx| {
                window.set_window_title("Filetree");
                cx.new(|cx| {
                    let browser = FileBrowser::new(home.clone(), session.clone(), window, cx);
                    browser.focus_list(window, cx);
                    browser
                })
            },
        )
        .expect("failed to open Filetree window");
        cx.activate(true);
    });
}
