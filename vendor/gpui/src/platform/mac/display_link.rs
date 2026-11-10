use cocoa::base::id;
use objc::{
    class,
    declare::ClassDecl,
    msg_send,
    rc::StrongPtr,
    runtime::{Class, Object, Sel},
    sel, sel_impl,
};
use std::{ffi::c_void, sync::OnceLock};

/// A main-run-loop display link. Invalidation removes pending callbacks and
/// releases the target before the window's callback context can be destroyed.
/// StrongPtr also keeps this owner confined to the main thread.
pub struct DisplayLink {
    link: StrongPtr,
}

#[derive(Clone, Copy)]
struct Callback {
    data: *mut c_void,
    invoke: unsafe extern "C" fn(*mut c_void),
}

impl DisplayLink {
    /// The view and callback context must remain valid until this link is dropped.
    /// Construction, callbacks, and destruction must all occur on the main thread.
    pub unsafe fn new(view: id, callback: unsafe extern "C" fn(*mut c_void)) -> Self {
        static CLASS: OnceLock<&Class> = OnceLock::new();
        let class = CLASS.get_or_init(|| unsafe {
            let mut class = ClassDecl::new("GPUIDisplayLinkTarget", class!(NSObject)).unwrap();
            class.add_ivar::<*mut c_void>("callback");
            class.add_method(sel!(step:), step as extern "C" fn(&Object, Sel, id));
            class.add_method(sel!(dealloc), dealloc as extern "C" fn(&mut Object, Sel));
            class.register()
        });
        unsafe {
            let target = StrongPtr::new(msg_send![*class, new]);
            (**target).set_ivar(
                "callback",
                Box::into_raw(Box::new(Callback {
                    data: view as *mut c_void,
                    invoke: callback,
                })) as *mut c_void,
            );
            let link: id = msg_send![view, displayLinkWithTarget: *target selector: sel!(step:)];
            let link = StrongPtr::retain(link);
            let run_loop: id = msg_send![class!(NSRunLoop), mainRunLoop];
            // Use the system constant so resizing and other tracking modes keep rendering.
            unsafe extern "C" {
                static NSRunLoopCommonModes: id;
            }
            let _: () = msg_send![*link, addToRunLoop: run_loop forMode: NSRunLoopCommonModes];
            Self { link }
        }
    }
}

impl Drop for DisplayLink {
    fn drop(&mut self) {
        unsafe {
            let _: () = msg_send![*self.link, invalidate];
        }
    }
}

extern "C" fn step(target: &Object, _: Sel, _: id) {
    unsafe {
        // A frame can close its own window and invalidate the link. Keep the
        // target alive until that callback returns, even if invalidation releases it.
        let _target = StrongPtr::retain(target as *const Object as id);
        let callback = *(*target.get_ivar::<*mut c_void>("callback") as *const Callback);
        (callback.invoke)(callback.data);
    }
}

extern "C" fn dealloc(target: &mut Object, _: Sel) {
    unsafe {
        let callback = *target.get_ivar::<*mut c_void>("callback") as *mut Callback;
        if !callback.is_null() {
            drop(Box::from_raw(callback));
        }
        let _: () = msg_send![super(target, class!(NSObject)), dealloc];
    }
}
