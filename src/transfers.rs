use std::{
    ops::Range,
    path::PathBuf,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use gpui::{
    Context, IntoElement, MouseButton, PathBuilder, Pixels, Window, canvas, div, point, prelude::*,
    px, rgb, uniform_list,
};

use crate::{
    FileBrowser, SEARCH_CLOSED, SEARCH_OPEN, TAB_ROW_END,
    file_list::format_size,
    native_icons,
    ui::{dp, radius, text_size},
};

pub(super) const BUTTON_WIDTH: f32 = 32.;
const ROW_HEIGHT: f32 = 46.;
const HEADER_HEIGHT: f32 = 36.;
const INSET: f32 = 12.;
#[cfg(any(debug_assertions, feature = "dev-tools"))]
const SIMULATION_STAGGER: Duration = Duration::from_secs(4);
#[cfg(any(debug_assertions, feature = "dev-tools"))]
const SIMULATION_DURATION: Duration = Duration::from_secs(12);

pub(super) enum Status {
    Queued,
    Preparing { copy: bool },
    Transferring { copy: bool, total: Option<u64> },
    Finalizing,
    Cancelling,
    Cancelled,
    Done,
    Skipped,
    Failed(String),
}

impl Status {
    fn can_cancel(&self) -> bool {
        matches!(
            self,
            Self::Queued | Self::Preparing { .. } | Self::Transferring { .. }
        )
    }

    fn cancelled(&self) -> bool {
        matches!(self, Self::Cancelling | Self::Cancelled)
    }
}

pub(super) struct Item {
    pub source: PathBuf,
    pub folder: PathBuf,
    status: Mutex<Status>,
    bytes: AtomicU64,
}

impl Item {
    pub fn new(source: PathBuf, folder: PathBuf) -> Self {
        Self {
            source,
            folder,
            status: Mutex::new(Status::Queued),
            bytes: AtomicU64::new(0),
        }
    }

    pub fn name(&self) -> std::borrow::Cow<'_, str> {
        self.source
            .file_name()
            .unwrap_or(self.source.as_os_str())
            .to_string_lossy()
    }

    fn status(&self) -> std::sync::MutexGuard<'_, Status> {
        self.status.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn advance(&self, next: Status) -> bool {
        let mut status = self.status();
        if status.cancelled() {
            return false;
        }
        *status = next;
        true
    }

    /// Records a worker's result, acknowledging a pending cancellation first.
    pub fn finish(&self, result: Status) -> bool {
        let mut status = self.status();
        let completed = !status.cancelled();
        *status = if completed { result } else { Status::Cancelled };
        completed
    }

    pub fn is_cancelled(&self) -> bool {
        self.status().cancelled()
    }

    pub(super) fn cancel(&self) {
        let mut status = self.status();
        if status.can_cancel() {
            *status = if matches!(*status, Status::Queued) {
                Status::Cancelled
            } else {
                Status::Cancelling
            };
        }
    }

    pub fn set_bytes(&self, bytes: u64) {
        self.bytes.fetch_max(bytes, Ordering::Relaxed);
    }

    fn fraction(&self, status: &Status) -> f32 {
        match status {
            Status::Transferring {
                total: Some(total), ..
            } if *total != 0 => {
                (self.bytes.load(Ordering::Relaxed) as f64 / *total as f64).min(1.) as f32
            }
            Status::Done | Status::Skipped | Status::Cancelled | Status::Finalizing => 1.,
            _ => 0.,
        }
    }

    fn label(&self, status: &Status) -> String {
        match status {
            Status::Queued => "Queued".into(),
            Status::Preparing { copy: true } => "Preparing copy…".into(),
            Status::Preparing { copy: false } => "Preparing move…".into(),
            Status::Transferring { copy, total } => {
                let action = if *copy { "Copying" } else { "Moving" };
                let bytes = self.bytes.load(Ordering::Relaxed);
                match total {
                    Some(total) if *total > 0 => format!(
                        "{action} · {} / {}",
                        format_size(bytes.min(*total)),
                        format_size(*total)
                    ),
                    _ if bytes > 0 => format!("{action} · {}", format_size(bytes)),
                    _ => format!("{action}…"),
                }
            }
            Status::Finalizing => "Finishing…".into(),
            Status::Cancelling => "Cancelling…".into(),
            Status::Cancelled => "Cancelled".into(),
            Status::Done => "Done".into(),
            Status::Skipped => "Skipped".into(),
            Status::Failed(_) => "Failed".into(),
        }
    }
}

