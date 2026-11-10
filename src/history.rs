//! Per-tab navigation memory: back and forward history, and the folder path
//! the tab has walked up from. Each history direction keeps at most `LIMIT`
//! locations; past that, the oldest are dropped.

use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
};

const LIMIT: usize = 5_000;

#[derive(Default)]
pub(super) struct History {
    back: VecDeque<PathBuf>,
    forward: VecDeque<PathBuf>,
    /// The deepest folder on the tab's current path: going up keeps it, so
    /// the way back down is remembered; going anywhere off it replaces it.
    trail: Option<PathBuf>,
}

impl History {
    pub(super) fn can_go_back(&self) -> bool {
        !self.back.is_empty()
    }

    pub(super) fn can_go_forward(&self) -> bool {
        !self.forward.is_empty()
    }

    /// Moves `current` to `location`, recording where it was and clearing the
    /// forward history. Returns false when already there.
    pub(super) fn visit(&mut self, current: &mut PathBuf, location: PathBuf) -> bool {
        if *current == location {
            return false;
        }
        self.follow(current);
        push(&mut self.back, std::mem::replace(current, location));
        self.forward.clear();
        self.follow(current);
        true
    }

    pub(super) fn go_back(&mut self, current: &mut PathBuf) -> bool {
        self.follow(current);
        let moved = step(&mut self.back, &mut self.forward, current);
        self.follow(current);
        moved
    }

    pub(super) fn go_forward(&mut self, current: &mut PathBuf) -> bool {
        self.follow(current);
        let moved = step(&mut self.forward, &mut self.back, current);
        self.follow(current);
        moved
    }

    /// The folders below `current` on the remembered path, nearest first:
    /// where walking back down leads.
    pub(super) fn deeper(&self, current: &Path) -> Vec<PathBuf> {
        let Some(rest) = self
            .trail
            .as_deref()
            .and_then(|trail| trail.strip_prefix(current).ok())
        else {
            return Vec::new();
        };
        let mut path = current.to_path_buf();
        rest.components()
            .map(|component| {
                path.push(component);
                path.clone()
            })
            .collect()
    }

    /// Points remembered locations at a folder's new name.
    pub(super) fn rename(&mut self, from: &Path, to: &Path) {
        let moved = |path: &mut PathBuf| {
            if let Ok(rest) = path.strip_prefix(from) {
                *path = to.join(rest);
            }
        };
        self.back.iter_mut().for_each(moved);
        self.forward.iter_mut().for_each(moved);
        self.trail.iter_mut().for_each(moved);
    }

    /// Keeps the trail if `current` is on it, else starts a new one there.
    /// Called before and after every move, so the folder a tab started in
    /// counts too.
    fn follow(&mut self, current: &Path) {
        if !self
            .trail
            .as_deref()
            .is_some_and(|trail| trail.starts_with(current))
        {
            self.trail = Some(current.to_path_buf());
        }
    }
}

fn step(from: &mut VecDeque<PathBuf>, to: &mut VecDeque<PathBuf>, current: &mut PathBuf) -> bool {
    let Some(location) = from.pop_back() else {
        return false;
    };
    push(to, std::mem::replace(current, location));
    true
}

fn push(stack: &mut VecDeque<PathBuf>, location: PathBuf) {
    if stack.len() == LIMIT {
        stack.pop_front();
    }
    stack.push_back(location);
}
