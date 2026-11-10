//! Changes to files on disk: Move to Trash, Rename, New Folder, and moving or
//! copying items into a folder. Each validates its input and leaves the disk
//! unchanged when it fails. Operations on several items report what they did
//! before any failure, so every change can be undone.

use std::{
    fs,
    io::ErrorKind,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, bail};
use cocoa::base::{id, nil};
use objc::{class, msg_send, rc::StrongPtr, sel, sel_impl};

/// An item moved to the Trash, and where it went there.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Trashed {
    pub original: PathBuf,
    pub in_trash: PathBuf,
}

/// Moves each path to the Trash through macOS, so Finder's Put Back works.
/// Stops at the first failure, returning the items trashed before it.
pub(super) fn trash(paths: &[PathBuf]) -> (Vec<Trashed>, Option<anyhow::Error>) {
    let mut trashed = Vec::new();
    for path in paths {
        match trash_one(path) {
            Ok(in_trash) => trashed.push(Trashed {
                original: path.clone(),
                in_trash,
            }),
            Err(error) => return (trashed, Some(error)),
        }
    }
    (trashed, None)
}

fn trash_one(path: &Path) -> Result<PathBuf> {
    // SAFETY: Foundation file APIs are thread-safe. The pool releases the
    // autoreleased URLs and error before returning; only a copied path escapes.
    unsafe {
        let _pool = StrongPtr::new(msg_send![class!(NSAutoreleasePool), new]);
        let manager: id = msg_send![class!(NSFileManager), defaultManager];
        let url = file_url(manager, path)
            .with_context(|| format!("Could not decode {}", path.display()))?;
        let mut resulting: id = nil;
        let mut error: id = nil;
        let done: bool = msg_send![manager,
            trashItemAtURL: url
            resultingItemURL: &mut resulting
            error: &mut error
        ];
        if !done {
            bail!("Could not move {} to the Trash", path.display());
        }
        path_of(resulting)
            .with_context(|| format!("macOS did not report where {} went", path.display()))
    }
}

/// Moves a trashed item back where it came from. Refuses if something else
/// now has its name, rather than replacing it.
pub(super) fn put_back(item: &Trashed) -> Result<()> {
    move_item(&item.in_trash, &item.original)
}

/// Moves one item to exactly `to`, never replacing anything already there.
pub(super) fn move_item(from: &Path, to: &Path) -> Result<()> {
    if fs::symlink_metadata(to).is_ok() {
        bail!("{} already exists", to.display());
    }
    file_manager(Op::Move, from, to)
}

/// Renames `path` within its folder and returns the new path. Refuses empty
/// names, `/`, `.` and `..`, and names already taken by another item; a
/// change of letter case alone is allowed.
pub(super) fn rename(path: &Path, name: &str) -> Result<PathBuf> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        bail!("“{name}” is not a valid name");
    }
    let folder = path.parent().context("Cannot rename the top of the disk")?;
    let target = folder.join(name);
    if target == path {
        return Ok(target);
    }
    if let Ok(existing) = fs::symlink_metadata(&target) {
        let current = fs::symlink_metadata(path)?;
        let same = existing.dev() == current.dev() && existing.ino() == current.ino();
        if !same {
            bail!("“{name}” already exists");
        }
    }
    fs::rename(path, &target).with_context(|| format!("Could not rename {}", path.display()))?;
    Ok(target)
}

/// Creates “untitled folder” in `folder`, numbering it if that name is taken,
/// and returns its path.
pub(super) fn new_folder(folder: &Path) -> Result<PathBuf> {
    for number in 1.. {
        let name = match number {
            1 => "untitled folder".to_owned(),
            number => format!("untitled folder {number}"),
        };
        let path = folder.join(name);
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Could not create a folder in {}", folder.display()));
            }
        }
    }
    unreachable!("folder numbers do not run out")
}

/// How `transfer` decides between moving and copying.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Transfer {
    /// As a Finder drop: move within a volume, copy across volumes.
    Drop,
    /// Always copy, as Option-drop, Paste, and Duplicate do.
    Copy,
    /// Always move, as Option-Command-V does.
    Move,
}

/// One item `transfer` put in place.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Placed {
    pub from: PathBuf,
    pub to: PathBuf,
    pub copied: bool,
}