struct Batch {
    id: u64,
    started: Instant,
    finished: bool,
    failed: bool,
    items: Vec<Arc<Item>>,
}

#[derive(Default)]
pub(super) struct Transfers {
    batches: Vec<Batch>,
    next_id: u64,
    pub open: bool,
    #[cfg(any(debug_assertions, feature = "dev-tools"))]
    simulating: bool,
}

impl Transfers {
    const SHOW_DELAY: Duration = Duration::from_millis(300);

    #[cfg(any(debug_assertions, feature = "dev-tools"))]
    pub fn simulate(&mut self, cx: &mut Context<FileBrowser>) {
        self.open = true;
        cx.notify();
        if self.simulating {
            return;
        }
        let items = [
            ("Photos", true, 1_200_000_000),
            ("Project Archive", false, 3_600_000_000),
            ("Footage", true, 2_400_000_000),
        ]
        .map(|(name, copy, total)| {
            (
                Arc::new(Item::new(name.into(), PathBuf::new())),
                copy,
                total,
            )
        });
        let batch = self.start(items.iter().map(|(item, _, _)| item.clone()).collect());
        self.simulating = true;
        let started = Instant::now();
        let worker = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            loop {
                let elapsed = started.elapsed();
                let mut finished = true;
                for (index, (item, copy, total)) in items.iter().enumerate() {
                    let age = elapsed.checked_sub(SIMULATION_STAGGER * index as u32);
                    finished &= advance_simulation(item, *copy, *total, age);
                }
                let updated = this.update(cx, |this, cx| {
                    if finished {
                        this.transfers.simulating = false;
                        this.transfers.finish(batch, false);
                    }
                    cx.notify();
                });
                if finished || updated.is_err() {
                    break;
                }
                worker.timer(Duration::from_millis(100)).await;
            }
        })
        .detach();
    }

    pub fn start(&mut self, items: Vec<Arc<Item>>) -> u64 {
        if !self.active() {
            self.batches.retain(|batch| batch.failed);
        }
        let id = self.next_id;
        self.next_id += 1;
        self.batches.push(Batch {
            id,
            started: Instant::now(),
            finished: false,
            failed: false,
            items,
        });
        id
    }

    pub fn finish(&mut self, id: u64, failed: bool) {
        if let Some(batch) = self.batches.iter_mut().find(|batch| batch.id == id) {
            batch.finished = true;
            batch.failed = failed;
        }
        if !self.open {
            self.clear_closed();
        }
    }

    pub fn close(&mut self) {
        self.open = false;
        self.clear_closed();
    }

    fn clear_closed(&mut self) {
        if !self.active() {
            self.batches.retain(|batch| batch.failed);
        }
        for batch in self.batches.iter_mut().filter(|batch| batch.failed) {
            batch
                .items
                .retain(|item| matches!(*item.status(), Status::Failed(_)));
        }
    }

    pub fn dismiss_finished(&mut self) {
        self.batches.retain(|batch| !batch.finished);
        self.open = !self.batches.is_empty();
    }

    pub fn visible(&self) -> bool {
        self.open
            || self.batches.iter().any(|batch| {
                if batch.finished {
                    batch.failed
                } else {
                    batch.started.elapsed() >= Self::SHOW_DELAY
                }
            })
    }

    fn has_finished(&self) -> bool {
        self.batches.iter().any(|batch| batch.finished)
    }

    fn active(&self) -> bool {
        self.batches.iter().any(|batch| !batch.finished)
    }

    fn failed(&self) -> bool {
        self.batches.iter().any(|batch| batch.failed)
    }

    fn progress(&self) -> f32 {
        let mut sum = 0.;
        let mut count = 0;
        for item in self
            .batches
            .iter()
            .filter(|batch| !batch.failed)
            .flat_map(|batch| &batch.items)
        {
            let status = item.status();
            sum += item.fraction(&status);
            count += 1;
        }
        if count == 0 { 1. } else { sum / count as f32 }
    }

    fn rows(&self, range: Range<usize>) -> Vec<gpui::Div> {
        self.batches
            .iter()
            .flat_map(|batch| batch.items.iter())
            .skip(range.start)
            .take(range.len())
            .map(|item| {
                let status = item.status();
                let failure = match &*status {
                    Status::Failed(reason) => Some(reason),
                    _ => None,
                };
                let progress = matches!(*status, Status::Transferring { .. } | Status::Finalizing);
                div()
                    .w_full()
                    .h(dp(ROW_HEIGHT))
                    .flex()
                    .flex_col()
                    .justify_center()
                    .gap(dp(4.))
                    .px(dp(INSET))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap(dp(8.))
                            .line_height(dp(18.))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(rgb(0xeeeeee))
                                    .child(item.name().into_owned()),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .max_w(gpui::relative(0.66))
                                    .truncate()
                                    .text_size(dp(text_size::DETAIL))
                                    .text_color(rgb(if failure.is_some() {
                                        0xff9d96
                                    } else {
                                        0xaaaaaa
                                    }))
                                    .child(item.label(&status)),
                            )
                            .when(status.can_cancel(), |row| {
                                let item = item.clone();
                                row.child(
                                    crate::ui::close_button((
                                        "cancel-transfer",
                                        Arc::as_ptr(&item) as usize,
                                    ))
                                    .on_click(
                                        move |_, window, cx| {
                                            cx.stop_propagation();
                                            item.cancel();
                                            window.refresh();
                                        },
                                    ),
                                )
                            }),
                    )
                    .when_some(failure, |row, reason| {
                        row.child(
                            div()
                                .truncate()
                                .text_size(dp(text_size::DETAIL))
                                .line_height(dp(14.))
                                .text_color(rgb(0xaaaaaa))
                                .child(reason.clone()),
                        )
                    })
                    .when(progress, |row| {
                        row.child(
                            div()
                                .w_full()
                                .h(dp(5.))
                                .rounded_full()
                                .bg(rgb(0x434343))
                                .child(
                                    div()
                                        .h_full()
                                        .rounded_full()
                                        .bg(rgb(0x0a84ff))
                                        .w(gpui::relative(item.fraction(&status))),
                                ),
                        )
                    })
            })
            .collect()
    }

    pub fn button(&self, cx: &mut Context<FileBrowser>) -> impl IntoElement {
        div()
            .id("transfers")
            .size(dp(BUTTON_WIDTH))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .rounded(dp(radius::MD))
            .when(self.open, |button| button.bg(rgb(0x353535)))
            .hover(|button| button.bg(rgb(0x353535)))
            .cursor_pointer()
            .on_click(cx.listener(|this, _, _, cx| {
                this.transfers.open = true;
                cx.notify();
            }))
            .map(|button| {
                if self.failed() && !self.active() {
                    button.child(native_icons::symbol("exclamationmark.circle", 16.))
                } else {
                    button.child(ring(self.progress()))
                }
            })
    }

    pub fn popover(
        &self,
        window: &Window,
        top: Pixels,
        search_amount: f32,
        cx: &mut Context<FileBrowser>,
    ) -> impl IntoElement {
        let rem = window.rem_size();
        let margin = dp(TAB_ROW_END).to_pixels(rem);
        let viewport = window.viewport_size();
        let width = dp(330.)
            .to_pixels(rem)
            .min((viewport.width - margin * 2.).max(px(0.)));
        let search_width = SEARCH_CLOSED + (SEARCH_OPEN - SEARCH_CLOSED) * search_amount;
        let anchor = dp(TAB_ROW_END + search_width + 4.).to_pixels(rem);
        let inset = anchor.min((viewport.width - width - margin).max(margin));
        let count = self
            .batches
            .iter()
            .map(|batch| batch.items.len())
            .sum::<usize>();
        let list_height = dp(ROW_HEIGHT * count as f32)
            .to_pixels(rem)
            .min(dp(320.).to_pixels(rem))
            .min(
                (viewport.height - top - margin - dp(HEADER_HEIGHT + 2.).to_pixels(rem))
                    .max(px(0.)),
            );
        let browser = cx.entity().downgrade();
        let list = uniform_list("transfer-rows", count, move |range, _, cx| {
            browser
                .read_with(cx, |this, _| this.transfers.rows(range))
                .unwrap_or_default()
        })
        .h(list_height)
        .w_full();
        div()
            .id("transfer-overlay")
            .absolute()
            .inset_0()
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    cx.stop_propagation();
                    this.transfers.close();
                    cx.notify();
                }),
            )
            .child(
                div()
                    .id("transfer-popover")
                    .absolute()
                    .top(top)
                    .right(inset)
                    .w(width)
                    .flex()
                    .flex_col()
                    .rounded(dp(radius::LG))
                    .border_1()
                    .border_color(rgb(0x444444))
                    .bg(rgb(0x282828))
                    .shadow_lg()
                    .overflow_hidden()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .flex()
                            .h(dp(HEADER_HEIGHT))
                            .flex_shrink_0()
                            .items_center()
                            .justify_between()
                            .px(dp(INSET))
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child("Transfers"),
                            )
                            .when(self.has_finished(), |header| {
                                header.child(
                                    div()
                                        .id("dismiss-transfers")
                                        .flex_shrink_0()
                                        .px(dp(6.))
                                        .py(dp(3.))
                                        .rounded(dp(radius::XS))
                                        .text_size(dp(text_size::DETAIL))
                                        .text_color(rgb(0xcccccc))
                                        .cursor_pointer()
                                        .hover(|button| button.bg(rgb(0x383838)))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.transfers.dismiss_finished();
                                            cx.notify();
                                        }))
                                        .child("Dismiss"),
                                )
                            }),
                    )
                    .child(list),
            )
    }
}

