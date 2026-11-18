//! Folder opens from Launch Services and reveal requests from the macOS file viewer.

use std::{
    ffi::{CStr, OsStr, c_void},
    os::unix::ffi::OsStrExt,
    path::PathBuf,
};

use anyhow::{Result, bail};
use cocoa::{
    base::{BOOL, YES, id, nil},
    foundation::NSString,
};
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use gpui::Application;
use objc::{
    class,
    declare::ClassDecl,
    msg_send,
    rc::StrongPtr,
    runtime::{Object, Sel},
    sel, sel_impl,
};

use crate::file_ops::path_of;

unsafe extern "C" {
    static NSURLNameKey: id;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFPreferencesAnyApplication: id;
    static kCFPreferencesCurrentUser: id;
    static kCFPreferencesAnyHost: id;
    fn CFPreferencesCopyValue(key: id, app: id, user: id, host: id) -> id;
    fn CFPreferencesSetValue(key: id, value: id, app: id, user: id, host: id);
    fn CFPreferencesSynchronize(app: id, user: id, host: id) -> u8;
}

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn LSCopyDefaultRoleHandlerForContentType(content_type: id, roles: u32) -> id;
}

const FOLDER_TYPE: &str = "public.folder";
const VIEWER_KEY: &str = "NSFileViewer";
const HANDLERS_DOMAIN: &str = "com.apple.LaunchServices/com.apple.launchservices.secure";
const ALL_ROLES: u32 = u32::MAX;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum DefaultStatus {
    Unavailable,
    Other,
    RestartRequired,
    Filetree,
}

pub(super) fn default_status() -> DefaultStatus {
    // SAFETY: used on the main thread. Only the enum leaves the pool.
    unsafe {
        let _pool = StrongPtr::new(msg_send![class!(NSAutoreleasePool), new]);
        let bundle: id = msg_send![class!(NSBundle), mainBundle];
        let bundle_id: id = msg_send![bundle, bundleIdentifier];
        if bundle_id == nil {
            return DefaultStatus::Unavailable;
        }

        let key = StrongPtr::new(NSString::alloc(nil).init_str(VIEWER_KEY));
        let viewer = StrongPtr::new(CFPreferencesCopyValue(
            *key,
            kCFPreferencesAnyApplication,
            kCFPreferencesCurrentUser,
            kCFPreferencesAnyHost,
        ));
        if !equal(*viewer, bundle_id) {
            return DefaultStatus::Other;
        }

        let folder = StrongPtr::new(NSString::alloc(nil).init_str(FOLDER_TYPE));
        let active = StrongPtr::new(LSCopyDefaultRoleHandlerForContentType(*folder, ALL_ROLES));
        if equal(*active, bundle_id) {
            DefaultStatus::Filetree
        } else if FolderHandlers::load(HANDLERS_DOMAIN)
            .is_ok_and(|handlers| handlers.configured_for(bundle_id))
        {
            DefaultStatus::RestartRequired
        } else {
            DefaultStatus::Other
        }
    }
}

/// The user explicitly opts in through Settings. Launch Services protects
/// public.folder from the NSWorkspace setter, so record the user's choice in
/// the same preferences domain used by the manual default-file-manager setup.
pub(super) fn make_default() -> Result<DefaultStatus> {
    // SAFETY: main thread. Each preference write is scoped to one key for the
    // current user. No Foundation object escapes the autorelease pool.
    unsafe {
        let _pool = StrongPtr::new(msg_send![class!(NSAutoreleasePool), new]);
        let bundle: id = msg_send![class!(NSBundle), mainBundle];
        let bundle_id: id = msg_send![bundle, bundleIdentifier];
        if bundle_id == nil {
            bail!("Open Filetree from its app bundle to make it the default.");
        }

        FolderHandlers::load(HANDLERS_DOMAIN)?.set_bundle(bundle_id)?;
        let key = StrongPtr::new(NSString::alloc(nil).init_str(VIEWER_KEY));
        CFPreferencesSetValue(
            *key,
            bundle_id,
            kCFPreferencesAnyApplication,
            kCFPreferencesCurrentUser,
            kCFPreferencesAnyHost,
        );
        if CFPreferencesSynchronize(
            kCFPreferencesAnyApplication,
            kCFPreferencesCurrentUser,
            kCFPreferencesAnyHost,
        ) == 0
        {
            bail!("Unable to save the preference for Reveal in Finder.");
        }
        match default_status() {
            status @ (DefaultStatus::Filetree | DefaultStatus::RestartRequired) => Ok(status),
            _ => bail!("macOS did not save the default file manager preferences."),
        }
    }
}

