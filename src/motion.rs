use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

use cocoa::base::{BOOL, YES, id};
use objc::{class, msg_send, sel, sel_impl};

pub(super) const DURATION: Duration = Duration::from_millis(220);
const MAX_FRAME_STEP: Duration = Duration::from_millis(33);

pub(super) fn reduced() -> bool {
    // SAFETY: Called during rendering on the AppKit thread. NSWorkspace owns
    // the shared instance, and the preference read returns a BOOL.
    unsafe {
        let workspace: id = msg_send![class!(NSWorkspace), sharedWorkspace];
        let reduced: BOOL = msg_send![workspace, accessibilityDisplayShouldReduceMotion];
        reduced == YES
    }
}

/// A reveal that can change direction without jumping back to an endpoint.
pub(super) struct Presence {
    shown: bool,
    value: f32,
    from: f32,
    elapsed: Duration,
    last_frame: Option<Instant>,
}

impl Presence {
    pub fn new(shown: bool) -> Self {
        Self {
            shown,
            value: if shown { 1. } else { 0. },
            from: 0.,
            elapsed: DURATION,
            last_frame: None,
        }
    }

    pub fn set(&mut self, shown: bool) {
        if self.shown != shown {
            self.shown = shown;
            self.from = self.value;
            self.elapsed = Duration::ZERO;
            self.last_frame = None;
        }
    }

    pub fn value(&self) -> f32 {
        self.value
    }

    /// Advances only while painted, limiting the jump after a slow frame.
    pub fn advance(&mut self, now: Instant, reduced: bool) -> bool {
        let target = if self.shown { 1. } else { 0. };
        if reduced || self.from == target {
            self.elapsed = DURATION;
        }
        if self.elapsed >= DURATION {
            self.value = target;
            return false;
        }
        if let Some(last) = self.last_frame.replace(now) {
            self.elapsed += now.saturating_duration_since(last).min(MAX_FRAME_STEP);
        }
        if self.elapsed >= DURATION {
            self.value = target;
            return false;
        }
        self.value = self.from
            + (target - self.from) * ease(self.elapsed.as_secs_f32() / DURATION.as_secs_f32());
        true
    }
}

/// Cubic Bézier (0.3, 0, 0, 1). Solve the input coordinate, then evaluate the output.
fn ease(x: f32) -> f32 {
    let mut t = x;
    for _ in 0..8 {
        let u = 1. - t;
        let curve_x = 0.9 * u * u * t + t * t * t;
        let slope = 0.9 * u * u - 1.8 * u * t + 3. * t * t;
        t = (t - (curve_x - x) / slope).clamp(0., 1.);
    }
    t * t * (3. - 2. * t)
}

pub(super) struct TabLabel {
    pub id: u64,
    pub title: String,
    pub active: bool,
}

struct TabSlot {
    label: TabLabel,
    presence: Presence,
    live: bool,
}

/// Keeps a snapshot of a closed tab's chrome while its slot collapses.
#[derive(Default)]
pub(super) struct TabStrip {
    slots: Vec<TabSlot>,
}

impl TabStrip {
    pub fn rekey(&mut self, from: u64, to: u64) {
        if from == to
            || !self
                .slots
                .iter()
                .any(|slot| slot.label.id == from && slot.live)
        {
            return;
        }
        // A pane can become its group's survivor while its old standalone
        // tab is still fading out. The group keeps the slot in that case.
        self.slots.retain(|slot| slot.label.id != to);
        if let Some(slot) = self
            .slots
            .iter_mut()
            .find(|slot| slot.label.id == from && slot.live)
        {
            slot.label.id = to;
        }
    }

