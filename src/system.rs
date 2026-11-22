//! Handing files to macOS: the clipboard in Finder's own format (so copy and
//! paste work with Finder both ways), Show in Finder, and the apps that can
//! open a file.

use std::{
    path::{Path, PathBuf},
    sync::{Mutex, PoisonError},
};

use anyhow::{Result, bail};
use block::ConcreteBlock;
use cocoa::{
    base::{BOOL, YES, id, nil},
    foundation::NSString,
};
use futures::channel::mpsc;
use objc::{class, msg_send, rc::StrongPtr, sel, sel_impl};

use crate::file_ops::{file_url, path_of};

/// Puts `paths` on the clipboard as files, as Finder's Copy does.
pub(super) fn copy_files(paths: &[PathBuf]) {
    // SAFETY: main thread; the pool releases the autoreleased URLs and array.
    unsafe {
        let _pool = StrongPtr::new(msg_send![class!(NSAutoreleasePool), new]);
        let manager: id = msg_send![class!(NSFileManager), defaultManager];
        let urls: id = msg_send![class!(NSMutableArray), array];
        for path in paths {
            if let Some(url) = file_url(manager, path) {
                let _: () = msg_send![urls, addObject: url];
            }
        }
        let pasteboard: id = msg_send![class!(NSPasteboard), generalPasteboard];
        let _: isize = msg_send![pasteboard, clearContents];
        let _: BOOL = msg_send![pasteboard, writeObjects: urls];
    }
}

/// The files on the clipboard, from this app or Finder; empty for text.
pub(super) fn pasted_files() -> Vec<PathBuf> {
    // SAFETY: main thread; the pool releases the autoreleased objects, and
    // paths are copied out before it drains.
    unsafe {
        let _pool = StrongPtr::new(msg_send![class!(NSAutoreleasePool), new]);
        let pasteboard: id = msg_send![class!(NSPasteboard), generalPasteboard];
        let classes: id = msg_send![class!(NSArray), arrayWithObject: class!(NSURL)];
        let key = NSString::alloc(nil).init_str("NSPasteboardURLReadingFileURLsOnlyKey");
        let yes: id = msg_send![class!(NSNumber), numberWithBool: YES];
        let options: id = msg_send![class!(NSDictionary), dictionaryWithObject: yes forKey: key];
        let _: () = msg_send![key, release];
        let urls: id = msg_send![pasteboard, readObjectsForClasses: classes options: options];
        if urls == nil {
            return Vec::new();
        }
        let count: usize = msg_send![urls, count];
        (0..count)
            .filter_map(|index| {
                let url: id = msg_send![urls, objectAtIndex: index];
                path_of(url)
            })
            .collect()
    }
}

/// Opens Finder windows with `paths` selected.
pub(super) fn show_in_finder(paths: &[PathBuf]) {
    // SAFETY: main thread; the pool releases the autoreleased URLs and array.
    unsafe {
        let _pool = StrongPtr::new(msg_send![class!(NSAutoreleasePool), new]);
        let manager: id = msg_send![class!(NSFileManager), defaultManager];
        let urls: id = msg_send![class!(NSMutableArray), array];
        for path in paths {
            if let Some(url) = file_url(manager, path) {
                let _: () = msg_send![urls, addObject: url];
            }
        }
        let workspace: id = msg_send![class!(NSWorkspace), sharedWorkspace];
        let _: () = msg_send![workspace, activateFileViewerSelectingURLs: urls];
    }
}

/// An app that can open a file.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct App {
    pub path: PathBuf,
    pub name: String,
    /// Opens the file on double-click.
    pub default: bool,
}

