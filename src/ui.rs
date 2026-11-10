use std::time::{Duration, Instant};

use gpui::{Rems, Window, px, rems};

const BASE_REM: f32 = 16.;

pub(super) mod text_size {
    pub const DETAIL: f32 = 12.;
    pub const BODY: f32 = 14.;
    pub const INPUT: f32 = 15.;
}

pub(super) mod row {
    pub const HEIGHT: f32 = 28.;
    pub const ICON: f32 = 16.;
    pub const GAP: f32 = 8.;
}

pub(super) mod radius {
    pub const XS: f32 = 4.;
    pub const SM: f32 = 6.;
    pub const MD: f32 = 8.;
    pub const LG: f32 = 12.;
}

/// The system alert sound, for an action that could not be done.
pub(super) fn beep() {
    #[link(name = "AppKit", kind = "framework")]
    unsafe extern "C" {
        fn NSBeep();
    }
    // SAFETY: NSBeep takes no arguments and only plays the alert sound.
    unsafe { NSBeep() }
}

/// macOS system sounds for file actions.
#[derive(Clone, Copy)]
pub(super) enum Sound {
    /// The Dock's drag-to-Trash sound, for Move to Trash.
    Trash,
    /// The Dock's “poof”, for files moved or copied by a drop.
    Move,
}

impl Sound {
    fn file(self) -> &'static str {
        match self {
            Self::Trash => "dock/drag to trash.aif",
            Self::Move => "dock/poof item off dock.aif",
        }
    }
}

