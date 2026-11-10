//! Quick Look through macOS's shared preview panel. The panel takes its
//! controller from the key window's responder chain, so GPUI's window class
//! gains the three controller methods, and a small data source serves the
//! paths most recently shown. While the panel is open, keys it does not use
//! go back to the app's window, so arrow keys keep moving the selection and
//! the preview follows it.

use std::{
    cell::{Cell, RefCell},
    os::unix::ffi::OsStrExt,
    path::PathBuf,
    sync::Once,
};

use cocoa::base::{BOOL, NO, YES, id, nil};
use objc::{
    class,
    declare::ClassDecl,
    msg_send,
    runtime::{Class, Object, Sel},
    sel, sel_impl,
};

#[link(name = "Quartz", kind = "framework")]
unsafe extern "C" {}

/// Most items previewed at once; a large selection previews its first ones.
pub(super) const LIMIT: usize = 100;

thread_local! {
    static ITEMS: RefCell<Vec<PathBuf>> = const { RefCell::new(Vec::new()) };
    static SOURCE: Cell<id> = const { Cell::new(std::ptr::null_mut()) };
}

/// Shows `paths` in Quick Look, or closes the panel if it is already open.
pub(super) fn toggle(paths: Vec<PathBuf>) {
    if is_open() {
        // SAFETY: main thread; the shared panel exists while it is open.
        unsafe {
            let panel: id = msg_send![class!(QLPreviewPanel), sharedPreviewPanel];
            let _: () = msg_send![panel, orderOut: nil];
        }
        return;
    }
    if paths.is_empty() {
        return;
    }
    install();
    ITEMS.set(paths);
    // SAFETY: main thread. `updateController` asks the key window, which now
    // answers the controller methods installed above.
    unsafe {
        let panel: id = msg_send![class!(QLPreviewPanel), sharedPreviewPanel];
        let _: () = msg_send![panel, updateController];
        let _: () = msg_send![panel, reloadData];
        let _: () = msg_send![panel, makeKeyAndOrderFront: nil];
    }
}

/// While the panel is open, previews `paths` instead when they differ.
pub(super) fn follow(paths: Vec<PathBuf>) {
    if paths.is_empty() || !is_open() || ITEMS.with_borrow(|items| *items == paths) {
        return;
    }
    ITEMS.set(paths);
    // SAFETY: main thread; the shared panel exists while it is open.
    unsafe {
        let panel: id = msg_send![class!(QLPreviewPanel), sharedPreviewPanel];
        let _: () = msg_send![panel, reloadData];
    }
}

pub(super) fn is_open() -> bool {
    // SAFETY: main thread. Checking existence first avoids creating the panel.
    unsafe {
        let exists: BOOL = msg_send![class!(QLPreviewPanel), sharedPreviewPanelExists];
        if exists == NO {
            return false;
        }
        let panel: id = msg_send![class!(QLPreviewPanel), sharedPreviewPanel];
        let visible: BOOL = msg_send![panel, isVisible];
        visible == YES
    }
}

