//! The settings panel (⌘,): file preferences, and what the file index
//! occupies on disk as one bar split by indexed folder.
use gpui::{IntoElement, MouseButton, canvas, div, fill, prelude::*, px, rgb};

use crate::{
    file_list::format_size,
    search_index::IndexUsage,
    storage::UserSettings,
    ui::{self, dp, radius, text_size},
};

const BAR_HEIGHT: f32 = 14.;
/// The panel's width: narrow enough that controls stay near their labels.
const COLUMN_WIDTH: f32 = 600.;
const ROW_HEIGHT: f32 = 40.;
/// Grouped rows sit on this fill, a step above the panel surface.
const GROUP_FILL: u32 = 0x2a2a2a;
/// The panel's least distance from the window edges.
const MARGIN: f32 = 32.;
/// The Gaussian's standard deviation for the window content behind the panel.
const BACKDROP_BLUR: f32 = 24.;
/// Folder slots in order (validated as a dark categorical set on the panel
/// surface), then a neutral for Other that stays separable from the last slot
/// under color-vision deficiency.
const FOLDER_COLORS: [u32; 5] = [0x3987e5, 0xd95926, 0x199e70, 0xc98500, 0xd55181];
const OTHER_COLOR: u32 = 0x4f4f4f;

/// The dimmed overlay and panel. `usage` is `None` until an index is loaded.
pub(super) fn render(usage: Option<&IndexUsage>, settings: &UserSettings) -> impl IntoElement {
    div()
        .id("settings-overlay")
        .absolute()
        .inset_0()
        .occlude()
        .p(dp(MARGIN))
        .flex()
        .items_center()
        .justify_center()
        .on_mouse_down(MouseButton::Left, |_, window, cx| {
            cx.stop_propagation();
            window.dispatch_action(Box::new(crate::ToggleSettings), cx);
        })
        .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
        // Blur what is already painted, then dim it. The overlay itself has no
        // fill, which the blur would otherwise take in.
        .child(
            canvas(
                |_, _, _| (),
                |bounds, _, window, _| {
                    let blur = dp(BACKDROP_BLUR).to_pixels(window.rem_size());
                    window.paint_backdrop_blur(bounds, blur, px(0.), None);
                    window.paint_quad(fill(bounds, gpui::rgba(0x00000055)));
                },
            )
            .absolute()
            .inset_0(),
        )
        .child(
            div()
                .id("settings")
                // Sized to its contents, within the window's margin.
                .w(dp(COLUMN_WIDTH))
                .max_w_full()
                .max_h_full()
                .overflow_y_scroll()
                .rounded(dp(radius::LG))
                .bg(rgb(0x222222))
                .shadow_lg()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .relative()
                .child(
                    div()
                        .px(dp(20.))
                        .pb(dp(20.))
                        .flex()
                        .flex_col()
                        .gap(dp(20.))
                        .child(
                            div()
                                // Centered on the close button (12 + 28 / 2 = 26 from the top).
                                .mt(dp(4.))
                                .h(dp(44.))
                                // On the section headings' and row labels' column.
                                .px(dp(12.))
                                .flex()
                                .items_center()
                                .text_size(dp(text_size::INPUT))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .child("Settings"),
                        )
                        .child(section("General", files(settings)))
                        .child(section("Storage", index_storage(usage)))
                        .child(
                            div()
                                .flex()
                                .justify_center()
                                .text_size(dp(text_size::DETAIL))
                                .text_color(rgb(0x808080))
                                .child(concat!("Filetree ", env!("CARGO_PKG_VERSION"))),
                        ),
                )
                // The panel's corner, not the column's.
                .child(
                    div()
                        .absolute()
                        .top(dp(12.))
                        .right(dp(12.))
                        .child(close_button()),
                ),
        )
}

fn close_button() -> impl IntoElement {
    div()
        .id("close-settings")
        .flex_shrink_0()
        .size(dp(28.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(dp(radius::SM))
        .cursor_pointer()
        .hover(|button| button.bg(rgb(0x303030)))
        .on_click(|_, window, cx| {
            cx.stop_propagation();
            window.dispatch_action(Box::new(crate::ToggleSettings), cx);
        })
        .child(crate::native_icons::symbol("xmark", 12.))
}

/// A small heading over a rounded group of rows.
fn section(title: &'static str, rows: impl IntoElement) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(dp(8.))
        .child(
            div()
                .px(dp(12.))
                .text_size(dp(text_size::DETAIL))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(rgb(0xaaaaaa))
                .child(title),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .rounded(dp(radius::MD))
                .bg(rgb(GROUP_FILL))
                .px(dp(12.))
                .child(rows),
        )
}

/// A labeled row; rows after the first get a hairline above them.
fn row(label: impl IntoElement, first: bool) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(dp(16.))
        .h(dp(ROW_HEIGHT))
        .when(!first, |row| row.border_t_1().border_color(rgb(0x333333)))
        .child(label)
}

/// One option of a two-way choice, highlighted when chosen.
fn choice(
    id: &'static str,
    label: &'static str,
    chosen: bool,
    action: fn() -> Box<dyn gpui::Action>,
) -> impl IntoElement {
    div()
        .id(id)
        .px(dp(10.))
        .h(dp(26.))
        .flex()
        .items_center()
        .rounded(dp(radius::SM))
        .text_size(dp(text_size::DETAIL))
        .cursor_pointer()
        .when(chosen, |option| {
            option.bg(rgb(0x3a3a3a)).text_color(rgb(0xeeeeee))
        })
        .when(!chosen, |option| {
            option
                .text_color(rgb(0xaaaaaa))
                .hover(|option| option.bg(rgb(0x2e2e2e)))
        })
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            window.dispatch_action(action(), cx);
        })
        .child(label)
}

