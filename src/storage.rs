//! What the app keeps in Application Support besides the file index: user
//! settings, the single source of truth for preferences shown in both the
//! command menu and the settings panel, and the session restored at launch
//! (window, tabs, zoom, sidebar, and recent items). Both are JSON files read
//! at launch and rewritten atomically.

use std::{
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// Recent items kept, newest first.
pub(super) const RECENT_LIMIT: usize = 10;

/// Missing fields take their defaults, so files from older versions load and
/// new settings can be added without migrations.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct UserSettings {
    pub show_hidden_files: bool,
    pub search_scope: SearchScope,
    /// Silences the app's Trash and move sounds. Stored as "muted" so the
    /// default, sounds on, needs no special case.
    pub mute_sounds: bool,
    pub arrow_keys: ArrowKeys,
    pub view: View,
}

/// How the file list shows a folder, in every tab.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum View {
    /// Rows with name, date modified, and size.
    #[default]
    List,
    /// Large icons or thumbnails in a grid, named beneath.
    Tiles,
}

/// What ⌘← and ⌘→ do in the file list. ⌘[ ⌘] always go back and forward and
/// ⌘↑ always opens the parent, whichever is chosen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ArrowKeys {
    /// Back and forward through the folders visited, anywhere.
    History,
    /// Up to the parent, and back down the way the tab came up.
    #[default]
    FolderPath,
}

/// Where the titlebar search looks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum SearchScope {
    /// The current folder and everything below it.
    #[default]
    CurrentFolder,
    /// Every indexed folder, wherever the tab is.
    Indexed,
}

/// The window and browsing state restored at the next launch.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Session {
    pub window: Option<WindowFrame>,
    pub tabs: Vec<PathBuf>,
    pub active_tab: usize,
    /// The window rem size in points; `None` is the default zoom.
    pub zoom: Option<f32>,
    pub sidebar: SidebarState,
    /// Files and folders the user opened, newest first.
    pub recent: Vec<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct WindowFrame {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub maximized: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct SidebarState {
    pub open: bool,
    pub favorites_open: bool,
    pub recent_open: bool,
    pub locations_open: bool,
}

impl Default for SidebarState {
    fn default() -> Self {
        Self {
            open: true,
            favorites_open: true,
            recent_open: true,
            locations_open: true,
        }
    }
}

impl Session {
    /// Moves `path` to the front of the recent items, keeping `RECENT_LIMIT`.
    pub(super) fn remember(recent: &mut Vec<PathBuf>, path: PathBuf) {
        recent.retain(|item| *item != path);
        recent.insert(0, path);
        recent.truncate(RECENT_LIMIT);
    }
}

pub(super) fn settings_path(home: &Path) -> PathBuf {
    home.join("Library/Application Support/Filetree/settings.json")
}

pub(super) fn session_path(home: &Path) -> PathBuf {
    home.join("Library/Application Support/Filetree/session.json")
}

/// Reads a JSON file. A missing file means defaults. An unreadable or invalid
/// file is moved aside to `<name>.invalid.json` rather than being overwritten
/// by the next save, so a hand edit gone wrong is never lost.
pub(super) fn load<T: DeserializeOwned + Default>(path: &Path) -> T {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => return T::default(),
        Err(error) => {
            eprintln!("Could not read {}: {error}", path.display());
            return T::default();
        }
    };
    match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("Invalid data in {}: {error}", path.display());
            let aside = path.with_extension("invalid.json");
            if let Err(error) = fs::rename(path, &aside) {
                eprintln!("Could not move aside {}: {error}", path.display());
            }
            T::default()
        }
    }
}

/// Writes through a temporary file and a rename, so a crash mid-write leaves
/// the previous contents intact.
pub(super) fn save<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let folder = path.parent().context("Storage path has no folder")?;
    fs::create_dir_all(folder).with_context(|| format!("Could not create {}", folder.display()))?;
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    let staging = path.with_extension("json.tmp");
    fs::write(&staging, text).with_context(|| format!("Could not write {}", staging.display()))?;
    fs::rename(&staging, path).with_context(|| format!("Could not save {}", path.display()))
}
