use crate::{
    native_icons,
    search_input::SearchInput,
    ui::{self, dp, radius, row, text_size},
};
use anyhow::Context as _;
use chrono::{DateTime, Local};
use gpui::{
    Animation, AnimationExt, App, BorderStyle, Bounds, Context, CursorStyle, DispatchPhase, Entity,
    EventEmitter, ExternalPaths, FocusHandle, Focusable, IntoElement, KeyBinding, MouseButton,
    MouseDownEvent, MouseMoveEvent, Pixels, Point, Render, ScrollHandle, SharedString,
    Subscription, Task, UniformListScrollHandle, Window, actions, canvas, div, ease_in_out, point,
    prelude::*, px, quad, relative, rgb, rgba, size, uniform_list,
};
use std::{
    cmp::Ordering,
    collections::HashSet,
    fs,
    ops::Range,
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant, SystemTime},
};

actions!(
    file_list,
    [
        SelectAll,
        ClearSelection,
        OpenSelected,
        SelectPrevious,
        SelectNext,
        ExtendPrevious,
        ExtendNext,
        SelectFirst,
        SelectLast,
        QuickLookSelected,
        TrashSelected,
        CopySelectedPaths,
        NewFolder,
        CommitRename,
        CancelRename,
        RenameSelected
    ]
);

pub(super) fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-a", SelectAll, Some("FileList")),
        KeyBinding::new("escape", ClearSelection, Some("FileList")),
        KeyBinding::new("enter", OpenSelected, Some("FileList")),
        KeyBinding::new("cmd-down", OpenSelected, Some("FileList")),
        KeyBinding::new("up", SelectPrevious, Some("FileList")),
        KeyBinding::new("down", SelectNext, Some("FileList")),
        KeyBinding::new("shift-up", ExtendPrevious, Some("FileList")),
        KeyBinding::new("shift-down", ExtendNext, Some("FileList")),
        KeyBinding::new("alt-up", SelectFirst, Some("FileList")),
        KeyBinding::new("alt-down", SelectLast, Some("FileList")),
        KeyBinding::new("space", QuickLookSelected, Some("FileList")),
        KeyBinding::new("cmd-backspace", TrashSelected, Some("FileList")),
        KeyBinding::new("alt-cmd-c", CopySelectedPaths, Some("FileList")),
        KeyBinding::new("shift-cmd-n", NewFolder, Some("FileList")),
        KeyBinding::new("r", RenameSelected, Some("FileList")),
        KeyBinding::new("enter", CommitRename, Some("Rename")),
        KeyBinding::new("escape", CancelRename, Some("Rename")),
    ]);
}

/// What the list asks the browser to do. The list owns selection and inline
/// editing; the browser performs file operations and shows menus.
pub(super) enum Request {
    /// Double-click, Enter, or ⌘↓.
    Open(Vec<PathBuf>),
    QuickLook(Vec<PathBuf>),
    Trash(Vec<PathBuf>),
    CopyPath(Vec<PathBuf>),
    NewFolder,
    /// An inline rename was committed with a changed name.
    Rename {
        from: PathBuf,
        name: String,
    },
    /// Files dropped from this list, another tab, or any app. `into` is the
    /// folder row they were dropped on, or `None` for the list itself.
    Drop {
        paths: Vec<PathBuf>,
        into: Option<PathBuf>,
    },
    /// A drag of these paths out of the list ended, wherever it dropped.
    DragEnded(Vec<PathBuf>),
    /// A right-click at `position` (window coordinates). `paths` is the
    /// selection it applies to, empty on blank space.
    ContextMenu {
        position: Point<Pixels>,
        paths: Vec<PathBuf>,
    },
}

impl EventEmitter<Request> for FileList {}

/// An inline rename in progress.
struct Renaming {
    path: PathBuf,
    editor: Entity<SearchInput>,
    _blur: Subscription,
}

/// Folders browse in place; files and packages such as `.app` bundles open
/// in their default application.
pub(super) fn opens_as_folder(path: &std::path::Path) -> bool {
    use cocoa::base::{NO, id, nil};
    use objc::{class, msg_send, sel, sel_impl};
    use std::os::unix::ffi::OsStrExt;

    if !path.is_dir() {
        return false;
    }
    let bytes = path.as_os_str().as_bytes();
    // SAFETY: Foundation and NSWorkspace calls on the main thread; the
    // returned string is autoreleased and not retained past this call.
    unsafe {
        let manager: id = msg_send![class!(NSFileManager), defaultManager];
        let native_path: id = msg_send![manager,
            stringWithFileSystemRepresentation: bytes.as_ptr().cast::<std::ffi::c_char>()
            length: bytes.len()
        ];
        if native_path == nil {
            return true;
        }
        let workspace: id = msg_send![class!(NSWorkspace), sharedWorkspace];
        let package: cocoa::base::BOOL = msg_send![workspace, isFilePackageAtPath: native_path];
        package == NO
    }
}

#[derive(Clone, PartialEq)]
pub(super) struct FileEntry {
    pub name: SharedString,
    pub path: PathBuf,
    sort_name: String,
    modified: Option<SystemTime>,
    bytes: Option<u64>,
    icon: native_icons::FileIcon,
    /// A folder that accepts drops: not a package such as `.app`.
    drop_target: bool,
}

pub(super) fn read_directory(directory: &std::path::Path) -> anyhow::Result<Vec<FileEntry>> {
    fsearch::no_materialize();
    let listing = match fs::read_dir(directory) {
        Ok(listing) => listing,
        // Denied by macOS privacy settings (or plain permissions): say where
        // to grant access rather than showing "Operation not permitted".
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            let name = directory
                .file_name()
                .unwrap_or(directory.as_os_str())
                .to_string_lossy();
            // Desktop, Documents, Downloads, and volumes have their own
            // switches; everything else protected, such as the Trash, needs
            // Full Disk Access.
            let own_switch = ["Desktop", "Documents", "Downloads"].contains(&name.as_ref())
                || directory.starts_with("/Volumes");
            let setting = if own_switch {
                "Files and Folders"
            } else {
                "Full Disk Access"
            };
            let name = if name == ".Trash" {
                "Trash".into()
            } else {
                name
            };
            anyhow::bail!(
                "Filetree doesn’t have access to “{name}”. Allow it in System Settings › \
                 Privacy & Security › {setting}."
            );
        }
        Err(error) => {
            return Err(error).with_context(|| format!("Could not read {}", directory.display()));
        }
    };
    let paths = listing
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    // Name order matches the default view, so refreshed listings that keep
    // the current order can be applied without re-sorting.
    let mut entries = entries_for_paths(paths);
    entries.sort_unstable_by(|a, b| compare_entries(a, b, Column::Name, true));
    Ok(entries)
}

