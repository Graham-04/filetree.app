use std::{
    path::{Path, PathBuf},
    rc::Rc,
    time::Instant,
};

use gpui::{
    App, ExternalPaths, IntoElement, MouseButton, MouseDownEvent, Pixels, Point, Window, div,
    prelude::*, px, rgb,
};

use crate::{
    native_icons::FileIcon,
    storage::SidebarState,
    ui::{Reveal, dp, radius, row, text_size},
};

pub(super) const WIDTH: f32 = 220.;
const INDEX_STATUS_HEIGHT: f32 = 20. + 6. + 3. + 8. * 2.;

/// Opens a recent item in the browser.
pub(super) type OpenRecent = Rc<dyn Fn(PathBuf, &mut Window, &mut App)>;

/// What the sidebar shows this frame.
pub(super) struct SidebarView<'a> {
    pub width: f32,
    pub header_height: Pixels,
    pub progress: &'a crate::search_index::Progress,
    pub index_amount: f32,
    pub home: &'a Path,
    pub location: &'a Path,
    pub recent: &'a [(PathBuf, FileIcon)],
    pub open_recent: OpenRecent,
    pub drop_into: DropInto,
    pub sounds_muted: bool,
    /// Mounted volumes, with their icons.
    pub volumes: &'a [(crate::system::Volume, FileIcon)],
    pub trash: &'a Path,
    pub go_to: GoTo,
    pub eject: Eject,
    pub trash_files: TrashFiles,
    pub context_menu: ContextMenuAt,
}

/// What kind of sidebar item was right-clicked, which decides its menu.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Place {
    /// A favorite folder or the Trash.
    Folder,
    /// A Recent item, a file or a folder.
    Recent,
    Volume {
        ejectable: bool,
    },
}

/// Opens the right-click menu for a sidebar item at a window position.
pub(super) type ContextMenuAt = Rc<dyn Fn(Point<Pixels>, PathBuf, Place, &mut Window, &mut App)>;

/// Opens a folder in the active tab.
pub(super) type GoTo = Rc<dyn Fn(PathBuf, &mut Window, &mut App)>;
/// Ejects a volume.
pub(super) type Eject = Rc<dyn Fn(PathBuf, &mut App)>;
/// Moves dropped files to the Trash.
pub(super) type TrashFiles = Rc<dyn Fn(Vec<PathBuf>, &mut App)>;

/// Moves or copies dropped files into a sidebar folder.
pub(super) type DropInto = Rc<dyn Fn(Vec<PathBuf>, PathBuf, &mut App)>;

pub(super) struct Sidebar {
    reveal: Reveal,
    index_reveal: Reveal,
    favorites_open: bool,
    recent_open: bool,
    locations_open: bool,
}

impl Sidebar {
    pub(super) fn new(state: SidebarState) -> Self {
        Self {
            reveal: Reveal::new(state.open),
            index_reveal: Reveal::new(false),
            favorites_open: state.favorites_open,
            recent_open: state.recent_open,
            locations_open: state.locations_open,
        }
    }

    /// What to restore at the next launch.
    pub(super) fn state(&self) -> SidebarState {
        SidebarState {
            open: self.reveal.is_open(),
            favorites_open: self.favorites_open,
            recent_open: self.recent_open,
            locations_open: self.locations_open,
        }
    }

    pub(super) fn toggle_favorites(&mut self) {
        self.favorites_open = !self.favorites_open;
    }

    pub(super) fn toggle_recent(&mut self) {
        self.recent_open = !self.recent_open;
    }

    pub(super) fn toggle_locations(&mut self) {
        self.locations_open = !self.locations_open;
    }

    pub(super) fn index_amount(&mut self, active: bool, window: &Window) -> f32 {
        self.index_reveal.set(active, Instant::now());
        self.index_reveal.amount(window)
    }

