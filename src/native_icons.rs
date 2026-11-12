use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    ffi::c_void,
    future::ready,
    hash::{Hash, Hasher},
    os::unix::{ffi::OsStrExt, fs::PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::UNIX_EPOCH,
};

use block::ConcreteBlock;

use cocoa::{
    base::{YES, id, nil},
    foundation::{NSPoint, NSRect, NSSize, NSString},
};
use gpui::{App, Asset, ImageCacheError, ImageSource, Img, RenderImage, Window, img, prelude::*};
use objc::{class, msg_send, rc::StrongPtr, sel, sel_impl};

use crate::ui::dp;

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
enum Source {
    /// `on_accent` draws the symbol white, for use on an accent-filled button.
    Symbol {
        name: &'static str,
        on_accent: bool,
    },
    File(FileIcon),
}

/// A file's system icon, keyed by what decides its appearance so that files
/// of one type share a single raster. Folders and files of one extension look
/// alike; links, packages, and folders that can carry their own icon do not.
#[derive(Clone, Debug)]
pub(super) struct FileIcon {
    kind: IconKind,
    /// The file asked of macOS when this kind is first drawn. Shared, so rows
    /// clone it cheaply on every redraw.
    path: Arc<Path>,
    /// Set for files Quick Look can preview: their thumbnail replaces the
    /// type icon once it is made. Not part of the kind, so files still share
    /// their type icon until then.
    preview: Option<Preview>,
}

/// The version of a previewable file its thumbnail was made from, so an
/// edited file gets a new one.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
struct Preview {
    modified: u128,
    bytes: u64,
}

/// File types Finder shows a thumbnail for: images, PDFs and video.
fn previewable(extension: &str) -> bool {
    matches!(
        extension,
        "jpg"
            | "jpeg"
            | "png"
            | "gif"
            | "heic"
            | "heif"
            | "webp"
            | "avif"
            | "tif"
            | "tiff"
            | "bmp"
            | "psd"
            | "svg"
            | "dng"
            | "raw"
            | "cr2"
            | "cr3"
            | "nef"
            | "arw"
            | "orf"
            | "rw2"
            | "raf"
            | "pdf"
            | "mov"
            | "mp4"
            | "m4v"
            | "avi"
            | "mkv"
            | "3gp"
    )
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
enum IconKind {
    Own(Arc<Path>),
    Folder,
    File { extension: String, executable: bool },
}

impl FileIcon {
    /// Classifies `path` from its `symlink_metadata`, read by the caller, so
    /// listing a folder reads each entry once. Call off the UI thread: folders
    /// check for a custom icon file.
    pub(super) fn classify(path: &Path, metadata: Option<&std::fs::Metadata>) -> Self {
        let path: Arc<Path> = path.into();
        let mut preview = None;
        let kind = match metadata {
            Some(meta) if meta.is_dir() => {
                if path.extension().is_some() || has_own_folder_icon(&path) {
                    IconKind::Own(path.clone())
                } else {
                    IconKind::Folder
                }
            }
            Some(meta) if meta.is_file() => {
                let extension = path
                    .extension()
                    .map(|extension| extension.to_string_lossy().to_lowercase())
                    .unwrap_or_default();
                if previewable(&extension) {
                    preview = meta
                        .modified()
                        .ok()
                        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                        .map(|modified| Preview {
                            modified: modified.as_nanos(),
                            bytes: meta.len(),
                        });
                }
                IconKind::File {
                    executable: extension.is_empty() && meta.permissions().mode() & 0o111 != 0,
                    extension,
                }
            }
            // Symlinks get a badge, and anything unreadable is left to macOS.
            _ => IconKind::Own(path.clone()),
        };
        Self {
            kind,
            path,
            preview,
        }
    }
}

/// Folders at the top of the disk, home, or a volume include system folders
/// with distinct icons; any folder may carry a custom `Icon\r` file.
fn has_own_folder_icon(path: &Path) -> bool {
    let parent = path.parent().unwrap_or(path);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    parent == Path::new("/")
        || parent == Path::new("/Users")
        || parent == Path::new("/Volumes")
        || home.as_deref() == Some(parent)
        || home.as_deref() == Some(path)
        || path.join("Icon\r").exists()
}

impl PartialEq for FileIcon {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind
    }
}