/// Registers the data source and teaches GPUI's window class to control the
/// panel. Runs once, after GPUI has registered that class.
fn install() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        // SAFETY: main thread, once. The source class is new; the methods are
        // added to GPUI's registered window class, which defines none of them.
        unsafe {
            let Some(mut decl) = ClassDecl::new("FiletreeQuickLookSource", class!(NSObject)) else {
                return;
            };
            decl.add_method(
                sel!(numberOfPreviewItemsInPreviewPanel:),
                item_count as extern "C" fn(&Object, Sel, id) -> isize,
            );
            decl.add_method(
                sel!(previewPanel:previewItemAtIndex:),
                item_at as extern "C" fn(&Object, Sel, id, isize) -> id,
            );
            decl.add_method(
                sel!(previewPanel:handleEvent:),
                handle_event as extern "C" fn(&Object, Sel, id, id) -> BOOL,
            );
            let source: id = msg_send![decl.register(), new];
            SOURCE.set(source);

            let Some(window) = Class::get("GPUIWindow") else {
                return;
            };
            let window = window as *const Class as *mut Class;
            let bool_type = if cfg!(target_arch = "aarch64") {
                c"B@:@"
            } else {
                c"c@:@"
            };
            objc::runtime::class_addMethod(
                window,
                sel!(acceptsPreviewPanelControl:),
                std::mem::transmute::<extern "C" fn(&Object, Sel, id) -> BOOL, objc::runtime::Imp>(
                    accepts_control,
                ),
                bool_type.as_ptr(),
            );
            objc::runtime::class_addMethod(
                window,
                sel!(beginPreviewPanelControl:),
                std::mem::transmute::<extern "C" fn(&Object, Sel, id), objc::runtime::Imp>(
                    begin_control,
                ),
                c"v@:@".as_ptr(),
            );
            objc::runtime::class_addMethod(
                window,
                sel!(endPreviewPanelControl:),
                std::mem::transmute::<extern "C" fn(&Object, Sel, id), objc::runtime::Imp>(
                    end_control,
                ),
                c"v@:@".as_ptr(),
            );
        }
    });
}

extern "C" fn accepts_control(_: &Object, _: Sel, _: id) -> BOOL {
    YES
}

extern "C" fn begin_control(_: &Object, _: Sel, panel: id) {
    // SAFETY: called by AppKit on the main thread with the shared panel.
    unsafe {
        let source = SOURCE.get();
        let _: () = msg_send![panel, setDataSource: source];
        let _: () = msg_send![panel, setDelegate: source];
    }
}

extern "C" fn end_control(_: &Object, _: Sel, panel: id) {
    // SAFETY: called by AppKit on the main thread with the shared panel.
    unsafe {
        let _: () = msg_send![panel, setDataSource: nil];
        let _: () = msg_send![panel, setDelegate: nil];
    }
}

extern "C" fn item_count(_: &Object, _: Sel, _: id) -> isize {
    ITEMS.with_borrow(|items| items.len() as isize)
}

extern "C" fn item_at(_: &Object, _: Sel, _: id, index: isize) -> id {
    ITEMS.with_borrow(|items| {
        let Some(path) = usize::try_from(index)
            .ok()
            .and_then(|index| items.get(index))
        else {
            return nil;
        };
        let bytes = path.as_os_str().as_bytes();
        // SAFETY: main thread; the URL is autoreleased, as the panel expects.
        unsafe {
            let manager: id = msg_send![class!(NSFileManager), defaultManager];
            let native_path: id = msg_send![manager,
                stringWithFileSystemRepresentation: bytes.as_ptr().cast::<std::ffi::c_char>()
                length: bytes.len()
            ];
            if native_path == nil {
                return nil;
            }
            msg_send![class!(NSURL), fileURLWithPath: native_path]
        }
    })
}

/// Sends arrow keys and Space back to the app's window, so arrows move the
/// selection and Space closes the panel through the list, as in Finder.
/// Everything else, such as Escape, stays with the panel.
extern "C" fn handle_event(_: &Object, _: Sel, _: id, event: id) -> BOOL {
    const KEY_DOWN: u64 = 10;
    const SPACE: u16 = 49;
    const ARROWS: std::ops::RangeInclusive<u16> = 123..=126;
    // SAFETY: called by AppKit on the main thread with a live event.
    unsafe {
        let kind: u64 = msg_send![event, type];
        if kind != KEY_DOWN {
            return NO;
        }
        let key: u16 = msg_send![event, keyCode];
        if key != SPACE && !ARROWS.contains(&key) {
            return NO;
        }
        let app: id = msg_send![class!(NSApplication), sharedApplication];
        let window: id = msg_send![app, mainWindow];
        if window == nil {
            return NO;
        }
        let _: () = msg_send![window, sendEvent: event];
        YES
    }
}