/// Moves or copies `paths` into `folder` and reports where they landed. A
/// taken name gets a number ("notes 2.txt"); copying into the item's own
/// folder makes "notes copy.txt". Moving an item to where it already is, or a
/// folder onto itself, does nothing; a folder cannot go inside itself. Stops at
/// the first failure, returning what was placed before it.
pub(super) fn transfer(
    paths: &[PathBuf],
    folder: &Path,
    mode: Transfer,
) -> (Vec<Placed>, Option<anyhow::Error>) {
    let mut placed = Vec::new();
    let result = (|| -> Result<()> {
        let folder_device = fs::metadata(folder)
            .with_context(|| format!("Could not open {}", folder.display()))?
            .dev();
        for path in paths {
            // Dropping a folder on itself, a common slip, does nothing.
            if folder == path {
                continue;
            }
            if folder.starts_with(path) {
                bail!("Cannot put {} inside itself", path.display());
            }
            let Some(name) = path.file_name() else {
                continue;
            };
            let device = fs::symlink_metadata(path)
                .with_context(|| format!("Could not find {}", path.display()))?
                .dev();
            let copy = match mode {
                Transfer::Drop => device != folder_device,
                Transfer::Copy => true,
                Transfer::Move => false,
            };
            let in_place = path.parent() == Some(folder);
            if in_place && !copy {
                continue;
            }
            let target = free_name(folder, name, in_place);
            file_manager(if copy { Op::Copy } else { Op::Move }, path, &target)?;
            placed.push(Placed {
                from: path.clone(),
                to: target,
                copied: copy,
            });
        }
        Ok(())
    })();
    (placed, result.err())
}

/// `name` in `folder`, numbered until unused; `duplicate` adds " copy" first.
fn free_name(folder: &Path, name: &std::ffi::OsStr, duplicate: bool) -> PathBuf {
    let name = Path::new(name);
    let stem = name
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let extension = name
        .extension()
        .map(|extension| format!(".{}", extension.to_string_lossy()))
        .unwrap_or_default();
    let base = if duplicate {
        format!("{stem} copy")
    } else {
        stem
    };
    let first = if duplicate {
        folder.join(format!("{base}{extension}"))
    } else {
        folder.join(name)
    };
    if fs::symlink_metadata(&first).is_err() {
        return first;
    }
    (2..)
        .map(|number| folder.join(format!("{base} {number}{extension}")))
        .find(|candidate| fs::symlink_metadata(candidate).is_err())
        .expect("numbers do not run out")
}

enum Op {
    Move,
    Copy,
}

/// An autoreleased file URL for `path`, decoded by Foundation so names that
/// are not UTF-8 work. Call inside an autorelease pool.
pub(super) unsafe fn file_url(manager: id, path: &Path) -> Option<id> {
    let bytes = path.as_os_str().as_bytes();
    // SAFETY: the caller holds an autorelease pool; both objects are autoreleased.
    unsafe {
        let string: id = msg_send![manager,
            stringWithFileSystemRepresentation: bytes.as_ptr().cast::<std::ffi::c_char>()
            length: bytes.len()
        ];
        (string != nil).then(|| msg_send![class!(NSURL), fileURLWithPath: string])
    }
}

/// The filesystem path of a file URL, copied out of Foundation.
pub(super) unsafe fn path_of(url: id) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    if url == nil {
        return None;
    }
    // SAFETY: `url` is a live NSURL; the C string is copied before returning.
    unsafe {
        let bytes: *const std::ffi::c_char = msg_send![url, fileSystemRepresentation];
        if bytes.is_null() {
            return None;
        }
        let bytes = std::ffi::CStr::from_ptr(bytes).to_bytes().to_vec();
        Some(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
    }
}

/// Moves or copies one item with NSFileManager, which keeps metadata and
/// extended attributes, copies folders whole, and moves across volumes.
fn file_manager(op: Op, from: &Path, to: &Path) -> Result<()> {
    // SAFETY: Foundation file APIs are thread-safe. The pool releases the
    // autoreleased URLs and error before returning.
    unsafe {
        let _pool = StrongPtr::new(msg_send![class!(NSAutoreleasePool), new]);
        let manager: id = msg_send![class!(NSFileManager), defaultManager];
        let (Some(source), Some(target)) = (file_url(manager, from), file_url(manager, to)) else {
            bail!("Could not decode {}", from.display());
        };
        let mut error: id = nil;
        let done: bool = match op {
            Op::Move => msg_send![manager, moveItemAtURL: source toURL: target error: &mut error],
            Op::Copy => msg_send![manager, copyItemAtURL: source toURL: target error: &mut error],
        };
        if !done {
            bail!(
                "Could not {} {}",
                match op {
                    Op::Move => "move",
                    Op::Copy => "copy",
                },
                from.display()
            );
        }
    }
    Ok(())
}

/// The text Copy Path puts on the clipboard: one path per line.
pub(super) fn path_text(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.to_string_lossy())
        .collect::<Vec<_>>()
        .join("\n")
}