    pub(super) fn render(&self, view: SidebarView<'_>) -> impl IntoElement {
        let SidebarView {
            width,
            header_height,
            progress,
            index_amount,
            home,
            location,
            recent,
            open_recent,
            drop_into,
            sounds_muted,
            volumes,
            trash,
            go_to,
            eject,
            trash_files,
            context_menu,
        } = view;
        // Right-clicking a row opens its menu, as in Finder's sidebar.
        let menu_for = |path: PathBuf, place: Place| {
            let open = context_menu.clone();
            move |event: &MouseDownEvent, window: &mut Window, cx: &mut App| {
                cx.stop_propagation();
                open(event.position, path.clone(), place, window, cx);
            }
        };
        div()
            .w(px(width))
            .h_full()
            .flex_shrink_0()
            .overflow_hidden()
            .child(
                div()
                    .w(dp(WIDTH))
                    .h_full()
                    .flex()
                    .flex_col()
                    .bg(rgb(0x181818))
                    .pt(header_height)
                    .child(
                        div()
                            .id("sidebar-scroll")
                            .flex_1()
                            .min_h(dp(0.))
                            .overflow_y_scroll()
                            .px(dp(10.))
                            .pb(dp(10.))
                            .child(
                                section_heading(
                                    "toggle-favorites",
                                    "Folders",
                                    self.favorites_open,
                                    || Box::new(crate::ToggleFavorites),
                                )
                                .mt_0(),
                            )
                            .when(self.favorites_open, |list| {
                                type Favorite = (
                                    &'static str,
                                    &'static str,
                                    fn() -> Box<dyn gpui::Action>,
                                    std::path::PathBuf,
                                );
                                let favorites: [Favorite; 8] = [
                                    (
                                        "Home",
                                        "house",
                                        || Box::new(crate::GoHome),
                                        home.to_path_buf(),
                                    ),
                                    (
                                        "Desktop",
                                        "menubar.dock.rectangle",
                                        || Box::new(crate::GoDesktop),
                                        home.join("Desktop"),
                                    ),
                                    (
                                        "Documents",
                                        "doc",
                                        || Box::new(crate::GoDocuments),
                                        home.join("Documents"),
                                    ),
                                    (
                                        "Downloads",
                                        "arrow.down.circle",
                                        || Box::new(crate::GoDownloads),
                                        home.join("Downloads"),
                                    ),
                                    (
                                        "Applications",
                                        "square.grid.2x2",
                                        || Box::new(crate::GoApplications),
                                        "/Applications".into(),
                                    ),
                                    (
                                        "Pictures",
                                        "photo",
                                        || Box::new(crate::GoPictures),
                                        home.join("Pictures"),
                                    ),
                                    (
                                        "Music",
                                        "music.note",
                                        || Box::new(crate::GoMusic),
                                        home.join("Music"),
                                    ),
                                    (
                                        "Movies",
                                        "film",
                                        || Box::new(crate::GoMovies),
                                        home.join("Movies"),
                                    ),
                                ];
                                list.children(favorites.into_iter().map(
                                    |(label, symbol, action, path)| {
                                        let drop = drop_into.clone();
                                        let target = path.clone();
                                        item(label, symbol)
                                            .id(label)
                                            .drag_over::<ExternalPaths>(|row, _, _, _| {
                                                row.bg(rgb(0x1d4f7d))
                                            })
                                            .on_drop(move |paths: &ExternalPaths, _, cx| {
                                                drop(paths.paths().to_vec(), target.clone(), cx)
                                            })
                                            .rounded(dp(radius::SM))
                                            .cursor_pointer()
                                            // Hover stays quieter than the current location.
                                            .when(path == location, |row| row.bg(rgb(0x303030)))
                                            .when(path != location, |row| {
                                                row.hover(|row| row.bg(rgb(0x252525)))
                                            })
                                            .on_mouse_down(
                                                MouseButton::Right,
                                                menu_for(path.clone(), Place::Folder),
                                            )
                                            .on_click(move |_, window, cx| {
                                                cx.stop_propagation();
                                                window.dispatch_action(action(), cx);
                                            })
                                    },
                                ))
                            })
                            .child(section_heading(
                                "toggle-recent",
                                "Recent",
                                self.recent_open,
                                || Box::new(crate::ToggleRecent),
                            ))
                            .when(self.recent_open, |list| {
                                list.children(recent.iter().enumerate().map(
                                    |(index, (path, icon))| {
                                        let name = path
                                            .file_name()
                                            .unwrap_or(path.as_os_str())
                                            .to_string_lossy()
                                            .into_owned();
                                        let open = open_recent.clone();
                                        let target = path.clone();
                                        div()
                                            .id(("recent", index))
                                            .flex()
                                            .items_center()
                                            .gap(dp(row::GAP))
                                            .h(dp(row::HEIGHT))
                                            .px(dp(10.))
                                            .rounded(dp(radius::SM))
                                            .cursor_pointer()
                                            .when(path == location, |row| row.bg(rgb(0x303030)))
                                            .when(path != location, |row| {
                                                row.hover(|row| row.bg(rgb(0x252525)))
                                            })
                                            .on_mouse_down(
                                                MouseButton::Right,
                                                menu_for(path.clone(), Place::Recent),
                                            )
                                            .on_click(move |_, window, cx| {
                                                cx.stop_propagation();
                                                open(target.clone(), window, cx);
                                            })
                                            .child(crate::native_icons::file_icon(
                                                icon.clone(),
                                                row::ICON,
                                            ))
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w(dp(0.))
                                                    .text_ellipsis()
                                                    .child(name),
                                            )
                                    },
                                ))
                            })
                            .child(section_heading(
                                "toggle-locations",
                                "Locations",
                                self.locations_open,
                                || Box::new(crate::ToggleLocations),
                            ))
                            .when(self.locations_open, |list| {
                                list.children(volumes.iter().enumerate().map(
                                    |(index, (volume, icon))| {
                                        let target = volume.path.clone();
                                        let go = go_to.clone();
                                        let drop = drop_into.clone();
                                        let drop_target = volume.path.clone();
                                        div()
                                            .id(("volume", index))
                                            .flex()
                                            .items_center()
                                            .gap(dp(row::GAP))
                                            .h(dp(row::HEIGHT))
                                            .px(dp(10.))
                                            .rounded(dp(radius::SM))
                                            .cursor_pointer()
                                            .when(volume.path == location, |row| {
                                                row.bg(rgb(0x303030))
                                            })
                                            .when(volume.path != location, |row| {
                                                row.hover(|row| row.bg(rgb(0x252525)))
                                            })
                                            .drag_over::<ExternalPaths>(|row, _, _, _| {
                                                row.bg(rgb(0x1d4f7d))
                                            })
                                            .on_drop(move |paths: &ExternalPaths, _, cx| {
                                                drop(
                                                    paths.paths().to_vec(),
                                                    drop_target.clone(),
                                                    cx,
                                                )
                                            })
                                            .on_mouse_down(
                                                MouseButton::Right,
                                                menu_for(
                                                    volume.path.clone(),
                                                    Place::Volume {
                                                        ejectable: volume.ejectable,
                                                    },
                                                ),
                                            )
                                            .on_click(move |_, window, cx| {
                                                cx.stop_propagation();
                                                go(target.clone(), window, cx);
                                            })
                                            .child(crate::native_icons::file_icon(
                                                icon.clone(),
                                                row::ICON,
                                            ))
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w(dp(0.))
                                                    .text_ellipsis()
                                                    .child(volume.name.clone()),
                                            )
                                            .when(volume.ejectable, |row| {
                                                let eject = eject.clone();
                                                let path = volume.path.clone();
                                                row.child(eject_button(index, move |cx| {
                                                    eject(path.clone(), cx)
                                                }))
                                            })
                                    },
                                ))
                            }),
                    )
                    .child(
                        div()
                            .h(dp(INDEX_STATUS_HEIGHT * index_amount))
                            .min_h(dp(0.))
                            .flex_shrink_0()
                            .overflow_hidden()
                            .when(index_amount > 0., |view| {
                                view.child(
                                    div()
                                        .h(dp(INDEX_STATUS_HEIGHT))
                                        .px(dp(20.))
                                        .py(dp(8.))
                                        .opacity(index_amount)
                                        .child(index_status(progress))
                                        .child(
                                            div()
                                                .mt(dp(6.))
                                                .h(dp(3.))
                                                .w_full()
                                                .rounded_full()
                                                .overflow_hidden()
                                                .bg(rgb(0x303030))
                                                .child(
                                                    div()
                                                        .h_full()
                                                        .w(gpui::relative(
                                                            (progress.percent / 100.).clamp(0., 1.),
                                                        ))
                                                        .bg(rgb(0x0a84ff)),
                                                ),
                                        ),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(dp(4.))
                            .px(dp(10.))
                            .py(dp(6.))
                            .border_t_1()
                            .border_color(rgb(0x262626))
                            .child({
                                let go = go_to.clone();
                                let target = trash.to_path_buf();
                                item("Trash", "trash")
                                    .id("trash")
                                    .flex_1()
                                    .min_w(dp(0.))
                                    .rounded(dp(radius::SM))
                                    .cursor_pointer()
                                    .when(trash == location, |row| row.bg(rgb(0x303030)))
                                    .when(trash != location, |row| {
                                        row.hover(|row| row.bg(rgb(0x252525)))
                                    })
                                    // Dropping on Trash moves the files there.
                                    .drag_over::<ExternalPaths>(|row, _, _, _| {
                                        row.bg(rgb(0x1d4f7d))
                                    })
                                    .on_drop(move |paths: &ExternalPaths, _, cx| {
                                        trash_files(paths.paths().to_vec(), cx)
                                    })
                                    .on_mouse_down(
                                        MouseButton::Right,
                                        menu_for(trash.to_path_buf(), Place::Folder),
                                    )
                                    .on_click(move |_, window, cx| {
                                        cx.stop_propagation();
                                        go(target.clone(), window, cx);
                                    })
                            })
                            .child(sound_button(sounds_muted))
                            .child(settings_button()),
                    ),
            )
    }

    pub(super) fn toggle(&mut self, now: Instant) {
        self.reveal.set(!self.reveal.is_open(), now);
    }

    pub(super) fn width(&self, window: &Window) -> f32 {
        f32::from(dp(WIDTH).to_pixels(window.rem_size())) * self.reveal.amount(window)
    }
}

fn item(label: &'static str, symbol: &'static str) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(dp(row::GAP))
        .h(dp(row::HEIGHT))
        .px(dp(10.))
        .child(crate::native_icons::symbol(symbol, row::ICON))
        .child(div().flex_1().min_w(dp(0.)).text_ellipsis().child(label))
}

