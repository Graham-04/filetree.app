use std::{
    collections::BTreeMap,
    ffi::CString,
    fs, io,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::{Mutex, PoisonError},
};

use anyhow::{Context as _, Result, ensure};
use cocoa::base::{BOOL, YES, id, nil};
use cocoa::foundation::NSString;
use objc::{class, msg_send, rc::StrongPtr, sel, sel_impl};

const ATTRIBUTE: &std::ffi::CStr = c"com.apple.metadata:_kMDItemUserTags";
const BINARY_PLIST: usize = 200;

#[link(name = "Foundation", kind = "framework")]
unsafe extern "C" {
    static NSURLLabelNumberKey: id;
}

static WRITES: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Tag {
    pub name: String,
    pub color: Option<u32>,
}

const COLORS: [(&str, u32); 7] = [
    ("Gray", 0xa8a8ad),
    ("Green", 0x68cd77),
    ("Purple", 0xcf70eb),
    ("Blue", 0x4ba8fb),
    ("Yellow", 0xffd64e),
    ("Red", 0xff6259),
    ("Orange", 0xffa249),
];

const DEFAULT_NAMES: [&str; 7] = ["Red", "Orange", "Yellow", "Green", "Blue", "Purple", "Gray"];

fn default_color(name: &str) -> Option<u32> {
    COLORS
        .iter()
        .find_map(|&(label, color)| (label == name).then_some(color))
}

impl Tag {
    fn decode(value: String) -> Self {
        if let Some((name, suffix)) = value.rsplit_once('\n')
            && let [index @ b'0'..=b'7'] = suffix.as_bytes()
        {
            let color = usize::from(*index - b'0')
                .checked_sub(1)
                .map(|index| COLORS[index].1);
            return Self {
                name: name.to_owned(),
                color,
            };
        }
        Self {
            color: default_color(&value),
            name: value,
        }
    }

    fn color_index(&self) -> Option<usize> {
        let color = self.color?;
        COLORS
            .iter()
            .position(|item| item.1 == color)
            .map(|index| index + 1)
    }
}

pub(super) fn choices(paths: &[PathBuf]) -> Result<Vec<(Tag, bool)>> {
    let selected: Vec<_> = paths
        .iter()
        .map(|path| {
            read(path).with_context(|| format!("Could not read tags on {}", path.display()))
        })
        .collect::<Result<_>>()?;
    let mut extra = BTreeMap::new();
    for tag in selected.iter().flatten() {
        if !DEFAULT_NAMES.contains(&tag.name.as_str()) {
            let color = extra.entry(tag.name.as_str()).or_insert(None);
            *color = color.or(tag.color);
        }
    }
    Ok(DEFAULT_NAMES
        .into_iter()
        .map(|name| (name, default_color(name)))
        .chain(extra)
        .map(|(name, color)| {
            let checked = !selected.is_empty()
                && selected
                    .iter()
                    .all(|tags| tags.iter().any(|tag| tag.name == name));
            (
                Tag {
                    name: name.to_owned(),
                    color,
                },
                checked,
            )
        })
        .collect())
}

/// Reads the Finder metadata for this item without following symbolic links.
/// Only tagged items need Foundation to decode the property list.
pub(super) fn read(path: &Path) -> Result<Vec<Tag>> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    let mut small = [0_u8; 512];
    match get_attribute(&path, &mut small) {
        Ok(length) => decode_list(&small[..length]),
        Err(error) if matches!(error.raw_os_error(), Some(libc::ENOATTR | libc::ENOTSUP)) => {
            Ok(Vec::new())
        }
        Err(error) if error.raw_os_error() == Some(libc::ERANGE) => {
            let size = get_attribute(&path, &mut [])?;
            if size == 0 {
                return Ok(Vec::new());
            }
            let mut bytes = vec![0; size];
            let length = get_attribute(&path, &mut bytes)?;
            decode_list(&bytes[..length])
        }
        Err(error) => Err(error.into()),
    }
}

fn get_attribute(path: &std::ffi::CStr, buffer: &mut [u8]) -> io::Result<usize> {
    let data = if buffer.is_empty() {
        std::ptr::null_mut()
    } else {
        buffer.as_mut_ptr().cast()
    };
    // SAFETY: Both C strings are live. With a zero-sized buffer getxattr only
    // reports the size, otherwise it writes at most buffer.len() bytes.
    let length = unsafe {
        libc::getxattr(
            path.as_ptr(),
            ATTRIBUTE.as_ptr(),
            data,
            buffer.len(),
            0,
            libc::XATTR_NOFOLLOW,
        )
    };
    if length < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(length as usize)
}