/// Reads and updates the single Launch Services preference that contains
/// handler records, preserving all associations unrelated to opening folders.
struct FolderHandlers {
    domain: StrongPtr,
    key: StrongPtr,
    entries: StrongPtr,
    type_key: StrongPtr,
    role_key: StrongPtr,
    folder: StrongPtr,
}

impl FolderHandlers {
    unsafe fn load(domain_name: &str) -> Result<Self> {
        // SAFETY: the caller holds an autorelease pool. CF preference objects
        // are toll-free bridged to their Foundation equivalents.
        unsafe {
            let domain = StrongPtr::new(NSString::alloc(nil).init_str(domain_name));
            let key = StrongPtr::new(NSString::alloc(nil).init_str("LSHandlers"));
            let entries = StrongPtr::new(CFPreferencesCopyValue(
                *key,
                *domain,
                kCFPreferencesCurrentUser,
                kCFPreferencesAnyHost,
            ));
            let entries = if *entries == nil {
                StrongPtr::new(msg_send![class!(NSArray), new])
            } else {
                let is_array: BOOL = msg_send![*entries, isKindOfClass: class!(NSArray)];
                if is_array != YES {
                    bail!("The macOS folder association list could not be read.");
                }
                entries
            };
            Ok(Self {
                domain,
                key,
                entries,
                type_key: StrongPtr::new(NSString::alloc(nil).init_str("LSHandlerContentType")),
                role_key: StrongPtr::new(NSString::alloc(nil).init_str("LSHandlerRoleAll")),
                folder: StrongPtr::new(NSString::alloc(nil).init_str(FOLDER_TYPE)),
            })
        }
    }

    unsafe fn folder_records(&self) -> impl Iterator<Item = (usize, id)> + '_ {
        // SAFETY: entries is an NSArray retained by self. Check each entry's
        // class before reading a key, and keep it alive for the iterator.
        unsafe {
            let count: usize = msg_send![*self.entries, count];
            (0..count).filter_map(|index| {
                let entry: id = msg_send![*self.entries, objectAtIndex: index];
                let is_record: BOOL = msg_send![entry, isKindOfClass: class!(NSDictionary)];
                if is_record != YES {
                    return None;
                }
                let content_type: id = msg_send![entry, objectForKey: *self.type_key];
                let role: id = msg_send![entry, objectForKey: *self.role_key];
                (equal(content_type, *self.folder) && role != nil).then_some((index, entry))
            })
        }
    }

    unsafe fn configured_for(&self, bundle_id: id) -> bool {
        // SAFETY: all record objects remain retained by self.
        unsafe {
            self.folder_records().last().is_some_and(|(_, entry)| {
                let role: id = msg_send![entry, objectForKey: *self.role_key];
                equal(role, bundle_id)
            })
        }
    }

    unsafe fn set_bundle(&self, bundle_id: id) -> Result<()> {
        // SAFETY: all objects are retained or autoreleased in the caller's
        // pool. Rewrite only folder records that specify an all-roles handler.
        unsafe {
            let updated = StrongPtr::new(msg_send![*self.entries, mutableCopy]);
            let mut found = false;
            for (index, entry) in self.folder_records() {
                let record = StrongPtr::new(msg_send![entry, mutableCopy]);
                let _: () = msg_send![*record, setObject: bundle_id forKey: *self.role_key];
                let _: () = msg_send![*updated, replaceObjectAtIndex: index withObject: *record];
                found = true;
            }
            if !found {
                let record: id = msg_send![class!(NSMutableDictionary), dictionary];
                let _: () = msg_send![record, setObject: *self.folder forKey: *self.type_key];
                let _: () = msg_send![record, setObject: bundle_id forKey: *self.role_key];
                let _: () = msg_send![*updated, addObject: record];
            }
            CFPreferencesSetValue(
                *self.key,
                *updated,
                *self.domain,
                kCFPreferencesCurrentUser,
                kCFPreferencesAnyHost,
            );
            if CFPreferencesSynchronize(
                *self.domain,
                kCFPreferencesCurrentUser,
                kCFPreferencesAnyHost,
            ) == 0
            {
                bail!("Unable to save Filetree as the preferred app for folders.");
            }
            Ok(())
        }
    }
}