fn section_heading(
    id: &'static str,
    label: &'static str,
    open: bool,
    action: fn() -> Box<dyn gpui::Action>,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_between()
        .mt(dp(16.))
        .h(dp(row::HEIGHT))
        .px(dp(10.))
        .rounded(dp(radius::SM))
        .text_size(dp(text_size::DETAIL))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(rgb(0xaaaaaa))
        .cursor_pointer()
        .hover(|row| row.bg(rgb(0x252525)))
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            window.dispatch_action(action(), cx);
        })
        .child(label)
        .child(crate::native_icons::symbol(
            if open {
                "chevron.down"
            } else {
                "chevron.right"
            },
            12.,
        ))
}

/// Turns the app's sounds on or off; crossed out while muted.
fn sound_button(muted: bool) -> impl IntoElement {
    div()
        .id("toggle-sounds")
        .flex_shrink_0()
        .size(dp(row::HEIGHT))
        .flex()
        .items_center()
        .justify_center()
        .rounded(dp(radius::SM))
        .cursor_pointer()
        .hover(|button| button.bg(rgb(0x252525)))
        .on_click(|_, window, cx| {
            cx.stop_propagation();
            window.dispatch_action(Box::new(crate::ToggleSounds), cx);
        })
        .child(crate::native_icons::symbol(
            if muted {
                "speaker.slash"
            } else {
                "speaker.wave.2"
            },
            row::ICON,
        ))
}