/// Reads each path's metadata once, across cores: a folder of tens of
/// thousands of files is bound by one metadata call per entry. Display labels
/// are formatted per visible row at render time instead of stored per entry.
pub(super) fn entries_for_paths(paths: Vec<PathBuf>) -> Vec<FileEntry> {
    use rayon::prelude::*;
    paths
        .into_par_iter()
        .with_min_len(256)
        .map(|path| {
            let name: SharedString = path
                .file_name()
                .unwrap_or(path.as_os_str())
                .to_string_lossy()
                .into_owned()
                .into();
            let link = fs::symlink_metadata(&path).ok();
            // Only links need a second read, for the size and date of their target.
            let target = match &link {
                Some(meta) if meta.file_type().is_symlink() => fs::metadata(&path).ok(),
                _ => link.clone(),
            };
            FileEntry {
                drop_target: target.as_ref().is_some_and(|meta| meta.is_dir())
                    && path.extension().is_none(),
                icon: native_icons::FileIcon::classify(&path, link.as_ref()),
                sort_name: name.to_lowercase(),
                name,
                modified: target.as_ref().and_then(|meta| meta.modified().ok()),
                bytes: target
                    .as_ref()
                    .filter(|meta| meta.is_file())
                    .map(|meta| meta.len()),
                path,
            }
        })
        .collect()
}

/// Drops dotfiles. In a folder only the entry's own name counts, so browsing
/// inside a hidden folder still lists it; search results also drop anything
/// inside a hidden folder. Returns the same listing when nothing is hidden.
pub(super) fn without_hidden(entries: Rc<[FileEntry]>, in_search: bool) -> Rc<[FileEntry]> {
    use std::os::unix::ffi::OsStrExt;
    let hidden = |entry: &FileEntry| {
        if in_search {
            entry
                .path
                .components()
                .any(|component| component.as_os_str().as_bytes().starts_with(b"."))
        } else {
            entry.name.starts_with('.')
        }
    };
    if !entries.iter().any(hidden) {
        return entries;
    }
    entries
        .iter()
        .filter(|entry| !hidden(entry))
        .cloned()
        .collect()
}

/// Local-time modification label, using Today and Yesterday for recent dates.
fn modified_label(modified: Option<SystemTime>, today: chrono::NaiveDate) -> SharedString {
    let Some(time) = modified else {
        return "—".into();
    };
    let date: DateTime<Local> = time.into();
    let format = if date.date_naive() == today {
        "Today, %-I:%M %p"
    } else if today.pred_opt() == Some(date.date_naive()) {
        "Yesterday, %-I:%M %p"
    } else {
        "%b %-d, %Y, %-I:%M %p"
    };
    date.format(format).to_string().into()
}

