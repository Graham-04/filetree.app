//! Native fullscreen chrome tracking, without mouse polling or app-owned animation.
use crate::{ForegroundExecutor, Pixels, px};
use cocoa::{
    appkit::{NSApplication, NSScreen, NSView, NSWindow, NSWindowStyleMask},
    base::{id, nil},
    foundation::{NSPoint, NSRect, NSSize},
};
use objc::{
    class,
    declare::ClassDecl,
    msg_send,
    runtime::{Class, Object, Sel, YES},
    sel, sel_impl,
};
use std::{
    cell::{Cell, RefCell},
    ffi::c_void,
    rc::Rc,
    sync::OnceLock,
};

const STATE: &str = "fullscreenState";

#[derive(Clone)]
pub(super) struct FullscreenChrome(Rc<State>);

struct State {
    reveal: Cell<f64>,
    menu_height: Cell<f64>,
    changed: RefCell<Option<Box<dyn FnMut()>>>,
    executor: ForegroundExecutor,
    scheduled: Cell<bool>,
}

impl FullscreenChrome {
    pub fn new(executor: ForegroundExecutor) -> Self {
        Self(Rc::new(State {
            reveal: Cell::new(0.),
            menu_height: Cell::new(0.),
            changed: RefCell::new(None),
            executor,
            scheduled: Cell::new(false),
        }))
    }

    pub unsafe fn attach(&self, window: id) {
        static CLASS: OnceLock<&Class> = OnceLock::new();
        let class = CLASS.get_or_init(|| unsafe {
            let mut class = ClassDecl::new(
                "GPUIFullscreenChrome",
                class!(NSTitlebarAccessoryViewController),
            )
            .unwrap();
            class.add_ivar::<*mut c_void>(STATE);
            class.add_method(
                sel!(setRevealAmount:),
                reveal_changed as extern "C" fn(&Object, Sel, f64),
            );
            class.add_method(sel!(dealloc), dealloc as extern "C" fn(&Object, Sel));
            class.register()
        });
        unsafe {
            let controller: id = msg_send![*class, new];
            (*controller).set_ivar(
                STATE,
                Box::into_raw(Box::new(self.0.clone())) as *mut c_void,
            );
            let view: id = msg_send![class!(NSView), new];
            let _: () = msg_send![controller, setView: view];
            let _: () = msg_send![view, release];
            let _: () = msg_send![controller, setHidden: YES];
            let _: () = msg_send![window, addTitlebarAccessoryViewController: controller];
            let _: () = msg_send![controller, release];
        }
    }

    pub fn on_change(&self, callback: Box<dyn FnMut()>) {
        *self.0.changed.borrow_mut() = Some(callback);
    }

    pub unsafe fn top_inset(&self, window: id, view: id) -> Pixels {
        unsafe {
            if !window
                .styleMask()
                .contains(NSWindowStyleMask::NSFullScreenWindowMask)
            {
                return px(0.);
            }
            let screen = window.screen();
            if screen == nil {
                return px(0.);
            }
            let menu: id = msg_send![NSApplication::sharedApplication(nil), mainMenu];
            // mainMenu is optional, including while restoring a fullscreen window
            // at startup. objc's msg_send! dereferences its receiver in Rust, so
            // Objective-C's usual nil-message behavior does not apply here.
            if menu != nil {
                let menu_height: f64 = msg_send![menu, menuBarHeight];
                if menu_height > 0. {
                    self.0.menu_height.set(menu_height);
                }
            }
            let content: NSRect = msg_send![view, convertRect: NSView::bounds(view) toView: nil];
            let content: NSRect = msg_send![window, convertRectToScreen: content];
            let screen_frame = NSScreen::frame(screen);
            // Notched displays and an always-visible menu can already reserve the menu area.
            let reserved = (screen_frame.origin.y + screen_frame.size.height
                - content.origin.y
                - content.size.height)
                .max(0.);
            let menu_overlap = (self.0.menu_height.get() - reserved).max(0.);
            let empty = NSRect::new(NSPoint::new(0., 0.), NSSize::new(0., 0.));
            let titled: NSRect = msg_send![class!(NSWindow), frameRectForContentRect: empty
                styleMask: NSWindowStyleMask::NSTitledWindowMask];
            px(((titled.size.height + menu_overlap) * self.0.reveal.get()) as f32)
        }
    }
}

extern "C" fn reveal_changed(this: &Object, _: Sel, amount: f64) {
    unsafe {
        // AppKit's contentLayoutRect remains unchanged during fullscreen reveal.
        // This undocumented accessory setter is also used by Firefox and Chromium
        // to follow the native animation: no public reveal-progress API exists.
        let _: () = msg_send![super(this, class!(NSTitlebarAccessoryViewController)), setRevealAmount: amount];
        let state = *this.get_ivar::<*mut c_void>(STATE) as *const Rc<State>;
        let Some(state) = state.as_ref() else { return };
        state.reveal.set(amount.clamp(0., 1.));
        if state.scheduled.replace(true) {
            return;
        }
        let weak = Rc::downgrade(state);
        // AppKit can call back while GPUI is updating a window. Defer notification
        // until that update completes, and coalesce changes within the same frame.
        state
            .executor
            .spawn(async move {
                if let Some(state) = weak.upgrade() {
                    state.scheduled.set(false);
                    if let Some(callback) = state.changed.borrow_mut().as_mut() {
                        callback();
                    }
                }
            })
            .detach();
    }
}

extern "C" fn dealloc(this: &Object, _: Sel) {
    unsafe {
        let state = *this.get_ivar::<*mut c_void>(STATE) as *mut Rc<State>;
        if !state.is_null() {
            drop(Box::from_raw(state));
        }
        let _: () = msg_send![
            super(this, class!(NSTitlebarAccessoryViewController)),
            dealloc
        ];
    }
}