fn decode_list(bytes: &[u8]) -> Result<Vec<Tag>> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    // SAFETY: Foundation parses a copied buffer and returns autoreleased
    // objects. All strings are copied out before the pool is released.
    unsafe {
        let _pool = StrongPtr::new(msg_send![class!(NSAutoreleasePool), new]);
        let data: id = msg_send![class!(NSData), dataWithBytes: bytes.as_ptr() length: bytes.len()];
        let list: id = msg_send![class!(NSPropertyListSerialization),
            propertyListWithData: data options: 0_usize format: nil error: nil
        ];
        let is_array: BOOL = msg_send![list, isKindOfClass: class!(NSArray)];
        ensure!(is_array == YES, "Invalid Finder tags");
        let count: usize = msg_send![list, count];
        (0..count)
            .map(|index| {
                let value: id = msg_send![list, objectAtIndex: index];
                let is_string: BOOL = msg_send![value, isKindOfClass: class!(NSString)];
                ensure!(is_string == YES, "Invalid Finder tag entry");
                let value = crate::system::string(value).context("Could not decode Finder tag")?;
                Ok(Tag::decode(value))
            })
            .collect()
    }
}

/// Writes the full tag list so editing one name preserves existing colors.
pub(super) fn set(paths: &[PathBuf], name: &str, present: bool) -> Result<()> {
    let _write = WRITES.lock().unwrap_or_else(PoisonError::into_inner);
    for path in paths {
        let result: Result<()> = (|| {
            let mut tags = read(path)?;
            let tagged = tags.iter().any(|tag| tag.name == name);
            if tagged == present {
                return Ok(());
            }
            if present {
                tags.push(Tag {
                    name: name.to_owned(),
                    color: default_color(name),
                });
            } else {
                tags.retain(|tag| tag.name != name);
            }
            write(path, &tags)
        })();
        result.with_context(|| format!("Could not update tags on {}", path.display()))?;
    }
    Ok(())
}

fn write(path: &Path, tags: &[Tag]) -> Result<()> {
    let native_path = CString::new(path.as_os_str().as_bytes())?;
    let is_symlink = fs::symlink_metadata(path)?.is_symlink();
    // SAFETY: Foundation owns the property list and its bytes until the
    // pool drains. setxattr copies those bytes before returning.
    unsafe {
        let _pool = StrongPtr::new(msg_send![class!(NSAutoreleasePool), new]);
        let list: id = msg_send![class!(NSMutableArray), array];
        for tag in tags {
            let value = format!("{}\n{}", tag.name, tag.color_index().unwrap_or(0));
            let value = StrongPtr::new(NSString::alloc(nil).init_str(&value));
            let _: () = msg_send![list, addObject: *value];
        }
        let data: id = msg_send![class!(NSPropertyListSerialization),
            dataWithPropertyList: list format: BINARY_PLIST options: 0_usize error: nil
        ];
        ensure!(data != nil, "Could not encode tags");
        let pointer: *const std::ffi::c_void = msg_send![data, bytes];
        let length: usize = msg_send![data, length];
        let result = libc::setxattr(
            native_path.as_ptr(),
            ATTRIBUTE.as_ptr(),
            pointer,
            length,
            0,
            libc::XATTR_NOFOLLOW,
        );
        if result != 0 {
            return Err(io::Error::last_os_error().into());
        }
        if !is_symlink {
            // Older tools read the last colored tag as a single Finder label.
            let index = tags.iter().rev().find_map(Tag::color_index).unwrap_or(0);
            let manager: id = msg_send![class!(NSFileManager), defaultManager];
            let url =
                crate::file_ops::file_url(manager, path).context("Could not read the file path")?;
            let value: id = msg_send![class!(NSNumber), numberWithUnsignedInteger: index];
            let updated: BOOL =
                msg_send![url, setResourceValue: value forKey: NSURLLabelNumberKey error: nil];
            ensure!(
                updated == YES,
                "Tags were saved, but the Finder label could not be updated"
            );
        }
    }
    Ok(())
}
