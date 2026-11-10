//! Recently listed folders, shared by every tab. A cached folder shows
//! instantly and is then re-read in the background, so it is never trusted
//! beyond the next visit. Memory stays bounded by folder and row counts.

use std::{
    path::{Path, PathBuf},
    rc::Rc,
};

const FOLDERS: usize = 64;
const ROWS: usize = 100_000;

pub(super) struct FolderCache<T> {
    /// Least recently used first.
    folders: Vec<(PathBuf, Rc<[T]>)>,
    rows: usize,
}

impl<T> FolderCache<T> {
    pub(super) fn new() -> Self {
        Self {
            folders: Vec::new(),
            rows: 0,
        }
    }

    pub(super) fn get(&mut self, folder: &Path) -> Option<Rc<[T]>> {
        let position = self.folders.iter().position(|(path, _)| path == folder)?;
        let entry = self.folders.remove(position);
        let rows = entry.1.clone();
        self.folders.push(entry);
        Some(rows)
    }

    /// Forgets a folder whose contents just changed.
    pub(super) fn remove(&mut self, folder: &Path) {
        if let Some(position) = self.folders.iter().position(|(path, _)| path == folder) {
            let (_, old) = self.folders.remove(position);
            self.rows -= old.len();
        }
    }

    /// Stores a fresh listing, evicting the least recently used folders to stay
    /// within limits. A listing larger than the whole budget is not kept.
    pub(super) fn insert(&mut self, folder: PathBuf, rows: Rc<[T]>) {
        if let Some(position) = self.folders.iter().position(|(path, _)| *path == folder) {
            let (_, old) = self.folders.remove(position);
            self.rows -= old.len();
        }
        if rows.len() > ROWS {
            return;
        }
        while self.folders.len() == FOLDERS || self.rows + rows.len() > ROWS {
            let (_, evicted) = self.folders.remove(0);
            self.rows -= evicted.len();
        }
        self.rows += rows.len();
        self.folders.push((folder, rows));
    }
}
