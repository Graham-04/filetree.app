//! Changes to files on disk: Move to Trash, Rename, New Folder, and moving or
//! copying items into a folder. Each validates its input and leaves the disk
//! unchanged when it fails. Operations on several items report what they did
//! before any failure, so every change can be undone.

use std::{
    collections::HashSet,
    ffi::{CStr, CString, OsStr},
    fs,
    io::{self, ErrorKind},
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context as _, Result, bail};
use cocoa::base::{BOOL, YES, id, nil};
use objc::{class, msg_send, rc::StrongPtr, sel, sel_impl};

use crate::transfers::{Item, Status};

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

/// Transfers one selected item to a free name, reporting progress for its contents.
pub(super) fn transfer(item: &Item, mode: Transfer) -> Result<Option<Placed>> {
    if item.is_cancelled() {
        return Ok(None);
    }
    let path = item.source.as_path();
    let folder = item.folder.as_path();
    let folder_device = fs::metadata(folder)
        .with_context(|| format!("Could not open {}", folder.display()))?
        .dev();
    // Dropping a folder on itself, a common slip, does nothing.
    if folder == path {
        return Ok(None);
    }
    let Some(name) = path.file_name() else {
        return Ok(None);
    };
    let source =
        fs::symlink_metadata(path).with_context(|| format!("Could not find {}", path.display()))?;
    let same_volume = source.dev() == folder_device;
    let copy = match mode {
        Transfer::Drop => !same_volume,
        Transfer::Copy => true,
        Transfer::Move => false,
    };
    let in_place = path.parent() == Some(folder);
    if in_place && !copy {
        return Ok(None);
    }
    if folder.starts_with(path)
        || (source.is_dir() && fs::canonicalize(folder)?.starts_with(fs::canonicalize(path)?))
    {
        bail!("Cannot put {} inside itself", path.display());
    }
    let target = free_name(folder, name, in_place);
    if !item.advance(Status::Preparing { copy }) {
        return Ok(None);
    }
    if !copy && same_volume {
        if !item.advance(Status::Finalizing) {
            return Ok(None);
        }
        move_item(path, &target)?;
    } else {
        let size = content_size(path, &source, !copy, || item.is_cancelled());
        let total = if copy {
            size.ok()
        } else {
            Some(size.with_context(|| format!("Could not check {}", path.display()))?)
        };
        if !item.advance(Status::Transferring { copy, total }) {
            return Ok(None);
        }
        if source.is_dir() || source.is_file() {
            if !copy_with_progress(item, &target, same_volume)? {
                return Ok(None);
            }
        } else {
            if !item.advance(Status::Finalizing) {
                return Ok(None);
            }
            file_manager(Op::Copy, path, &target)?;
        }
        if !copy {
            // Keep the complete destination if the source cannot be removed.
            // Check the original before touching it in case it was replaced
            // or edited during the copy.
            let remove = || -> Result<()> {
                let current = fs::symlink_metadata(path)?;
                anyhow::ensure!(
                    (source.dev(), source.ino(), source.len(), source.modified()?)
                        == (
                            current.dev(),
                            current.ino(),
                            current.len(),
                            current.modified()?
                        ),
                    "The source changed during the copy"
                );
                file_manager(Op::Remove, path, &target)
            };
            remove().context("The copy finished, but the original could not be removed")?;
        }
    }
    Ok(Some(Placed {
        from: path.to_path_buf(),
        to: target,
        copied: copy,
    }))
}

/// Counts logical regular-file data without following symbolic links.
fn content_size(
    path: &Path,
    root: &fs::Metadata,
    moving: bool,
    cancelled: impl Fn() -> bool,
) -> io::Result<u64> {
    if !root.is_dir() {
        return Ok(if root.is_file() { root.len() } else { 0 });
    }
    let mut bytes = 0_u64;
    let mut directories = vec![fs::read_dir(path)?];
    while let Some(entries) = directories.last_mut() {
        if cancelled() {
            return Err(ErrorKind::Interrupted.into());
        }
        let Some(entry) = entries.next() else {
            directories.pop();
            continue;
        };
        let entry = entry?;
        let metadata = entry.metadata()?;
        if moving && metadata.dev() != root.dev() {
            return Err(io::Error::other("The folder contains a mounted volume"));
        }
        if metadata.is_dir() {
            directories.push(fs::read_dir(entry.path())?);
        } else if metadata.is_file() {
            bytes = bytes.saturating_add(metadata.len());
        }
    }
    Ok(bytes)
}

