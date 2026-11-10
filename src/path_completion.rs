//! Path entry for the command menu: typing `/` or `~` turns the query into a
//! path, and the folder being typed into supplies completions. Only that one
//! folder is listed, so this works anywhere on disk without the search index.

use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::native_icons::FileIcon;

const LIMIT: usize = 8;

pub(super) struct Child {
    name: String,
    lowercase: String,
    is_dir: bool,
    icon: FileIcon,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Completion {
    pub path: PathBuf,
    pub label: String,
    pub is_dir: bool,
    /// Shares the file list's per-type icons rather than drawing per path.
    pub icon: FileIcon,
}

/// The full path `query` names, with `~` expanded, or `None` if it is not a
/// path.
pub(super) fn resolve(query: &str, home: &Path) -> Option<PathBuf> {
    let query = query.trim();
    if query == "~" {
        Some(home.to_path_buf())
    } else if let Some(rest) = query.strip_prefix("~/") {
        Some(home.join(rest))
    } else if query.starts_with('/') {
        Some(PathBuf::from(query))
    } else {
        None
    }
}

/// The folder whose children complete `query`, or `None` for a command query.
pub(super) fn folder(query: &str, home: &Path) -> Option<PathBuf> {
    let (folder, _) = split(query, home)?;
    Some(folder)
}

/// Lists a folder's children, following symlinks to tell folders apart.
pub(super) fn read_children(folder: &Path) -> Vec<Child> {
    let Ok(entries) = fs::read_dir(folder) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            // Does not follow links: the link's own metadata, for its icon.
            let metadata = entry.metadata().ok();
            let is_dir = match &metadata {
                Some(meta) if meta.file_type().is_symlink() => path.is_dir(),
                Some(meta) => meta.is_dir(),
                None => false,
            };
            Child {
                icon: FileIcon::classify(&path, metadata.as_ref()),
                lowercase: name.to_lowercase(),
                name,
                is_dir,
            }
        })
        .collect()
}

/// The typed path itself when it exists, then children of its folder whose
/// names start with the typed prefix, then those containing it. Hidden items
/// appear only once the prefix starts with a dot, which matches by prefix only,
/// unless `show_hidden` is set.
pub(super) fn completions(
    query: &str,
    home: &Path,
    children: &[Child],
    show_hidden: bool,
) -> Vec<Completion> {
    let Some((folder, prefix)) = split(query, home) else {
        return Vec::new();
    };
    let needle = prefix.to_lowercase();
    let visible =
        |child: &&Child| show_hidden || needle.starts_with('.') || !child.name.starts_with('.');
    let mut starts: Vec<&Child> = children
        .iter()
        .filter(visible)
        .filter(|child| child.lowercase.starts_with(&needle))
        .collect();
    let mut contains: Vec<&Child> = children
        .iter()
        .filter(visible)
        .filter(|child| {
            !needle.is_empty()
                && !needle.starts_with('.')
                && !child.lowercase.starts_with(&needle)
                && child.lowercase.contains(&needle)
        })
        .collect();
    starts.sort_by(|a, b| a.lowercase.cmp(&b.lowercase));
    contains.sort_by(|a, b| a.lowercase.cmp(&b.lowercase));

    let mut results = Vec::new();
    if prefix.is_empty() {
        results.push(Completion {
            label: display(&folder, home),
            icon: FileIcon::classify(&folder, None),
            path: folder.clone(),
            is_dir: true,
        });
    } else if let Some(exact) = children.iter().find(|child| child.name == prefix) {
        let path = folder.join(&exact.name);
        results.push(Completion {
            label: display(&path, home),
            path,
            is_dir: exact.is_dir,
            icon: exact.icon.clone(),
        });
    }
    results.extend(
        starts
            .into_iter()
            .chain(contains)
            .filter(|child| child.name != prefix)
            .map(|child| {
                let path = folder.join(&child.name);
                Completion {
                    label: display(&path, home),
                    path,
                    is_dir: child.is_dir,
                    icon: child.icon.clone(),
                }
            }),
    );
    results.truncate(LIMIT);
    results
}

/// The query text after accepting `completion`, keeping `~` if it was typed.
/// Folders gain a trailing slash so their contents complete next.
pub(super) fn completed_text(query: &str, home: &Path, completion: &Completion) -> String {
    let mut text = if query.starts_with('~') {
        display(&completion.path, home)
    } else {
        completion.path.to_string_lossy().into_owned()
    };
    if completion.is_dir && !text.ends_with('/') {
        text.push('/');
    }
    text
}

/// Splits a path query into the folder to list and the partial name typed
/// after its last slash.
fn split(query: &str, home: &Path) -> Option<(PathBuf, String)> {
    let expanded = if query == "~" {
        format!("{}/", home.display())
    } else if let Some(rest) = query.strip_prefix("~/") {
        format!("{}/{rest}", home.display())
    } else if query.starts_with('/') {
        query.to_owned()
    } else {
        return None;
    };
    let slash = expanded.rfind('/')?;
    let folder = &expanded[..=slash];
    Some((PathBuf::from(folder), expanded[slash + 1..].to_owned()))
}

/// Recent items as menu rows, read once when the menu opens so typing never
/// touches the disk.
pub(super) fn recent(items: &[(PathBuf, FileIcon)], home: &Path) -> Vec<Completion> {
    items
        .iter()
        .map(|(path, icon)| Completion {
            label: display(path, home),
            path: path.clone(),
            is_dir: path.is_dir(),
            icon: icon.clone(),
        })
        .collect()
}

/// Recent rows whose path contains every word of `query`, case-insensitively:
/// the newest few with nothing typed, every match otherwise.
pub(super) fn matching_recent(query: &str, recent: &[Completion]) -> Vec<Completion> {
    const UNFILTERED: usize = 5;
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let limit = if words.is_empty() {
        UNFILTERED
    } else {
        recent.len()
    };
    recent
        .iter()
        .filter(|item| {
            let label = item.label.to_lowercase();
            words.iter().all(|word| label.contains(word))
        })
        .take(limit)
        .cloned()
        .collect()
}

/// `path` as the menus show it: `~` inside the home folder.
pub(super) fn display(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}
