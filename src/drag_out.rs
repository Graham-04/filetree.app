//! Dragging files out of the list as a native macOS drag, so they can be
//! dropped on Finder, the Desktop, Mail, or any app, with system icons as the
//! drag image. Drops back onto this app arrive through GPUI's file drops like
//! any other, so there is one drop path for files from anywhere.
//!
//! AppKit runs the drag itself and swallows the mouse release; GPUI would
//! keep seeing the button held and synthesizing drag moves. When the drag
//! ends, a release is posted back to the window so both settle.

use std::{cell::Cell, os::unix::ffi::OsStrExt, path::PathBuf, sync::Once};

use cocoa::{
    base::{BOOL, YES, id, nil},
    foundation::{NSPoint, NSRect, NSSize},
};
use gpui::Window;
use objc::{
    class,
    declare::ClassDecl,
    msg_send,
    runtime::{Object, Sel},
    sel, sel_impl,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

const OPERATION_COPY: usize = 1;
const OPERATION_GENERIC: usize = 4;
const OPERATION_MOVE: usize = 16;
const OPTION_KEY: usize = 1 << 19;
const LEFT_MOUSE_UP: usize = 2;
const ICON_SIZE: f64 = 32.;

thread_local! {
    static SOURCE: Cell<id> = const { Cell::new(std::ptr::null_mut()) };
}

/// Whether Option is held right now, which makes a drop copy instead of move.
/// Read from the hardware state, since AppKit owns the keyboard during a drag.
pub(super) fn option_held() -> bool {
    // SAFETY: a class method returning the current modifier flags.
    let flags: usize = unsafe { msg_send![class!(NSEvent), modifierFlags] };
    flags & OPTION_KEY != 0
}

/// Starts dragging `paths` from the mouse event GPUI is handling now. Returns
/// false, and nothing happens, outside a mouse drag.
pub(super) fn begin(window: &Window, paths: &[PathBuf]) -> bool {
    if paths.is_empty() {
        return false;
    }
    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return false;
    };
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return false;
    };
    install();
    // SAFETY: main thread, inside GPUI's handling of the current mouse event.
    // The view is GPUI's live NSView; created objects are autoreleased or
    // retained by the dragging session, and none outlive this call here.
    unsafe {
        let view = handle.ns_view.as_ptr() as id;
        let app: id = msg_send![class!(NSApplication), sharedApplication];
        let event: id = msg_send![app, currentEvent];
        if event == nil {
            return false;
        }
        let in_window: NSPoint = msg_send![event, locationInWindow];
        let at: NSPoint = msg_send![view, convertPoint: in_window fromView: nil];
        let workspace: id = msg_send![class!(NSWorkspace), sharedWorkspace];
        let items: id = msg_send![class!(NSMutableArray), array];
        for (index, path) in paths.iter().enumerate() {
            let Some(path_string) = native_path(path) else {
                continue;
            };
            let url: id = msg_send![class!(NSURL), fileURLWithPath: path_string];
            let item: id = msg_send![class!(NSDraggingItem), alloc];
            let item: id = msg_send![item, initWithPasteboardWriter: url];
            let icon: id = msg_send![workspace, iconForFile: path_string];
            // Stacked slightly, as Finder does, with the pointer at the
            // first icon's center.
            let offset = (index.min(4) as f64) * 4.;
            let frame = NSRect::new(
                NSPoint::new(
                    at.x - ICON_SIZE / 2. + offset,
                    at.y - ICON_SIZE / 2. - offset,
                ),
                NSSize::new(ICON_SIZE, ICON_SIZE),
            );
            let _: () = msg_send![item, setDraggingFrame: frame contents: icon];
            let _: () = msg_send![items, addObject: item];
            let _: () = msg_send![item, release];
        }
        let count: usize = msg_send![items, count];
        if count == 0 {
            return false;
        }
        let session: id = msg_send![view,
            beginDraggingSessionWithItems: items
            event: event
            source: SOURCE.get()
        ];
        let _: () = msg_send![session, setAnimatesToStartingPositionsOnCancelOrFail: YES];
        session != nil
    }
}

/// An `NSString` for `path`, decoded by Foundation so non-UTF-8 names work.
unsafe fn native_path(path: &std::path::Path) -> Option<id> {
    let bytes = path.as_os_str().as_bytes();
    // SAFETY: the caller is on the main thread; the string is autoreleased.
    unsafe {
        let manager: id = msg_send![class!(NSFileManager), defaultManager];
        let string: id = msg_send![manager,
            stringWithFileSystemRepresentation: bytes.as_ptr().cast::<std::ffi::c_char>()
            length: bytes.len()
        ];
        (string != nil).then_some(string)
    }
}

/// Registers the dragging source once.
fn install() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let Some(mut decl) = ClassDecl::new("FiletreeDragSource", class!(NSObject)) else {
            return;
        };
        // SAFETY: main thread, once; the class is new and its methods match
        // the NSDraggingSource selectors they implement.
        unsafe {
            decl.add_method(
                sel!(draggingSession:sourceOperationMaskForDraggingContext:),
                operations as extern "C" fn(&Object, Sel, id, isize) -> usize,
            );
            decl.add_method(
                sel!(draggingSession:endedAtPoint:operation:),
                ended as extern "C" fn(&Object, Sel, id, NSPoint, usize),
            );
            decl.add_method(
                sel!(ignoreModifierKeysForDraggingSession:),
                ignore_modifiers as extern "C" fn(&Object, Sel, id) -> BOOL,
            );
            let source: id = msg_send![decl.register(), new];
            SOURCE.set(source);
        }
    });
}

/// Destinations choose: Finder moves within a volume and copies across, and
/// Option makes it copy.
extern "C" fn operations(_: &Object, _: Sel, _: id, _: isize) -> usize {
    OPERATION_COPY | OPERATION_MOVE | OPERATION_GENERIC
}

extern "C" fn ignore_modifiers(_: &Object, _: Sel, _: id) -> BOOL {
    cocoa::base::NO
}

/// Posts the mouse release the drag swallowed to the window it started in.
extern "C" fn ended(_: &Object, _: Sel, _: id, screen_point: NSPoint, _: usize) {
    // SAFETY: called by AppKit on the main thread when the drag ends.
    unsafe {
        let app: id = msg_send![class!(NSApplication), sharedApplication];
        let window: id = msg_send![app, keyWindow];
        let window = if window == nil {
            msg_send![app, mainWindow]
        } else {
            window
        };
        if window == nil {
            return;
        }
        let screen_rect = NSRect::new(screen_point, NSSize::new(0., 0.));
        let local: NSRect = msg_send![window, convertRectFromScreen: screen_rect];
        let number: isize = msg_send![window, windowNumber];
        let uptime: f64 = {
            let info: id = msg_send![class!(NSProcessInfo), processInfo];
            msg_send![info, systemUptime]
        };
        let release: id = msg_send![class!(NSEvent),
            mouseEventWithType: LEFT_MOUSE_UP
            location: local.origin
            modifierFlags: 0usize
            timestamp: uptime
            windowNumber: number
            context: nil
            eventNumber: 0isize
            clickCount: 1isize
            pressure: 0f32
        ];
        if release != nil {
            let _: () = msg_send![app, postEvent: release atStart: YES];
        }
    }
}