pub(super) fn item_size(path: &Path, metadata: &fs::Metadata) -> io::Result<u64> {
    content_size(path, metadata, false, || false)
}

/// Keeps an incomplete output outside its final name, on the same volume.
struct StagingDir(PathBuf);

impl StagingDir {
    fn new(folder: &Path) -> Result<Self> {
        let mut template =
            CString::new(folder.join(".filetree-work-XXXXXX").as_os_str().as_bytes())?
                .into_bytes_with_nul();
        // SAFETY: mkdtemp receives a writable, null-terminated template.
        let stage = unsafe { libc::mkdtemp(template.as_mut_ptr().cast()) };
        if stage.is_null() {
            return Err(io::Error::last_os_error())
                .context("Could not prepare the destination folder");
        }
        // SAFETY: mkdtemp returned the modified template, still valid here.
        let name = unsafe { CStr::from_ptr(stage) }.to_bytes();
        Ok(Self(PathBuf::from(OsStr::from_bytes(name))))
    }
}

impl Drop for StagingDir {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0) {
            eprintln!(
                "Could not remove temporary folder {}: {error}",
                self.0.display()
            );
        }
    }
}

/// Writes an archive beside the selection and publishes it after it is complete.
pub(super) fn compress(paths: &[PathBuf]) -> Result<PathBuf> {
    let selected: HashSet<&Path> = paths.iter().map(PathBuf::as_path).collect();
    let sources: Vec<&Path> = paths
        .iter()
        .map(PathBuf::as_path)
        .filter(|path| {
            !path
                .ancestors()
                .skip(1)
                .any(|parent| selected.contains(parent))
        })
        .collect();
    let first = sources.first().context("Select files to compress")?;
    let folder = first.parent().context("Cannot archive the top of a disk")?;
    let archive_name = if sources.len() == 1 {
        let mut name = first
            .file_name()
            .context("The source has no name")?
            .to_os_string();
        name.push(".zip");
        name
    } else {
        "Archive.zip".into()
    };
    let target = free_name(folder, &archive_name, false);
    let folder_real = fs::canonicalize(folder)?;
    let mut names = HashSet::new();
    for source in &sources {
        let name = source.file_name().context("The source has no name")?;
        anyhow::ensure!(
            names.insert(name),
            "Two selected items are named {}",
            name.to_string_lossy()
        );
        anyhow::ensure!(
            !fs::symlink_metadata(source)?.is_dir()
                || !folder_real.starts_with(fs::canonicalize(source)?),
            "The archive cannot be created inside a selected folder"
        );
    }
    let stage = StagingDir::new(folder)?;
    let staged = stage.0.join("archive.zip");
    let mut command;
    if sources.len() == 1 && !fs::symlink_metadata(first)?.is_symlink() {
        command = Command::new("/usr/bin/ditto");
        command
            .args(["-c", "-k", "--sequesterRsrc", "--keepParent"])
            .arg(first)
            .arg(&staged);
    } else {
        command = Command::new("/usr/bin/tar");
        command
            .args(["--format", "zip", "--mac-metadata", "-cf"])
            .arg(&staged);
        for source in sources {
            let parent = source
                .parent()
                .context("Cannot archive the top of a disk")?;
            let name = Path::new(".").join(source.file_name().context("The source has no name")?);
            command.arg("-C").arg(parent).arg(name);
        }
    }
    let output = command.output().context("Could not start compression")?;
    anyhow::ensure!(
        output.status.success(),
        "Could not compress selection. {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    move_item(&staged, &target)?;
    Ok(target)
}

/// Creates a Finder alias, using a bookmark so it can follow a renamed target.
pub(super) fn make_alias(path: &Path) -> Result<PathBuf> {
    let folder = path
        .parent()
        .context("Cannot create an alias beside the top of a disk")?;
    let mut name = path
        .file_name()
        .context("The source has no name")?
        .to_os_string();
    name.push(" alias.alias");
    let target = free_name(folder, &name, false);
    let stage = StagingDir::new(folder)?;
    let staged = stage.0.join("bookmark.alias");
    // SAFETY: Foundation owns the temporary objects until the pool drains.
    unsafe {
        const SUITABLE_FOR_BOOKMARK_FILE: usize = 1 << 10;
        let _pool = StrongPtr::new(msg_send![class!(NSAutoreleasePool), new]);
        let manager: id = msg_send![class!(NSFileManager), defaultManager];
        let source = file_url(manager, path).context("Could not read the source path")?;
        let destination = file_url(manager, &staged).context("Could not read the alias path")?;
        let mut error: id = nil;
        let bookmark: id = msg_send![source,
            bookmarkDataWithOptions: SUITABLE_FOR_BOOKMARK_FILE
            includingResourceValuesForKeys: nil relativeToURL: nil error: &mut error
        ];
        if bookmark == nil {
            bail!("Could not create an alias for {}", path.display());
        }
        let written: BOOL = msg_send![class!(NSURL),
            writeBookmarkData: bookmark toURL: destination options: 0_usize error: &mut error
        ];
        if written != YES {
            bail!("Could not write the alias for {}", path.display());
        }
    }
    move_item(&staged, &target)?;
    Ok(target)
}

/// Stages a copy beside the target. Returns false if cancelled before publishing.
fn copy_with_progress(item: &Item, target: &Path, same_volume: bool) -> Result<bool> {
    let stage = StagingDir::new(&item.folder)?;
    let staged = stage
        .0
        .join(target.file_name().context("The destination has no name")?);
    let source = CString::new(item.source.as_os_str().as_bytes())?;
    let destination = CString::new(staged.as_os_str().as_bytes())?;
    let mut context = CopyProgress {
        item,
        finished: 0,
        error: None,
    };
    let mut flags = libc::COPYFILE_METADATA
        | libc::COPYFILE_DATA
        | libc::COPYFILE_RECURSIVE
        | libc::COPYFILE_NOFOLLOW
        | libc::COPYFILE_EXCL
        | libc::COPYFILE_DATA_SPARSE;
    if same_volume {
        flags |= libc::COPYFILE_CLONE;
    }
    // SAFETY: The paths and callback context live until the synchronous
    // copy returns. Each state is released here, after reading errno.
    unsafe {
        let state = libc::copyfile_state_alloc();
        if state.is_null() {
            return Err(io::Error::last_os_error()).context("Could not prepare the copy");
        }
        let setup = libc::copyfile_state_set(
            state,
            libc::COPYFILE_STATE_STATUS_CB as u32,
            (copy_progress as *const ()).cast(),
        );
        let status = if setup == 0 {
            libc::copyfile_state_set(
                state,
                libc::COPYFILE_STATE_STATUS_CTX as u32,
                std::ptr::from_mut(&mut context).cast(),
            )
        } else {
            setup
        };
        let status = if status == 0 {
            libc::copyfile(source.as_ptr(), destination.as_ptr(), state, flags)
        } else {
            status
        };
        let error = context.error.unwrap_or_else(io::Error::last_os_error);
        libc::copyfile_state_free(state);
        if status != 0 {
            if item.is_cancelled() {
                return Ok(false);
            }
            return Err(error).with_context(|| format!("Could not copy {}", item.name()));
        }
    }
    // Publishing and removing a cross-volume move's source are one commit
    // phase for cancellation. The state change arbitrates the race with Cancel.
    if !item.advance(Status::Finalizing) {
        return Ok(false);
    }
    move_item(&staged, target)?;
    Ok(true)
}

struct CopyProgress<'a> {
    item: &'a Item,
    finished: u64,
    error: Option<io::Error>,
}

