use std::{
    collections::HashMap,
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use gpui::{
    Bounds, Context, IntoElement, MouseButton, MouseMoveEvent, Pixels, Point, ScrollHandle, Task,
    Window, div, img, point, prelude::*, px, rgb, size,
};

use crate::{
    FileAction, FileBrowser, classify, file_tags, native_file_menu, native_icons,
    show_file_action_error, sidebar,
    ui::{dp, radius, row, text_size},
};

const WIDTH: f32 = 220.;
const SUBMENU_WIDTH: f32 = 240.;
const SUBMENU_GAP: f32 = 3.;
const PADDING: f32 = 4.;
const SEPARATOR: f32 = 9.;
const OPEN_DELAY: Duration = Duration::from_millis(85);
const AIM_DELAY: Duration = Duration::from_millis(260);
const LEAVE_DELAY: Duration = Duration::from_millis(160);

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Branch {
    Action(FileAction),
    Shortcuts,
}

impl Branch {
    fn trigger(self) -> FileAction {
        match self {
            Self::Action(action) => action,
            Self::Shortcuts => FileAction::QuickActions,
        }
    }
}

#[derive(Clone)]
enum EntryAction {
    OpenWith(PathBuf),
    Tag { tag: file_tags::Tag, checked: bool },
    AddTag,
    Workflow(PathBuf),
    Shortcut(String),
    Service(String),
    Share { index: usize, title: String },
    BrowseShortcuts,
    BackToQuickActions,
}

struct Entry {
    title: String,
    action: EntryAction,
    icon: Option<EntryIcon>,
    trailing: Option<&'static str>,
}

enum EntryIcon {
    File(native_icons::FileIcon),
    Share(Arc<gpui::RenderImage>),
}

impl EntryIcon {
    fn render(&self) -> gpui::Img {
        match self {
            Self::File(icon) => native_icons::file_icon(icon.clone(), row::ICON),
            Self::Share(image) => img(image.clone()).size(dp(row::ICON)).flex_shrink_0(),
        }
    }
}

impl Entry {
    fn new(title: impl Into<String>, action: EntryAction) -> Self {
        Self {
            title: title.into(),
            action,
            icon: None,
            trailing: None,
        }
    }
}

struct Submenu {
    branch: Branch,
    selected: Option<usize>,
    scroll: ScrollHandle,
}

#[derive(Default)]
struct Pointer {
    last: Option<Point<Pixels>>,
    trigger: Option<Point<Pixels>>,
    aiming_since: Option<Instant>,
    highlighted: Option<FileAction>,
    pending: Option<(Option<FileAction>, Task<()>)>,
}

type Entries = std::result::Result<Vec<Entry>, String>;

pub(super) struct ContextMenu {
    pub position: Point<Pixels>,
    pub paths: Vec<PathBuf>,
    pub package: bool,
    pub sidebar: Option<(sidebar::Place, bool)>,
    can_paste: bool,
    selected: Option<usize>,
    scroll: ScrollHandle,
    submenu: Option<Submenu>,
    cache: HashMap<Branch, Option<Entries>>,
    pointer: Pointer,
    identity: Rc<()>,
}

impl ContextMenu {
    pub fn new(
        position: Point<Pixels>,
        paths: Vec<PathBuf>,
        package: bool,
        can_paste: bool,
        sidebar: Option<(sidebar::Place, bool)>,
    ) -> Self {
        Self {
            position,
            paths,
            package,
            sidebar,
            can_paste,
            selected: None,
            scroll: ScrollHandle::new(),
            submenu: None,
            cache: HashMap::new(),
            pointer: Pointer::default(),
            identity: Rc::new(()),
        }
    }

    pub fn items(&self) -> Vec<Option<FileAction>> {
        match self.sidebar {
            Some((place, folder)) => FileAction::sidebar_menu(place, folder),
            None => FileAction::menu(&self.paths, self.can_paste, self.package),
        }
    }

    pub fn release_share_icons(&mut self, window: &mut Window, cx: &mut gpui::App) {
        if let Some(Some(Ok(entries))) = self.cache.get_mut(&Branch::Action(FileAction::Share)) {
            for entry in entries {
                if let Some(EntryIcon::Share(image)) = entry.icon.take() {
                    cx.drop_image(image, Some(window));
                }
            }
        }
    }

    fn entries(&self, branch: Branch) -> Option<&Entries> {
        self.cache.get(&branch)?.as_ref()
    }

    fn selected_action(&self) -> Option<FileAction> {
        self.items().into_iter().flatten().nth(self.selected?)
    }

    fn active_trigger(&self) -> Option<FileAction> {
        self.submenu
            .as_ref()
            .map(|submenu| submenu.branch.trigger())
    }

    fn primary_at(&self, position: Point<Pixels>, layout: Layout) -> Option<FileAction> {
        if !layout.main.contains(&position) {
            return None;
        }
        let mut y = layout.main.top() + px(PADDING * layout.scale) + self.scroll.offset().y;
        for item in self.items() {
            let bottom = y + px(row_height(item) * layout.scale);
            if position.y >= y && position.y < bottom {
                return item;
            }
            y = bottom;
        }
        None
    }

    fn layout(&self, window: &Window, items: &[Option<FileAction>]) -> Layout {
        let scale = f32::from(dp(1.).to_pixels(window.rem_size()));
        let margin = px(8. * scale);
        let viewport = window.viewport_size();
        let max_height = (viewport.height - margin * 2.).max(px(0.));
        // Leave room beside the main menu even in a narrow, zoomed window.
        let max_width = (viewport.width - margin * 2.).max(px(0.));
        let width = px(WIDTH * scale).min(max_width * 0.65);
        let height =
            px((PADDING * 2. + items.iter().copied().map(row_height).sum::<f32>()) * scale)
                .min(max_height);
        let left = self
            .position
            .x
            .min(viewport.width - margin - width)
            .max(margin);
        let top = self
            .position
            .y
            .min(viewport.height - margin - height)
            .max(margin);
        let main = Bounds::new(point(left, top), size(width, height));
        let submenu = self.submenu.as_ref().map(|submenu| {
            let rows = match self.entries(submenu.branch) {
                Some(Ok(entries)) => entries.len().max(1),
                _ => 1,
            };
            let height = px((PADDING * 2. + rows as f32 * row::HEIGHT) * scale).min(max_height);
            let trigger = Some(submenu.branch.trigger());
            let before = items
                .iter()
                .copied()
                .take_while(|item| *item != trigger)
                .map(row_height)
                .sum::<f32>();
            let anchor = main.top() + px(before * scale) + self.scroll.offset().y;
            let y = anchor.min(viewport.height - margin - height).max(margin);
            Placement::beside(
                main,
                viewport.width,
                margin,
                px(SUBMENU_GAP * scale),
                size(px(SUBMENU_WIDTH * scale), height),
                y,
            )
        });
        Layout {
            main,
            submenu,
            scale,
        }
    }
}

fn row_height(action: Option<FileAction>) -> f32 {
    if action.is_some() {
        row::HEIGHT
    } else {
        SEPARATOR
    }
}

#[derive(Clone, Copy)]
struct Layout {
    main: Bounds<Pixels>,
    submenu: Option<Placement>,
    scale: f32,
}

impl Layout {
    fn in_submenu_gap(&self, pointer: Point<Pixels>) -> bool {
        self.submenu
            .is_some_and(|submenu| submenu.in_gap(self.main, pointer))
    }
}

#[derive(Clone, Copy)]
struct Placement {
    bounds: Bounds<Pixels>,
    opens_left: bool,
}

impl Placement {
    fn beside(
        main: Bounds<Pixels>,
        viewport_width: Pixels,
        margin: Pixels,
        gap: Pixels,
        desired: gpui::Size<Pixels>,
        y: Pixels,
    ) -> Self {
        let right = main.right() + gap;
        let left = main.left() - gap;
        let right_space = viewport_width - margin - right;
        let left_space = left - margin;
        let opens_left = right_space < desired.width && left_space > right_space;
        let available = if opens_left { left_space } else { right_space };
        let width = desired.width.min(available.max(px(0.)));
        let x = if opens_left { left - width } else { right };
        Self {
            bounds: Bounds::new(point(x, y), size(width, desired.height)),
            opens_left,
        }
    }

    /// The thin strip facing the submenu is safe even if the pointer pauses.
    fn in_gap(&self, main: Bounds<Pixels>, pointer: Point<Pixels>) -> bool {
        let (left, right) = if self.opens_left {
            (self.bounds.right(), main.left())
        } else {
            (main.right(), self.bounds.left())
        };
        pointer.x >= left
            && pointer.x <= right
            && pointer.y >= self.bounds.top()
            && pointer.y <= self.bounds.bottom()
    }

    /// Protect only motion toward the near edge, inside the triangle from the
    /// last point on the trigger to the submenu's two near corners.
    fn protects(
        &self,
        origin: Point<Pixels>,
        previous: Point<Pixels>,
        pointer: Point<Pixels>,
    ) -> bool {
        let edge = if self.opens_left {
            self.bounds.right()
        } else {
            self.bounds.left()
        };
        if (pointer.x - edge).abs() > (previous.x - edge).abs() + px(1.) || edge == origin.x {
            return false;
        }
        let ratio = f32::from(pointer.x - origin.x) / f32::from(edge - origin.x);
        if !(0. ..=1.).contains(&ratio) {
            return false;
        }
        let top = origin.y + (self.bounds.top() - origin.y) * ratio;
        let bottom = origin.y + (self.bounds.bottom() - origin.y) * ratio;
        pointer.y >= top && pointer.y <= bottom
    }
}

impl FileBrowser {
    pub(super) fn open_context_submenu(
        &mut self,
        action: FileAction,
        keyboard: bool,
        cx: &mut Context<Self>,
    ) {
        if action.expands() {
            self.show_submenu(Branch::Action(action), keyboard, cx);
        }
    }

    fn show_submenu(&mut self, branch: Branch, keyboard: bool, cx: &mut Context<Self>) {
        let Some(menu) = &mut self.context_menu else {
            return;
        };
        menu.pointer.pending = None;
        menu.pointer.aiming_since = None;
        if let Some(submenu) = &mut menu.submenu
            && submenu.branch == branch
        {
            if keyboard {
                submenu.selected = Some(0);
                cx.notify();
            }
            return;
        }
        menu.submenu = Some(Submenu {
            branch,
            scroll: ScrollHandle::new(),
            selected: keyboard.then_some(0),
        });
        cx.notify();
        if menu.cache.contains_key(&branch) {
            return;
        }
        if let Branch::Action(FileAction::OpenWith | FileAction::Share) = branch {
            let entries = if branch.trigger() == FileAction::OpenWith {
                menu.paths
                    .first()
                    .map(|path| system_apps(path))
                    .unwrap_or_default()
            } else {
                native_file_menu::share_choices(&menu.paths)
                    .into_iter()
                    .map(|choice| {
                        let mut entry = Entry::new(
                            choice.title.clone(),
                            EntryAction::Share {
                                index: choice.index,
                                title: choice.title,
                            },
                        );
                        entry.icon = choice.icon.map(EntryIcon::Share);
                        entry
                    })
                    .collect()
            };
            menu.cache.insert(branch, Some(Ok(entries)));
            return;
        }
        menu.cache.insert(branch, None);
        let identity = menu.identity.clone();
        let paths = menu.paths.clone();
        let home = self.home.clone();
        let worker = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            let entries = worker
                .spawn(async move { load_entries(branch, home, paths) })
                .await
                .map_err(|error| format!("{error:#}"));
            let _ = this.update(cx, |this, cx| {
                if let Some(menu) = &mut this.context_menu
                    && Rc::ptr_eq(&menu.identity, &identity)
                {
                    menu.cache.insert(branch, Some(entries));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn pointer_in_context_menu(
        &mut self,
        event: &MouseMoveEvent,
        layout: Layout,
        cx: &mut Context<Self>,
    ) {
        let Some(menu) = &mut self.context_menu else {
            return;
        };
        let position = event.position;
        let previous = menu.pointer.last.replace(position);
        if previous == Some(position) {
            return;
        }
        if layout
            .submenu
            .is_some_and(|placement| placement.bounds.contains(&position))
        {
            menu.pointer.pending = None;
            menu.pointer.aiming_since = None;
            menu.pointer.trigger = None;
            if let Some(submenu) = &mut menu.submenu
                && submenu.selected.take().is_some()
            {
                cx.notify();
            }
            return;
        }
        if layout.in_submenu_gap(position) {
            menu.pointer.pending = None;
            menu.pointer.aiming_since = None;
            return;
        }
        let target = menu.primary_at(position, layout);
        let active = menu.active_trigger();
        if active.is_some() && active == target {
            menu.pointer.trigger = Some(position);
            menu.pointer.pending = None;
            menu.pointer.aiming_since = None;
            return;
        }
        if let (Some(placement), Some(origin)) = (layout.submenu, menu.pointer.trigger)
            && placement.protects(origin, previous.unwrap_or(position), position)
        {
            let since = *menu.pointer.aiming_since.get_or_insert_with(Instant::now);
            if let Some(delay) = AIM_DELAY.checked_sub(since.elapsed()) {
                self.defer_hover(target, delay, cx);
                return;
            }
        }
        menu.pointer.aiming_since = None;
        if active.is_none() && target.is_some_and(FileAction::expands) {
            self.defer_hover(target, OPEN_DELAY, cx);
        } else if active.is_some() && target.is_none() {
            self.defer_hover(None, LEAVE_DELAY, cx);
        } else {
            self.apply_context_hover(target, cx);
        }
    }

    fn defer_hover(&mut self, target: Option<FileAction>, delay: Duration, cx: &mut Context<Self>) {
        let Some(menu) = &mut self.context_menu else {
            return;
        };
        if menu
            .pointer
            .pending
            .as_ref()
            .is_some_and(|(pending, _)| *pending == target)
        {
            return;
        }
        let timer = cx.background_executor().timer(delay);
        let task = cx.spawn(async move |this, cx| {
            timer.await;
            let _ = this.update(cx, |this, cx| this.apply_context_hover(target, cx));
        });
        menu.pointer.pending = Some((target, task));
    }

    fn apply_context_hover(&mut self, target: Option<FileAction>, cx: &mut Context<Self>) {
        let Some(menu) = &mut self.context_menu else {
            return;
        };
        menu.pointer.pending = None;
        let changed = menu.pointer.highlighted != target || menu.selected.is_some();
        menu.pointer.highlighted = target;
        menu.selected = None;
        if let Some(action) = target.filter(|action| action.expands()) {
            menu.pointer.trigger = menu.pointer.last;
            self.open_context_submenu(action, false, cx);
        } else {
            let closed = menu.submenu.take().is_some();
            if closed || changed {
                cx.notify();
            }
        }
    }

    pub(super) fn dismiss_context_submenu(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(menu) = &mut self.context_menu else {
            return false;
        };
        menu.pointer.pending = None;
        menu.pointer.trigger = None;
        menu.pointer.aiming_since = None;
        let Some(submenu) = menu.submenu.take() else {
            return false;
        };
        menu.selected = menu
            .items()
            .into_iter()
            .flatten()
            .position(|action| action == submenu.branch.trigger());
        cx.notify();
        true
    }

    pub(super) fn context_menu_right(&mut self, cx: &mut Context<Self>) {
        if let Some(action) = self
            .context_menu
            .as_ref()
            .and_then(|menu| menu.selected_action().or(menu.active_trigger()))
        {
            self.open_context_submenu(action, true, cx);
        }
    }

    pub(super) fn move_context_selection(&mut self, step: isize, cx: &mut Context<Self>) {
        let Some(menu) = &mut self.context_menu else {
            return;
        };
        menu.pointer.pending = None;
        menu.pointer.highlighted = None;
        if let Some(branch) = menu.submenu.as_ref().map(|submenu| submenu.branch) {
            let count = match menu.entries(branch) {
                Some(Ok(entries)) => entries.len(),
                _ => 0,
            };
            if let Some(submenu) = &mut menu.submenu {
                submenu.selected = next_index(submenu.selected, step, count);
                if let Some(index) = submenu.selected {
                    submenu.scroll.scroll_to_item(index);
                }
            }
        } else {
            let rows: Vec<_> = menu
                .items()
                .into_iter()
                .enumerate()
                .filter_map(|(index, action)| action.map(|_| index))
                .collect();
            menu.selected = next_index(menu.selected, step, rows.len());
            if let Some(index) = menu.selected {
                menu.scroll.scroll_to_item(rows[index]);
            }
        }
        cx.notify();
    }

    pub(super) fn run_context_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(menu) = &self.context_menu else {
            return;
        };
        if let Some(index) = menu.submenu.as_ref().and_then(|submenu| submenu.selected) {
            self.run_submenu_entry(index, window, cx);
        } else if let Some(action) = menu.selected_action().or(menu.active_trigger()) {
            if action.expands() {
                self.open_context_submenu(action, true, cx);
            } else {
                self.run_file_action(action, window, cx);
            }
        } else {
            self.close_context_menu(window, cx);
        }
    }

    fn run_submenu_entry(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(menu) = &self.context_menu else {
            return;
        };
        let Some(submenu) = &menu.submenu else {
            return;
        };
        let Some(Ok(entries)) = menu.entries(submenu.branch) else {
            return;
        };
        let Some(entry) = entries.get(index) else {
            return;
        };
        let action = entry.action.clone();
        match action {
            EntryAction::OpenWith(app) => return self.open_with(app, window, cx),
            EntryAction::BrowseShortcuts => return self.show_submenu(Branch::Shortcuts, true, cx),
            EntryAction::BackToQuickActions => {
                return self.open_context_submenu(FileAction::QuickActions, true, cx);
            }
            _ => {}
        }
        let title = submenu.branch.trigger().label();
        let paths = menu.paths.clone();
        self.close_context_menu(window, cx);
        let worker = cx.background_executor().clone();
        cx.spawn_in(window, async move |this, cx| {
            let mut tagged = None;
            let result = async {
                match action {
                    EntryAction::Tag { tag, checked } => {
                        tagged = Some(paths.clone());
                        worker
                            .spawn(async move { file_tags::set(&paths, &tag.name, !checked) })
                            .await
                    }
                    EntryAction::AddTag => {
                        if let Some(tag) = native_file_menu::request_tag().await? {
                            tagged = Some(paths.clone());
                            worker
                                .spawn(async move { file_tags::set(&paths, &tag, true) })
                                .await?;
                        }
                        Ok(())
                    }
                    EntryAction::Workflow(path) => {
                        worker
                            .spawn(async move { native_file_menu::run_workflow(&path, &paths) })
                            .await
                    }
                    EntryAction::Shortcut(name) => {
                        worker
                            .spawn(async move { native_file_menu::run_shortcut(&name, &paths) })
                            .await
                    }
                    EntryAction::Service(name) => {
                        native_file_menu::perform_service(name, paths).await?
                    }
                    EntryAction::Share { index, title } => {
                        native_file_menu::share(paths, index, title).await?
                    }
                    _ => Ok(()),
                }
            }
            .await;
            if tagged.is_some() || result.is_err() {
                let _ = this.update_in(cx, |this, window, cx| {
                    if let Some(paths) = tagged {
                        this.refresh_tagged_items(&paths, cx);
                    }
                    if let Err(error) = result {
                        show_file_action_error(title, &error, window, cx);
                    }
                });
            }
        })
        .detach();
    }
}

fn next_index(selected: Option<usize>, step: isize, count: usize) -> Option<usize> {
    (count > 0).then(|| match selected {
        Some(index) => (index as isize + step).rem_euclid(count as isize) as usize,
        None if step > 0 => 0,
        None => count - 1,
    })
}

fn system_apps(path: &std::path::Path) -> Vec<Entry> {
    crate::system::apps_for(path)
        .into_iter()
        .map(|app| {
            let icon = Some(EntryIcon::File(classify(&app.path)));
            Entry {
                title: app.name,
                icon,
                trailing: app.default.then_some("Default"),
                action: EntryAction::OpenWith(app.path),
            }
        })
        .collect()
}

fn load_entries(branch: Branch, home: PathBuf, paths: Vec<PathBuf>) -> anyhow::Result<Vec<Entry>> {
    Ok(match branch {
        Branch::Action(FileAction::Tags) => {
            let mut entries: Vec<_> = file_tags::choices(&paths)?
                .into_iter()
                .map(|(tag, checked)| {
                    let mut entry = Entry::new(tag.name.clone(), EntryAction::Tag { tag, checked });
                    entry.trailing = checked.then_some("✓");
                    entry
                })
                .collect();
            entries.push(Entry::new("Add Tag…", EntryAction::AddTag));
            entries
        }
        Branch::Action(FileAction::QuickActions) => {
            let mut entries: Vec<_> = native_file_menu::workflows(&home, &paths)
                .into_iter()
                .map(|workflow| Entry::new(workflow.title, EntryAction::Workflow(workflow.path)))
                .collect();
            if std::path::Path::new("/usr/bin/shortcuts").is_file() {
                entries.push(Entry::new("Run Shortcut…", EntryAction::BrowseShortcuts));
            }
            entries
        }
        Branch::Action(FileAction::Services) => native_file_menu::services(&home, &paths)
            .into_iter()
            .map(|name| Entry::new(name.clone(), EntryAction::Service(name)))
            .collect(),
        Branch::Shortcuts => {
            let mut entries = vec![Entry::new(
                "Back to Quick Actions",
                EntryAction::BackToQuickActions,
            )];
            entries.extend(native_file_menu::shortcuts()?.into_iter().map(|shortcut| {
                Entry::new(shortcut.title, EntryAction::Shortcut(shortcut.identifier))
            }));
            entries
        }
        _ => Vec::new(),
    })
}

fn panel(id: &'static str, bounds: Bounds<Pixels>) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .absolute()
        .left(bounds.left())
        .top(bounds.top())
        .w(bounds.size.width)
        .h(bounds.size.height)
        .p(dp(PADDING))
        .rounded(dp(radius::MD))
        .bg(rgb(0x222222))
        .shadow_lg()
        .overflow_y_scroll()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
}

fn menu_row() -> gpui::Div {
    div()
        .h(dp(row::HEIGHT))
        .px(dp(10.))
        .flex()
        .items_center()
        .gap(dp(row::GAP))
        .rounded(dp(radius::SM))
}

pub(super) fn render(
    menu: &ContextMenu,
    window: &Window,
    cx: &mut Context<FileBrowser>,
) -> impl IntoElement {
    let items = menu.items();
    let layout = menu.layout(window, &items);
    let mut action_index = 0;
    let active = menu.active_trigger();
    div()
        .id("context-menu-overlay")
        .absolute()
        .inset_0()
        .occlude()
        .on_mouse_move(
            cx.listener(move |this, event, _, cx| this.pointer_in_context_menu(event, layout, cx)),
        )
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                if !layout.in_submenu_gap(event.position) {
                    this.close_context_menu(window, cx);
                }
            }),
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                if !layout.in_submenu_gap(event.position) {
                    this.close_context_menu(window, cx);
                }
            }),
        )
        .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
        .child(
            panel("context-menu", layout.main)
                .track_scroll(&menu.scroll)
                .on_scroll_wheel(cx.listener(|this, _, _, cx| {
                    this.dismiss_context_submenu(cx);
                }))
                .children(items.into_iter().map(|item| {
                    let Some(action) = item else {
                        return div()
                            .h(dp(SEPARATOR))
                            .flex()
                            .items_center()
                            .px(dp(6.))
                            .child(div().h(px(1.)).w_full().bg(rgb(0x333333)))
                            .into_any_element();
                    };
                    let index = action_index;
                    action_index += 1;
                    let highlighted = active == Some(action)
                        || menu.selected == Some(index)
                        || (active.is_none()
                            && menu.selected.is_none()
                            && menu.pointer.highlighted == Some(action));
                    menu_row()
                        .id(("context-action", index))
                        .when(highlighted, |row| row.bg(rgb(0x303840)))
                        .cursor_pointer()
                        .on_click(
                            cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                                cx.stop_propagation();
                                if action.expands() {
                                    if let Some(menu) = &mut this.context_menu {
                                        menu.pointer.trigger = Some(event.position());
                                        menu.pointer.last = Some(event.position());
                                    }
                                    this.open_context_submenu(action, false, cx);
                                } else {
                                    this.run_file_action(action, window, cx);
                                }
                            }),
                        )
                        .child(div().flex_1().min_w_0().truncate().child(action.label()))
                        .when(action.expands(), |row| {
                            row.child(native_icons::symbol("chevron.right", 10.))
                        })
                        .children(action.shortcut().map(trailing))
                        .into_any_element()
                })),
        )
        .when_some(
            menu.submenu.as_ref().zip(layout.submenu),
            |overlay, (submenu, placement)| {
                let contents = menu.entries(submenu.branch);
                let entries = match contents {
                    Some(Ok(entries)) => entries.as_slice(),
                    _ => &[],
                };
                overlay.child(
                    panel("context-submenu", placement.bounds)
                        .track_scroll(&submenu.scroll)
                        .when(entries.is_empty(), |panel| {
                            let message = match contents {
                                None => "Loading…",
                                Some(Err(error)) => error,
                                Some(Ok(_)) => match submenu.branch.trigger() {
                                    FileAction::OpenWith => "No apps",
                                    FileAction::Share => "No sharing options",
                                    FileAction::Services => "No services for this selection",
                                    FileAction::QuickActions => {
                                        "No Quick Actions for this selection"
                                    }
                                    _ => "No options",
                                },
                            };
                            panel.child(
                                menu_row()
                                    .text_color(rgb(0x999999))
                                    .truncate()
                                    .child(message.to_owned()),
                            )
                        })
                        .children(entries.iter().enumerate().map(|(index, entry)| {
                            menu_row()
                                .id(("context-submenu-action", index))
                                .when(submenu.selected == Some(index), |row| row.bg(rgb(0x303840)))
                                .when(submenu.selected.is_none(), |row| {
                                    row.hover(|row| row.bg(rgb(0x303840)))
                                })
                                .cursor_pointer()
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.run_submenu_entry(index, window, cx);
                                }))
                                .children(entry.icon.as_ref().map(EntryIcon::render))
                                .children(tag_swatch(&entry.action))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .child(entry.title.clone()),
                                )
                                .children(entry.trailing.map(trailing))
                        })),
                )
            },
        )
}