pub(super) fn format_size(bytes: u64) -> String {
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    for unit in ["KB", "MB", "GB", "TB", "PB", "EB"] {
        value /= 1000.;
        if value < 1000. || unit == "EB" {
            return format!("{value:.1} {unit}");
        }
    }
    unreachable!("all u64 sizes fit the unit scale")
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Column {
    Name,
    Modified,
    Size,
}

fn optional_cmp<T: Ord>(a: Option<T>, b: Option<T>, ascending: bool) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => {
            if ascending {
                a.cmp(&b)
            } else {
                b.cmp(&a)
            }
        }
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn compare_entries(a: &FileEntry, b: &FileEntry, column: Column, ascending: bool) -> Ordering {
    let primary = match column {
        Column::Name => {
            if ascending {
                a.sort_name.cmp(&b.sort_name)
            } else {
                b.sort_name.cmp(&a.sort_name)
            }
        }
        Column::Modified => optional_cmp(a.modified, b.modified, ascending),
        Column::Size => optional_cmp(a.bytes, b.bytes, ascending),
    };
    primary
        .then_with(|| a.sort_name.cmp(&b.sort_name))
        .then_with(|| a.path.cmp(&b.path))
}

struct ColumnResize {
    column: Column,
    start_x: Pixels,
    width: f32,
}

// Sorted, disjoint half-open ranges. Selecting a million contiguous files
// takes one range; membership is a binary search rather than a per-file set.
#[derive(Clone, Default, Debug, PartialEq, Eq)]
struct Selection(Vec<Range<usize>>);

impl Selection {
    fn contains(&self, index: usize) -> bool {
        let slot = self.0.partition_point(|range| range.end <= index);
        self.0.get(slot).is_some_and(|range| range.contains(&index))
    }

    fn replace(&mut self, range: Range<usize>) {
        self.0.clear();
        if !range.is_empty() {
            self.0.push(range);
        }
    }

    fn add(&mut self, mut range: Range<usize>) {
        if range.is_empty() {
            return;
        }
        let start = self.0.partition_point(|item| item.end < range.start);
        let mut end = start;
        while end < self.0.len() && self.0[end].start <= range.end {
            range.start = range.start.min(self.0[end].start);
            range.end = range.end.max(self.0[end].end);
            end += 1;
        }
        self.0.splice(start..end, [range]);
    }

    fn toggle(&mut self, range: Range<usize>) {
        if range.is_empty() {
            return;
        }
        let mut edges: Vec<usize> = self
            .0
            .iter()
            .flat_map(|item| [item.start, item.end])
            .collect();
        for point in [range.start, range.end] {
            let index = edges.partition_point(|edge| *edge < point);
            edges.insert(index, point);
        }
        self.0.clear();
        let mut open = None;
        let mut index = 0;
        while index < edges.len() {
            let point = edges[index];
            let mut end = index + 1;
            while end < edges.len() && edges[end] == point {
                end += 1;
            }
            if (end - index) % 2 == 1 {
                if let Some(start) = open.take() {
                    self.0.push(start..point);
                } else {
                    open = Some(point);
                }
            }
            index = end;
        }
    }
}

fn selection_in_order(order: &[usize], selected_entries: &[bool]) -> Selection {
    let mut selection = Selection::default();
    let mut start = None;
    for (row, entry) in order.iter().enumerate() {
        if selected_entries[*entry] {
            start.get_or_insert(row);
        } else if let Some(first) = start.take() {
            selection.0.push(first..row);
        }
    }
    if let Some(first) = start {
        selection.0.push(first..order.len());
    }
    selection
}

struct Drag {
    origin: Point<Pixels>,
    current: Point<Pixels>,
    baseline: Selection,
    extend: bool,
    toggle: bool,
    active: bool,
    hits: Range<usize>,
    last_frame: Instant,
    row_height: Pixels,
}

pub(super) struct FileList {
    entries: Rc<[FileEntry]>,
    order: Vec<usize>,
    sort_column: Column,
    ascending: bool,
    explicit_sort: bool,
    modified_width: f32,
    size_width: f32,
    resizing: Option<ColumnResize>,
    message: Option<String>,
    /// Pending until a load runs long enough to deserve the bar, so quick
    /// results never flash it.
    loading: Option<Task<()>>,
    show_loading: bool,
    scroll: ScrollHandle,
    virtual_scroll: UniformListScrollHandle,
    focus: FocusHandle,
    selected: Selection,
    anchor: Option<usize>,
    drag: Option<Drag>,
    /// Refreshed rows waiting for a gesture to end, and the empty-list message.
    pending_entries: Option<(Rc<[FileEntry]>, &'static str)>,
    renaming: Option<Renaming>,
    /// A press on a row that may become a file drag.
    press: Option<Press>,
    /// Paths in a native drag that has not ended yet.
    dragging_out: Option<Vec<PathBuf>>,
}

/// A mouse press on a row, until it is released or becomes a drag.
struct Press {
    origin: Point<Pixels>,
    /// A plain press on an already selected row keeps the whole selection,
    /// so it can be dragged, and selects only that row on release.
    collapse_to: Option<usize>,
}

impl FileList {
    pub(super) fn new(entries: Rc<[FileEntry]>, cx: &mut Context<Self>) -> Self {
        let virtual_scroll = UniformListScrollHandle::new();
        let scroll = virtual_scroll.0.borrow().base_handle.clone();
        let mut order: Vec<usize> = (0..entries.len()).collect();
        order.sort_unstable_by(|a, b| {
            compare_entries(&entries[*a], &entries[*b], Column::Name, true)
        });
        Self {
            entries,
            order,
            sort_column: Column::Name,
            ascending: true,
            explicit_sort: false,
            modified_width: 188.,
            size_width: 92.,
            resizing: None,
            message: None,
            loading: None,
            show_loading: false,
            scroll,
            virtual_scroll,
            focus: cx.focus_handle(),
            selected: Selection::default(),
            anchor: None,
            drag: None,
            pending_entries: None,
            renaming: None,
            press: None,
            dragging_out: None,
        }
    }

    /// Shown centered in place of rows when the list is empty.
    pub(super) fn set_message(&mut self, message: Option<String>, cx: &mut Context<Self>) {
        self.message = message;
        self.stop_loading();
        cx.notify();
    }

    /// Shows a thin indeterminate bar at the top once a load takes over 150ms.
    pub(super) fn start_loading(&mut self, cx: &mut Context<Self>) {
        self.loading = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(150))
                .await;
            let _ = this.update(cx, |this, cx| {
                this.show_loading = true;
                cx.notify();
            });
        }));
    }

    fn stop_loading(&mut self) {
        self.loading = None;
        self.show_loading = false;
    }

    pub(super) fn set_search_entries(&mut self, entries: Rc<[FileEntry]>, cx: &mut Context<Self>) {
        self.replace_entries(entries, self.explicit_sort, cx);
    }

    pub(super) fn set_entries(&mut self, entries: Rc<[FileEntry]>, cx: &mut Context<Self>) {
        self.replace_entries(entries, true, cx);
    }

    /// Refreshed rows for the same folder or search retain selection by path,
    /// sorting, and scroll offset. Let an in-progress gesture finish against
    /// the rows it started with.
    pub(super) fn reconcile_entries(
        &mut self,
        entries: Rc<[FileEntry]>,
        empty_message: &'static str,
        cx: &mut Context<Self>,
    ) {
        self.stop_loading();
        self.pending_entries = Some((entries, empty_message));
        if self.drag.is_none() && self.resizing.is_none() {
            self.apply_pending_entries();
        }
        cx.notify();
    }

    fn apply_pending_entries(&mut self) -> bool {
        let Some((entries, empty_message)) = self.pending_entries.take() else {
            return false;
        };
        self.message = entries.is_empty().then(|| empty_message.into());
        if self.entries == entries {
            return false;
        }
        let selected: HashSet<_> = self
            .order
            .iter()
            .enumerate()
            .filter(|(row, _)| self.selected.contains(*row))
            .map(|(_, entry)| self.entries[*entry].path.clone())
            .collect();
        let anchor = self
            .anchor
            .and_then(|row| self.order.get(row))
            .map(|entry| self.entries[*entry].path.clone());
        self.entries = entries;
        self.order = (0..self.entries.len()).collect();
        if self.explicit_sort {
            self.reorder();
        }
        let selected_entries: Vec<_> = self
            .entries
            .iter()
            .map(|entry| selected.contains(&entry.path))
            .collect();
        self.selected = selection_in_order(&self.order, &selected_entries);
        self.anchor = anchor.and_then(|path| {
            self.order
                .iter()
                .position(|entry| self.entries[*entry].path == path)
        });
        true
    }

    fn replace_entries(&mut self, entries: Rc<[FileEntry]>, sort: bool, cx: &mut Context<Self>) {
        self.message = None;
        self.stop_loading();
        self.scroll.set_offset(point(px(0.), px(0.)));
        self.entries = entries;
        self.order = (0..self.entries.len()).collect();
        self.selected = Selection::default();
        self.anchor = None;
        self.drag = None;
        self.pending_entries = None;
        if sort {
            self.reorder();
        }
        cx.notify();
    }

    fn reorder(&mut self) {
        self.order.sort_unstable_by(|a, b| {
            compare_entries(
                &self.entries[*a],
                &self.entries[*b],
                self.sort_column,
                self.ascending,
            )
        });
    }

    fn sort(&mut self, column: Column, cx: &mut Context<Self>) {
        self.finish_drag();
        self.explicit_sort = true;
        let mut selected = vec![false; self.entries.len()];
        for (row, entry) in self.order.iter().enumerate() {
            selected[*entry] = self.selected.contains(row);
        }
        let anchor = self.anchor.and_then(|row| self.order.get(row)).copied();
        if column == self.sort_column {
            self.ascending = !self.ascending;
        } else {
            self.sort_column = column;
            self.ascending = column == Column::Name;
        }
        self.reorder();
        self.selected = selection_in_order(&self.order, &selected);
        self.anchor = anchor.and_then(|entry| self.order.iter().position(|index| *index == entry));
        self.scroll.set_offset(point(px(0.), px(0.)));
        cx.notify();
    }

    fn resize_columns(&mut self, event: &MouseMoveEvent, window: &Window, cx: &mut Context<Self>) {
        let Some(resize) = &self.resizing else {
            return;
        };
        if !event.dragging() {
            self.end_drag(cx);
            return;
        }
        let scale = f32::from(dp(1.).to_pixels(window.rem_size()));
        let width = resize.width - f32::from(event.position.x - resize.start_x) / scale;
        match resize.column {
            Column::Modified => self.modified_width = width.clamp(136., 320.),
            Column::Size => self.size_width = width.clamp(64., 180.),
            Column::Name => {}
        }
        cx.notify();
    }

    fn column_header(
        &self,
        label: &'static str,
        column: Column,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active = self.sort_column == column;
        div()
            .id(label)
            .relative()
            .h_full()
            .flex()
            .items_center()
            .gap(dp(6.))
            .px(dp(8.))
            .when(column == Column::Name, |header| {
                header
                    .flex_1()
                    .min_w(dp(100.))
                    // Line the label up with the row icons.
                    .pl(dp(14. + 6.))
            })
            .when(column == Column::Modified, |header| {
                header.w(dp(self.modified_width)).flex_shrink_0()
            })
            .when(column == Column::Size, |header| {
                header.w(dp(self.size_width)).flex_shrink_0().justify_end()
            })
            .cursor_pointer()
            .hover(|header| header.bg(rgb(0x252525)))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.focus.focus(window);
                this.sort(column, cx);
            }))
            .child(label)
            .when(active, |header| {
                header.child(native_icons::symbol(
                    if self.ascending {
                        "chevron.up"
                    } else {
                        "chevron.down"
                    },
                    9.,
                ))
            })
            .when(column != Column::Name, |header| {
                header.child(
                    div()
                        .id((label, 0_usize))
                        .absolute()
                        .left(dp(0.))
                        .top_0()
                        .bottom_0()
                        .w(dp(6.))
                        .cursor(CursorStyle::ResizeLeftRight)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                                cx.stop_propagation();
                                this.finish_drag();
                                let width = if column == Column::Modified {
                                    this.modified_width
                                } else {
                                    this.size_width
                                };
                                this.resizing = Some(ColumnResize {
                                    column,
                                    start_x: event.position.x,
                                    width,
                                });
                                cx.notify();
                            }),
                        )
                        .on_click(|_, _, cx| cx.stop_propagation()),
                )
            })
    }

    fn header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .h(dp(row::HEIGHT))
            .flex_shrink_0()
            .pr(dp(14.))
            .bg(rgb(0x181818))
            .text_size(dp(text_size::DETAIL))
            .text_color(rgb(0xaaaaaa))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(self.column_header("Name", Column::Name, cx))
            .child(self.column_header("Date Modified", Column::Modified, cx))
            .child(self.column_header("Size", Column::Size, cx))
    }

    /// A press released without dragging: a plain click on a row in a
    /// multiple selection selects just that row.
    fn release_press(&mut self, cx: &mut Context<Self>) {
        if let Some(Press {
            collapse_to: Some(row),
            ..
        }) = self.press.take()
        {
            self.selected.replace(row..row + 1);
            self.anchor = Some(row);
            cx.notify();
        }
    }

    pub(super) fn end_drag(&mut self, cx: &mut Context<Self>) {
        self.release_press(cx);
        // The release a native drag posts when it ends.
        if let Some(paths) = self.dragging_out.take() {
            cx.emit(Request::DragEnded(paths));
        }
        let resizing = self.resizing.take().is_some();
        if self.finish_drag() || resizing {
            cx.notify();
        }
    }

    fn finish_drag(&mut self) -> bool {
        let drag = self.drag.take();
        let finished = drag.is_some();
        if let Some(drag) = drag.filter(|drag| drag.active) {
            if drag.toggle {
                self.selected = drag.baseline;
                self.selected.toggle(drag.hits);
            } else if drag.extend {
                self.selected = drag.baseline;
                self.selected.add(drag.hits);
            } else {
                self.selected.replace(drag.hits);
            }
        }
        let updated = self.resizing.is_none() && self.apply_pending_entries();
        finished || updated
    }

    fn is_selected(&self, index: usize) -> bool {
        match self.drag.as_ref().filter(|drag| drag.active) {
            Some(drag) if drag.toggle => drag.baseline.contains(index) ^ drag.hits.contains(&index),
            Some(drag) if drag.extend => {
                drag.baseline.contains(index) || drag.hits.contains(&index)
            }
            Some(drag) => drag.hits.contains(&index),
            None => self.selected.contains(index),
        }
    }

    fn content_point(&self, position: Point<Pixels>) -> Point<Pixels> {
        position - self.scroll.bounds().origin - self.scroll.offset()
    }

    pub(super) fn focus(&self, window: &mut Window) {
        self.focus.focus(window);
    }

    /// Moves the selection by `step` rows (clamped to the list), or extends it
    /// from the anchor when `extend` is set, and scrolls the moved row into view.
    fn step_selection(&mut self, step: isize, extend: bool, cx: &mut Context<Self>) {
        let head = self.anchor.map(|anchor| {
            match self.selected.0.iter().find(|range| range.contains(&anchor)) {
                Some(range) if range.start < anchor => range.start,
                Some(range) => range.end - 1,
                None => anchor,
            }
        });
        let Some((range, anchor, target)) =
            arrow_selection(self.anchor, head, self.entries.len(), step, extend)
        else {
            return;
        };
        self.drag = None;
        self.selected.replace(range);
        self.anchor = Some(anchor);
        self.virtual_scroll
            .scroll_to_item(target, gpui::ScrollStrategy::Top);
        cx.notify();
    }

    /// The selected rows' paths, in display order, at most `limit`.
    pub(super) fn selected_paths(&self, limit: usize) -> Vec<PathBuf> {
        self.selected
            .0
            .iter()
            .flat_map(|range| range.clone())
            .take(limit)
            .map(|row| self.entries[self.order[row]].path.clone())
            .collect()
    }

    /// Emits `request` for the selection, if anything is selected.
    fn request_selected(&mut self, request: fn(Vec<PathBuf>) -> Request, cx: &mut Context<Self>) {
        let paths = self.selected_paths(usize::MAX);
        if !paths.is_empty() {
            cx.emit(request(paths));
        }
    }

    fn open_selected(&mut self, _: &OpenSelected, _: &mut Window, cx: &mut Context<Self>) {
        self.request_selected(Request::Open, cx);
    }

    /// The row under `position` (window coordinates), if any.
    fn row_at(&self, position: Point<Pixels>, window: &Window) -> Option<usize> {
        let position = self.content_point(position);
        let height = dp(row::HEIGHT).to_pixels(window.rem_size());
        let index = (f32::from(position.y) / f32::from(height)).floor();
        let inset = dp(14.).to_pixels(window.rem_size());
        (index >= 0.
            && index < self.entries.len() as f32
            && position.x >= inset
            && position.x <= self.scroll.bounds().size.width - inset)
            .then_some(index as usize)
    }

    /// Right-click selects the row under the pointer unless it is already
    /// part of the selection, as in Finder; on blank space it clears it.
    fn right_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus.focus(window);
        self.drag = None;
        match self.row_at(event.position, window) {
            Some(row) if !self.selected.contains(row) => {
                self.selected.replace(row..row + 1);
                self.anchor = Some(row);
            }
            Some(_) => {}
            None => {
                self.selected.0.clear();
                self.anchor = None;
            }
        }
        cx.emit(Request::ContextMenu {
            position: event.position,
            paths: self.selected_paths(usize::MAX),
        });
        cx.notify();
    }

    /// Edits `path`'s name in its row, with the name before any extension
    /// selected. Enter or clicking away commits; Escape cancels.
    pub(super) fn begin_rename(
        &mut self,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self
            .order
            .iter()
            .position(|&entry| self.entries[entry].path == path)
        else {
            return;
        };
        self.selected.replace(row..row + 1);
        self.anchor = Some(row);
        self.drag = None;
        self.virtual_scroll
            .scroll_to_item(row, gpui::ScrollStrategy::Top);
        let name = self.entries[self.order[row]].name.to_string();
        let stem = match name.rfind('.') {
            Some(dot) if dot > 0 && !path.is_dir() => dot,
            _ => name.len(),
        };
        let editor = cx.new(|cx| SearchInput::new("", cx));
        editor.update(cx, |input, cx| input.edit(name, 0..stem, window, cx));
        let focus = editor.read(cx).focus_handle(cx);
        let blur = cx.on_blur(&focus, window, |this, window, cx| {
            this.commit_rename(window, cx)
        });
        self.renaming = Some(Renaming {
            path: path.to_path_buf(),
            editor,
            _blur: blur,
        });
        cx.notify();
    }

    /// Renames the selection when it is exactly one item.
    pub(super) fn rename_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.selected_paths(2).as_slice() {
            [path] => {
                let path = path.clone();
                self.begin_rename(&path, window, cx);
            }
            _ => crate::ui::beep(),
        }
    }

    fn commit_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(renaming) = self.renaming.take() else {
            return;
        };
        let name = renaming.editor.read(cx).text().to_owned();
        let unchanged = renaming
            .path
            .file_name()
            .is_some_and(|current| current.to_string_lossy() == name);
        if !unchanged {
            cx.emit(Request::Rename {
                from: renaming.path,
                name,
            });
        }
        self.focus.focus(window);
        cx.notify();
    }

    fn cancel_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.renaming = None;
        self.focus.focus(window);
        cx.notify();
    }

    /// Shows `from` as `to` at once, keeping it selected; the next listing
    /// confirms it.
    pub(super) fn apply_rename(&mut self, from: &Path, to: PathBuf, cx: &mut Context<Self>) {
        let mut entries: Vec<FileEntry> = self.entries.to_vec();
        let Some(index) = entries.iter().position(|entry| entry.path == from) else {
            return;
        };
        if let Some(entry) = entries_for_paths(vec![to.clone()]).pop() {
            entries[index] = entry;
        }
        self.replace_keeping_selection(entries, &[to], cx);
    }

    /// Adds a just-created item and starts renaming it.
    pub(super) fn insert_and_rename(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut entries: Vec<FileEntry> = self.entries.to_vec();
        entries.extend(entries_for_paths(vec![path.clone()]));
        self.replace_keeping_selection(entries, std::slice::from_ref(&path), cx);
        self.begin_rename(&path, window, cx);
    }

    /// Hides trashed items at once; the next listing confirms it.
    pub(super) fn remove_paths(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) {
        let entries: Vec<FileEntry> = self
            .entries
            .iter()
            .filter(|entry| !paths.contains(&entry.path))
            .cloned()
            .collect();
        self.replace_keeping_selection(entries, &[], cx);
    }

    /// Replaces the rows in place, keeping scroll and sort, and selects `select`.
    fn replace_keeping_selection(
        &mut self,
        entries: Vec<FileEntry>,
        select: &[PathBuf],
        cx: &mut Context<Self>,
    ) {
        self.entries = entries.into();
        self.order = (0..self.entries.len()).collect();
        self.reorder();
        let rows: Vec<bool> = self
            .entries
            .iter()
            .map(|entry| select.contains(&entry.path))
            .collect();
        self.selected = selection_in_order(&self.order, &rows);
        self.anchor = self.selected.0.first().map(|range| range.start);
        self.drag = None;
        cx.notify();
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window);
        let position = self.content_point(event.position);
        let height = dp(row::HEIGHT).to_pixels(window.rem_size());
        let hit = self.row_at(event.position, window);
        if let Some(index) = hit
            && event.click_count == 2
            && !event.modifiers.shift
            && !event.modifiers.platform
        {
            self.drag = None;
            cx.emit(Request::Open(vec![
                self.entries[self.order[index]].path.clone(),
            ]));
            return;
        }
        if let Some(index) = hit {
            let mut collapse_to = None;
            if event.modifiers.shift {
                let anchor = self.anchor.unwrap_or(index);
                if !event.modifiers.platform {
                    self.selected.0.clear();
                }
                self.selected.add(anchor.min(index)..anchor.max(index) + 1);
            } else if event.modifiers.platform {
                self.selected.toggle(index..index + 1);
                self.anchor = Some(index);
            } else if self.selected.contains(index) {
                collapse_to = Some(index);
                self.anchor = Some(index);
            } else {
                self.selected.replace(index..index + 1);
                self.anchor = Some(index);
            }
            // Dragging from a row drags files; the marquee starts on blank space.
            self.drag = None;
            self.press = Some(Press {
                origin: event.position,
                collapse_to,
            });
            cx.notify();
            return;
        }
        let mut baseline = self.selected.clone();
        if let Some(index) = hit {
            if event.modifiers.shift {
                let anchor = self.anchor.unwrap_or(index);
                if !event.modifiers.platform {
                    self.selected.0.clear();
                }
                self.selected.add(anchor.min(index)..anchor.max(index) + 1);
            } else if event.modifiers.platform {
                self.selected.toggle(index..index + 1);
                self.anchor = Some(index);
            } else {
                self.selected.replace(index..index + 1);
                self.anchor = Some(index);
            }
        } else if !event.modifiers.platform && !event.modifiers.shift {
            self.selected.0.clear();
            self.anchor = None;
        }
        if event.modifiers.shift {
            baseline = self.selected.clone();
        }
        self.drag = Some(Drag {
            origin: position,
            current: position,
            baseline,
            extend: event.modifiers.shift,
            toggle: event.modifiers.platform,
            active: false,
            hits: 0..0,
            last_frame: Instant::now(),
            row_height: height,
        });
        cx.notify();
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(press) = &self.press {
            if !event.dragging() {
                self.release_press(cx);
                return;
            }
            let delta = event.position - press.origin;
            let threshold = f32::from(dp(4.).to_pixels(window.rem_size()));
            if f32::from(delta.x).hypot(f32::from(delta.y)) >= threshold {
                // A drag keeps the selection as it is; nothing collapses.
                self.press = None;
                let paths = self.selected_paths(usize::MAX);
                if crate::drag_out::begin(window, &paths) {
                    self.dragging_out = Some(paths);
                    cx.notify();
                }
            }
            return;
        }
        if self.drag.is_none() {
            return;
        }
        if !event.dragging() {
            self.end_drag(cx);
            return;
        }
        self.update_drag(event.position, window);
        cx.notify();
    }

    fn update_drag(&mut self, mouse: Point<Pixels>, window: &Window) {
        let position = self.content_point(mouse);
        let Some(drag) = &mut self.drag else {
            return;
        };
        let delta = position - drag.origin;
        let threshold = f32::from(dp(4.).to_pixels(window.rem_size()));
        if !drag.active && f32::from(delta.x).hypot(f32::from(delta.y)) < threshold {
            return;
        }
        drag.active = true;
        drag.current = position;
        drag.hits = intersecting_rows(
            drag.origin,
            position,
            dp(row::HEIGHT).to_pixels(window.rem_size()),
            self.entries.len(),
            dp(14.).to_pixels(window.rem_size()),
            self.scroll.bounds().size.width - dp(14.).to_pixels(window.rem_size()),
        );
    }

    fn auto_scroll(&mut self, window: &Window) {
        if !window.is_window_active() {
            self.finish_drag();
            return;
        }
        let Some(drag) = &mut self.drag else {
            return;
        };
        if drag.row_height != dp(row::HEIGHT).to_pixels(window.rem_size()) {
            self.finish_drag();
            return;
        }
        if !drag.active {
            return;
        }
        let now = Instant::now();
        let elapsed = now.duration_since(drag.last_frame).as_secs_f32().min(0.05);
        drag.last_frame = now;
        let mouse = window.mouse_position();
        let bounds = self.scroll.bounds();
        let scale = f32::from(dp(1.).to_pixels(window.rem_size()));
        let velocity = edge_scroll_speed(
            f32::from(mouse.y - bounds.top()) / scale,
            f32::from(bounds.size.height) / scale,
        ) * scale;
        if velocity != 0. {
            let mut offset = self.scroll.offset();
            let limit = -self.scroll.max_offset().height;
            let has_room =
                (velocity > 0. && offset.y < px(0.)) || (velocity < 0. && offset.y > limit);
            offset.y = (offset.y + px(velocity * elapsed)).clamp(limit, px(0.));
            if offset != self.scroll.offset() {
                self.scroll.set_offset(offset);
            }
            // Very short frames can round away tiny movement deep in a large
            // list. Keep ticking while there is room, but stop at scroll limits.
            if has_room {
                window.request_animation_frame();
            }
        }
        self.update_drag(mouse, window);
    }
}

