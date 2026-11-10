//! Undo and redo for file changes, app-wide as in Finder. Each change is a
//! list of steps that one ⌘Z reverts together; the last `LIMIT` changes are
//! kept for the session. Reverting never replaces anything: if a file has
//! since taken a name a step needs, that step fails and the rest still run.

use std::{collections::VecDeque, path::PathBuf};

use crate::file_ops::{self, Placed, Trashed};

const LIMIT: usize = 100;

/// One reversible effect on disk.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Step {
    /// An item moved or renamed from `from` to `to`.
    Moved { from: PathBuf, to: PathBuf },
    /// An item moved to the Trash.
    Trashed(Trashed),
    /// An item that appeared: a copy, a new folder, or one put back from the
    /// Trash. Reverting it moves it to the Trash, as Finder does.
    Placed(PathBuf),
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Change(Vec<Step>);

impl Change {
    /// `None` when nothing happened, so empty changes are never recorded.
    pub(super) fn new(steps: Vec<Step>) -> Option<Self> {
        (!steps.is_empty()).then_some(Self(steps))
    }

    pub(super) fn trashed(items: Vec<Trashed>) -> Option<Self> {
        Self::new(items.into_iter().map(Step::Trashed).collect())
    }

    pub(super) fn transferred(placed: Vec<Placed>) -> Option<Self> {
        Self::new(
            placed
                .into_iter()
                .map(|placed| match placed.copied {
                    true => Step::Placed(placed.to),
                    false => Step::Moved {
                        from: placed.from,
                        to: placed.to,
                    },
                })
                .collect(),
        )
    }

    pub(super) fn steps(&self) -> &[Step] {
        &self.0
    }

    /// The folders whose listings this change affects.
    pub(super) fn folders(&self) -> Vec<PathBuf> {
        let mut folders = Vec::new();
        let mut add = |path: &PathBuf| {
            if let Some(parent) = path.parent()
                && !folders.iter().any(|folder| folder == parent)
            {
                folders.push(parent.to_path_buf());
            }
        };
        for step in &self.0 {
            match step {
                Step::Moved { from, to } => {
                    add(from);
                    add(to);
                }
                Step::Trashed(item) => add(&item.original),
                Step::Placed(path) => add(path),
            }
        }
        folders
    }

    /// Undoes this change on disk, newest step first; call off the UI thread.
    /// Returns the change that redoes whatever was reverted, and the first
    /// failure, if any.
    pub(super) fn revert(self) -> (Option<Change>, Option<anyhow::Error>) {
        let mut inverse = Vec::new();
        let mut failure = None;
        for step in self.0.into_iter().rev() {
            let reverted = match step {
                Step::Moved { from, to } => {
                    file_ops::move_item(&to, &from).map(|()| Step::Moved { from: to, to: from })
                }
                Step::Trashed(item) => {
                    file_ops::put_back(&item).map(|()| Step::Placed(item.original))
                }
                Step::Placed(path) => match file_ops::trash(&[path]) {
                    (mut trashed, None) if trashed.len() == 1 => {
                        Ok(Step::Trashed(trashed.remove(0)))
                    }
                    (_, error) => Err(error.unwrap_or_else(|| anyhow::anyhow!("Trash failed"))),
                },
            };
            match reverted {
                Ok(step) => inverse.push(step),
                Err(error) => {
                    failure.get_or_insert(error);
                }
            }
        }
        // Reverting ran newest first; redoing runs in the original order.
        inverse.reverse();
        (Change::new(inverse), failure)
    }
}

#[derive(Default)]
pub(super) struct UndoHistory {
    undo: VecDeque<Change>,
    redo: Vec<Change>,
}

impl UndoHistory {
    /// Records a change the user just made; a new change clears redo.
    pub(super) fn record(&mut self, change: Option<Change>) {
        let Some(change) = change else {
            return;
        };
        if self.undo.len() == LIMIT {
            self.undo.pop_front();
        }
        self.undo.push_back(change);
        self.redo.clear();
    }

    pub(super) fn take_undo(&mut self) -> Option<Change> {
        self.undo.pop_back()
    }

    pub(super) fn take_redo(&mut self) -> Option<Change> {
        self.redo.pop()
    }

    /// Stores what redoes an undo that just ran.
    pub(super) fn undone(&mut self, inverse: Option<Change>) {
        self.redo.extend(inverse);
    }

    /// Stores what undoes a redo that just ran, keeping the rest of redo.
    pub(super) fn redone(&mut self, inverse: Option<Change>) {
        if let Some(change) = inverse {
            if self.undo.len() == LIMIT {
                self.undo.pop_front();
            }
            self.undo.push_back(change);
        }
    }
}
