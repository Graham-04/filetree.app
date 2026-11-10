//! Exercise the native inset query before AppKit has an application menu.
//! Runs on the main thread without opening a window or entering fullscreen.
#[cfg(target_os = "macos")]
use gpui::{ForegroundExecutor, Pixels, px};

#[cfg(target_os = "macos")]
#[path = "../src/platform/mac/fullscreen.rs"]
#[allow(dead_code)]
mod fullscreen;

fn main() {
    #[cfg(target_os = "macos")]
    native::fullscreen_before_menu_installation();
}

#[cfg(target_os = "macos")]
mod native {
    use super::*;
    use cocoa::{
        appkit::{NSApplication, NSView, NSWindowStyleMask},
        base::{id, nil},
        foundation::{NSAutoreleasePool, NSPoint, NSRect, NSSize},
    };
    use objc::{
        class,
        declare::ClassDecl,
        msg_send,
        runtime::{Object, Sel},
        sel, sel_impl,
    };

    // Only the window geometry is simulated. The query uses the real AppKit
    // application, optional main menu, screen, view and titlebar metrics.
    extern "C" fn style_mask(_: &Object, _: Sel) -> u64 {
        NSWindowStyleMask::NSFullScreenWindowMask.bits()
    }
    extern "C" fn screen(_: &Object, _: Sel) -> id {
        unsafe { msg_send![class!(NSScreen), mainScreen] }
    }
    extern "C" fn to_screen(_: &Object, _: Sel, rect: NSRect) -> NSRect {
        rect
    }

    pub fn fullscreen_before_menu_installation() {
        unsafe {
            let pool = NSAutoreleasePool::new(nil);
            let app = NSApplication::sharedApplication(nil);
            let menu: id = msg_send![app, mainMenu];
            assert!(
                menu.is_null(),
                "regression requires the startup state with no menu"
            );

            let mut class = ClassDecl::new("FullscreenGeometryFixture", class!(NSObject)).unwrap();
            class.add_method(
                sel!(styleMask),
                style_mask as extern "C" fn(&Object, Sel) -> u64,
            );
            class.add_method(sel!(screen), screen as extern "C" fn(&Object, Sel) -> id);
            class.add_method(
                sel!(convertRectToScreen:),
                to_screen as extern "C" fn(&Object, Sel, NSRect) -> NSRect,
            );
            let class = class.register();
            let window: id = msg_send![class, new];
            let screen: id = msg_send![window, screen];
            assert!(
                !screen.is_null(),
                "a screen is required to reach the menu query"
            );
            let view = NSView::initWithFrame_(
                NSView::alloc(nil),
                NSRect::new(NSPoint::new(0., 0.), NSSize::new(800., 600.)),
            );
            let cx = gpui::TestAppContext::single();
            let chrome = fullscreen::FullscreenChrome::new(cx.foreground_executor().clone());
            assert_eq!(chrome.top_inset(window, view), px(0.));
            // The same query must continue working after the app installs a menu.
            let menu: id = msg_send![class!(NSMenu), new];
            let _: () = msg_send![app, setMainMenu: menu];
            assert_eq!(chrome.top_inset(window, view), px(0.));
            let _: () = msg_send![app, setMainMenu: nil];
            let _: () = msg_send![menu, release];
            let _: () = msg_send![view, release];
            let _: () = msg_send![window, release];
            cx.quit();
            pool.drain();
        }
    }
}