fn intersecting_rows(
    origin: Point<Pixels>,
    end: Point<Pixels>,
    row_height: Pixels,
    count: usize,
    left: Pixels,
    right: Pixels,
) -> Range<usize> {
    if count == 0 || origin.x.max(end.x) < left || origin.x.min(end.x) > right {
        return 0..0;
    }
    let top = f32::from(origin.y.min(end.y)).max(0.);
    let bottom = f32::from(origin.y.max(end.y));
    let height = f32::from(row_height);
    if bottom < 0. || top >= height * count as f32 {
        return 0..0;
    }
    let first = (top / height).floor() as usize;
    let last = ((bottom.max(0.) / height).ceil() as usize)
        .max(first + 1)
        .min(count);
    first..last
}

// Logical points per second. Within the viewport, ease into edge scrolling;
// outside, rise gently from 500 to 800 over 160pt and stop accelerating there.
fn edge_scroll_speed(y: f32, height: f32) -> f32 {
    if height <= 0. {
        return 0.;
    }
    let edge = 28_f32.min(height / 2.);
    let (penetration, sign) = if y < edge {
        (edge - y, 1.)
    } else if y > height - edge {
        (y - height + edge, -1.)
    } else {
        return 0.;
    };
    let speed = if penetration <= edge {
        500. * (penetration / edge).powi(2)
    } else {
        500. + 300. * ((penetration - edge) / 160.).min(1.)
    };
    sign * speed
}