fn files(settings: &UserSettings) -> impl IntoElement {
    let arrows = settings.arrow_keys;
    div()
        .child(
            row(
                div()
                    .flex()
                    .items_center()
                    .gap(dp(6.))
                    .child("⌘← and ⌘→")
                    .child(
                        div()
                            .id("arrow-keys-help")
                            // A 20-point target around the 13-point glyph.
                            .size(dp(20.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .tooltip(ui::tooltip(
                                "Back and forward: ⌘← and ⌘→ move through folders you've visited, like a browser.\n\nFolder path: ⌘← goes up to the parent folder, and ⌘→ goes back down.",
                            ))
                            .child(crate::native_icons::symbol("info.circle", 13.)),
                    ),
                true,
            )
            .child(
                div()
                    .flex()
                    .p(dp(2.))
                    .gap(dp(2.))
                    .rounded(dp(radius::MD))
                    .bg(rgb(0x1f1f1f))
                    .child(choice(
                        "arrows-history",
                        "Back and forward",
                        arrows == crate::storage::ArrowKeys::History,
                        || Box::new(crate::ArrowKeysHistory),
                    ))
                    .child(choice(
                        "arrows-folder-path",
                        "Folder path",
                        arrows == crate::storage::ArrowKeys::FolderPath,
                        || Box::new(crate::ArrowKeysFolderPath),
                    )),
            ),
        )
        .child(
            row("Play sounds", false)
                .id("play-sounds")
                .cursor_pointer()
                .on_click(|_, window, cx| {
                    cx.stop_propagation();
                    window.dispatch_action(Box::new(crate::ToggleSounds), cx);
                })
                .child(ui::checkbox(!settings.mute_sounds)),
        )
        .child(
            row("Show hidden files", false)
                .id("show-hidden-files")
                .cursor_pointer()
                .on_click(|_, window, cx| {
                    cx.stop_propagation();
                    window.dispatch_action(Box::new(crate::ToggleHiddenFiles), cx);
                })
                .child(ui::checkbox(settings.show_hidden_files)),
        )
}

fn index_storage(usage: Option<&IndexUsage>) -> impl IntoElement {
    let parts: Vec<_> = usage
        .map(|usage| {
            usage
                .parts
                .iter()
                .enumerate()
                .map(|(slot, (label, bytes))| {
                    let color = if slot + 1 == usage.parts.len() {
                        OTHER_COLOR
                    } else {
                        FOLDER_COLORS[slot]
                    };
                    (label.clone(), *bytes, color)
                })
                .collect()
        })
        .unwrap_or_default();
    div()
        .flex()
        .flex_col()
        .gap(dp(10.))
        .pb(dp(12.))
        .child(
            row("File index", true).child(
                div()
                    .font(ui::tabular_font())
                    .text_color(rgb(0xaaaaaa))
                    .child(usage.map_or("Indexing…".into(), |usage| format_size(usage.total))),
            ),
        )
        .child(
            div()
                .flex()
                .h(dp(BAR_HEIGHT))
                .gap(dp(2.))
                .rounded(dp(radius::XS))
                .overflow_hidden()
                .when(usage.is_none(), |bar| bar.bg(rgb(0x2a2a2a)))
                .children(parts.iter().filter(|(_, bytes, _)| *bytes > 0).map(
                    |(_, bytes, color)| {
                        // Grow by share rather than a percentage basis, which
                        // needs a definite bar width to resolve against.
                        let mut segment = div()
                            .h_full()
                            .flex_basis(px(0.))
                            .min_w(dp(2.))
                            .rounded(dp(radius::XS))
                            .bg(rgb(*color));
                        segment.style().flex_grow = Some(*bytes as f32);
                        segment
                    },
                )),
        )
        .child(
            div()
                .flex()
                .flex_wrap()
                .gap_x(dp(16.))
                .gap_y(dp(6.))
                .text_size(dp(text_size::DETAIL))
                .children(parts.into_iter().map(|(label, bytes, color)| {
                    div()
                        .flex()
                        .items_center()
                        .gap(dp(6.))
                        .child(div().size(dp(8.)).rounded_full().bg(rgb(color)))
                        .child(label)
                        .child(
                            div()
                                .font(ui::tabular_font())
                                .text_color(rgb(0xaaaaaa))
                                .child(format_size(bytes)),
                        )
                })),
        )
}