impl Eq for FileIcon {}

impl Hash for FileIcon {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.kind.hash(state);
    }
}

pub(super) fn symbol(name: &'static str, size: f32) -> Img {
    icon(
        Source::Symbol {
            name,
            on_accent: false,
        },
        size,
    )
}

/// A white symbol for an accent-filled button.
pub(super) fn symbol_on_accent(name: &'static str, size: f32) -> Img {
    icon(
        Source::Symbol {
            name,
            on_accent: true,
        },
        size,
    )
}

/// A file's icon: its thumbnail when Quick Look can make one, and until then
/// (or if it cannot) the icon shared with every file of the same kind.
pub(super) fn file_icon(icon: FileIcon, size: f32) -> Img {
    let preview = icon.preview.map(|preview| (icon.path.clone(), preview));
    let source = Source::File(icon);
    let source = ImageSource::Custom(Arc::new(move |window, cx| {
        let (points, pixels) = raster_size(size, window);
        if let Some((path, preview)) = &preview {
            let key = (path.clone(), *preview, pixels);
            let thumbnail = window.use_asset::<Thumbnail>(&key, cx);
            shown(&key, thumbnail.as_ref());
            if let Some(Ok(thumbnail)) = thumbnail {
                return Some(Ok(thumbnail));
            }
        }
        window.use_asset::<IconRaster>(&(source.clone(), pixels, points.to_bits()), cx)
    }));
    img(source).size(dp(size)).flex_shrink_0()
}

fn icon(source: Source, size: f32) -> Img {
    let source = ImageSource::Custom(Arc::new(move |window, cx| {
        let (points, pixels) = raster_size(size, window);
        window.use_asset::<IconRaster>(&(source.clone(), pixels, points.to_bits()), cx)
    }));
    img(source).size(dp(size)).flex_shrink_0()
}

/// An icon's size in points, and the square of device pixels it needs.
fn raster_size(size: f32, window: &Window) -> (f32, u32) {
    let points = f32::from(dp(size).to_pixels(window.rem_size()));
    (
        points,
        (points * window.scale_factor()).ceil().max(1.) as u32,
    )
}

/// Bytes of type icons kept before all are released and the visible ones
/// redrawn: thousands at list size, hundreds at tile size.
const RASTER_BUDGET: usize = 24 << 20;

thread_local! {
    /// Every raster handed to GPUI. GPUI keeps cached assets and their GPU
    /// atlas tiles for the life of the app; this record lets them be released.
    static RASTERS: RefCell<Vec<(<IconRaster as Asset>::Source, Arc<RenderImage>)>> =
        const { RefCell::new(Vec::new()) };
    /// Device pixels per point the current rasters were drawn for.
    static RASTER_SCALE: Cell<f32> = const { Cell::new(0.) };
}

/// Bytes of thumbnails kept: 4 KB each in the list on a Retina display, 64 KB
/// as tiles. Past it, the ones shown longest ago are released.
const THUMBNAIL_BUDGET: usize = 32 << 20;
/// Thumbnails tracked, so files Quick Look cannot draw (which hold no image)
/// stay bounded too.
const THUMBNAIL_LIMIT: usize = 8192;

/// The bytes of a `pixels` square raster.
fn raster_bytes(pixels: u32) -> usize {
    pixels as usize * pixels as usize * 4
}

type ThumbnailKey = <Thumbnail as Asset>::Source;