unsafe extern "C" fn copy_progress(
    event: libc::c_int,
    stage: libc::c_int,
    state: libc::copyfile_state_t,
    source: *const libc::c_char,
    _: *const libc::c_char,
    context: *mut libc::c_void,
) -> libc::c_int {
    // SAFETY: copyfile calls back synchronously with the context and a live
    // per-file state. The source string is valid for the callback.
    unsafe {
        let context = &mut *context.cast::<CopyProgress<'_>>();
        if context.item.is_cancelled() {
            return libc::COPYFILE_QUIT;
        }
        match (event, stage) {
            (_, libc::COPYFILE_ERR) => {
                // Continuing a recursive error can turn a partial copy into
                // a successful return.
                context.error = Some(io::Error::last_os_error());
                return libc::COPYFILE_QUIT;
            }
            (libc::COPYFILE_COPY_DATA, libc::COPYFILE_PROGRESS) => {
                let mut bytes: libc::off_t = 0;
                if libc::copyfile_state_get(
                    state,
                    libc::COPYFILE_STATE_COPIED as u32,
                    std::ptr::from_mut(&mut bytes).cast(),
                ) == 0
                {
                    context
                        .item
                        .set_bytes(context.finished.saturating_add(bytes.max(0) as u64));
                }
            }
            (libc::COPYFILE_RECURSE_FILE, libc::COPYFILE_FINISH) if !source.is_null() => {
                // Count a whole file once, including clones and sparse regions.
                let source = Path::new(OsStr::from_bytes(CStr::from_ptr(source).to_bytes()));
                if let Ok(metadata) = fs::symlink_metadata(source)
                    && metadata.is_file()
                {
                    context.finished = context.finished.saturating_add(metadata.len());
                    context.item.set_bytes(context.finished);
                }
            }
            _ => {}
        }
    }
    libc::COPYFILE_CONTINUE
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
    Remove,
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
            Op::Remove => msg_send![manager, removeItemAtURL: source error: &mut error],
        };
        if !done {
            bail!(
                "Could not {} {}",
                match op {
                    Op::Move => "move",
                    Op::Copy => "copy",
                    Op::Remove => "remove",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_finder_tags_and_keeps_custom_colors_through_edits() {
        let scratch = StagingDir::new(&std::env::temp_dir()).unwrap();
        let path = scratch.0.join("Tagged.txt");
        fs::write(&path, "fixture").unwrap();
        let paths = std::slice::from_ref(&path);
        assert!(crate::file_tags::read(&path).unwrap().is_empty());
        let fixture = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><array>{}<string>Workshop&#10;4</string><string>Note&#10;0</string><string>Green&#10;2</string></array></plist>",
            " ".repeat(600),
        );
        let native_path = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: the path and byte buffer are live and belong to this fixture.
        assert_eq!(
            unsafe {
                libc::setxattr(
                    native_path.as_ptr(),
                    c"com.apple.metadata:_kMDItemUserTags".as_ptr(),
                    fixture.as_ptr().cast(),
                    fixture.len(),
                    0,
                    libc::XATTR_NOFOLLOW,
                )
            },
            0
        );

        let tags = crate::file_tags::read(&path).unwrap();
        assert_eq!(tags[0].name, "Workshop");
        assert!(tags[0].color.is_some());
        assert_eq!(tags[1].name, "Note");
        assert!(tags[1].color.is_none());
        assert_eq!(tags[2].name, "Green");
        assert_ne!(tags[0].color, tags[2].color);

        crate::file_tags::set(paths, "Red", true).unwrap();
        crate::file_tags::set(paths, "Green", false).unwrap();
        let updated = crate::file_tags::read(&path).unwrap();
        assert_eq!(&updated[..2], &tags[..2]);
        assert_eq!(updated[2].name, "Red");
        assert!(updated[2].color.is_some());
        assert!(
            crate::file_tags::choices(paths)
                .unwrap()
                .iter()
                .any(|(tag, checked)| *tag == tags[0] && *checked)
        );
    }

    #[test]
    fn cancelled_transfers_keep_the_source_and_remove_staging() {
        let scratch = StagingDir::new(&std::env::temp_dir()).unwrap();
        let source = scratch.0.join("Example");
        let folder = scratch.0.join("Destination");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&folder).unwrap();
        fs::write(source.join("Contents.txt"), "keep this").unwrap();
        let target = folder.join("Example");

        let queued = Item::new(source.clone(), folder.clone());
        queued.cancel();
        assert!(transfer(&queued, Transfer::Move).unwrap().is_none());

        let copying = Item::new(source.clone(), folder.clone());
        copying.advance(Status::Transferring {
            copy: false,
            total: None,
        });
        copying.cancel();
        assert!(!copy_with_progress(&copying, &target, false).unwrap());
        assert_eq!(
            fs::read_to_string(source.join("Contents.txt")).unwrap(),
            "keep this"
        );
        assert_eq!(fs::read_dir(folder).unwrap().count(), 0);
    }

    #[test]
    fn archives_multiple_items_without_replacing_an_existing_archive() {
        let scratch = StagingDir::new(&std::env::temp_dir()).unwrap();
        let first = scratch.0.join("first.txt");
        let second = scratch.0.join("second.txt");
        let existing = scratch.0.join("Archive.zip");
        fs::write(&first, "first input").unwrap();
        fs::write(&second, "second input").unwrap();
        fs::write(&existing, "keep this").unwrap();

        let archive = compress(&[first, second]).unwrap();
        assert_eq!(archive.file_name().unwrap(), "Archive 2.zip");
        assert_eq!(fs::read_to_string(existing).unwrap(), "keep this");
        let listing = Command::new("/usr/bin/tar")
            .arg("-tf")
            .arg(archive)
            .output()
            .unwrap();
        assert!(listing.status.success());
        let entries = String::from_utf8(listing.stdout).unwrap();
        assert!(entries.lines().any(|name| name.ends_with("first.txt")));
        assert!(entries.lines().any(|name| name.ends_with("second.txt")));
    }

    #[test]
    fn makes_finder_alias_that_resolves_to_the_original() {
        let scratch = StagingDir::new(&std::env::temp_dir()).unwrap();
        let source = scratch.0.join("Example.txt");
        fs::write(&source, "hello").unwrap();
        let alias = make_alias(&source).unwrap();
        assert_ne!(alias, source);
        assert!(alias.is_file());
        // SAFETY: Foundation URLs and the resolved bookmark remain in the pool.
        unsafe {
            let _pool = StrongPtr::new(msg_send![class!(NSAutoreleasePool), new]);
            let manager: id = msg_send![class!(NSFileManager), defaultManager];
            let url = file_url(manager, &alias).unwrap();
            let resolved: id = msg_send![class!(NSURL),
                URLByResolvingAliasFileAtURL: url options: (1_usize << 8) error: nil
            ];
            assert_eq!(
                path_of(resolved).unwrap().canonicalize().unwrap(),
                source.canonicalize().unwrap()
            );
        }
    }

    #[test]
    fn finds_installed_workflows_that_accept_the_selection_type() {
        let scratch = StagingDir::new(&std::env::temp_dir()).unwrap();
        let workflow = scratch.0.join("Library/Services/Example.workflow/Contents");
        fs::create_dir_all(&workflow).unwrap();
        fs::write(workflow.join("Info.plist"), r#"
<plist version="1.0"><dict><key>NSServices</key><array><dict>
<key>NSMenuItem</key><dict><key>default</key><string>Filetree Example</string></dict>
<key>NSSendFileTypes</key><array><string>public.text</string></array>
<key>NSRequiredContext</key><dict><key>NSApplicationIdentifier</key><string>com.apple.finder</string></dict>
</dict></array></dict></plist>"#).unwrap();
        let text_file = scratch.0.join("example.txt");
        fs::write(&text_file, "fixture").unwrap();
        let text_actions = crate::native_file_menu::workflows(&scratch.0, &[text_file]);
        assert!(
            text_actions
                .iter()
                .any(|action| action.path.starts_with(&scratch.0))
        );

        let selected_folder = scratch.0.join("Folder");
        fs::create_dir(&selected_folder).unwrap();
        let folder_actions = crate::native_file_menu::workflows(&scratch.0, &[selected_folder]);
        assert!(
            !folder_actions
                .iter()
                .any(|action| action.path.starts_with(&scratch.0))
        );
    }

    #[test]
    fn finds_file_input_services_from_app_manifests() {
        let scratch = StagingDir::new(&std::env::temp_dir()).unwrap();
        let app = scratch
            .0
            .join("Applications/Utilities/Example.app/Contents");
        fs::create_dir_all(&app).unwrap();
        fs::write(app.join("Info.plist"), r#"
<plist version="1.0"><dict><key>NSServices</key><array>
<dict><key>NSMenuItem</key><dict><key>default</key><string>Filetree Fixture Service</string></dict>
<key>NSSendFileTypes</key><array><string>public.folder</string></array></dict>
<dict><key>NSMenuItem</key><dict><key>default</key><string>Filetree Text Only Fixture</string></dict>
<key>NSSendTypes</key><array><string>public.utf8-plain-text</string></array></dict>
</array></dict></plist>"#).unwrap();
        let services =
            crate::native_file_menu::services(&scratch.0, std::slice::from_ref(&scratch.0));
        assert!(
            services
                .iter()
                .any(|name| name == "Filetree Fixture Service")
        );
        assert!(
            !services
                .iter()
                .any(|name| name == "Filetree Text Only Fixture")
        );
    }
}
