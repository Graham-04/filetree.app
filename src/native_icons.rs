use std::{
    cell::{Cell, RefCell},
    future::ready,
    hash::{Hash, Hasher},
    os::unix::{ffi::OsStrExt, fs::PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};

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
                IconKind::File {
                    executable: extension.is_empty() && meta.permissions().mode() & 0o111 != 0,
                    extension,
                }
            }
            // Symlinks get a badge, and anything unreadable is left to macOS.
            _ => IconKind::Own(path.clone()),
        };
        Self { kind, path }
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

/// A classified file icon, shared with every file of the same kind.
pub(super) fn file_icon(icon_kind: FileIcon, size: f32) -> Img {
    icon(Source::File(icon_kind), size)
}

fn icon(source: Source, size: f32) -> Img {
    let source = ImageSource::Custom(Arc::new(move |window, cx| {
        let points = f32::from(dp(size).to_pixels(window.rem_size()));
        let pixels = (points * window.scale_factor()).ceil().max(1.) as u32;
        window.use_asset::<IconRaster>(&(source.clone(), pixels, points.to_bits()), cx)
    }));
    img(source).size(dp(size)).flex_shrink_0()
}

/// Rasters kept before all are released and the visible ones redrawn.
const RASTER_BUDGET: usize = 1024;

thread_local! {
    /// Every raster handed to GPUI. GPUI keeps cached assets and their GPU
    /// atlas tiles for the life of the app; this record lets them be released.
    static RASTERS: RefCell<Vec<(<IconRaster as Asset>::Source, Arc<RenderImage>)>> =
        const { RefCell::new(Vec::new()) };
    /// Device pixels per point the current rasters were drawn for.
    static RASTER_SCALE: Cell<f32> = const { Cell::new(0.) };
}

/// Keeps icon memory bounded. Call once per frame before drawing: after a
/// zoom or display-scale change every raster is for an old size, and past
/// the budget the ones still visible are simply redrawn on demand.
pub(super) fn release_stale_rasters(window: &mut Window, cx: &mut App) {
    let scale = f32::from(dp(1.).to_pixels(window.rem_size())) * window.scale_factor();
    let rescaled = RASTER_SCALE.replace(scale) != scale;
    let over_budget = RASTERS.with_borrow(|rasters| rasters.len() > RASTER_BUDGET);
    if !rescaled && !over_budget {
        return;
    }
    for (key, image) in RASTERS.take() {
        cx.remove_asset::<IconRaster>(&key);
        cx.drop_image(image, Some(window));
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

        let mut bytes = std::slice::from_raw_parts(data, length).to_vec();
        // AppKit draws premultiplied RGBA; GPUI expects straight BGRA.
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
}