/// Plays `sound` when "Play user interface sound effects" is on in System
/// Settings › Sound. Each sound is loaded once and kept, since a sound must
/// stay alive while it plays; playing it again restarts it.
pub(super) fn play(sound: Sound) {
    use cocoa::{
        base::{id, nil},
        foundation::NSString,
    };
    use objc::{class, msg_send, sel, sel_impl};

    const FOLDER: &str =
        "/System/Library/Components/CoreAudio.component/Contents/SharedSupport/SystemSounds";
    thread_local! {
        static LOADED: std::cell::RefCell<Vec<(&'static str, id)>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }
    // SAFETY: main thread. The defaults and sound objects are owned by
    // Foundation and AppKit; loaded sounds are retained for the app's life.
    unsafe {
        let defaults: id = msg_send![class!(NSUserDefaults), standardUserDefaults];
        let key = NSString::alloc(nil).init_str("com.apple.sound.uiaudio.enabled");
        let setting: id = msg_send![defaults, objectForKey: key];
        let _: () = msg_send![key, release];
        // Unset means the macOS default, which is on.
        if setting != nil {
            let enabled: bool = msg_send![setting, boolValue];
            if !enabled {
                return;
            }
        }
        let file = sound.file();
        let loaded = LOADED.with_borrow(|loaded| {
            loaded
                .iter()
                .find(|(name, _)| *name == file)
                .map(|(_, sound)| *sound)
        });
        let sound = match loaded {
            Some(sound) => sound,
            None => {
                let path = NSString::alloc(nil).init_str(&format!("{FOLDER}/{file}"));
                let sound: id = msg_send![class!(NSSound), alloc];
                let sound: id = msg_send![sound, initWithContentsOfFile: path byReference: true];
                let _: () = msg_send![path, release];
                if sound.is_null() {
                    return;
                }
                LOADED.with_borrow_mut(|loaded| loaded.push((file, sound)));
                sound
            }
        };
        let _: bool = msg_send![sound, stop];
        let _: bool = msg_send![sound, play];
    }
}

/// The checkbox for every boolean setting, so a setting looks the same in the
/// command menu and the settings panel.
pub(super) fn checkbox(checked: bool) -> gpui::Div {
    use gpui::{div, prelude::*, rgb};
    div()
        .flex_shrink_0()
        .size(dp(16.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(dp(radius::XS))
        .when(checked, |checkbox| {
            checkbox
                .bg(rgb(0x0a84ff))
                .child(crate::native_icons::symbol_on_accent("checkmark", 10.))
        })
        .when(!checked, |checkbox| {
            checkbox.border_1().border_color(rgb(0x5a5a5a))
        })
}

/// Builds a tooltip showing `text`, for `StatefulInteractiveElement::tooltip`.
pub(super) fn tooltip(
    text: &'static str,
) -> impl Fn(&mut Window, &mut gpui::App) -> gpui::AnyView + 'static {
    move |_, cx| gpui::AppContext::new(cx, |_| Tooltip(text)).into()
}

/// A small elevated note that wraps to a readable width.
struct Tooltip(&'static str);

impl gpui::Render for Tooltip {
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl gpui::IntoElement {
        use gpui::{div, prelude::*, rgb};
        div()
            .max_w(dp(260.))
            .px(dp(10.))
            .py(dp(6.))
            .rounded(dp(radius::SM))
            .bg(rgb(0x333333))
            .border_1()
            .border_color(rgb(0x404040))
            .shadow_md()
            .font_family(".SystemUIFont")
            .text_size(dp(text_size::DETAIL))
            .line_height(dp(17.))
            .text_color(rgb(0xeeeeee))
            .child(self.0)
    }
}

/// The system font with tabular numerals, for figures that update in place or
/// line up in a column.
pub(super) fn tabular_font() -> gpui::Font {
    let mut font = gpui::font(".SystemUIFont");
    font.features = gpui::FontFeatures(std::sync::Arc::new(vec![("tnum".into(), 1)]));
    font
}

/// All UI dimensions are points at 100%, expressed relative to the window rem.
pub(super) fn dp(points: f32) -> Rems {
    rems(points / BASE_REM)
}

pub(super) struct Zoom {
    rem_size: f32,
}

impl Zoom {
    /// A zoom from a saved rem size, or the default; out-of-range values clamp.
    pub(super) fn restore(rem_size: Option<f32>) -> Self {
        Self {
            rem_size: rem_size.unwrap_or(BASE_REM).clamp(8., 40.),
        }
    }

    /// The rem size to save, or `None` at the default zoom.
    pub(super) fn saved(&self) -> Option<f32> {
        (self.rem_size != BASE_REM).then_some(self.rem_size)
    }

    pub(super) fn change(&mut self, delta: f32) {
        self.rem_size = (self.rem_size + delta).clamp(8., 40.);
    }

    pub(super) fn reset(&mut self) {
        self.rem_size = BASE_REM;
    }

    pub(super) fn apply(&self, window: &mut Window) {
        window.set_rem_size(px(self.rem_size));
    }
}

/// An eased 0–1 open amount that reverses smoothly from wherever it is.
pub(super) struct Reveal {
    open: bool,
    from: f32,
    since: Instant,
}

impl Reveal {
    const DURATION: Duration = Duration::from_millis(140);

    pub(super) fn new(open: bool) -> Self {
        Self {
            open,
            from: if open { 1. } else { 0. },
            since: Instant::now() - Self::DURATION,
        }
    }

    pub(super) fn is_open(&self) -> bool {
        self.open
    }

    pub(super) fn set(&mut self, open: bool, now: Instant) {
        if open != self.open {
            self.from = self.amount_at(now);
            self.open = open;
            self.since = now;
        }
    }

    /// The current amount; keeps frames coming only while it is moving.
    pub(super) fn amount(&self, window: &Window) -> f32 {
        let now = Instant::now();
        if now.duration_since(self.since) < Self::DURATION {
            window.request_animation_frame();
        }
        self.amount_at(now)
    }

    fn amount_at(&self, now: Instant) -> f32 {
        let progress =
            (now.duration_since(self.since).as_secs_f32() / Self::DURATION.as_secs_f32()).min(1.);
        let target = if self.open { 1. } else { 0. };
        self.from + (target - self.from) * gpui::ease_out_quint()(progress)
    }
}