unsafe fn equal(value: id, expected: id) -> bool {
    if value == nil {
        return false;
    }
    // SAFETY: both objects are live for the comparison.
    let matches: BOOL = unsafe { msg_send![value, isEqual: expected] };
    matches == YES
}

pub(super) enum Request {
    Open(Vec<PathBuf>),
    Reveal(Vec<PathBuf>),
}

const DIRECT_OBJECT: u32 = u32::from_be_bytes(*b"----"); // keyDirectObject
const LIST: u32 = u32::from_be_bytes(*b"list"); // typeAEList
const FILE_URL: u32 = u32::from_be_bytes(*b"furl"); // typeFileURL
const REVEAL_EVENTS: [(u32, u32); 2] = [
    (u32::from_be_bytes(*b"misc"), u32::from_be_bytes(*b"mvis")),
    (u32::from_be_bytes(*b"core"), u32::from_be_bytes(*b"slct")),
];

/// Owns the native event target and its sender for the duration of the AppKit run loop.
pub(super) struct RevealHandler {
    _target: StrongPtr,
    _sender: Box<UnboundedSender<Request>>,
}

impl RevealHandler {
    /// Registers before AppKit starts so requests that launch Filetree are queued
    /// just like requests sent to an already-open window.
    pub(super) fn listen(app: &Application) -> (Option<Self>, UnboundedReceiver<Request>) {
        let (sender, requests) = unbounded();
        let open_sender = sender.clone();
        app.on_open_urls(move |urls| {
            // SAFETY: AppKit invokes this callback on the main thread. Copy the
            // file paths before releasing the temporary Foundation objects.
            let paths: Vec<_> = unsafe {
                let _pool = StrongPtr::new(msg_send![class!(NSAutoreleasePool), new]);
                urls.iter()
                    .filter_map(|url| {
                        let string = StrongPtr::new(NSString::alloc(nil).init_str(url));
                        let url: id = msg_send![class!(NSURL), URLWithString: *string];
                        url_path(url)
                    })
                    .collect()
            };
            if !paths.is_empty() {
                let _ = open_sender.unbounded_send(Request::Open(paths));
            }
        });
        (Self::install(sender), requests)
    }

    fn install(sender: UnboundedSender<Request>) -> Option<Self> {
        // SAFETY: registration and delivery take place on the main thread.
        // The boxed sender stays at this address until the handlers are removed.
        unsafe {
            let mut class = ClassDecl::new("FiletreeRevealHandler", class!(NSObject))?;
            let selector = sel!(handleReveal:withReply:);
            class.add_ivar::<*const c_void>("requestSender");
            class.add_method(
                selector,
                handle_reveal as extern "C" fn(&Object, Sel, id, id),
            );
            let _pool = StrongPtr::new(msg_send![class!(NSAutoreleasePool), new]);
            let sender = Box::new(sender);
            let target: id = msg_send![class.register(), new];
            target.as_mut()?.set_ivar(
                "requestSender",
                std::ptr::from_ref(&*sender).cast::<c_void>(),
            );
            let target = StrongPtr::new(target);
            let manager: id = msg_send![class!(NSAppleEventManager), sharedAppleEventManager];
            for (event_class, event_id) in REVEAL_EVENTS {
                let _: () = msg_send![manager, setEventHandler: *target
                    andSelector: selector forEventClass: event_class andEventID: event_id];
            }
            Some(Self {
                _target: target,
                _sender: sender,
            })
        }
    }
}