impl Render for FileList {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.auto_scroll(window);
        if self.resizing.is_some() {
            let list = cx.entity();
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                if phase == DispatchPhase::Bubble {
                    list.update(cx, |this, cx| this.resize_columns(event, window, cx));
                }
            });
        }
        // Follow the pointer during a marquee and during a press on a row,
        // which becomes a file drag once it moves far enough.
        if self.drag.is_some() || self.press.is_some() {
            let list = cx.entity();
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                if phase == DispatchPhase::Bubble {
                    list.update(cx, |this, cx| this.mouse_move(event, window, cx));
                }
            });
        }
        // An open Quick Look panel previews whatever is selected now.
        if crate::quick_look::is_open() {
            crate::quick_look::follow(self.selected_paths(crate::quick_look::LIMIT));
        }
        let marquee = self
            .drag
            .as_ref()
            .filter(|drag| drag.active)
            .map(|drag| (drag.origin, drag.current));
        let scroll = self.scroll.clone();
        div()
            .id("file-list")
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .overflow_hidden()
            .overflow_x_scroll()
            .track_focus(&self.focus)
            // While renaming, keys go to the name field: Space types a space
            // rather than opening Quick Look.
            .when(self.renaming.is_none(), |list| list.key_context("FileList"))
            .on_action(cx.listener(|this, _: &SelectAll, _, cx| {
                this.selected.replace(0..this.entries.len());
                this.drag = None;
                cx.notify();
            }))
            .on_action(cx.listener(Self::open_selected))
            .on_action(cx.listener(|this, _: &QuickLookSelected, _, cx| {
                this.request_selected(Request::QuickLook, cx)
            }))
            .on_action(cx.listener(|this, _: &TrashSelected, _, cx| {
                this.request_selected(Request::Trash, cx)
            }))
            .on_action(cx.listener(|this, _: &CopySelectedPaths, _, cx| {
                this.request_selected(Request::CopyPath, cx)
            }))
            .on_action(cx.listener(|_, _: &NewFolder, _, cx| cx.emit(Request::NewFolder)))
            .on_action(
                cx.listener(|this, _: &CommitRename, window, cx| this.commit_rename(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &RenameSelected, window, cx| {
                    this.rename_selected(window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &CancelRename, window, cx| this.cancel_rename(window, cx)),
            )
            .on_mouse_down(MouseButton::Right, cx.listener(Self::right_mouse_down))
            // Drops on the list itself go into the folder it shows.
            .on_drop(cx.listener(|_, paths: &ExternalPaths, _, cx| {
                cx.emit(Request::Drop {
                    paths: paths.paths().to_vec(),
                    into: None,
                });
            }))
            .on_action(
                cx.listener(|this, _: &SelectPrevious, _, cx| this.step_selection(-1, false, cx)),
            )
            .on_action(cx.listener(|this, _: &SelectNext, _, cx| this.step_selection(1, false, cx)))
            .on_action(
                cx.listener(|this, _: &ExtendPrevious, _, cx| this.step_selection(-1, true, cx)),
            )
            .on_action(cx.listener(|this, _: &ExtendNext, _, cx| this.step_selection(1, true, cx)))
            .on_action(cx.listener(|this, _: &SelectFirst, _, cx| {
                this.step_selection(isize::MIN, false, cx)
            }))
            .on_action(
                cx.listener(|this, _: &SelectLast, _, cx| {
                    this.step_selection(isize::MAX, false, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &ClearSelection, _, cx| {
                this.selected.0.clear();
                this.anchor = None;
                this.drag = None;
                cx.notify();
            }))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.end_drag(cx);
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.end_drag(cx);
                }),
            )
            .child(
                div()
                    .w_full()
                    .min_w(dp(160. + self.modified_width + self.size_width))
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(self.header(cx))
                    .child(
                        uniform_list("files-scroll", self.entries.len(), {
                            let list = cx.entity();
                            let numerals = ui::tabular_font();
                            move |range, _, cx| {
                                let this = list.read(cx);
                                let today = Local::now().date_naive();
                                range
                                    .map(|index| {
                                        let entry = &this.entries[this.order[index]];
                                        let selected = this.is_selected(index);
                                        let group_start = selected
                                            && (index == 0 || !this.is_selected(index - 1));
                                        let group_end = selected && !this.is_selected(index + 1);
                                        // While dragged, the selection turns the sidebar's
                                        // active gray so blue only marks where it will drop.
                                        let carried = selected && this.dragging_out.is_some();
                                        div()
                                            .id(("file", index))
                                            .when(entry.drop_target && !carried, |row| {
                                                let folder = entry.path.clone();
                                                let list = list.clone();
                                                row.drag_over::<ExternalPaths>(|row, _, _, _| {
                                                    row.bg(rgb(0x1d4f7d))
                                                })
                                                .on_drop(move |paths: &ExternalPaths, _, cx| {
                                                    let paths = paths.paths().to_vec();
                                                    let into = Some(folder.clone());
                                                    list.update(cx, |_, cx| {
                                                        cx.emit(Request::Drop { paths, into })
                                                    });
                                                })
                                            })
                                            .w_full()
                                            .flex()
                                            .items_center()
                                            .h(dp(row::HEIGHT))
                                            .pl(dp(6.))
                                            .when(!selected, |row| row.rounded(dp(radius::SM)))
                                            .when(group_start, |row| row.rounded_t(dp(radius::SM)))
                                            .when(group_end, |row| row.rounded_b(dp(radius::SM)))
                                            .when(carried, |row| row.bg(rgb(0x303030)))
                                            .when(selected && !carried, |row| {
                                                row.bg(rgb(0x174b78)).text_color(rgb(0xffffff))
                                            })
                                            .when(!selected, |row| {
                                                row.hover(|style| style.bg(rgb(0x1b2026)))
                                            })
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w(dp(0.))
                                                    .flex()
                                                    .items_center()
                                                    .gap(dp(row::GAP))
                                                    .child(native_icons::file_icon(
                                                        entry.icon.clone(),
                                                        row::ICON,
                                                    ))
                                                    .child(match &this.renaming {
                                                        Some(renaming)
                                                            if renaming.path == entry.path =>
                                                        {
                                                            div()
                                                                .key_context("Rename")
                                                                .flex_1()
                                                                .min_w(dp(0.))
                                                                .h(dp(row::HEIGHT - 6.))
                                                                .px(dp(4.))
                                                                .flex()
                                                                .items_center()
                                                                .rounded(dp(radius::XS))
                                                                .bg(rgb(0x101010))
                                                                .border_1()
                                                                .border_color(rgb(0x0a84ff))
                                                                .text_color(rgb(0xeeeeee))
                                                                .on_mouse_down(
                                                                    MouseButton::Left,
                                                                    |_, _, cx| {
                                                                        cx.stop_propagation()
                                                                    },
                                                                )
                                                                .child(renaming.editor.clone())
                                                        }
                                                        _ => div()
                                                            .flex_1()
                                                            .min_w(dp(0.))
                                                            .text_ellipsis()
                                                            .child(entry.name.clone()),
                                                    }),
                                            )
                                            .child(
                                                div()
                                                    .w(dp(this.modified_width))
                                                    .flex_shrink_0()
                                                    .px(dp(8.))
                                                    .text_ellipsis()
                                                    .when(!selected, |cell| {
                                                        cell.text_color(rgb(0xaaaaaa))
                                                    })
                                                    .child(modified_label(entry.modified, today)),
                                            )
                                            .child(
                                                div()
                                                    .w(dp(this.size_width))
                                                    .flex_shrink_0()
                                                    .px(dp(8.))
                                                    .font(numerals.clone())
                                                    .text_right()
                                                    .text_ellipsis()
                                                    .when(!selected, |cell| {
                                                        cell.text_color(rgb(0xaaaaaa))
                                                    })
                                                    .child(
                                                        entry.bytes.map_or("—".into(), format_size),
                                                    ),
                                            )
                                    })
                                    .collect()
                            }
                        })
                        .w_full()
                        .flex_1()
                        .min_h(dp(0.))
                        .track_scroll(self.virtual_scroll.clone())
                        .px(dp(14.))
                        .pb(dp(20.)),
                    ),
            )
            .when_some(marquee, |view, (origin, end)| {
                view.child(
                    canvas(
                        |_, _, _| (),
                        move |_, _, window, _| {
                            let bounds = scroll.bounds();
                            let offset = scroll.offset();
                            let start = bounds.origin + origin + offset;
                            let end = bounds.origin + end + offset;
                            let rect = Bounds::new(
                                point(start.x.min(end.x), start.y.min(end.y)),
                                size(
                                    (end.x - start.x).abs().max(px(1.)),
                                    (end.y - start.y).abs().max(px(1.)),
                                ),
                            );
                            window.with_content_mask(
                                Some(gpui::ContentMask { bounds }),
                                |window| {
                                    window.paint_quad(quad(
                                        rect,
                                        dp(radius::SM).to_pixels(window.rem_size()),
                                        rgba(0x0a84ff25),
                                        px(1.),
                                        rgb(0x409fff),
                                        BorderStyle::Solid,
                                    ))
                                },
                            );
                        },
                    )
                    .absolute()
                    .inset_0(),
                )
            })
            .when(self.entries.is_empty(), |view| {
                view.when_some(self.message.clone(), |view, message| {
                    view.child(
                        div()
                            .absolute()
                            .inset_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .px(dp(20.))
                            .text_color(rgb(0x999999))
                            .child(message),
                    )
                })
            })
            .when(self.show_loading, |view| {
                view.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .h(dp(2.))
                        .overflow_hidden()
                        .child(
                            div()
                                .absolute()
                                .top_0()
                                .h_full()
                                .w(relative(0.3))
                                .rounded_full()
                                .bg(rgb(0x0a84ff))
                                .with_animation(
                                    "loading-bar",
                                    Animation::new(Duration::from_millis(1100))
                                        .repeat()
                                        .with_easing(ease_in_out),
                                    |bar, t| bar.left(relative(-0.3 + 1.3 * t)),
                                ),
                        ),
                )
            })
    }
}

/// Arrow-key selection over `len` rows: the selected range, the new anchor,
/// and the row to reveal. With nothing selected, Down starts at the first row
/// and Up at the last. Extending keeps the anchor and moves the far end.
fn arrow_selection(
    anchor: Option<usize>,
    head: Option<usize>,
    len: usize,
    step: isize,
    extend: bool,
) -> Option<(Range<usize>, usize, usize)> {
    let last = len.checked_sub(1)?;
    let target = match head {
        None if step > 0 => 0,
        None => last,
        Some(head) => head.saturating_add_signed(step).min(last),
    };
    let anchor = match (extend, anchor) {
        (true, Some(anchor)) => anchor.min(last),
        _ => target,
    };
    Some((anchor.min(target)..anchor.max(target) + 1, anchor, target))
}