/// One thumbnail GPUI holds as an asset.
struct ThumbnailSlot {
    /// The frame that last drew its file.
    shown: u64,
    state: ThumbnailState,
}

enum ThumbnailState {
    /// Asked of Quick Look: the request, kept so it can be cancelled.
    Requested(StrongPtr),
    /// Finished: the image, or `None` for a file Quick Look cannot draw.
    Made(Option<Arc<RenderImage>>),
}

thread_local! {
    /// Every thumbnail asked for and not yet released.
    static THUMBNAILS: RefCell<HashMap<ThumbnailKey, ThumbnailSlot>> =
        RefCell::new(HashMap::new());
    /// Counts frames, to tell which thumbnails the last one drew.
    static FRAME: Cell<u64> = const { Cell::new(0) };
}

/// Notes that this frame drew `key`'s file, and its thumbnail once made.
fn shown(key: &ThumbnailKey, thumbnail: Option<&<Thumbnail as Asset>::Output>) {
    THUMBNAILS.with_borrow_mut(|thumbnails| {
        let Some(slot) = thumbnails.get_mut(key) else {
            return;
        };
        slot.shown = FRAME.get();
        if let (ThumbnailState::Requested(_), Some(thumbnail)) = (&slot.state, thumbnail) {
            slot.state = ThumbnailState::Made(thumbnail.as_ref().ok().cloned());
        }
    });
}

/// Keeps icon and thumbnail memory bounded, and Quick Look working only on
/// what is on screen. Call once per frame before drawing.
///
/// After a zoom or display-scale change every raster is for an old size and
/// is released. Type icons past their budget are all released and the
/// visible ones redrawn on demand. Thumbnail requests for files the last
/// frame did not draw are cancelled, so scrolling past thousands of photos
/// leaves no queue ahead of the ones in view; finished thumbnails past their
/// budget are released oldest-shown first. The two budgets are separate, so
/// scrolling through photos never discards the type icons.
pub(super) fn release_stale_rasters(window: &mut Window, cx: &mut App) {
    let scale = f32::from(dp(1.).to_pixels(window.rem_size())) * window.scale_factor();
    let rescaled = RASTER_SCALE.replace(scale) != scale;
    let icon_bytes: usize = RASTERS.with_borrow(|rasters| {
        rasters
            .iter()
            .map(|((_, pixels, _), _)| raster_bytes(*pixels))
            .sum()
    });
    if rescaled || icon_bytes > RASTER_BUDGET {
        for (key, image) in RASTERS.take() {
            cx.remove_asset::<IconRaster>(&key);
            cx.drop_image(image, Some(window));
        }
    }

    let last_frame = FRAME.get();
    FRAME.set(last_frame + 1);
    let released: Vec<(ThumbnailKey, ThumbnailSlot)> = THUMBNAILS.with_borrow_mut(|thumbnails| {
        if rescaled {
            return thumbnails.drain().collect();
        }
        let mut release: Vec<ThumbnailKey> = thumbnails
            .iter()
            .filter(|(_, slot)| {
                matches!(slot.state, ThumbnailState::Requested(_)) && slot.shown < last_frame
            })
            .map(|(key, _)| key.clone())
            .collect();
        let made = |slot: &ThumbnailSlot| matches!(slot.state, ThumbnailState::Made(Some(_)));
        let mut bytes: usize = thumbnails
            .iter()
            .filter(|(_, slot)| made(slot))
            .map(|((_, _, pixels), _)| raster_bytes(*pixels))
            .sum();
        let mut count = thumbnails.len() - release.len();
        if bytes > THUMBNAIL_BUDGET || count > THUMBNAIL_LIMIT {
            // Down to three quarters, so this runs once per many new ones.
            let mut finished: Vec<(u64, &ThumbnailKey, bool)> = thumbnails
                .iter()
                .filter(|(_, slot)| matches!(slot.state, ThumbnailState::Made(_)))
                .map(|(key, slot)| (slot.shown, key, made(slot)))
                .collect();
            finished.sort_unstable_by_key(|(shown, ..)| *shown);
            let mut oldest = Vec::new();
            for (_, key, has_image) in finished {
                if bytes <= THUMBNAIL_BUDGET / 4 * 3 && count <= THUMBNAIL_LIMIT / 4 * 3 {
                    break;
                }
                if has_image {
                    bytes -= raster_bytes(key.2);
                }
                count -= 1;
                oldest.push(key.clone());
            }
            release.extend(oldest);
        }
        release
            .into_iter()
            .filter_map(|key| thumbnails.remove_entry(&key))
            .collect()
    });
    for (key, slot) in released {
        match slot.state {
            ThumbnailState::Requested(request) => cancel_thumbnail(&request),
            ThumbnailState::Made(Some(image)) => cx.drop_image(image, Some(window)),
            ThumbnailState::Made(None) => {}
        }
        // Forgotten by GPUI too, so the file asks again when next shown.
        cx.remove_asset::<Thumbnail>(&key);
    }
}