fn tag_swatch(action: &EntryAction) -> Option<gpui::Div> {
    let EntryAction::Tag { tag, .. } = action else {
        return None;
    };
    Some(
        div()
            .size(dp(12.))
            .flex_shrink_0()
            .rounded(dp(radius::XS))
            .when_some(tag.color, |swatch, color| swatch.bg(rgb(color))),
    )
}

fn trailing(label: &'static str) -> gpui::Div {
    div()
        .flex_shrink_0()
        .text_size(dp(text_size::DETAIL))
        .text_color(rgb(0x999999))
        .child(label)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(x: f32, y: f32) -> Point<Pixels> {
        point(px(x), px(y))
    }

    #[test]
    fn safe_triangle_holds_diagonal_motion_toward_either_side() {
        let right = Placement {
            bounds: Bounds::new(at(200., 40.), size(px(240.), px(160.))),
            opens_left: false,
        };
        let left = Placement {
            bounds: Bounds::new(at(0., 40.), size(px(200.), px(160.))),
            opens_left: true,
        };
        assert!(right.protects(at(80., 70.), at(100., 75.), at(155., 105.)));
        assert!(left.protects(at(320., 70.), at(300., 75.), at(255., 105.)));
        assert!(right.protects(at(80., 70.), at(155., 105.), at(155., 106.)));
        assert!(!right.protects(at(80., 70.), at(155., 105.), at(125., 100.)));
        assert!(!left.protects(at(320., 70.), at(300., 75.), at(255., 195.)));
    }

    #[test]
    fn submenu_gap_stays_open_on_either_side() {
        let gap = px(SUBMENU_GAP);
        for x in [40., 400.] {
            let main = Bounds::new(at(x, 20.), size(px(220.), px(300.)));
            let submenu = Placement::beside(
                main,
                px(680.),
                px(8.),
                gap,
                size(px(240.), px(160.)),
                px(68.),
            );
            let (separation, midpoint) = if submenu.opens_left {
                (main.left() - submenu.bounds.right(), main.left() - gap / 2.)
            } else {
                (
                    submenu.bounds.left() - main.right(),
                    main.right() + gap / 2.,
                )
            };
            assert_eq!(submenu.opens_left, x == 400.);
            assert_eq!(separation, gap);
            assert!(submenu.in_gap(main, point(midpoint, px(102.))));
            assert!(!submenu.in_gap(main, point(midpoint, px(52.))));
        }
    }

    #[test]
    fn narrow_submenu_shrinks_without_consuming_the_gap() {
        let main = Bounds::new(at(100., 20.), size(px(220.), px(300.)));
        let submenu = Placement::beside(
            main,
            px(480.),
            px(8.),
            px(SUBMENU_GAP),
            size(px(240.), px(160.)),
            px(68.),
        );
        assert_eq!(submenu.bounds.left() - main.right(), px(SUBMENU_GAP));
        assert_eq!(submenu.bounds.right(), px(472.));
    }

    #[test]
    fn keyboard_selection_wraps_and_skips_empty_menus() {
        assert_eq!(next_index(None, -1, 3), Some(2));
        assert_eq!(next_index(Some(2), 1, 3), Some(0));
        assert_eq!(next_index(Some(0), -1, 3), Some(2));
        assert_eq!(next_index(Some(0), 1, 0), None);
    }
}