    pub fn update(&mut self, labels: Vec<TabLabel>, now: Instant, reduced: bool) -> bool {
        if self
            .slots
            .iter()
            .filter(|slot| slot.live)
            .map(|slot| slot.label.id)
            .eq(labels.iter().map(|label| label.id))
        {
            for (slot, label) in self.slots.iter_mut().filter(|slot| slot.live).zip(labels) {
                slot.label = label;
            }
        } else {
            let initial = self.slots.is_empty();
            let mut previous: Vec<_> = std::mem::take(&mut self.slots)
                .into_iter()
                .map(Some)
                .collect();
            let current: HashSet<_> = labels.iter().map(|label| label.id).collect();
            for slot in previous.iter_mut().flatten() {
                slot.live = current.contains(&slot.label.id);
                slot.presence.set(slot.live);
            }
            for label in labels {
                let old = previous
                    .iter()
                    .position(|slot| slot.as_ref().is_some_and(|slot| slot.label.id == label.id));
                if let Some(old) = old {
                    for slot in previous[..old]
                        .iter_mut()
                        .filter(|slot| slot.as_ref().is_some_and(|slot| !slot.live))
                    {
                        self.slots.extend(slot.take());
                    }
                    if let Some(mut slot) = previous[old].take() {
                        slot.label = label;
                        self.slots.push(slot);
                    }
                } else {
                    let mut presence = Presence::new(initial);
                    presence.set(true);
                    self.slots.push(TabSlot {
                        label,
                        presence,
                        live: true,
                    });
                }
            }
            self.slots.extend(previous.into_iter().flatten());
        }
        let mut moving = false;
        self.slots.retain_mut(|slot| {
            moving |= slot.presence.advance(now, reduced);
            slot.live || slot.presence.value() > 0.
        });
        moving
    }

    /// Pairs each visual slot with its live position, if it still has one.
    pub fn slots(&self) -> impl Iterator<Item = (Option<usize>, &TabLabel, f32)> {
        let mut position = 0;
        self.slots.iter().map(move |slot| {
            let live = slot.live.then(|| {
                let index = position;
                position += 1;
                index
            });
            (live, &slot.label, slot.presence.value())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tabs(ids: &[u64]) -> Vec<TabLabel> {
        ids.iter()
            .map(|&id| TabLabel {
                id,
                title: id.to_string(),
                active: Some(&id) == ids.last(),
            })
            .collect()
    }

    #[test]
    fn tab_closes_in_its_old_slot_while_other_tabs_remain_live() {
        let now = Instant::now();
        let mut strip = TabStrip::default();
        assert!(!strip.update(tabs(&[1, 2, 3]), now, false));
        assert!(strip.update(tabs(&[1, 3, 4]), now, false));
        let layout: Vec<_> = strip
            .slots()
            .map(|(live, label, amount)| (live, label.id, amount))
            .collect();
        assert_eq!(
            layout,
            vec![
                (Some(0), 1, 1.),
                (None, 2, 1.),
                (Some(1), 3, 1.),
                (Some(2), 4, 0.)
            ]
        );
        for frame in 1..=8 {
            strip.update(tabs(&[1, 3, 4]), now + MAX_FRAME_STEP * frame, false);
        }
        assert_eq!(
            strip
                .slots()
                .map(|(_, label, value)| (label.id, value))
                .collect::<Vec<_>>(),
            vec![(1, 1.), (3, 1.), (4, 1.)]
        );
    }

    #[test]
    fn reversing_and_enabling_reduced_motion_preserve_the_requested_state() {
        let now = Instant::now();
        let mut presence = Presence::new(false);
        presence.set(true);
        assert!(presence.advance(now, false));
        assert!(presence.advance(now + MAX_FRAME_STEP, false));
        let intermediate = presence.value();
        assert!(intermediate > 0. && intermediate < 1.);
        presence.set(false);
        assert!(presence.advance(now + MAX_FRAME_STEP * 2, false));
        assert_eq!(presence.value(), intermediate);
        assert!(!presence.advance(now + MAX_FRAME_STEP * 3, true));
        assert_eq!(presence.value(), 0.);
    }

    #[test]
    fn a_group_survivor_uses_one_slot_even_while_its_old_tab_is_closing() {
        let now = Instant::now();
        let mut strip = TabStrip::default();
        strip.update(tabs(&[1, 2, 3]), now, false);
        strip.rekey(1, 4);
        strip.update(tabs(&[4, 3]), now, false);
        strip.rekey(4, 2);
        strip.update(tabs(&[2, 3, 5]), now, false);
        let ids: Vec<_> = strip.slots().map(|(_, label, _)| label.id).collect();
        assert_eq!(ids, [2, 3, 5]);
    }
}