/// A file's Quick Look thumbnail at a pixel size, made off the main thread
/// from macOS's own thumbnail cache where it can.
struct Thumbnail;

impl Asset for Thumbnail {
    type Source = (Arc<Path>, Preview, u32);
    type Output = Result<Arc<RenderImage>, ImageCacheError>;

    fn load(
        (path, preview, pixels): Self::Source,
        _: &mut App,
    ) -> impl Future<Output = Self::Output> + Send + 'static {
        // On the main thread, where assets are asked for.
        let (done, made) = futures::channel::oneshot::channel();
        let state = match request_thumbnail(&path, pixels, done) {
            Some(request) => ThumbnailState::Requested(request),
            None => ThumbnailState::Made(None),
        };
        THUMBNAILS.with_borrow_mut(|thumbnails| {
            thumbnails.insert(
                (path, preview, pixels),
                ThumbnailSlot {
                    shown: FRAME.get(),
                    state,
                },
            );
        });
        async move {
            made.await
                .unwrap_or_else(|_| Err(anyhow::anyhow!("Quick Look dropped the request")))
                .map_err(|error| ImageCacheError::Other(Arc::new(error)))
        }
    }
}

#[link(name = "QuickLookThumbnailing", kind = "framework")]
unsafe extern "C" {}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGImageGetWidth(image: *const c_void) -> usize;
    fn CGImageGetHeight(image: *const c_void) -> usize;
    fn CGColorSpaceCreateDeviceRGB() -> *mut c_void;
    fn CGColorSpaceRelease(space: *mut c_void);
    fn CGBitmapContextCreate(
        data: *mut c_void,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        space: *mut c_void,
        bitmap_info: u32,
    ) -> *mut c_void;
    fn CGBitmapContextGetData(context: *mut c_void) -> *mut c_void;
    fn CGContextSetInterpolationQuality(context: *mut c_void, quality: i32);
    fn CGContextDrawImage(context: *mut c_void, rect: NSRect, image: *const c_void);
    fn CGContextRelease(context: *mut c_void);
}

type ThumbnailResult = anyhow::Result<Arc<RenderImage>>;

