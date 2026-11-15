//! How a tab's panes are arranged: one pane, or a row or column split in two,
//! each part arranged the same way. Panes are named by their tab's id; the
//! browser owns the tabs themselves.

use serde::{Deserialize, Serialize};

/// The most panes one tab holds: a 2 × 2 grid.
pub(super) const LIMIT: usize = 4;
/// The smallest share either side of a divider keeps.
pub(super) const MIN_SHARE: f32 = 0.2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Axis {
    /// Side by side.
    Row,
    /// One above the other.
    Column,
}

/// Where a pane goes beside another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

impl Side {
    fn axis(self) -> Axis {
        match self {
            Self::Left | Self::Right => Axis::Row,
            Self::Top | Self::Bottom => Axis::Column,
        }
    }

    /// Whether the new pane comes first along the axis.
    fn before(self) -> bool {
        matches!(self, Self::Left | Self::Top)
    }

    /// The side of a pane nearest `(x, y)`, each from 0 to 1 across it.
    pub(super) fn nearest(x: f32, y: f32) -> Self {
        if x.min(1. - x) < y.min(1. - y) {
            if x < 0.5 { Self::Left } else { Self::Right }
        } else if y < 0.5 {
            Self::Top
        } else {
            Self::Bottom
        }
    }
}

/// The way to one split from the top: at each split, `false` for its first
/// part and `true` for its second.
pub(super) type Path = Vec<bool>;

/// Saved with the session as `Layout<usize>`, panes by position in the saved
/// tab list.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Layout<P> {
    Pane(P),
    Split {
        axis: Axis,
        /// The first part's share of the length along the axis.
        share: f32,
        first: Box<Layout<P>>,
        second: Box<Layout<P>>,
    },
}

impl<P: Copy + PartialEq> Layout<P> {
    /// The panes, in reading order: left to right, top to bottom.
    pub(super) fn panes(&self) -> Vec<P> {
        let mut panes = Vec::new();
        self.collect(&mut panes);
        panes
    }

    fn collect(&self, panes: &mut Vec<P>) {
        match self {
            Self::Pane(pane) => panes.push(*pane),
            Self::Split { first, second, .. } => {
                first.collect(panes);
                second.collect(panes);
            }
        }
    }

    pub(super) fn contains(&self, pane: P) -> bool {
        match self {
            Self::Pane(own) => *own == pane,
            Self::Split { first, second, .. } => first.contains(pane) || second.contains(pane),
        }
    }

    /// Puts `new` on `side` of `target`, halving the room `target` had.
    /// Returns false if `target` is not here or the tab is full.
    pub(super) fn insert(&mut self, target: P, new: P, side: Side) -> bool {
        if self.panes().len() >= LIMIT || self.contains(new) {
            return false;
        }
        self.insert_at(target, new, side)
    }

    fn insert_at(&mut self, target: P, new: P, side: Side) -> bool {
        match self {
            Self::Pane(own) if *own == target => {
                let (first, second) = if side.before() {
                    (new, target)
                } else {
                    (target, new)
                };
                *self = Self::Split {
                    axis: side.axis(),
                    share: 0.5,
                    first: Box::new(Self::Pane(first)),
                    second: Box::new(Self::Pane(second)),
                };
                true
            }
            Self::Pane(_) => false,
            Self::Split { first, second, .. } => {
                first.insert_at(target, new, side) || second.insert_at(target, new, side)
            }
        }
    }

    /// Takes `pane` out, giving its room to its neighbor. Returns false if
    /// it is not here or is the only pane.
    pub(super) fn remove(&mut self, pane: P) -> bool {
        match self {
            Self::Pane(_) => false,
            Self::Split { first, second, .. } => {
                let kept = match (&**first, &**second) {
                    (Self::Pane(own), _) if *own == pane => Some(second.as_ref().clone()),
                    (_, Self::Pane(own)) if *own == pane => Some(first.as_ref().clone()),
                    _ => None,
                };
                match kept {
                    Some(kept) => {
                        *self = kept;
                        true
                    }
                    None => first.remove(pane) || second.remove(pane),
                }
            }
        }
    }

    /// Sets the share of the split at `path`, kept within `MIN_SHARE` of
    /// either end.
    pub(super) fn resize(&mut self, path: &[bool], share: f32) {
        match (self, path.split_first()) {
            (Self::Split { share: own, .. }, None) => {
                *own = share.clamp(MIN_SHARE, 1. - MIN_SHARE);
            }
            (Self::Split { first, second, .. }, Some((&in_second, rest))) => {
                if in_second {
                    second.resize(rest, share);
                } else {
                    first.resize(rest, share);
                }
            }
            (Self::Pane(_), _) => {}
        }
    }

    /// The pane at the top right, where the view controls go.
    pub(super) fn top_right(&self) -> P {
        match self {
            Self::Pane(pane) => *pane,
            Self::Split {
                axis: Axis::Row,
                second,
                ..
            } => second.top_right(),
            Self::Split { first, .. } => first.top_right(),
        }
    }

    /// The same arrangement with each pane mapped, or `None` if any maps to
    /// `None`: between tab ids and saved tab positions. Shares are kept
    /// within bounds, so a hand-edited session cannot collapse a pane.
    pub(super) fn map<Q>(&self, f: &impl Fn(P) -> Option<Q>) -> Option<Layout<Q>> {
        Some(match self {
            Self::Pane(pane) => Layout::Pane(f(*pane)?),
            Self::Split {
                axis,
                share,
                first,
                second,
            } => Layout::Split {
                axis: *axis,
                share: share.clamp(MIN_SHARE, 1. - MIN_SHARE),
                first: Box::new(first.map(f)?),
                second: Box::new(second.map(f)?),
            },
        })
    }
}