/// The apps macOS knows can open `path`, the default first, then by name.
pub(super) fn apps_for(path: &Path) -> Vec<App> {
    // SAFETY: main thread; the pool releases the autoreleased objects, and
    // every path and name is copied out before it drains.
    unsafe {
        let _pool = StrongPtr::new(msg_send![class!(NSAutoreleasePool), new]);
        let manager: id = msg_send![class!(NSFileManager), defaultManager];
        let Some(url) = file_url(manager, path) else {
            return Vec::new();
        };
        let workspace: id = msg_send![class!(NSWorkspace), sharedWorkspace];
        let default: id = msg_send![workspace, URLForApplicationToOpenURL: url];
        let default = path_of(default);
        let urls: id = msg_send![workspace, URLsForApplicationsToOpenURL: url];
        let count: usize = if urls == nil {
            0
        } else {
            msg_send![urls, count]
        };
        let mut apps: Vec<App> = (0..count)
            .filter_map(|index| {
                let app: id = msg_send![urls, objectAtIndex: index];
                let path = path_of(app)?;
                let name: id = msg_send![app, path];
                let name: id = msg_send![manager, displayNameAtPath: name];
                let name = string(name).unwrap_or_default();
                Some(App {
                    name: name.strip_suffix(".app").unwrap_or(&name).to_owned(),
                    default: Some(&path) == default.as_ref(),
                    path,
                })
            })
            .collect();
        // macOS can list the same app more than once.
        apps.sort_by(|a, b| a.path.cmp(&b.path));
        apps.dedup_by(|a, b| a.path == b.path);
        apps.sort_by(|a, b| {
            b.default
                .cmp(&a.default)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        apps
    }
}

/// Opens `paths` with `app`.
pub(super) fn open_with(app: &Path, paths: &[PathBuf]) -> Result<()> {
    let status = std::process::Command::new("/usr/bin/open")
        .arg("-a")
        .arg(app)
        .arg("--")
        .args(paths)
        .status()?;
    if !status.success() {
        bail!("{} could not open the files", app.display());
    }
    Ok(())
}

/// A mounted volume shown under Locations.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Volume {
    pub path: PathBuf,
    pub name: String,
    /// External, removable, disk image, or network: offers Eject.
    pub ejectable: bool,
}

#[link(name = "Foundation", kind = "framework")]
unsafe extern "C" {
    static NSURLVolumeLocalizedNameKey: id;
    static NSURLVolumeIsEjectableKey: id;
    static NSURLVolumeIsRemovableKey: id;
    static NSURLVolumeIsInternalKey: id;
    static NSURLVolumeIsLocalKey: id;
}

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {
    static NSWorkspaceDidMountNotification: id;
    static NSWorkspaceDidUnmountNotification: id;
    static NSWorkspaceDidRenameVolumeNotification: id;
}

/// Keeps the workspace's volume observers registered until dropped.
pub(super) struct VolumeObserver {
    center: StrongPtr,
    tokens: [StrongPtr; 3],
}

impl VolumeObserver {
    pub(super) fn new() -> (Self, mpsc::Receiver<()>) {
        let (wake, changes) = mpsc::channel(0);
        let wake = Mutex::new(wake);
        // SAFETY: Called on the main thread. The copied block may run on the
        // posting thread, so it only signals through the protected sender.
        unsafe {
            let _pool = StrongPtr::new(msg_send![class!(NSAutoreleasePool), new]);
            let workspace: id = msg_send![class!(NSWorkspace), sharedWorkspace];
            let center: id = msg_send![workspace, notificationCenter];
            let block = ConcreteBlock::new(move |_: id| {
                // A pending wake already causes the full volume list to be read.
                let _ = wake
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .try_send(());
            })
            .copy();
            let tokens = [
                NSWorkspaceDidMountNotification,
                NSWorkspaceDidUnmountNotification,
                NSWorkspaceDidRenameVolumeNotification,
            ]
            .map(|name| {
                let token: id = msg_send![center,
                    addObserverForName: name object: workspace queue: nil usingBlock: &*block
                ];
                StrongPtr::retain(token)
            });
            let center = StrongPtr::retain(center);
            (Self { center, tokens }, changes)
        }
    }
}

impl Drop for VolumeObserver {
    fn drop(&mut self) {
        // SAFETY: The center and the registration tokens are still retained.
        unsafe {
            for token in &self.tokens {
                let _: () = msg_send![*self.center, removeObserver: **token];
            }
        }
    }
}

/// The volumes Finder lists, the startup disk first. Hidden system volumes
/// (Data, Preboot, and so on) are left out.
pub(super) fn volumes() -> Vec<Volume> {
    const SKIP_HIDDEN_VOLUMES: usize = 1 << 1;
    // SAFETY: Foundation's volume APIs are thread-safe; the pool releases the
    // autoreleased objects, and values are copied out before it drains.
    unsafe {
        let _pool = StrongPtr::new(msg_send![class!(NSAutoreleasePool), new]);
        let keys: id = msg_send![class!(NSMutableArray), array];
        for key in [
            NSURLVolumeLocalizedNameKey,
            NSURLVolumeIsEjectableKey,
            NSURLVolumeIsRemovableKey,
            NSURLVolumeIsInternalKey,
            NSURLVolumeIsLocalKey,
        ] {
            let _: () = msg_send![keys, addObject: key];
        }
        let manager: id = msg_send![class!(NSFileManager), defaultManager];
        let urls: id = msg_send![manager,
            mountedVolumeURLsIncludingResourceValuesForKeys: keys
            options: SKIP_HIDDEN_VOLUMES
        ];
        if urls == nil {
            return Vec::new();
        }
        let count: usize = msg_send![urls, count];
        let mut volumes: Vec<Volume> = (0..count)
            .filter_map(|index| {
                let url: id = msg_send![urls, objectAtIndex: index];
                let path = path_of(url)?;
                if path.starts_with("/System/Volumes") {
                    return None;
                }
                let values: id = msg_send![url, resourceValuesForKeys: keys error: nil];
                let flag = |key: id| -> bool {
                    let value: id = msg_send![values, objectForKey: key];
                    value != nil && {
                        let on: BOOL = msg_send![value, boolValue];
                        on == YES
                    }
                };
                let name: id = msg_send![values, objectForKey: NSURLVolumeLocalizedNameKey];
                let name = string(name).unwrap_or_else(|| path.display().to_string());
                let network = !flag(NSURLVolumeIsLocalKey);
                let ejectable = path != Path::new("/")
                    && (flag(NSURLVolumeIsEjectableKey)
                        || flag(NSURLVolumeIsRemovableKey)
                        || !flag(NSURLVolumeIsInternalKey)
                        || network);
                Some(Volume {
                    path,
                    name,
                    ejectable,
                })
            })
            .collect();
        volumes.sort_by(|a, b| {
            (b.path == Path::new("/"))
                .cmp(&(a.path == Path::new("/")))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        volumes
    }
}

/// Unmounts and ejects a volume, as Finder's Eject does. Blocks until done;
/// call off the UI thread. Fails if files on it are in use.
pub(super) fn eject(path: &Path) -> Result<()> {
    // SAFETY: NSWorkspace's eject is thread-safe; the pool releases the URL.
    unsafe {
        let _pool = StrongPtr::new(msg_send![class!(NSAutoreleasePool), new]);
        let manager: id = msg_send![class!(NSFileManager), defaultManager];
        let Some(url) = file_url(manager, path) else {
            bail!("Could not decode {}", path.display());
        };
        let workspace: id = msg_send![class!(NSWorkspace), sharedWorkspace];
        let mut error: id = nil;
        let ejected: BOOL = msg_send![workspace, unmountAndEjectDeviceAtURL: url error: &mut error];
        if ejected != YES {
            bail!("Could not eject {}", path.display());
        }
    }
    Ok(())
}

unsafe fn string(value: id) -> Option<String> {
    if value == nil {
        return None;
    }
    // SAFETY: `value` is a live NSString; the UTF-8 bytes are copied out.
    unsafe {
        let bytes: *const std::ffi::c_char = msg_send![value, UTF8String];
        (!bytes.is_null()).then(|| {
            std::ffi::CStr::from_ptr(bytes)
                .to_string_lossy()
                .into_owned()
        })
    }
}
