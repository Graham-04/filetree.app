use cocoa::{
    base::{id, nil},
    foundation::{NSPoint, NSRect},
};
use gpui::{Pixels, Window, px};
use objc::{msg_send, sel, sel_impl};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::ui::dp;

pub(super) struct Titlebar {
    pub height: Pixels,
    pub sidebar_top: Pixels,
    pub toggle_left: Pixels,
    pub toggle_top: Pixels,
    pub toggle_size: Pixels,
    pub controls_width: Pixels,
    /// Left edge of the update button, when it fits between the traffic
    /// lights and the sidebar toggle.
    pub update_left: Option<Pixels>,
    pub update_top: Pixels,
    pub update_width: Pixels,
    pub update_height: Pixels,
}

impl Titlebar {
    pub(super) fn layout(window: &Window, sidebar_width: f32) -> Self {
        let fullscreen = window.is_fullscreen();
        let height = dp(40.).to_pixels(window.rem_size());
        let height = if fullscreen {
            height
        } else {
            height.max(px(32.))
        };
        let toggle_size = dp(32.).to_pixels(window.rem_size());
        let gap = dp(4.).to_pixels(window.rem_size());
        let right_edge = if fullscreen {
            // The sidebar starts at the screen's top; its toggle moves into
            // the tab row instead of keeping an empty native-controls strip.
            px(sidebar_width)
        } else {
            align_traffic_lights(window, height)
        };
        let toggle_left = if fullscreen {
            right_edge + gap
        } else {
            (px(sidebar_width) - toggle_size - gap).max(right_edge + gap)
        };
        let update_width = dp(76.).to_pixels(window.rem_size());
        let update_height = dp(24.).to_pixels(window.rem_size());
        let update_left = toggle_left - gap - update_width;
        let update_fits = !fullscreen && update_left >= right_edge + gap * 2.;
        Self {
            height,
            sidebar_top: if fullscreen { px(0.) } else { height },
            toggle_left,
            toggle_top: (height - toggle_size) / 2.,
            toggle_size,
            controls_width: toggle_left + toggle_size + gap,
            update_left: update_fits.then_some(update_left),
            update_top: (height - update_height) / 2.,
            update_width,
            update_height,
        }
    }
}

fn align_traffic_lights(window: &Window, height: Pixels) -> Pixels {
    // GPUI's macOS backend always supplies a live AppKit NSView handle.
    let handle = HasWindowHandle::window_handle(window).expect("GPUI macOS window has a handle");
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        unreachable!("This application uses the macOS backend")
    };
    // SAFETY: run during render on the main thread. The native views are borrowed
    // from the live GPUI window; no pointers escape or change ownership.
    unsafe {
        let view = handle.ns_view.as_ptr() as id;
        let native_window: id = msg_send![view, window];
        let buttons: [id; 3] = [
            msg_send![native_window, standardWindowButton: 0_usize],
            msg_send![native_window, standardWindowButton: 1_usize],
            msg_send![native_window, standardWindowButton: 2_usize],
        ];
        // Titled GPUI windows have all three standard window controls.
        assert!(buttons.iter().all(|button| *button != nil));
        let close: NSRect = msg_send![buttons[0], frame];
        let minimize: NSRect = msg_send![buttons[1], frame];
        let spacing = minimize.origin.x - close.origin.x;
        let mut right_edge = 0.;
        for (index, button) in buttons.into_iter().enumerate() {
            let parent: id = msg_send![button, superview];
            let bounds: NSRect = msg_send![parent, bounds];
            let mut frame: NSRect = msg_send![button, frame];
            let origin = NSPoint::new(
                9. + spacing * index as f64,
                bounds.size.height - f64::from(f32::from(height)) / 2. - frame.size.height / 2.,
            );
            if frame.origin.x != origin.x || frame.origin.y != origin.y {
                frame.origin = origin;
                let _: () = msg_send![button, setFrame: frame];
            }
            right_edge = frame.origin.x + frame.size.width;
        }
        px(right_edge as f32)
    }
}