/// Asks Quick Look for `path`'s thumbnail, `pixels` square; `done` receives
/// it, drawn, from Quick Look's own queue. Only a real thumbnail is asked
/// for, so files it cannot preview fail fast and keep their type icon.
/// Returns the request, for cancelling, or `None` if it could not be made.
fn request_thumbnail(
    path: &Path,
    pixels: u32,
    done: futures::channel::oneshot::Sender<ThumbnailResult>,
) -> Option<StrongPtr> {
    const THUMBNAIL_ONLY: usize = 1 << 2;
    // SAFETY: Quick Look's generator is thread-safe. The pool releases the
    // autoreleased URL; the returned pointer owns the request, and the
    // copied block owns the only handle to `done`.
    unsafe {
        let _pool = StrongPtr::new(msg_send![class!(NSAutoreleasePool), new]);
        let manager: id = msg_send![class!(NSFileManager), defaultManager];
        let Some(url) = crate::file_ops::file_url(manager, path) else {
            let _ = done.send(Err(anyhow::anyhow!("Could not decode {}", path.display())));
            return None;
        };
        let request: id = msg_send![class!(QLThumbnailGenerationRequest), alloc];
        let request: id = msg_send![request,
            initWithFileAtURL: url
            size: NSSize::new(pixels as f64, pixels as f64)
            scale: 1_f64
            representationTypes: THUMBNAIL_ONLY
        ];
        if request == nil {
            let _ = done.send(Err(anyhow::anyhow!(
                "Quick Look refused {}",
                path.display()
            )));
            return None;
        }
        let request = StrongPtr::new(request);
        // Called once, on Quick Look's queue.
        let done = Mutex::new(Some(done));
        let block = ConcreteBlock::new(move |thumbnail: id, _error: id| {
            let drawn = draw_thumbnail(thumbnail, pixels);
            if let Some(done) = done.lock().unwrap_or_else(PoisonError::into_inner).take() {
                let _ = done.send(drawn);
            }
        })
        .copy();
        let generator: id = msg_send![class!(QLThumbnailGenerator), sharedGenerator];
        let _: () = msg_send![generator,
            generateBestRepresentationForRequest: *request
            completionHandler: &*block
        ];
        Some(request)
    }
}

/// Stops Quick Look working on a request whose file is no longer shown. Its
/// completion handler still runs, with no thumbnail.
fn cancel_thumbnail(request: &StrongPtr) {
    // SAFETY: the generator is thread-safe and `request` is alive; cancelling
    // a finished request does nothing.
    unsafe {
        let generator: id = msg_send![class!(QLThumbnailGenerator), sharedGenerator];
        let _: () = msg_send![generator, cancelRequest: **request];
    }
}

/// Draws a Quick Look thumbnail centered in a `pixels` square, keeping its
/// proportions, as Finder's list shows them.
fn draw_thumbnail(thumbnail: id, pixels: u32) -> ThumbnailResult {
    anyhow::ensure!(thumbnail != nil, "No thumbnail");
    // SAFETY: `thumbnail` is alive for this call, and so is its CGImage. The
    // bitmap context is created and released here, and its bytes are copied
    // out before then.
    unsafe {
        let image: *const c_void = msg_send![thumbnail, CGImage];
        anyhow::ensure!(!image.is_null(), "Thumbnail has no image");
        let (width, height) = (
            CGImageGetWidth(image) as f64,
            CGImageGetHeight(image) as f64,
        );
        anyhow::ensure!(width > 0. && height > 0., "Thumbnail has no size");
        const PREMULTIPLIED_LAST: u32 = 1;
        let side = pixels as usize;
        let space = CGColorSpaceCreateDeviceRGB();
        let context = CGBitmapContextCreate(
            std::ptr::null_mut(),
            side,
            side,
            8,
            side * 4,
            space,
            PREMULTIPLIED_LAST,
        );
        CGColorSpaceRelease(space);
        anyhow::ensure!(!context.is_null(), "Could not create thumbnail bitmap");
        const HIGH_QUALITY: i32 = 3;
        CGContextSetInterpolationQuality(context, HIGH_QUALITY);
        let scale = pixels as f64 / width.max(height);
        let (drawn_width, drawn_height) = (width * scale, height * scale);
        CGContextDrawImage(
            context,
            NSRect::new(
                NSPoint::new(
                    (pixels as f64 - drawn_width) / 2.,
                    (pixels as f64 - drawn_height) / 2.,
                ),
                NSSize::new(drawn_width, drawn_height),
            ),
            image,
        );
        let data = CGBitmapContextGetData(context) as *const u8;
        let bytes =
            (!data.is_null()).then(|| std::slice::from_raw_parts(data, side * side * 4).to_vec());
        CGContextRelease(context);
        render_image(
            bytes.ok_or_else(|| anyhow::anyhow!("Thumbnail bitmap has no pixels"))?,
            pixels,
        )
    }
}