/// Opens Settings (also ⌘, and ⌘K), for anyone who does not know the shortcut.
fn settings_button() -> impl IntoElement {
    div()
        .id("open-settings")
        .flex_shrink_0()
        .size(dp(row::HEIGHT))
        .flex()
        .items_center()
        .justify_center()
        .rounded(dp(radius::SM))
        .cursor_pointer()
        .hover(|button| button.bg(rgb(0x252525)))
        .on_click(|_, window, cx| {
            cx.stop_propagation();
            window.dispatch_action(Box::new(crate::ToggleSettings), cx);
        })
        .child(crate::native_icons::symbol("gearshape", row::ICON))
}

fn eject_button(index: usize, eject: impl Fn(&mut App) + 'static) -> impl IntoElement {
    div()
        .id(("eject", index))
        .flex_shrink_0()
        .size(dp(20.))
        // Extend into the row's padding so the glyph lines up with the
        // section chevrons above.
        .mr(dp(-4.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(dp(radius::SM))
        .cursor_pointer()
        .hover(|button| button.bg(rgb(0x2e2e2e)))
        .on_click(move |_, _, cx| {
            cx.stop_propagation();
            eject(cx);
        })
        .child(crate::native_icons::symbol("eject", 12.))
}

fn index_status(progress: &crate::search_index::Progress) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .h(dp(20.))
        .gap(dp(6.))
        .font(crate::ui::tabular_font())
        .text_size(dp(text_size::DETAIL))
        .text_color(rgb(0xaaaaaa))
        .child(
            div()
                .flex_1()
                .min_w(dp(0.))
                .text_ellipsis()
                .child(progress.label.clone()),
        )
        .child(
            div()
                .w(dp(34.))
                .flex_shrink_0()
                .text_right()
                .child(format!("{:.0}%", progress.percent)),
        )
}