impl Drop for RevealHandler {
    fn drop(&mut self) {
        // SAFETY: this guard is dropped on the main thread, before its target and sender.
        unsafe {
            let _pool = StrongPtr::new(msg_send![class!(NSAutoreleasePool), new]);
            let manager: id = msg_send![class!(NSAppleEventManager), sharedAppleEventManager];
            for (event_class, event_id) in REVEAL_EVENTS {
                let _: () = msg_send![manager, removeEventHandlerForEventClass: event_class
                    andEventID: event_id];
            }
        }
    }
}

extern "C" fn handle_reveal(target: &Object, _: Sel, event: id, _: id) {
    // SAFETY: AppKit supplies a live event descriptor. The target's guard owns
    // the sender for the entire callback, and only owned paths leave it.
    unsafe {
        let direct: id = msg_send![event, paramDescriptorForKeyword: DIRECT_OBJECT];
        let paths = paths_in_descriptor(direct);
        if !paths.is_empty() {
            let sender = *target.get_ivar::<*const c_void>("requestSender");
            let sender = &*sender.cast::<UnboundedSender<Request>>();
            let _ = sender.unbounded_send(Request::Reveal(paths));
        }
    }
}

unsafe fn paths_in_descriptor(descriptor: id) -> Vec<PathBuf> {
    if descriptor == nil {
        return Vec::new();
    }
    // SAFETY: the caller holds the descriptor live, including any child descriptors.
    unsafe {
        let kind: u32 = msg_send![descriptor, descriptorType];
        if kind == LIST {
            let count: isize = msg_send![descriptor, numberOfItems];
            (1..=count)
                .filter_map(|index| {
                    descriptor_path(msg_send![descriptor, descriptorAtIndex: index])
                })
                .collect()
        } else {
            descriptor_path(descriptor).into_iter().collect()
        }
    }
}

unsafe fn descriptor_path(descriptor: id) -> Option<PathBuf> {
    if descriptor == nil {
        return None;
    }
    // SAFETY: the caller holds the descriptor live. Coercion also handles aliases.
    unsafe {
        let file: id = msg_send![descriptor, coerceToDescriptorType: FILE_URL];
        if file == nil {
            return None;
        }
        let url: id = msg_send![file, fileURLValue];
        url_path(url)
    }
}

