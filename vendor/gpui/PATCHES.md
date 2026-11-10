# Presentation changes

## Native views and Metal frames

`Window::on_next_present` schedules native-view changes alongside the next GPUI
frame on macOS with Metal, so a native view can be shown or hidden in the same
frame as the GPUI content around it.

The renderer prepares the drawable and waits for its command buffer to be
scheduled. The platform window then applies the native changes and presents the
drawable in one Core Animation transaction with implicit animations disabled.
Native callbacks run outside the window mutex. If drawable preparation fails,
the updates stay pending and native visibility does not change.

Frames without native updates retain the ordinary asynchronous presentation path.

## Retained status animations

`StyledText::with_shimmer` records a sweep's start time and period alongside its
painted glyphs. `Window::paint_animated_path` records a geometry and color callback
inside fixed paint bounds. Presentation advances these scene operations without
invalidating views, laying out text, rebuilding hitboxes, or re-running native
WebKit coverage callbacks. Input and application changes still use normal draws.

Visible scene animations request presentation at up to 30 Hz while the window
is active. Removing them from the scene stops animation presentation. Cached paint
replay, batching, clipping, translated render targets, and foreground blur retain
the animation data. Glyph updates use the existing shader parameters and sprite
layout on all backends; no new shader or GPU buffer format is required.

Validation: the scene tests cover advancing cached glyphs and paths, translated
coordinates, foreground blur, clipped glyphs and removal. Paths reserve their full animation bounds for ordering, while each
presented frame uses its actual geometry bounds for gradients and compositing.

`Svg::with_rotation_animation` retains rotation timing alongside the sprite.
A shared Spinner uses it through `components::Icon`, preserving its SVG
assets and inherited colors. Rotation updates the existing transform matrix;
ordering and blur targets reserve the entire swept area. Tests cover rotation
through cached replay, translation, and foreground blur.

Passive pointer movement no longer extends the one-second fast-presentation
grace period. Hover changes still draw when invalidated; dragging, scrolling and
keyboard input retain the original grace period. This prevents a moving cursor
over unchanged content from continuously presenting the entire window at 120 Hz.

## Retained text reveals

`TextEffect::reveal` records a start time and duration for a finite blur/opacity
reveal. Presentation advances the foreground blur layer using the same easing as
the reveal's own timeline, without requesting layout or repainting the view.
The layer retains its maximum paint bounds throughout the effect. Delayed starts,
cached paint replay, and translated targets preserve the original timeline;
completed reveals stop requesting animation frames.

Validation covers delayed starts, intermediate and final paint values, stable
bounds, replay after completion, and translated layers.

## Subtree measurement

`Window::element_measurement` captures the current element namespace, rendering
view, and text style scopes. Its `measure` method measures a subtree in a separate
temporary layout tree, including inside Taffy's measurement callback. It returns
the unrounded size and restores the caller's scopes and layout tree. Painting
uses freshly constructed elements in the ordinary frame layout.

This lets a tall element reuse measurements of unchanged children at the same
width and build interactive elements only for the visible ones.

## Compact element identities

`GlobalElementId` owns a shared immutable path and its precomputed hash instead
of an inline 32-element buffer. State access, cached-frame replay, and inspector
paths clone the shared identifier. Draw-phase transitions no longer move that
large buffer, and state-map operations avoid hashing every ancestor repeatedly.
Equality still checks the full path, so hash collisions cannot mix element state.
The window's active namespace stack remains mutable and independent of captured
identifiers.

Validation exercises the public namespace/state APIs across sibling reordering,
removal and reinsertion, separately allocated equal names, and multiple state
types sharing one element.

## Native drags started inside a window

While a mouse button is held, the macOS window replays the last drag event every
16ms so content keeps scrolling during selections. When an app starts a native
AppKit drag from that press, AppKit takes over the mouse and the replay keeps
reporting the press's last position, alternating with the drag's real
positions; hover state under the pointer flickers. `dragging_entered` now ends
that replay, as a mouse release would, when a file drag enters the window.