// Cache by source and device resolution, shared by all tabs. Moving to another
// display requests the appropriate pixel size instead of scaling an old bitmap.
struct IconRaster;

impl Asset for IconRaster {
    type Source = (Source, u32, u32);
    type Output = Result<Arc<RenderImage>, ImageCacheError>;

    fn load(
        (source, pixels, points): Self::Source,
        _: &mut App,
    ) -> impl Future<Output = Self::Output> + Send + 'static {
        // Finish AppKit work on the main thread before returning the future.
        // Only the owned pixel buffer leaves this call.
        let raster = rasterize(&source, pixels, f32::from_bits(points));
        if let Ok(image) = &raster {
            RASTERS.with_borrow_mut(|rasters| {
                rasters.push(((source, pixels, points), image.clone()));
            });
        }
        ready(raster.map_err(|error| ImageCacheError::Other(Arc::new(error))))
    }
}

fn rasterize(source: &Source, pixels: u32, points: f32) -> anyhow::Result<Arc<RenderImage>> {
    // SAFETY: called on the application's main thread. The autorelease pool
    // owns temporary AppKit objects, and bitmap bytes are copied before it drains.
    // The bitmap is explicitly allocated as non-planar, 8-bit RGBA.
    unsafe {
        let _pool = StrongPtr::new(msg_send![class!(NSAutoreleasePool), new]);
        let image: id = match source {
            Source::Symbol { name, on_accent } => {
                let tint = match *name {
                    _ if *on_accent => Some(1_f64),
                    "xmark"
                    | "square.leftthird.inset.filled"
                    | "chevron.up"
                    | "chevron.down"
                    | "chevron.left"
                    | "chevron.right"
                    | "magnifyingglass"
                    | "plus"
                    | "list.bullet"
                    | "rectangle.grid.2x2"
                    | "lock"
                    | "eject" => Some(0.8_f64),
                    // Muted reads as off: a quiet gray rather than the blue
                    // of controls that are on.
                    "speaker.slash" => Some(0.45_f64),
                    // Help glyphs sit at the muted text tone.
                    "info.circle" => Some(0.67_f64),
                    _ => None,
                };
                // NSFontWeightLight keeps the sidebar toggle's outline to a
                // fine 1pt stroke.
                let weight = if *name == "square.leftthird.inset.filled" {
                    -0.4_f64
                } else {
                    0_f64
                };
                let name = StrongPtr::new(NSString::alloc(nil).init_str(name));
                let image: id = msg_send![class!(NSImage), imageWithSystemSymbolName: *name accessibilityDescription: nil];
                anyhow::ensure!(image != nil, "SF Symbol is unavailable");
                let sizing: id = msg_send![class!(NSImageSymbolConfiguration), configurationWithPointSize: points as f64 weight: weight];
                let color: id = match tint {
                    Some(white) => {
                        msg_send![class!(NSColor), colorWithCalibratedWhite: white alpha: 1_f64]
                    }
                    None => msg_send![class!(NSColor), systemBlueColor],
                };
                let colors: id = msg_send![class!(NSArray), arrayWithObject: color];
                let palette: id = msg_send![class!(NSImageSymbolConfiguration), configurationWithPaletteColors: colors];
                let config: id = msg_send![sizing, configurationByApplyingConfiguration: palette];
                let image: id = msg_send![image, imageWithSymbolConfiguration: config];
                anyhow::ensure!(image != nil, "Could not configure SF Symbol");
                image
            }
            Source::File(FileIcon { path, .. }) => {
                // Let Foundation decode the filesystem path, including paths
                // whose bytes are not UTF-8. No lossy filename conversion here.
                let bytes = path.as_os_str().as_bytes();
                let manager: id = msg_send![class!(NSFileManager), defaultManager];
                let native_path: id = msg_send![manager,
                    stringWithFileSystemRepresentation: bytes.as_ptr().cast::<std::ffi::c_char>()
                    length: bytes.len()
                ];
                anyhow::ensure!(
                    native_path != nil,
                    "Could not decode icon path {}",
                    path.display()
                );
                let workspace: id = msg_send![class!(NSWorkspace), sharedWorkspace];
                let image: id = msg_send![workspace, iconForFile: native_path];
                anyhow::ensure!(image != nil, "No system icon for {}", path.display());
                image
            }
        };

        let space = StrongPtr::new(NSString::alloc(nil).init_str("NSDeviceRGBColorSpace"));
        let bitmap: id = msg_send![class!(NSBitmapImageRep), alloc];
        let bitmap = StrongPtr::new(msg_send![bitmap,
            initWithBitmapDataPlanes: std::ptr::null_mut::<*mut u8>()
            pixelsWide: pixels as isize pixelsHigh: pixels as isize
            bitsPerSample: 8_isize samplesPerPixel: 4_isize
            hasAlpha: YES isPlanar: cocoa::base::NO colorSpaceName: *space
            bitmapFormat: 0_usize bytesPerRow: (pixels as isize * 4) bitsPerPixel: 32_isize
        ]);
        anyhow::ensure!(*bitmap != nil, "Could not allocate icon bitmap");
        let data: *mut u8 = msg_send![*bitmap, bitmapData];
        anyhow::ensure!(!data.is_null(), "Icon bitmap has no pixel buffer");
        let length = pixels as usize * pixels as usize * 4;
        std::ptr::write_bytes(data, 0, length);
        let context: id =
            msg_send![class!(NSGraphicsContext), graphicsContextWithBitmapImageRep: *bitmap];
        anyhow::ensure!(context != nil, "Could not create icon graphics context");
        let size: NSSize = msg_send![image, size];
        anyhow::ensure!(
            size.width > 0. && size.height > 0.,
            "Native icon has no size"
        );
        let scale = pixels as f64 / size.width.max(size.height);
        let width = size.width * scale;
        let height = size.height * scale;
        let rect = NSRect::new(
            NSPoint::new((pixels as f64 - width) / 2., (pixels as f64 - height) / 2.),
            NSSize::new(width, height),
        );
        let _: () = msg_send![class!(NSGraphicsContext), saveGraphicsState];
        let _: () = msg_send![class!(NSGraphicsContext), setCurrentContext: context];
        let _: () = msg_send![image, drawInRect: rect fromRect: NSRect::new(NSPoint::new(0., 0.), size) operation: 2_usize fraction: 1_f64];
        let _: () = msg_send![class!(NSGraphicsContext), restoreGraphicsState];

        render_image(std::slice::from_raw_parts(data, length).to_vec(), pixels)
    }
}

/// A `pixels` square of premultiplied RGBA, as AppKit and CoreGraphics draw
/// it, converted to the straight BGRA GPUI expects.
fn render_image(mut bytes: Vec<u8>, pixels: u32) -> anyhow::Result<Arc<RenderImage>> {
    for pixel in bytes.chunks_exact_mut(4) {
        let alpha = pixel[3] as u32;
        if alpha != 0 {
            for channel in &mut pixel[..3] {
                *channel = ((*channel as u32 * 255 + alpha / 2) / alpha).min(255) as u8;
            }
        }
        pixel.swap(0, 2);
    }
    let buffer = image::RgbaImage::from_raw(pixels, pixels, bytes)
        .ok_or_else(|| anyhow::anyhow!("Invalid icon bitmap dimensions"))?;
    Ok(Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])))
}