unsafe fn url_path(url: id) -> Option<PathBuf> {
    if url == nil {
        return None;
    }
    // SAFETY: the caller holds the URL live while its path is copied.
    unsafe {
        let is_file: BOOL = msg_send![url, isFileURL];
        if is_file != YES {
            return None;
        }
        let mut path = path_of(url).filter(|path| path.is_absolute())?;
        if path.file_name().is_some() {
            // NSURL's filesystem representation can decompose Unicode. Use the
            // actual directory entry's spelling so it matches read_dir.
            let mut name: id = nil;
            let found: BOOL = msg_send![url, getResourceValue: &mut name as *mut id
                forKey: NSURLNameKey error: std::ptr::null_mut::<id>()];
            if found == YES && name != nil {
                let bytes = name.UTF8String();
                if !bytes.is_null() {
                    path.set_file_name(OsStr::from_bytes(CStr::from_ptr(bytes).to_bytes()));
                }
            }
        }
        Some(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn changing_the_folder_app_preserves_other_associations() {
        let folder =
            std::env::temp_dir().join(format!("filetree-default-test-{}", std::process::id()));
        std::fs::create_dir(&folder).expect("temporary preferences folder");
        let domain = folder.join("launch-services");

        // SAFETY: this uses a disposable preferences domain. The pool keeps
        // the Foundation objects alive through the assertions.
        unsafe {
            let _pool = StrongPtr::new(msg_send![class!(NSAutoreleasePool), new]);
            let handlers = FolderHandlers::load(domain.to_str().expect("test domain path"))
                .expect("empty test domain");
            let candidate = StrongPtr::new(NSString::alloc(nil).init_str("test.filetree"));
            let previous = StrongPtr::new(NSString::alloc(nil).init_str("test.finder"));
            let text_type = StrongPtr::new(NSString::alloc(nil).init_str("public.plain-text"));
            let entry = |content_type: id| -> id {
                let row: id = msg_send![class!(NSMutableDictionary), dictionary];
                let _: () = msg_send![row, setObject: content_type forKey: *handlers.type_key];
                let _: () = msg_send![row, setObject: *previous forKey: *handlers.role_key];
                row
            };
            let other = entry(*text_type);
            let entries: id = msg_send![class!(NSMutableArray), arrayWithObject: other];
            let _: () = msg_send![entries, addObject: entry(*handlers.folder)];
            let _: () = msg_send![entries, addObject: entry(*handlers.folder)];
            CFPreferencesSetValue(
                *handlers.key,
                entries,
                *handlers.domain,
                kCFPreferencesCurrentUser,
                kCFPreferencesAnyHost,
            );

            let handlers = FolderHandlers::load(domain.to_str().expect("test domain path"))
                .expect("read list");
            handlers.set_bundle(*candidate).expect("save association");
            let saved = FolderHandlers::load(domain.to_str().expect("test domain path"))
                .expect("read saved list");
            let count: usize = msg_send![*saved.entries, count];
            let first: id = msg_send![*saved.entries, objectAtIndex: 0_usize];
            assert_eq!(count, 3);
            assert!(equal(first, other));
            assert!(saved.folder_records().all(|(_, entry)| {
                let bundle: id = msg_send![entry, objectForKey: *saved.role_key];
                equal(bundle, *candidate)
            }));
            assert!(saved.configured_for(*candidate));
        }
        std::fs::remove_dir_all(folder).expect("remove temporary preferences");
    }

    #[test]
    fn reveal_decodes_a_single_file_or_a_list_of_files() {
        let folder = std::env::temp_dir().join(format!("filetree-reveal-{}", std::process::id()));
        std::fs::create_dir(&folder).expect("temporary test folder");
        let first = folder.join("report #1.txt");
        let second = folder.join("café.txt");
        std::fs::write(&first, "").expect("first test file");
        std::fs::write(&second, "").expect("second test file");

        // SAFETY: the test keeps all descriptors alive in this autorelease pool.
        let (single, multiple) = unsafe {
            let _pool = StrongPtr::new(msg_send![class!(NSAutoreleasePool), new]);
            let manager: id = msg_send![class!(NSFileManager), defaultManager];
            let descriptor_class = class!(NSAppleEventDescriptor);
            let file = |path: &Path| -> id {
                let url = crate::file_ops::file_url(manager, path).expect("file URL");
                msg_send![descriptor_class, descriptorWithFileURL: url]
            };
            let first_file = file(&first);

            let list: id = msg_send![descriptor_class, listDescriptor];
            let invalid: id = msg_send![descriptor_class, descriptorWithInt32: 42_i32];
            let second_file = file(&second);
            let _: () = msg_send![list, insertDescriptor: first_file atIndex: 1_isize];
            let _: () = msg_send![list, insertDescriptor: invalid atIndex: 2_isize];
            let _: () = msg_send![list, insertDescriptor: second_file atIndex: 3_isize];
            (paths_in_descriptor(first_file), paths_in_descriptor(list))
        };
        std::fs::remove_dir_all(folder).expect("remove temporary test folder");

        assert_eq!(single, std::slice::from_ref(&first));
        assert_eq!(multiple, [first, second]);
    }
}