#[cfg(any(debug_assertions, feature = "dev-tools"))]
fn advance_simulation(item: &Item, copy: bool, total: u64, elapsed: Option<Duration>) -> bool {
    if item.is_cancelled() {
        item.finish(Status::Cancelled);
        return true;
    }
    let Some(elapsed) = elapsed else {
        return false;
    };
    if elapsed >= SIMULATION_DURATION {
        item.set_bytes(total);
        item.finish(Status::Done);
        return true;
    }
    if !item.advance(Status::Transferring {
        copy,
        total: Some(total),
    }) {
        item.finish(Status::Cancelled);
        return true;
    }
    let progress = elapsed.as_secs_f64() / SIMULATION_DURATION.as_secs_f64();
    item.set_bytes((total as f64 * progress) as u64);
    false
}

fn ring(fraction: f32) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let center = bounds.center();
            let stroke = dp(1.75).to_pixels(window.rem_size());
            let radius = (bounds.size.width.min(bounds.size.height) - stroke) / 2.;
            let radii = point(radius, radius);
            let top = point(center.x, center.y - radius);
            let bottom = point(center.x, center.y + radius);
            let color = rgb(0x0a84ff);

            let mut track = PathBuilder::stroke(stroke);
            track.move_to(top);
            track.arc_to(radii, px(0.), false, true, bottom);
            track.arc_to(radii, px(0.), false, true, top);
            track.close();
            if let Ok(track) = track.build() {
                window.paint_path(track, if fraction >= 1. { color } else { rgb(0x555555) });
            }

            if fraction > 0. && fraction < 1. {
                let angle = fraction * std::f32::consts::TAU;
                let end = point(
                    center.x + radius * angle.sin(),
                    center.y - radius * angle.cos(),
                );
                let mut progress = PathBuilder::stroke(stroke);
                progress.move_to(top);
                progress.arc_to(radii, px(0.), fraction > 0.5, true, end);
                if let Ok(progress) = progress.build() {
                    window.paint_path(progress, color);
                }
            }
        },
    )
    .size(dp(18.))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_queued_items_and_keeps_failures_until_dismissed() {
        let first = Arc::new(Item::new("Projects".into(), "Drive".into()));
        let second = Arc::new(Item::new("Photos".into(), "Drive".into()));
        let mut transfers = Transfers::default();
        let batch = transfers.start(vec![first.clone(), second.clone()]);

        assert!(matches!(*second.status(), Status::Queued));
        first.advance(Status::Transferring {
            copy: true,
            total: Some(200),
        });
        first.set_bytes(50);
        assert_eq!(transfers.progress(), 0.125);
        transfers.batches[0].started -= Transfers::SHOW_DELAY;
        assert!(transfers.visible());

        first.finish(Status::Done);
        assert_eq!(transfers.progress(), 0.5);
        second.finish(Status::Failed("Disk is full".into()));
        transfers.finish(batch, true);
        assert!(transfers.visible());
        assert_eq!(transfers.batches[0].items.len(), 1);

        transfers.open = true;
        transfers.dismiss_finished();
        assert!(!transfers.visible());
    }

    #[cfg(any(debug_assertions, feature = "dev-tools"))]
    #[test]
    fn simulated_items_begin_four_seconds_apart_and_finish_at_twenty_seconds() {
        let last = Item::new("Footage".into(), PathBuf::new());
        let starts_at = SIMULATION_STAGGER * 2;
        let advance = |seconds| {
            advance_simulation(
                &last,
                true,
                200,
                Duration::from_secs(seconds).checked_sub(starts_at),
            )
        };
        assert!(!advance(7));
        assert!(matches!(*last.status(), Status::Queued));
        assert!(!advance(14));
        assert_eq!(last.fraction(&last.status()), 0.5);
        assert!(!advance(19));
        assert!(advance(20));
        assert!(matches!(*last.status(), Status::Done));
    }

    #[cfg(any(debug_assertions, feature = "dev-tools"))]
    #[test]
    fn cancelled_simulations_stay_cancelled_after_their_start_and_end_times() {
        let queued = Item::new("Photos".into(), PathBuf::new());
        let active = Item::new("Footage".into(), PathBuf::new());
        assert!(!advance_simulation(
            &active,
            true,
            200,
            Some(Duration::ZERO)
        ));
        queued.cancel();
        active.cancel();

        assert!(matches!(*queued.status(), Status::Cancelled));
        assert!(matches!(*active.status(), Status::Cancelling));
        assert!(advance_simulation(&queued, true, 200, None));
        assert!(advance_simulation(
            &active,
            true,
            200,
            Some(SIMULATION_DURATION),
        ));
        assert!(matches!(*active.status(), Status::Cancelled));
    }

    #[test]
    fn finalizing_and_cancellation_are_mutually_exclusive() {
        let first = Item::new("Photos".into(), PathBuf::new());
        first.advance(Status::Preparing { copy: false });
        first.cancel();
        assert!(!first.advance(Status::Finalizing));
        assert!(!first.finish(Status::Skipped));
        assert!(matches!(*first.status(), Status::Cancelled));

        let second = Item::new("Footage".into(), PathBuf::new());
        assert!(second.advance(Status::Finalizing));
        second.cancel();
        assert!(second.finish(Status::Done));
        assert!(matches!(*second.status(), Status::Done));
    }
}
