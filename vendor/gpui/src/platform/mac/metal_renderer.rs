use super::metal_atlas::MetalAtlas;
use crate::{
    AtlasTextureId, Background, BlurLayer, BlurSource, Bounds, ContentMask, DevicePixels,
    MonochromeSprite, PaintSurface, Path, Point, PolychromeSprite, PrimitiveBatch, Quad,
    ScaledPixels, Scene, Shadow, Size, Underline, point, size,
};
use anyhow::Result;
use block::ConcreteBlock;
use cocoa::{
    base::{NO, YES},
    foundation::{NSSize, NSUInteger},
    quartzcore::AutoresizingMask,
};

use core_foundation::base::TCFType;
use core_video::{
    metal_texture::{CVMetalTexture, CVMetalTextureGetTexture},
    metal_texture_cache::CVMetalTextureCache,
    pixel_buffer::{
        CVPixelBuffer, kCVPixelFormatType_32BGRA, kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
    },
};
use foreign_types::{ForeignType, ForeignTypeRef};
use metal::{
    CAMetalLayer, CommandQueue, MTLPixelFormat, MTLResourceOptions, NSRange,
    RenderPassColorAttachmentDescriptorRef,
};
use objc::{self, msg_send, sel, sel_impl};
use parking_lot::Mutex;

use std::{cell::Cell, ffi::c_void, mem, ptr, sync::Arc};

// Exported to metal
pub(crate) type PointF = crate::Point<f32>;

#[cfg(not(feature = "runtime_shaders"))]
const SHADERS_METALLIB: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/shaders.metallib"));
#[cfg(feature = "runtime_shaders")]
const SHADERS_SOURCE_FILE: &str = include_str!(concat!(env!("OUT_DIR"), "/stitched_shaders.metal"));
// Use 4x MSAA, all devices support it.
// https://developer.apple.com/documentation/metal/mtldevice/1433355-supportstexturesamplecount
const PATH_SAMPLE_COUNT: u32 = 4;

pub type Context = Arc<Mutex<InstanceBufferPool>>;
pub type Renderer = MetalRenderer;

pub unsafe fn new_renderer(
    context: self::Context,
    _native_window: *mut c_void,
    _native_view: *mut c_void,
    _bounds: crate::Size<f32>,
    _transparent: bool,
) -> Renderer {
    MetalRenderer::new(context)
}

pub(crate) struct InstanceBufferPool {
    buffer_size: usize,
    buffers: Vec<metal::Buffer>,
}

impl Default for InstanceBufferPool {
    fn default() -> Self {
        Self {
            buffer_size: 2 * 1024 * 1024,
            buffers: Vec::new(),
        }
    }
}

pub(crate) struct InstanceBuffer {
    metal_buffer: metal::Buffer,
    size: usize,
}

impl InstanceBufferPool {
    pub(crate) fn reset(&mut self, buffer_size: usize) {
        self.buffer_size = buffer_size;
        self.buffers.clear();
    }

    pub(crate) fn acquire(&mut self, device: &metal::Device) -> InstanceBuffer {
        let buffer = self.buffers.pop().unwrap_or_else(|| {
            device.new_buffer(
                self.buffer_size as u64,
                MTLResourceOptions::StorageModeManaged,
            )
        });
        InstanceBuffer {
            metal_buffer: buffer,
            size: self.buffer_size,
        }
    }

    pub(crate) fn release(&mut self, buffer: InstanceBuffer) {
        if buffer.size == self.buffer_size {
            self.buffers.push(buffer.metal_buffer)
        }
    }
}

pub(crate) struct MetalRenderer {
    device: metal::Device,
    layer: metal::MetalLayer,
    presents_with_transaction: bool,
    frame_monitor: std::sync::Weak<crate::FrameMonitor>,
    command_queue: CommandQueue,
    paths_rasterization_pipeline_state: metal::RenderPipelineState,
    path_sprites_pipeline_state: metal::RenderPipelineState,
    shadows_pipeline_state: metal::RenderPipelineState,
    quads_pipeline_state: metal::RenderPipelineState,
    underlines_pipeline_state: metal::RenderPipelineState,
    monochrome_sprites_pipeline_state: metal::RenderPipelineState,
    polychrome_sprites_pipeline_state: metal::RenderPipelineState,
    surfaces_pipeline_state: metal::RenderPipelineState,
    blur_pipeline_state: metal::ComputePipelineState,
    backdrop_sample_pipeline_state: metal::ComputePipelineState,
    blur_composite_pipeline_state: metal::RenderPipelineState,
    backdrop_composite_pipeline_state: metal::RenderPipelineState,
    blur_textures: Vec<[metal::Texture; 2]>,
    unit_vertices: metal::Buffer,
    #[allow(clippy::arc_with_non_send_sync)]
    instance_buffer_pool: Arc<Mutex<InstanceBufferPool>>,
    sprite_atlas: Arc<MetalAtlas>,
    core_video_texture_cache: core_video::metal_texture_cache::CVMetalTextureCache,
    path_intermediate_texture: Option<metal::Texture>,
    path_intermediate_msaa_texture: Option<metal::Texture>,
    path_intermediates_used: bool,
    path_sample_count: u32,
}

#[repr(C)]
pub struct PathRasterizationVertex {
    pub xy_position: Point<ScaledPixels>,
    pub st_position: Point<f32>,
    pub color: Background,
    pub bounds: Bounds<ScaledPixels>,
}

impl MetalRenderer {
    pub fn new(instance_buffer_pool: Arc<Mutex<InstanceBufferPool>>) -> Self {
        // Prefer low‐power integrated GPUs on Intel Mac. On Apple
        // Silicon, there is only ever one GPU, so this is equivalent to
        // `metal::Device::system_default()`.
        let mut devices = metal::Device::all();
        devices.sort_by_key(|device| (device.is_removable(), device.is_low_power()));
        let Some(device) = devices.pop() else {
            log::error!("unable to access a compatible graphics device");
            std::process::exit(1);
        };

        let layer = metal::MetalLayer::new();
        layer.set_device(&device);
        layer.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
        layer.set_opaque(false);
        layer.set_framebuffer_only(false);
        layer.set_maximum_drawable_count(3);
        unsafe {
            let _: () = msg_send![&*layer, setAllowsNextDrawableTimeout: NO];
            let _: () = msg_send![&*layer, setNeedsDisplayOnBoundsChange: YES];
            let _: () = msg_send![
                &*layer,
                setAutoresizingMask: AutoresizingMask::WIDTH_SIZABLE
                    | AutoresizingMask::HEIGHT_SIZABLE
            ];
        }
        #[cfg(feature = "runtime_shaders")]
        let library = device
            .new_library_with_source(&SHADERS_SOURCE_FILE, &metal::CompileOptions::new())
            .expect("error building metal library");
        #[cfg(not(feature = "runtime_shaders"))]
        let library = device
            .new_library_with_data(SHADERS_METALLIB)
            .expect("error building metal library");

        fn to_float2_bits(point: PointF) -> u64 {
            let mut output = point.y.to_bits() as u64;
            output <<= 32;
            output |= point.x.to_bits() as u64;
            output
        }

        let unit_vertices = [
            to_float2_bits(point(0., 0.)),
            to_float2_bits(point(1., 0.)),
            to_float2_bits(point(0., 1.)),
            to_float2_bits(point(0., 1.)),
            to_float2_bits(point(1., 0.)),
            to_float2_bits(point(1., 1.)),
        ];
        let unit_vertices = device.new_buffer_with_data(
            unit_vertices.as_ptr() as *const c_void,
            mem::size_of_val(&unit_vertices) as u64,
            MTLResourceOptions::StorageModeManaged,
        );

        let paths_rasterization_pipeline_state = build_path_rasterization_pipeline_state(
            &device,
            &library,
            "paths_rasterization",
            "path_rasterization_vertex",
            "path_rasterization_fragment",
            MTLPixelFormat::BGRA8Unorm,
            PATH_SAMPLE_COUNT,
        );
        let path_sprites_pipeline_state = build_path_sprite_pipeline_state(
            &device,
            &library,
            "path_sprites",
            "path_sprite_vertex",
            "path_sprite_fragment",
            MTLPixelFormat::BGRA8Unorm,
            false,
        );
        let shadows_pipeline_state = build_pipeline_state(
            &device,
            &library,
            "shadows",
            "shadow_vertex",
            "shadow_fragment",
            MTLPixelFormat::BGRA8Unorm,
        );
        let quads_pipeline_state = build_pipeline_state(
            &device,
            &library,
            "quads",
            "quad_vertex",
            "quad_fragment",
            MTLPixelFormat::BGRA8Unorm,
        );
        let underlines_pipeline_state = build_pipeline_state(
            &device,
            &library,
            "underlines",
            "underline_vertex",
            "underline_fragment",
            MTLPixelFormat::BGRA8Unorm,
        );
        let monochrome_sprites_pipeline_state = build_pipeline_state(
            &device,
            &library,
            "monochrome_sprites",
            "monochrome_sprite_vertex",
            "monochrome_sprite_fragment",
            MTLPixelFormat::BGRA8Unorm,
        );
        let polychrome_sprites_pipeline_state = build_pipeline_state(
            &device,
            &library,
            "polychrome_sprites",
            "polychrome_sprite_vertex",
            "polychrome_sprite_fragment",
            MTLPixelFormat::BGRA8Unorm,
        );
        let surfaces_pipeline_state = build_pipeline_state(
            &device,
            &library,
            "surfaces",
            "surface_vertex",
            "surface_fragment",
            MTLPixelFormat::BGRA8Unorm,
        );

        let command_queue = device.new_command_queue();
        let blur_pipeline_state = device
            .new_compute_pipeline_state_with_function(
                &library
                    .get_function("gaussian_blur", None)
                    .expect("missing Gaussian shader"),
            )
            .expect("could not create Gaussian pipeline");
        let blur_composite_pipeline_state = build_path_sprite_pipeline_state(
            &device,
            &library,
            "blur_composite",
            "blur_composite_vertex",
            "blur_composite_fragment",
            MTLPixelFormat::BGRA8Unorm,
            false,
        );
        let backdrop_sample_pipeline_state = device
            .new_compute_pipeline_state_with_function(
                &library
                    .get_function("backdrop_sample", None)
                    .expect("missing backdrop shader"),
            )
            .expect("could not create backdrop pipeline");
        let backdrop_composite_pipeline_state = build_path_sprite_pipeline_state(
            &device,
            &library,
            "backdrop_composite",
            "blur_composite_vertex",
            "backdrop_composite_fragment",
            MTLPixelFormat::BGRA8Unorm,
            true,
        );
        let sprite_atlas = Arc::new(MetalAtlas::new(device.clone()));
        let core_video_texture_cache =
            CVMetalTextureCache::new(None, device.clone(), None).unwrap();

        Self {
            device,
            layer,
            presents_with_transaction: false,
            frame_monitor: std::sync::Weak::new(),
            command_queue,
            paths_rasterization_pipeline_state,
            path_sprites_pipeline_state,
            shadows_pipeline_state,
            quads_pipeline_state,
            underlines_pipeline_state,
            monochrome_sprites_pipeline_state,
            polychrome_sprites_pipeline_state,
            surfaces_pipeline_state,
            blur_pipeline_state,
            backdrop_sample_pipeline_state,
            blur_composite_pipeline_state,
            backdrop_composite_pipeline_state,
            blur_textures: Vec::new(),
            unit_vertices,
            instance_buffer_pool,
            sprite_atlas,
            core_video_texture_cache,
            path_intermediate_texture: None,
            path_intermediate_msaa_texture: None,
            path_intermediates_used: false,
            path_sample_count: PATH_SAMPLE_COUNT,
        }
    }

    pub fn layer(&self) -> &metal::MetalLayerRef {
        &self.layer
    }

    pub fn layer_ptr(&self) -> *mut CAMetalLayer {
        self.layer.as_ptr()
    }

    pub fn sprite_atlas(&self) -> &Arc<MetalAtlas> {
        &self.sprite_atlas
    }

    pub fn set_presents_with_transaction(&mut self, presents_with_transaction: bool) {
        self.presents_with_transaction = presents_with_transaction;
        self.layer
            .set_presents_with_transaction(presents_with_transaction);
    }

    pub fn update_drawable_size(&mut self, size: Size<DevicePixels>) {
        let size = NSSize {
            width: size.width.0 as f64,
            height: size.height.0 as f64,
        };
        unsafe {
            let _: () = msg_send![
                self.layer(),
                setDrawableSize: size
            ];
        }
        // Allocate scratch targets only when a path batch is actually drawn.
        // Windows containing only text, sprites and quads do not need them.
        self.path_intermediate_texture = None;
        self.path_intermediate_msaa_texture = None;
    }

    fn update_path_intermediate_textures(&mut self, size: Size<DevicePixels>) {
        // We are uncertain when this happens, but sometimes size can be 0 here. Most likely before
        // the layout pass on window creation. Zero-sized texture creation causes SIGABRT.
        // https://github.com/zed-industries/zed/issues/36229
        if size.width.0 <= 0 || size.height.0 <= 0 {
            self.path_intermediate_texture = None;
            self.path_intermediate_msaa_texture = None;
            return;
        }

        let texture_descriptor = metal::TextureDescriptor::new();
        texture_descriptor.set_width(size.width.0 as u64);
        texture_descriptor.set_height(size.height.0 as u64);
        texture_descriptor.set_pixel_format(metal::MTLPixelFormat::BGRA8Unorm);
        texture_descriptor.set_storage_mode(metal::MTLStorageMode::Private);
        texture_descriptor
            .set_usage(metal::MTLTextureUsage::RenderTarget | metal::MTLTextureUsage::ShaderRead);
        self.path_intermediate_texture = Some(self.device.new_texture(&texture_descriptor));

        if self.path_sample_count > 1 {
            let mut msaa_descriptor = texture_descriptor;
            msaa_descriptor.set_texture_type(metal::MTLTextureType::D2Multisample);
            msaa_descriptor.set_usage(metal::MTLTextureUsage::RenderTarget);
            // Each path pass clears these samples and resolves them into the
            // single-sample texture. Only that resolved texture is read later,
            // so Apple GPUs can keep the 4x samples entirely in tile memory.
            msaa_descriptor.set_storage_mode(
                if self.device.supports_family(metal::MTLGPUFamily::Apple2) {
                    metal::MTLStorageMode::Memoryless
                } else {
                    metal::MTLStorageMode::Private
                },
            );
            msaa_descriptor.set_sample_count(self.path_sample_count as _);
            self.path_intermediate_msaa_texture = Some(self.device.new_texture(&msaa_descriptor));
        } else {
            self.path_intermediate_msaa_texture = None;
        }
    }

    pub fn update_transparency(&self, _transparent: bool) {
        // todo(mac)?
    }

    pub fn destroy(&self) {
        // nothing to do
    }

    pub fn set_frame_monitor(&mut self, monitor: std::sync::Weak<crate::FrameMonitor>) {
        self.frame_monitor = monitor;
    }

    pub fn draw(&mut self, scene: &Scene) {
        let Some((drawable, command_buffer)) = self.prepare_draw(scene) else {
            return;
        };
        if self.presents_with_transaction {
            command_buffer.commit();
            command_buffer.wait_until_scheduled();
            drawable.present();
        } else {
            command_buffer.present_drawable(&drawable);
            command_buffer.commit();
        }
    }

    /// Schedule the replacement pixels before the caller changes native views.
    /// The caller presents the drawable and restores the mode after committing
    /// its Core Animation transaction, without holding the window lock.
    pub fn prepare_native_update(&mut self, scene: &Scene) -> Option<(metal::MetalDrawable, bool)> {
        let previous = self.presents_with_transaction;
        self.set_presents_with_transaction(true);
        let Some((drawable, command_buffer)) = self.prepare_draw(scene) else {
            self.set_presents_with_transaction(previous);
            return None;
        };
        command_buffer.commit();
        command_buffer.wait_until_scheduled();
        Some((drawable, previous))
    }

    fn prepare_draw(
        &mut self,
        scene: &Scene,
    ) -> Option<(metal::MetalDrawable, metal::CommandBuffer)> {
        let layer = self.layer.clone();
        let viewport_size = layer.drawable_size();
        let viewport_size: Size<DevicePixels> = size(
            (viewport_size.width.ceil() as i32).into(),
            (viewport_size.height.ceil() as i32).into(),
        );
        let drawable = if let Some(drawable) = layer.next_drawable() {
            drawable
        } else {
            log::error!(
                "failed to retrieve next drawable, drawable size: {:?}",
                viewport_size
            );
            return None;
        };

        match self.render_to_texture(scene, drawable.texture()) {
            Ok(command_buffer) => {
                if self.frame_monitor.strong_count() > 0 {
                    let monitor = self.frame_monitor.clone();
                    let presented = ConcreteBlock::new(move |drawable: &metal::DrawableRef| {
                        // A zero timestamp means this drawable was never displayed.
                        if drawable.presented_time() > 0.
                            && let Some(monitor) = monitor.upgrade()
                        {
                            monitor.record_presentation();
                        }
                    })
                    .copy();
                    drawable.add_presented_handler(&presented);
                }
                Some((drawable.to_owned(), command_buffer))
            }
            Err(error) => {
                log::error!("failed to render: {error}");
                None
            }
        }
    }

    /// Encode a scene into a BGRA8 render target. The returned buffer owns all
    /// resources until GPU completion; callers commit it after scheduling presentation
    /// or a readback. This is also the entry point used by on-screen drawing.
    pub fn render_to_texture(
        &mut self,
        scene: &Scene,
        target: &metal::TextureRef,
    ) -> Result<metal::CommandBuffer> {
        let viewport_size = size(
            DevicePixels(target.width() as i32),
            DevicePixels(target.height() as i32),
        );
        loop {
            let mut instance_buffer = self.instance_buffer_pool.lock().acquire(&self.device);

            let command_buffer =
                self.draw_primitives(scene, &mut instance_buffer, target, viewport_size);

            match command_buffer {
                Ok(command_buffer) => {
                    let instance_buffer_pool = self.instance_buffer_pool.clone();
                    let instance_buffer = Cell::new(Some(instance_buffer));
                    let block = ConcreteBlock::new(move |_| {
                        if let Some(instance_buffer) = instance_buffer.take() {
                            instance_buffer_pool.lock().release(instance_buffer);
                        }
                    });
                    let block = block.copy();
                    command_buffer.add_completed_handler(&block);

                    return Ok(command_buffer);
                }
                Err(err) => {
                    log::error!(
                        "failed to render: {}. retrying with larger instance buffer size",
                        err
                    );
                    let mut instance_buffer_pool = self.instance_buffer_pool.lock();
                    let buffer_size = instance_buffer_pool.buffer_size;
                    if buffer_size >= 256 * 1024 * 1024 {
                        return Err(err);
                    }
                    instance_buffer_pool.reset(buffer_size * 2);
                    log::info!(
                        "increased instance buffer size to {}",
                        instance_buffer_pool.buffer_size
                    );
                }
            }
        }
    }

    fn draw_primitives(
        &mut self,
        scene: &Scene,
        instance_buffer: &mut InstanceBuffer,
        target: &metal::TextureRef,
        viewport_size: Size<DevicePixels>,
    ) -> Result<metal::CommandBuffer> {
        let command_queue = self.command_queue.clone();
        let command_buffer = command_queue.new_command_buffer();
        let alpha = if self.layer.is_opaque() { 1. } else { 0. };
        let mut instance_offset = 0;
        let mut surface_textures = Vec::new();

        self.path_intermediates_used = false;
        self.draw_scene(
            scene,
            instance_buffer,
            &mut instance_offset,
            command_buffer,
            target,
            viewport_size,
            alpha,
            &mut surface_textures,
        )?;

        // Include paths drawn by nested blur scenes. A frame without any path
        // batches can release the cached targets; in-flight command buffers
        // retain their own references until the GPU has finished using them.
        if !self.path_intermediates_used {
            self.path_intermediate_texture = None;
            self.path_intermediate_msaa_texture = None;
        }

        instance_buffer.metal_buffer.did_modify_range(NSRange {
            location: 0,
            length: instance_offset as NSUInteger,
        });
        if !surface_textures.is_empty() {
            let surface_textures = Cell::new(Some(surface_textures));
            let completed = ConcreteBlock::new(move |_| drop(surface_textures.take())).copy();
            command_buffer.add_completed_handler(&completed);
        }
        Ok(command_buffer.to_owned())
    }

    fn draw_scene(
        &mut self,
        scene: &Scene,
        instance_buffer: &mut InstanceBuffer,
        instance_offset: &mut usize,
        command_buffer: &metal::CommandBufferRef,
        target: &metal::TextureRef,
        viewport_size: Size<DevicePixels>,
        alpha: f64,
        surface_textures: &mut Vec<(CVMetalTexture, CVPixelBuffer)>,
    ) -> Result<()> {
        let mut command_encoder =
            new_command_encoder(command_buffer, target, viewport_size, |color_attachment| {
                color_attachment.set_load_action(metal::MTLLoadAction::Clear);
                color_attachment.set_clear_color(metal::MTLClearColor::new(0., 0., 0., alpha));
            });

        for batch in scene.batches() {
            let ok = match batch {
                PrimitiveBatch::Shadows(shadows) => self.draw_shadows(
                    shadows,
                    instance_buffer,
                    instance_offset,
                    viewport_size,
                    command_encoder,
                ),
                PrimitiveBatch::Quads(quads) => self.draw_quads(
                    quads,
                    instance_buffer,
                    instance_offset,
                    viewport_size,
                    command_encoder,
                ),
                PrimitiveBatch::Paths(paths) => {
                    self.path_intermediates_used = true;
                    command_encoder.end_encoding();

                    if self
                        .path_intermediate_texture
                        .as_ref()
                        .is_none_or(|texture| {
                            texture.width() != viewport_size.width.0 as u64
                                || texture.height() != viewport_size.height.0 as u64
                        })
                    {
                        self.update_path_intermediate_textures(viewport_size);
                    }

                    let did_draw = self.draw_paths_to_intermediate(
                        paths,
                        instance_buffer,
                        instance_offset,
                        viewport_size,
                        command_buffer,
                    );

                    command_encoder = new_command_encoder(
                        command_buffer,
                        target,
                        viewport_size,
                        |color_attachment| {
                            color_attachment.set_load_action(metal::MTLLoadAction::Load);
                        },
                    );

                    if did_draw {
                        self.draw_paths_from_intermediate(
                            paths,
                            instance_buffer,
                            instance_offset,
                            viewport_size,
                            command_encoder,
                        )
                    } else {
                        false
                    }
                }
                PrimitiveBatch::Underlines(underlines) => self.draw_underlines(
                    underlines,
                    instance_buffer,
                    instance_offset,
                    viewport_size,
                    command_encoder,
                ),
                PrimitiveBatch::MonochromeSprites {
                    texture_id,
                    sprites,
                } => self.draw_monochrome_sprites(
                    texture_id,
                    sprites,
                    instance_buffer,
                    instance_offset,
                    viewport_size,
                    command_encoder,
                ),
                PrimitiveBatch::PolychromeSprites {
                    texture_id,
                    sprites,
                } => self.draw_polychrome_sprites(
                    texture_id,
                    sprites,
                    instance_buffer,
                    instance_offset,
                    viewport_size,
                    command_encoder,
                ),
                PrimitiveBatch::Surfaces(surfaces) => self.draw_surfaces(
                    surfaces,
                    instance_buffer,
                    instance_offset,
                    viewport_size,
                    command_encoder,
                    surface_textures,
                ),
                PrimitiveBatch::Blur(layer) => {
                    command_encoder.end_encoding();
                    let blurred = self.draw_blur_layer(
                        layer,
                        instance_buffer,
                        instance_offset,
                        command_buffer,
                        target,
                        surface_textures,
                    )?;
                    command_encoder =
                        new_command_encoder(command_buffer, target, viewport_size, |attachment| {
                            attachment.set_load_action(metal::MTLLoadAction::Load)
                        });
                    if let Some((textures, bounds)) = blurred {
                        self.composite_blur(
                            layer,
                            &textures[0],
                            bounds,
                            viewport_size,
                            command_encoder,
                        );
                        // Encoders execute in queue order; once composited, this pair
                        // can be reused by subsequent words, including in later frames.
                        if self.blur_textures.len() < 8 {
                            self.blur_textures.push(textures);
                        }
                    }
                    true
                }
            };
            if !ok {
                command_encoder.end_encoding();
                anyhow::bail!(
                    "scene too large: {} paths, {} shadows, {} quads, {} underlines, {} mono, {} poly, {} surfaces",
                    scene.paths.len(),
                    scene.shadows.len(),
                    scene.quads.len(),
                    scene.underlines.len(),
                    scene.monochrome_sprites.len(),
                    scene.polychrome_sprites.len(),
                    scene.surfaces.len(),
                );
            }
        }

        command_encoder.end_encoding();

        Ok(())
    }

    fn draw_blur_layer(
        &mut self,
        layer: &BlurLayer,
        instance_buffer: &mut InstanceBuffer,
        instance_offset: &mut usize,
        command_buffer: &metal::CommandBufferRef,
        target: &metal::TextureRef,
        surface_textures: &mut Vec<(CVMetalTexture, CVPixelBuffer)>,
    ) -> Result<Option<([metal::Texture; 2], Bounds<ScaledPixels>)>> {
        let mut bounds = layer
            .bounds
            .intersect(&layer.content_mask.bounds.dilate(layer.radius * 3.0));
        let backdrop = matches!(layer.source, BlurSource::Backdrop);
        if backdrop {
            bounds = bounds.intersect(&Bounds::new(
                point(ScaledPixels(0.0), ScaledPixels(0.0)),
                size(
                    ScaledPixels(target.width() as f32),
                    ScaledPixels(target.height() as f32),
                ),
            ));
        }
        if bounds.is_empty() || layer.opacity == 0.0 {
            return Ok(None);
        }
        let origin = point(
            ScaledPixels(bounds.left().0.floor()),
            ScaledPixels(bounds.top().0.floor()),
        );
        let width = (bounds.right().0.ceil() - origin.x.0) as u64;
        let height = (bounds.bottom().0.ceil() - origin.y.0) as u64;
        let bounds = Bounds::new(
            origin,
            size(ScaledPixels(width as f32), ScaledPixels(height as f32)),
        );
        // Backdrops use a quarter-resolution crop: 1/16 the pixels and a smaller
        // Gaussian kernel. Foreground glyph effects retain full resolution.
        let downsample = if backdrop { 4 } else { 1 };
        let width = width.div_ceil(downsample).div_ceil(64) * 64;
        let height = height.div_ceil(downsample).div_ceil(64) * 64;
        let textures = if let Some(index) = self.blur_textures.iter().position(|pair| {
            pair[0].width() >= width
                && pair[0].height() >= height
                && pair[0].width() <= width * 2
                && pair[0].height() <= height * 2
        }) {
            self.blur_textures.swap_remove(index)
        } else {
            let descriptor = metal::TextureDescriptor::new();
            descriptor.set_width(width);
            descriptor.set_height(height);
            descriptor.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
            descriptor.set_storage_mode(metal::MTLStorageMode::Private);
            descriptor.set_usage(
                metal::MTLTextureUsage::RenderTarget
                    | metal::MTLTextureUsage::ShaderRead
                    | metal::MTLTextureUsage::ShaderWrite,
            );
            // Discard unused undersized targets; command buffers retain in-flight textures.
            self.blur_textures.clear();
            [
                self.device.new_texture(&descriptor),
                self.device.new_texture(&descriptor),
            ]
        };
        let viewport = size(
            DevicePixels(textures[0].width() as i32),
            DevicePixels(textures[0].height() as i32),
        );
        match &layer.source {
            BlurSource::Scene(scene) => {
                let scene =
                    scene.translated(point(ScaledPixels(-origin.x.0), ScaledPixels(-origin.y.0)));
                self.draw_scene(
                    &scene,
                    instance_buffer,
                    instance_offset,
                    command_buffer,
                    &textures[0],
                    viewport,
                    0.0,
                    surface_textures,
                )?;
            }
            BlurSource::Backdrop => {
                let encoder = command_buffer.new_compute_command_encoder();
                encoder.set_compute_pipeline_state(&self.backdrop_sample_pipeline_state);
                encoder.set_texture(0, Some(target));
                encoder.set_texture(1, Some(&textures[0]));
                let params = [origin.x.0, origin.y.0, bounds.right().0, bounds.bottom().0];
                encoder.set_bytes(0, mem::size_of_val(&params) as u64, params.as_ptr().cast());
                encoder.dispatch_threads(
                    metal::MTLSize::new(textures[0].width(), textures[0].height(), 1),
                    metal::MTLSize::new(16, 16, 1),
                );
                encoder.end_encoding();
            }
        }
        if layer.radius.0 > 0.0 {
            for axis in 0..2u32 {
                let encoder = command_buffer.new_compute_command_encoder();
                encoder.set_compute_pipeline_state(&self.blur_pipeline_state);
                encoder.set_texture(0, Some(&textures[axis as usize]));
                encoder.set_texture(1, Some(&textures[1 - axis as usize]));
                let params = [
                    (layer.radius.0 / downsample as f32).to_bits(),
                    axis,
                    u32::from(backdrop),
                    downsample as u32,
                ];
                let fade = layer.fade.map_or([0.0; 4], |bounds| {
                    [
                        bounds.left().0,
                        bounds.top().0,
                        bounds.right().0,
                        bounds.bottom().0,
                    ]
                });
                let origin = [bounds.left().0, bounds.top().0];
                encoder.set_bytes(0, mem::size_of_val(&params) as u64, params.as_ptr().cast());
                encoder.set_bytes(1, mem::size_of_val(&fade) as u64, fade.as_ptr().cast());
                encoder.set_bytes(2, mem::size_of_val(&origin) as u64, origin.as_ptr().cast());
                encoder.dispatch_threads(
                    metal::MTLSize::new(textures[0].width(), textures[0].height(), 1),
                    metal::MTLSize::new(16, 16, 1),
                );
                encoder.end_encoding();
            }
        }
        Ok(Some((textures, bounds)))
    }

    fn composite_blur(
        &self,
        layer: &BlurLayer,
        texture: &metal::TextureRef,
        bounds: Bounds<ScaledPixels>,
        viewport: Size<DevicePixels>,
        encoder: &metal::RenderCommandEncoderRef,
    ) {
        let corners = layer.corners;
        let shape = corners.map_or(bounds, |(shape, _)| shape);
        // float4-aligned uniforms shared with the Metal shader.
        let params = [
            bounds.left().0,
            bounds.top().0,
            bounds.size.width.0,
            bounds.size.height.0,
            layer.content_mask.bounds.left().0,
            layer.content_mask.bounds.top().0,
            layer.content_mask.bounds.right().0,
            layer.content_mask.bounds.bottom().0,
            viewport.width.0 as f32,
            viewport.height.0 as f32,
            texture.width() as f32
                * if matches!(layer.source, BlurSource::Backdrop) {
                    4.0
                } else {
                    1.0
                },
            texture.height() as f32
                * if matches!(layer.source, BlurSource::Backdrop) {
                    4.0
                } else {
                    1.0
                },
            layer.opacity,
            if matches!(layer.source, BlurSource::Backdrop) {
                1.0
            } else {
                0.0
            },
            corners.map_or(0.0, |(_, radius)| radius.0),
            layer.radius.0,
            shape.left().0,
            shape.top().0,
            shape.right().0,
            shape.bottom().0,
            layer.fade.map_or(0.0, |bounds| bounds.left().0),
            layer.fade.map_or(0.0, |bounds| bounds.top().0),
            layer.fade.map_or(0.0, |bounds| bounds.right().0),
            layer.fade.map_or(0.0, |bounds| bounds.bottom().0),
        ];
        encoder.set_render_pipeline_state(if matches!(layer.source, BlurSource::Backdrop) {
            &self.backdrop_composite_pipeline_state
        } else {
            &self.blur_composite_pipeline_state
        });
        encoder.set_vertex_bytes(0, mem::size_of_val(&params) as u64, params.as_ptr().cast());
        encoder.set_fragment_bytes(0, mem::size_of_val(&params) as u64, params.as_ptr().cast());
        encoder.set_fragment_texture(0, Some(texture));
        encoder.draw_primitives(metal::MTLPrimitiveType::Triangle, 0, 6);
    }

    fn draw_paths_to_intermediate(
        &self,
        paths: &[Path<ScaledPixels>],
        instance_buffer: &mut InstanceBuffer,
        instance_offset: &mut usize,
        viewport_size: Size<DevicePixels>,
        command_buffer: &metal::CommandBufferRef,
    ) -> bool {
        if paths.is_empty() {
            return true;
        }
        let Some(intermediate_texture) = &self.path_intermediate_texture else {
            return false;
        };

        let render_pass_descriptor = metal::RenderPassDescriptor::new();
        let color_attachment = render_pass_descriptor
            .color_attachments()
            .object_at(0)
            .unwrap();
        color_attachment.set_load_action(metal::MTLLoadAction::Clear);
        color_attachment.set_clear_color(metal::MTLClearColor::new(0., 0., 0., 0.));

        if let Some(msaa_texture) = &self.path_intermediate_msaa_texture {
            color_attachment.set_texture(Some(msaa_texture));
            color_attachment.set_resolve_texture(Some(intermediate_texture));
            color_attachment.set_store_action(metal::MTLStoreAction::MultisampleResolve);
        } else {
            color_attachment.set_texture(Some(intermediate_texture));
            color_attachment.set_store_action(metal::MTLStoreAction::Store);
        }

        let command_encoder = command_buffer.new_render_command_encoder(render_pass_descriptor);
        command_encoder.set_render_pipeline_state(&self.paths_rasterization_pipeline_state);

        align_offset(instance_offset);
        let mut vertices = Vec::new();
        for path in paths {
            vertices.extend(path.vertices.iter().map(|v| PathRasterizationVertex {
                xy_position: v.xy_position,
                st_position: v.st_position,
                color: path.color,
                bounds: path.bounds.intersect(&path.content_mask.bounds),
            }));
        }
        let vertices_bytes_len = mem::size_of_val(vertices.as_slice());
        let next_offset = *instance_offset + vertices_bytes_len;
        if next_offset > instance_buffer.size {
            command_encoder.end_encoding();
            return false;
        }
        command_encoder.set_vertex_buffer(
            PathRasterizationInputIndex::Vertices as u64,
            Some(&instance_buffer.metal_buffer),
            *instance_offset as u64,
        );
        command_encoder.set_vertex_bytes(
            PathRasterizationInputIndex::ViewportSize as u64,
            mem::size_of_val(&viewport_size) as u64,
            &viewport_size as *const Size<DevicePixels> as *const _,
        );
        command_encoder.set_fragment_buffer(
            PathRasterizationInputIndex::Vertices as u64,
            Some(&instance_buffer.metal_buffer),
            *instance_offset as u64,
        );
        let buffer_contents =
            unsafe { (instance_buffer.metal_buffer.contents() as *mut u8).add(*instance_offset) };
        unsafe {
            ptr::copy_nonoverlapping(
                vertices.as_ptr() as *const u8,
                buffer_contents,
                vertices_bytes_len,
            );
        }
        command_encoder.draw_primitives(
            metal::MTLPrimitiveType::Triangle,
            0,
            vertices.len() as u64,
        );
        *instance_offset = next_offset;

        command_encoder.end_encoding();
        true
    }

    fn draw_shadows(
        &self,
        shadows: &[Shadow],
        instance_buffer: &mut InstanceBuffer,
        instance_offset: &mut usize,
        viewport_size: Size<DevicePixels>,
        command_encoder: &metal::RenderCommandEncoderRef,
    ) -> bool {
        if shadows.is_empty() {
            return true;
        }
        align_offset(instance_offset);

        command_encoder.set_render_pipeline_state(&self.shadows_pipeline_state);
        command_encoder.set_vertex_buffer(
            ShadowInputIndex::Vertices as u64,
            Some(&self.unit_vertices),
            0,
        );
        command_encoder.set_vertex_buffer(
            ShadowInputIndex::Shadows as u64,
            Some(&instance_buffer.metal_buffer),
            *instance_offset as u64,
        );
        command_encoder.set_fragment_buffer(
            ShadowInputIndex::Shadows as u64,
            Some(&instance_buffer.metal_buffer),
            *instance_offset as u64,
        );

        command_encoder.set_vertex_bytes(
            ShadowInputIndex::ViewportSize as u64,
            mem::size_of_val(&viewport_size) as u64,
            &viewport_size as *const Size<DevicePixels> as *const _,
        );

        let shadow_bytes_len = mem::size_of_val(shadows);
        let buffer_contents =
            unsafe { (instance_buffer.metal_buffer.contents() as *mut u8).add(*instance_offset) };

        let next_offset = *instance_offset + shadow_bytes_len;
        if next_offset > instance_buffer.size {
            return false;
        }

        unsafe {
            ptr::copy_nonoverlapping(
                shadows.as_ptr() as *const u8,
                buffer_contents,
                shadow_bytes_len,
            );
        }

        command_encoder.draw_primitives_instanced(
            metal::MTLPrimitiveType::Triangle,
            0,
            6,
            shadows.len() as u64,
        );
        *instance_offset = next_offset;
        true
    }

    fn draw_quads(
        &self,
        quads: &[Quad],
        instance_buffer: &mut InstanceBuffer,
        instance_offset: &mut usize,
        viewport_size: Size<DevicePixels>,
        command_encoder: &metal::RenderCommandEncoderRef,
    ) -> bool {
        if quads.is_empty() {
            return true;
        }
        align_offset(instance_offset);

        command_encoder.set_render_pipeline_state(&self.quads_pipeline_state);
        command_encoder.set_vertex_buffer(
            QuadInputIndex::Vertices as u64,
            Some(&self.unit_vertices),
            0,
        );
        command_encoder.set_vertex_buffer(
            QuadInputIndex::Quads as u64,
            Some(&instance_buffer.metal_buffer),
            *instance_offset as u64,
        );
        command_encoder.set_fragment_buffer(
            QuadInputIndex::Quads as u64,
            Some(&instance_buffer.metal_buffer),
            *instance_offset as u64,
        );

        command_encoder.set_vertex_bytes(
            QuadInputIndex::ViewportSize as u64,
            mem::size_of_val(&viewport_size) as u64,
            &viewport_size as *const Size<DevicePixels> as *const _,
        );

        let quad_bytes_len = mem::size_of_val(quads);
        let buffer_contents =
            unsafe { (instance_buffer.metal_buffer.contents() as *mut u8).add(*instance_offset) };

        let next_offset = *instance_offset + quad_bytes_len;
        if next_offset > instance_buffer.size {
            return false;
        }

        unsafe {
            ptr::copy_nonoverlapping(quads.as_ptr() as *const u8, buffer_contents, quad_bytes_len);
        }

        command_encoder.draw_primitives_instanced(
            metal::MTLPrimitiveType::Triangle,
            0,
            6,
            quads.len() as u64,
        );
        *instance_offset = next_offset;
        true
    }

    fn draw_paths_from_intermediate(
        &self,
        paths: &[Path<ScaledPixels>],
        instance_buffer: &mut InstanceBuffer,
        instance_offset: &mut usize,
        viewport_size: Size<DevicePixels>,
        command_encoder: &metal::RenderCommandEncoderRef,
    ) -> bool {
        let Some(first_path) = paths.first() else {
            return true;
        };

        let Some(ref intermediate_texture) = self.path_intermediate_texture else {
            return false;
        };

        command_encoder.set_render_pipeline_state(&self.path_sprites_pipeline_state);
        command_encoder.set_vertex_buffer(
            SpriteInputIndex::Vertices as u64,
            Some(&self.unit_vertices),
            0,
        );
        command_encoder.set_vertex_bytes(
            SpriteInputIndex::ViewportSize as u64,
            mem::size_of_val(&viewport_size) as u64,
            &viewport_size as *const Size<DevicePixels> as *const _,
        );

        command_encoder.set_fragment_texture(
            SpriteInputIndex::AtlasTexture as u64,
            Some(intermediate_texture),
        );

        // When copying paths from the intermediate texture to the drawable,
        // each pixel must only be copied once, in case of transparent paths.
        //
        // If all paths have the same draw order, then their bounds are all
        // disjoint, so we can copy each path's bounds individually. If this
        // batch combines different draw orders, we perform a single copy
        // for a minimal spanning rect.
        let sprites;
        if paths.last().unwrap().order == first_path.order {
            sprites = paths
                .iter()
                .map(|path| PathSprite {
                    bounds: path.clipped_bounds(),
                })
                .collect();
        } else {
            let mut bounds = first_path.clipped_bounds();
            for path in paths.iter().skip(1) {
                bounds = bounds.union(&path.clipped_bounds());
            }
            sprites = vec![PathSprite { bounds }];
        }

        align_offset(instance_offset);
        let sprite_bytes_len = mem::size_of_val(sprites.as_slice());
        let next_offset = *instance_offset + sprite_bytes_len;
        if next_offset > instance_buffer.size {
            return false;
        }

        command_encoder.set_vertex_buffer(
            SpriteInputIndex::Sprites as u64,
            Some(&instance_buffer.metal_buffer),
            *instance_offset as u64,
        );

        let buffer_contents =
            unsafe { (instance_buffer.metal_buffer.contents() as *mut u8).add(*instance_offset) };
        unsafe {
            ptr::copy_nonoverlapping(
                sprites.as_ptr() as *const u8,
                buffer_contents,
                sprite_bytes_len,
            );
        }

        command_encoder.draw_primitives_instanced(
            metal::MTLPrimitiveType::Triangle,
            0,
            6,
            sprites.len() as u64,
        );
        *instance_offset = next_offset;

        true
    }

    fn draw_underlines(
        &self,
        underlines: &[Underline],
        instance_buffer: &mut InstanceBuffer,
        instance_offset: &mut usize,
        viewport_size: Size<DevicePixels>,
        command_encoder: &metal::RenderCommandEncoderRef,
    ) -> bool {
        if underlines.is_empty() {
            return true;
        }
        align_offset(instance_offset);

        command_encoder.set_render_pipeline_state(&self.underlines_pipeline_state);
        command_encoder.set_vertex_buffer(
            UnderlineInputIndex::Vertices as u64,
            Some(&self.unit_vertices),
            0,
        );
        command_encoder.set_vertex_buffer(
            UnderlineInputIndex::Underlines as u64,
            Some(&instance_buffer.metal_buffer),
            *instance_offset as u64,
        );
        command_encoder.set_fragment_buffer(
            UnderlineInputIndex::Underlines as u64,
            Some(&instance_buffer.metal_buffer),
            *instance_offset as u64,
        );

        command_encoder.set_vertex_bytes(
            UnderlineInputIndex::ViewportSize as u64,
            mem::size_of_val(&viewport_size) as u64,
            &viewport_size as *const Size<DevicePixels> as *const _,
        );

        let underline_bytes_len = mem::size_of_val(underlines);
        let buffer_contents =
            unsafe { (instance_buffer.metal_buffer.contents() as *mut u8).add(*instance_offset) };

        let next_offset = *instance_offset + underline_bytes_len;
        if next_offset > instance_buffer.size {
            return false;
        }

        unsafe {
            ptr::copy_nonoverlapping(
                underlines.as_ptr() as *const u8,
                buffer_contents,
                underline_bytes_len,
            );
        }

        command_encoder.draw_primitives_instanced(
            metal::MTLPrimitiveType::Triangle,
            0,
            6,
            underlines.len() as u64,
        );
        *instance_offset = next_offset;
        true
    }

    fn draw_monochrome_sprites(
        &self,
        texture_id: AtlasTextureId,
        sprites: &[MonochromeSprite],
        instance_buffer: &mut InstanceBuffer,
        instance_offset: &mut usize,
        viewport_size: Size<DevicePixels>,
        command_encoder: &metal::RenderCommandEncoderRef,
    ) -> bool {
        if sprites.is_empty() {
            return true;
        }
        align_offset(instance_offset);

        let sprite_bytes_len = mem::size_of_val(sprites);
        let buffer_contents =
            unsafe { (instance_buffer.metal_buffer.contents() as *mut u8).add(*instance_offset) };

        let next_offset = *instance_offset + sprite_bytes_len;
        if next_offset > instance_buffer.size {
            return false;
        }

        let texture = self.sprite_atlas.metal_texture(texture_id);
        let texture_size = size(
            DevicePixels(texture.width() as i32),
            DevicePixels(texture.height() as i32),
        );
        command_encoder.set_render_pipeline_state(&self.monochrome_sprites_pipeline_state);
        command_encoder.set_vertex_buffer(
            SpriteInputIndex::Vertices as u64,
            Some(&self.unit_vertices),
            0,
        );
        command_encoder.set_vertex_buffer(
            SpriteInputIndex::Sprites as u64,
            Some(&instance_buffer.metal_buffer),
            *instance_offset as u64,
        );
        command_encoder.set_vertex_bytes(
            SpriteInputIndex::ViewportSize as u64,
            mem::size_of_val(&viewport_size) as u64,
            &viewport_size as *const Size<DevicePixels> as *const _,
        );
        command_encoder.set_vertex_bytes(
            SpriteInputIndex::AtlasTextureSize as u64,
            mem::size_of_val(&texture_size) as u64,
            &texture_size as *const Size<DevicePixels> as *const _,
        );
        command_encoder.set_fragment_buffer(
            SpriteInputIndex::Sprites as u64,
            Some(&instance_buffer.metal_buffer),
            *instance_offset as u64,
        );
        command_encoder.set_fragment_texture(SpriteInputIndex::AtlasTexture as u64, Some(&texture));

        unsafe {
            ptr::copy_nonoverlapping(
                sprites.as_ptr() as *const u8,
                buffer_contents,
                sprite_bytes_len,
            );
        }

        command_encoder.draw_primitives_instanced(
            metal::MTLPrimitiveType::Triangle,
            0,
            6,
            sprites.len() as u64,
        );
        *instance_offset = next_offset;
        true
    }

    fn draw_polychrome_sprites(
        &self,
        texture_id: AtlasTextureId,
        sprites: &[PolychromeSprite],
        instance_buffer: &mut InstanceBuffer,
        instance_offset: &mut usize,
        viewport_size: Size<DevicePixels>,
        command_encoder: &metal::RenderCommandEncoderRef,
    ) -> bool {
        if sprites.is_empty() {
            return true;
        }
        align_offset(instance_offset);

        let texture = self.sprite_atlas.metal_texture(texture_id);
        let texture_size = size(
            DevicePixels(texture.width() as i32),
            DevicePixels(texture.height() as i32),
        );
        command_encoder.set_render_pipeline_state(&self.polychrome_sprites_pipeline_state);
        command_encoder.set_vertex_buffer(
            SpriteInputIndex::Vertices as u64,
            Some(&self.unit_vertices),
            0,
        );
        command_encoder.set_vertex_buffer(
            SpriteInputIndex::Sprites as u64,
            Some(&instance_buffer.metal_buffer),
            *instance_offset as u64,
        );
        command_encoder.set_vertex_bytes(
            SpriteInputIndex::ViewportSize as u64,
            mem::size_of_val(&viewport_size) as u64,
            &viewport_size as *const Size<DevicePixels> as *const _,
        );
        command_encoder.set_vertex_bytes(
            SpriteInputIndex::AtlasTextureSize as u64,
            mem::size_of_val(&texture_size) as u64,
            &texture_size as *const Size<DevicePixels> as *const _,
        );
        command_encoder.set_fragment_buffer(
            SpriteInputIndex::Sprites as u64,
            Some(&instance_buffer.metal_buffer),
            *instance_offset as u64,
        );
        command_encoder.set_fragment_texture(SpriteInputIndex::AtlasTexture as u64, Some(&texture));

        let sprite_bytes_len = mem::size_of_val(sprites);
        let buffer_contents =
            unsafe { (instance_buffer.metal_buffer.contents() as *mut u8).add(*instance_offset) };

        let next_offset = *instance_offset + sprite_bytes_len;
        if next_offset > instance_buffer.size {
            return false;
        }

        unsafe {
            ptr::copy_nonoverlapping(
                sprites.as_ptr() as *const u8,
                buffer_contents,
                sprite_bytes_len,
            );
        }

        command_encoder.draw_primitives_instanced(
            metal::MTLPrimitiveType::Triangle,
            0,
            6,
            sprites.len() as u64,
        );
        *instance_offset = next_offset;
        true
    }

    fn draw_surfaces(
        &mut self,
        surfaces: &[PaintSurface],
        instance_buffer: &mut InstanceBuffer,
        instance_offset: &mut usize,
        viewport_size: Size<DevicePixels>,
        command_encoder: &metal::RenderCommandEncoderRef,
        surface_textures: &mut Vec<(CVMetalTexture, CVPixelBuffer)>,
    ) -> bool {
        command_encoder.set_render_pipeline_state(&self.surfaces_pipeline_state);
        command_encoder.set_vertex_buffer(
            SurfaceInputIndex::Vertices as u64,
            Some(&self.unit_vertices),
            0,
        );
        command_encoder.set_vertex_bytes(
            SurfaceInputIndex::ViewportSize as u64,
            mem::size_of_val(&viewport_size) as u64,
            &viewport_size as *const Size<DevicePixels> as *const _,
        );

        for surface in surfaces {
            let texture_size = size(
                DevicePixels::from(surface.image_buffer.get_width() as i32),
                DevicePixels::from(surface.image_buffer.get_height() as i32),
            );

            let format = surface.image_buffer.get_pixel_format();
            let bgra = format == kCVPixelFormatType_32BGRA;
            if !bgra && format != kCVPixelFormatType_420YpCbCr8BiPlanarFullRange {
                continue;
            }
            // A BGRA image is one plane; it fills both texture slots and the
            // shader samples only the first.
            let planes = if bgra {
                [
                    (MTLPixelFormat::BGRA8Unorm, None),
                    (MTLPixelFormat::BGRA8Unorm, None),
                ]
            } else {
                [
                    (MTLPixelFormat::R8Unorm, Some(0)),
                    (MTLPixelFormat::RG8Unorm, Some(1)),
                ]
            };
            let textures = planes.map(|(format, plane)| {
                let (width, height, plane) = match plane {
                    Some(plane) => (
                        surface.image_buffer.get_width_of_plane(plane),
                        surface.image_buffer.get_height_of_plane(plane),
                        plane,
                    ),
                    None => (
                        surface.image_buffer.get_width(),
                        surface.image_buffer.get_height(),
                        0,
                    ),
                };
                self.core_video_texture_cache
                    .create_texture_from_image(
                        surface.image_buffer.as_concrete_TypeRef(),
                        None,
                        format,
                        width,
                        height,
                        plane,
                    )
                    .ok()
            });
            let [Some(y_texture), Some(cb_cr_texture)] = textures else {
                continue;
            };

            align_offset(instance_offset);
            let next_offset = *instance_offset + mem::size_of::<SurfaceBounds>();
            if next_offset > instance_buffer.size {
                return false;
            }

            command_encoder.set_vertex_buffer(
                SurfaceInputIndex::Surfaces as u64,
                Some(&instance_buffer.metal_buffer),
                *instance_offset as u64,
            );
            command_encoder.set_vertex_bytes(
                SurfaceInputIndex::TextureSize as u64,
                mem::size_of_val(&texture_size) as u64,
                &texture_size as *const Size<DevicePixels> as *const _,
            );
            command_encoder.set_fragment_texture(SurfaceInputIndex::YTexture as u64, unsafe {
                let texture = CVMetalTextureGetTexture(y_texture.as_concrete_TypeRef());
                Some(metal::TextureRef::from_ptr(texture as *mut _))
            });
            command_encoder.set_fragment_texture(SurfaceInputIndex::CbCrTexture as u64, unsafe {
                let texture = CVMetalTextureGetTexture(cb_cr_texture.as_concrete_TypeRef());
                Some(metal::TextureRef::from_ptr(texture as *mut _))
            });

            unsafe {
                let buffer_contents = (instance_buffer.metal_buffer.contents() as *mut u8)
                    .add(*instance_offset)
                    as *mut SurfaceBounds;
                ptr::write(
                    buffer_contents,
                    SurfaceBounds {
                        bounds: surface.bounds,
                        content_mask: surface.content_mask.clone(),
                        turns: surface.turns,
                        bgra: bgra as u32,
                    },
                );
            }

            command_encoder.draw_primitives(metal::MTLPrimitiveType::Triangle, 0, 6);
            // Retain the source buffer until GPU completion too: pool-backed
            // producers must not recycle its pixels while Metal samples them.
            surface_textures.extend([
                (y_texture, surface.image_buffer.clone()),
                (cb_cr_texture, surface.image_buffer.clone()),
            ]);
            *instance_offset = next_offset;
        }
        true
    }
}

fn new_command_encoder<'a>(
    command_buffer: &'a metal::CommandBufferRef,
    target: &'a metal::TextureRef,
    viewport_size: Size<DevicePixels>,
    configure_color_attachment: impl Fn(&RenderPassColorAttachmentDescriptorRef),
) -> &'a metal::RenderCommandEncoderRef {
    let render_pass_descriptor = metal::RenderPassDescriptor::new();
    let color_attachment = render_pass_descriptor
        .color_attachments()
        .object_at(0)
        .unwrap();
    color_attachment.set_texture(Some(target));
    color_attachment.set_store_action(metal::MTLStoreAction::Store);
    configure_color_attachment(color_attachment);

    let command_encoder = command_buffer.new_render_command_encoder(render_pass_descriptor);
    command_encoder.set_viewport(metal::MTLViewport {
        originX: 0.0,
        originY: 0.0,
        width: i32::from(viewport_size.width) as f64,
        height: i32::from(viewport_size.height) as f64,
        znear: 0.0,
        zfar: 1.0,
    });
    command_encoder
}

fn build_pipeline_state(
    device: &metal::DeviceRef,
    library: &metal::LibraryRef,
    label: &str,
    vertex_fn_name: &str,
    fragment_fn_name: &str,
    pixel_format: metal::MTLPixelFormat,
) -> metal::RenderPipelineState {
    let vertex_fn = library
        .get_function(vertex_fn_name, None)
        .expect("error locating vertex function");
    let fragment_fn = library
        .get_function(fragment_fn_name, None)
        .expect("error locating fragment function");

    let descriptor = metal::RenderPipelineDescriptor::new();
    descriptor.set_label(label);
    descriptor.set_vertex_function(Some(vertex_fn.as_ref()));
    descriptor.set_fragment_function(Some(fragment_fn.as_ref()));
    let color_attachment = descriptor.color_attachments().object_at(0).unwrap();
    color_attachment.set_pixel_format(pixel_format);
    color_attachment.set_blending_enabled(true);
    color_attachment.set_rgb_blend_operation(metal::MTLBlendOperation::Add);
    color_attachment.set_alpha_blend_operation(metal::MTLBlendOperation::Add);
    color_attachment.set_source_rgb_blend_factor(metal::MTLBlendFactor::SourceAlpha);
    color_attachment.set_source_alpha_blend_factor(metal::MTLBlendFactor::One);
    color_attachment.set_destination_rgb_blend_factor(metal::MTLBlendFactor::OneMinusSourceAlpha);
    color_attachment.set_destination_alpha_blend_factor(metal::MTLBlendFactor::OneMinusSourceAlpha);

    device
        .new_render_pipeline_state(&descriptor)
        .expect("could not create render pipeline state")
}

fn build_path_sprite_pipeline_state(
    device: &metal::DeviceRef,
    library: &metal::LibraryRef,
    label: &str,
    vertex_fn_name: &str,
    fragment_fn_name: &str,
    pixel_format: metal::MTLPixelFormat,
    backdrop: bool,
) -> metal::RenderPipelineState {
    let vertex_fn = library
        .get_function(vertex_fn_name, None)
        .expect("error locating vertex function");
    let fragment_fn = library
        .get_function(fragment_fn_name, None)
        .expect("error locating fragment function");

    let descriptor = metal::RenderPipelineDescriptor::new();
    descriptor.set_label(label);
    descriptor.set_vertex_function(Some(vertex_fn.as_ref()));
    descriptor.set_fragment_function(Some(fragment_fn.as_ref()));
    let color_attachment = descriptor.color_attachments().object_at(0).unwrap();
    color_attachment.set_pixel_format(pixel_format);
    color_attachment.set_blending_enabled(true);
    color_attachment.set_rgb_blend_operation(metal::MTLBlendOperation::Add);
    color_attachment.set_alpha_blend_operation(metal::MTLBlendOperation::Add);
    color_attachment.set_source_rgb_blend_factor(metal::MTLBlendFactor::One);
    color_attachment.set_source_alpha_blend_factor(metal::MTLBlendFactor::One);
    let destination = if backdrop {
        metal::MTLBlendFactor::OneMinusSource1Alpha
    } else {
        metal::MTLBlendFactor::OneMinusSourceAlpha
    };
    color_attachment.set_destination_rgb_blend_factor(destination);
    color_attachment.set_destination_alpha_blend_factor(destination);

    device
        .new_render_pipeline_state(&descriptor)
        .expect("could not create render pipeline state")
}

fn build_path_rasterization_pipeline_state(
    device: &metal::DeviceRef,
    library: &metal::LibraryRef,
    label: &str,
    vertex_fn_name: &str,
    fragment_fn_name: &str,
    pixel_format: metal::MTLPixelFormat,
    path_sample_count: u32,
) -> metal::RenderPipelineState {
    let vertex_fn = library
        .get_function(vertex_fn_name, None)
        .expect("error locating vertex function");
    let fragment_fn = library
        .get_function(fragment_fn_name, None)
        .expect("error locating fragment function");

    let descriptor = metal::RenderPipelineDescriptor::new();
    descriptor.set_label(label);
    descriptor.set_vertex_function(Some(vertex_fn.as_ref()));
    descriptor.set_fragment_function(Some(fragment_fn.as_ref()));
    if path_sample_count > 1 {
        descriptor.set_raster_sample_count(path_sample_count as _);
        descriptor.set_alpha_to_coverage_enabled(false);
    }
    let color_attachment = descriptor.color_attachments().object_at(0).unwrap();
    color_attachment.set_pixel_format(pixel_format);
    color_attachment.set_blending_enabled(true);
    color_attachment.set_rgb_blend_operation(metal::MTLBlendOperation::Add);
    color_attachment.set_alpha_blend_operation(metal::MTLBlendOperation::Add);
    color_attachment.set_source_rgb_blend_factor(metal::MTLBlendFactor::One);
    color_attachment.set_source_alpha_blend_factor(metal::MTLBlendFactor::One);
    color_attachment.set_destination_rgb_blend_factor(metal::MTLBlendFactor::OneMinusSourceAlpha);
    color_attachment.set_destination_alpha_blend_factor(metal::MTLBlendFactor::OneMinusSourceAlpha);

    device
        .new_render_pipeline_state(&descriptor)
        .expect("could not create render pipeline state")
}

// Align to multiples of 256 make Metal happy.
fn align_offset(offset: &mut usize) {
    *offset = (*offset).div_ceil(256) * 256;
}

#[repr(C)]
enum ShadowInputIndex {
    Vertices = 0,
    Shadows = 1,
    ViewportSize = 2,
}

#[repr(C)]
enum QuadInputIndex {
    Vertices = 0,
    Quads = 1,
    ViewportSize = 2,
}

#[repr(C)]
enum UnderlineInputIndex {
    Vertices = 0,
    Underlines = 1,
    ViewportSize = 2,
}

#[repr(C)]
enum SpriteInputIndex {
    Vertices = 0,
    Sprites = 1,
    ViewportSize = 2,
    AtlasTextureSize = 3,
    AtlasTexture = 4,
}

#[repr(C)]
enum SurfaceInputIndex {
    Vertices = 0,
    Surfaces = 1,
    ViewportSize = 2,
    TextureSize = 3,
    YTexture = 4,
    CbCrTexture = 5,
}

#[repr(C)]
enum PathRasterizationInputIndex {
    Vertices = 0,
    ViewportSize = 1,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct PathSprite {
    pub bounds: Bounds<ScaledPixels>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct SurfaceBounds {
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    /// Clockwise quarter turns applied to the texture.
    pub turns: u32,
    /// 1 when the image is a single BGRA plane, 0 for biplanar YCbCr.
    pub bgra: u32,
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use crate::{Primitive, linear_color_stop, linear_gradient, rgba, white};

    fn bounds(x: f32, y: f32, width: f32, height: f32) -> Bounds<ScaledPixels> {
        Bounds::new(
            point(ScaledPixels(x), ScaledPixels(y)),
            size(ScaledPixels(width), ScaledPixels(height)),
        )
    }

    fn quad(area: Bounds<ScaledPixels>, color: u32) -> Primitive {
        Quad {
            bounds: area,
            content_mask: ContentMask {
                bounds: bounds(0.0, 0.0, 96.0, 96.0),
            },
            background: rgba(color).into(),
            ..Default::default()
        }
        .into()
    }

    // Exercise the same scene/renderer interfaces as the window, with a numeric
    // GPU readback. No app, native window, or screenshot is involved.
    fn render(renderer: &mut MetalRenderer, scene: &mut Scene) -> Vec<u8> {
        scene.finish();
        let descriptor = metal::TextureDescriptor::new();
        descriptor.set_width(96);
        descriptor.set_height(96);
        descriptor.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
        descriptor.set_storage_mode(metal::MTLStorageMode::Managed);
        descriptor
            .set_usage(metal::MTLTextureUsage::RenderTarget | metal::MTLTextureUsage::ShaderRead);
        let texture = renderer.layer().device().new_texture(&descriptor);
        let commands = renderer.render_to_texture(scene, &texture).unwrap();
        let blit = commands.new_blit_command_encoder();
        blit.synchronize_resource(&texture);
        blit.end_encoding();
        commands.commit();
        commands.wait_until_completed();
        assert_eq!(commands.status(), metal::MTLCommandBufferStatus::Completed);
        let mut bytes = vec![0; 96 * 96 * 4];
        texture.get_bytes(
            bytes.as_mut_ptr().cast(),
            96 * 4,
            metal::MTLRegion::new_2d(0, 0, 96, 96),
            0,
        );
        bytes
    }

    fn pixel(bytes: &[u8], x: usize, y: usize) -> &[u8] {
        &bytes[(y * 96 + x) * 4..(y * 96 + x + 1) * 4]
    }

    #[test]
    fn subtle_opacity_gradient_preserves_average_brightness_without_bands_or_shimmer() {
        let mut renderer = MetalRenderer::new(Arc::default());
        let mut scene = Scene::default();
        let area = bounds(0.0, 0.0, 96.0, 96.0);
        scene.insert_primitive(quad(area, 0x101010FF));
        scene.insert_primitive(Quad {
            bounds: area,
            content_mask: ContentMask { bounds: area },
            background: linear_gradient(
                180.0,
                linear_color_stop(white().opacity(0.08), 0.125),
                linear_color_stop(white().opacity(0.05), 0.875),
            ),
            ..Default::default()
        });
        let bytes = render(&mut renderer, &mut scene);
        for y in 20..76 {
            let t = (y as f32 + 0.5 - 12.0) / 72.0;
            let expected = 16.0 + (255.0 - 16.0) * (0.08 - 0.03 * t);
            let average = (0..96)
                .map(|x| f32::from(pixel(&bytes, x, y)[0]))
                .sum::<f32>()
                / 96.0;
            assert!(
                (average - expected).abs() < 0.12,
                "row {y} must represent fractional brightness: {average}, expected {expected}"
            );
            for x in 0..96 {
                let sample = pixel(&bytes, x, y);
                assert!(
                    (f32::from(sample[0]) - expected).abs() <= 1.0,
                    "dithering must stay within one output level"
                );
                assert_eq!(sample[0], sample[1]);
                assert_eq!(sample[1], sample[2]);
                assert_eq!(sample[3], 255);
            }
        }
        for x in 0..96 {
            assert_eq!(pixel(&bytes, x, 0), &[35, 35, 35, 255]);
            assert_eq!(pixel(&bytes, x, 95), &[28, 28, 28, 255]);
        }
        let mut replay = Scene::default();
        replay.replay(0..scene.len(), &scene);
        assert_eq!(
            render(&mut renderer, &mut replay),
            bytes,
            "repainting must not animate the dither"
        );
    }

    #[test]
    fn blur_composites_before_opacity_and_preserves_clipping_and_order() {
        let mut renderer = MetalRenderer::new(Arc::default());
        let mut scene = Scene::default();
        scene.push_layer(bounds(0.0, 0.0, 96.0, 96.0));
        scene.push_blur(
            ScaledPixels(2.0),
            0.5,
            ContentMask {
                bounds: bounds(20.0, 0.0, 76.0, 96.0),
            },
            None,
        );
        scene.insert_primitive(quad(bounds(20.0, 20.0, 16.0, 16.0), 0xFF000080));
        scene.insert_primitive(quad(bounds(24.0, 20.0, 16.0, 16.0), 0xFF000080));
        scene.pop_blur();
        // Later geometry must stay on top even within the same batching layer.
        scene.insert_primitive(quad(bounds(40.0, 20.0, 4.0, 16.0), 0x0000FFFF));
        scene.pop_layer();
        let bytes = render(&mut renderer, &mut scene);
        let center = pixel(&bytes, 30, 28);
        // Two half-alpha inputs compose to 0.75, then group opacity gives 0.375.
        assert!((i32::from(center[3]) - 96).abs() <= 3, "{center:?}");
        assert_eq!(
            center[2], center[3],
            "premultiplied red must not darken at transparent edges"
        );
        assert_eq!(
            pixel(&bytes, 19, 28),
            &[0, 0, 0, 0],
            "final clipping must contain the halo"
        );
        assert!(
            pixel(&bytes, 30, 18)[3] > 0,
            "blur must extend above the original geometry"
        );
        assert_eq!(
            pixel(&bytes, 42, 28),
            &[255, 0, 0, 255],
            "later primitives cover the blur"
        );
        let mut replay = Scene::default();
        replay.replay(0..scene.len(), &scene);
        assert_eq!(
            render(&mut renderer, &mut replay),
            bytes,
            "cached paint scopes must replay identically"
        );
    }

    #[test]
    fn blur_is_separable_nested_and_reuses_targets_without_stale_pixels() {
        let mut renderer = MetalRenderer::new(Arc::default());
        let mask = ContentMask {
            bounds: bounds(0.0, 0.0, 96.0, 96.0),
        };
        let mut scene = Scene::default();
        scene.push_blur(ScaledPixels(3.0), 1.0, mask.clone(), None);
        scene.insert_primitive(quad(bounds(44.0, 44.0, 8.0, 8.0), 0xFFFFFFFF));
        scene.pop_blur();
        let bytes = render(&mut renderer, &mut scene);
        assert!(pixel(&bytes, 40, 47)[3] > 0);
        for (x, y) in [(40, 47), (42, 44), (45, 49)] {
            assert!(
                (i32::from(pixel(&bytes, x, y)[3]) - i32::from(pixel(&bytes, y, x)[3])).abs() <= 1,
                "horizontal and vertical Gaussian passes must be symmetric"
            );
        }

        scene.clear();
        scene.push_blur(ScaledPixels(0.0), 0.5, mask.clone(), None);
        scene.push_blur(ScaledPixels(0.0), 0.5, mask, None);
        scene.insert_primitive(quad(bounds(10.0, 10.0, 8.0, 8.0), 0x00FF00FF));
        scene.pop_blur();
        scene.pop_blur();
        let bytes = render(&mut renderer, &mut scene);
        assert_eq!(
            pixel(&bytes, 12, 12),
            &[0, 64, 0, 64],
            "nested layers compose their opacities"
        );
        assert_eq!(
            pixel(&bytes, 48, 48),
            &[0, 0, 0, 0],
            "pooled textures must not leak prior content"
        );

        scene.clear();
        scene.push_blur(
            ScaledPixels(0.0),
            0.5,
            ContentMask {
                bounds: bounds(0.0, 0.0, 96.0, 96.0),
            },
            None,
        );
        scene.insert_primitive(Shadow {
            order: 0,
            blur_radius: ScaledPixels(3.0),
            bounds: bounds(40.0, 40.0, 12.0, 12.0),
            corner_radii: Default::default(),
            content_mask: ContentMask {
                bounds: bounds(0.0, 0.0, 96.0, 96.0),
            },
            color: rgba(0xFFFFFFFF).into(),
        });
        scene.pop_blur();
        let bytes = render(&mut renderer, &mut scene);
        assert!(
            pixel(&bytes, 38, 46)[3] > 0,
            "captured layers must include a shadow's shader-expanded bounds"
        );
    }

    #[test]
    fn backdrop_blurs_prior_pixels_and_preserves_alpha_clipping_and_foreground() {
        let mut renderer = MetalRenderer::new(Arc::default());
        let mut scene = Scene::default();
        scene.insert_primitive(quad(bounds(0.0, 0.0, 48.0, 96.0), 0xFF000080));
        scene.insert_primitive(quad(bounds(48.0, 0.0, 48.0, 96.0), 0x0000FF80));
        scene.insert_primitive(Primitive::Blur(BlurLayer {
            reveal: None,
            order: 0,
            bounds: bounds(0.0, 0.0, 96.0, 72.0),
            content_mask: ContentMask {
                bounds: bounds(0.0, 0.0, 96.0, 48.0),
            },
            radius: ScaledPixels(8.0),
            opacity: 1.0,
            source: BlurSource::Backdrop,
            corners: None,
            fade: None,
        }));
        scene.insert_primitive(quad(bounds(44.0, 16.0, 8.0, 8.0), 0x00FF00FF));
        let bytes = render(&mut renderer, &mut scene);
        let transition = pixel(&bytes, 47, 32);
        assert!(
            transition[0] > 30 && transition[2] > 30,
            "both sides must mix: {transition:?}"
        );
        assert!(
            (i32::from(transition[3]) - 128).abs() <= 1,
            "backdrop must replace, not double-composite alpha"
        );
        assert_eq!(
            pixel(&bytes, 0, 0),
            &[0, 0, 128, 128],
            "window edges must not darken"
        );
        assert_eq!(
            pixel(&bytes, 47, 50),
            &[0, 0, 128, 128],
            "outside the toolbar stays sharp"
        );
        assert_eq!(
            pixel(&bytes, 48, 20),
            &[0, 255, 0, 255],
            "controls stay sharp above the blur"
        );
        let mut replay = Scene::default();
        replay.replay(0..scene.len(), &scene);
        assert_eq!(
            render(&mut renderer, &mut replay),
            bytes,
            "cached painting must preserve the backdrop"
        );

        scene.clear();
        scene.insert_primitive(quad(bounds(0.0, 0.0, 96.0, 96.0), 0x00FF0080));
        scene.insert_primitive(Primitive::Blur(BlurLayer {
            reveal: None,
            order: 0,
            bounds: bounds(0.0, 0.0, 48.0, 48.0),
            content_mask: ContentMask {
                bounds: bounds(0.0, 0.0, 24.0, 24.0),
            },
            radius: ScaledPixels(8.0),
            opacity: 1.0,
            source: BlurSource::Backdrop,
            corners: None,
            fade: None,
        }));
        let bytes = render(&mut renderer, &mut scene);
        assert_eq!(pixel(&bytes, 0, 0), &[0, 128, 0, 128]);
        assert_eq!(
            pixel(&bytes, 23, 23),
            &[0, 128, 0, 128],
            "reused textures must contain only this frame"
        );
    }

    #[test]
    fn backdrop_fade_progressively_blurs_instead_of_overlaying_a_glow() {
        let mut renderer = MetalRenderer::new(Arc::default());
        let region = bounds(0.0, 0.0, 96.0, 96.0);
        let background = || {
            let mut scene = Scene::default();
            scene.insert_primitive(quad(bounds(0.0, 0.0, 48.0, 96.0), 0xFF000080));
            scene.insert_primitive(quad(bounds(48.0, 0.0, 48.0, 96.0), 0x0000FF80));
            scene
        };
        let original = render(&mut renderer, &mut background());
        let mut full = background();
        let mut layer = BlurLayer {
            reveal: None,
            order: 0,
            bounds: region,
            content_mask: ContentMask { bounds: region },
            radius: ScaledPixels(8.0),
            opacity: 1.0,
            source: BlurSource::Backdrop,
            corners: None,
            fade: None,
        };
        full.insert_primitive(Primitive::Blur(layer.clone()));
        let blurred = render(&mut renderer, &mut full);
        for clip in [region, bounds(0.0, 24.0, 96.0, 48.0)] {
            let mut faded = background();
            layer.content_mask.bounds = clip;
            layer.fade = Some(region);
            faded.insert_primitive(Primitive::Blur(layer.clone()));
            let bytes = render(&mut renderer, &mut faded);
            let mut previous = 0;
            for y in [0, 8, 24, 48, 71, 88, 95] {
                if (y as f32) >= clip.top().0 && (y as f32) < clip.bottom().0 {
                    let blue = pixel(&bytes, 44, y)[0] as i16;
                    assert!(blue + 2 >= previous, "blur must increase toward the input");
                    assert!(blue <= pixel(&blurred, 44, y)[0] as i16 + 2);
                    previous = blue;
                } else {
                    assert_eq!(pixel(&bytes, 44, y), pixel(&original, 44, y));
                }
                assert!((pixel(&bytes, 44, y)[3] as i16 - 128).abs() <= 1);
            }
            // Halfway through the fade a true narrower Gaussian softens the
            // color boundary. Crossfading a wide blur would retain a sharp edge.
            let mid = pixel(&bytes, 47, 48)[0] as f32;
            let ghost = pixel(&blurred, 47, 48)[0] as f32 * 0.5;
            assert!(
                mid > ghost + 10.0,
                "sharp source must not remain under a wide halo"
            );
        }
        assert!(
            pixel(&blurred, 44, 48)[0] > 20,
            "the reference must actually blur the red/blue boundary"
        );
    }

    #[test]
    fn backdrop_fade_softens_both_side_edges_without_tinting_them() {
        let mut renderer = MetalRenderer::new(Arc::default());
        let background = || {
            let mut scene = Scene::default();
            scene.insert_primitive(quad(bounds(0.0, 0.0, 96.0, 48.0), 0xFF000080));
            scene.insert_primitive(quad(bounds(0.0, 48.0, 96.0, 48.0), 0x0000FF80));
            scene
        };
        let original = render(&mut renderer, &mut background());
        let region = bounds(16.0, 8.0, 64.0, 80.0);
        let mut layer = BlurLayer {
            reveal: None,
            order: 0,
            bounds: region.dilate(ScaledPixels(24.0)),
            content_mask: ContentMask { bounds: region },
            radius: ScaledPixels(8.0),
            opacity: 1.0,
            source: BlurSource::Backdrop,
            corners: None,
            fade: None,
        };
        let mut full = background();
        full.insert_primitive(Primitive::Blur(layer.clone()));
        let blurred = render(&mut renderer, &mut full);
        layer.fade = Some(bounds(16.0, 8.0, 64.0, 24.0));
        let mut faded = background();
        faded.insert_primitive(Primitive::Blur(layer));
        let bytes = render(&mut renderer, &mut faded);
        for x in [0, 16, 20, 28, 48, 67, 75, 79, 95] {
            assert!((pixel(&bytes, x, 44)[3] as i16 - 128).abs() <= 1);
            if !(16..80).contains(&x) {
                assert_eq!(pixel(&bytes, x, 44), pixel(&original, x, 44));
            }
        }
        for x in [16, 79] {
            assert!(pixel(&bytes, x, 44)[0] <= 2, "side edges stay sharp");
        }
        assert!(pixel(&bytes, 20, 44)[0] < pixel(&bytes, 28, 44)[0]);
        assert!(pixel(&bytes, 28, 44)[0] < pixel(&bytes, 48, 44)[0]);
        assert!(pixel(&bytes, 75, 44)[0] < pixel(&bytes, 67, 44)[0]);
        assert!(pixel(&bytes, 67, 44)[0] < pixel(&bytes, 48, 44)[0]);
        assert!((pixel(&bytes, 48, 44)[0] as i16 - pixel(&blurred, 48, 44)[0] as i16).abs() <= 2);
        assert!(pixel(&blurred, 48, 44)[0] > 20);
        let mut replay = Scene::default();
        replay.replay(0..faded.len(), &faded);
        assert_eq!(render(&mut renderer, &mut replay), bytes);
    }

    #[test]
    fn rounded_backdrop_blur_leaves_its_corners_sharp() {
        let mut renderer = MetalRenderer::new(Arc::default());
        let mut scene = Scene::default();
        scene.insert_primitive(quad(bounds(0.0, 0.0, 48.0, 96.0), 0xFF000080));
        scene.insert_primitive(quad(bounds(48.0, 0.0, 48.0, 96.0), 0x0000FF80));
        let region = bounds(24.0, 24.0, 48.0, 48.0);
        scene.insert_primitive(Primitive::Blur(BlurLayer {
            reveal: None,
            order: 0,
            bounds: region.dilate(ScaledPixels(24.0)),
            content_mask: ContentMask { bounds: region },
            radius: ScaledPixels(8.0),
            opacity: 1.0,
            source: BlurSource::Backdrop,
            corners: Some((region, ScaledPixels(12.0))),
            fade: None,
        }));
        let bytes = render(&mut renderer, &mut scene);
        let inside = pixel(&bytes, 47, 26);
        assert!(
            inside[0] > 30 && inside[2] > 30,
            "the rounded region blurs across the seam: {inside:?}"
        );
        assert_eq!(
            pixel(&bytes, 25, 25),
            &[0, 0, 128, 128],
            "pixels past the corner's curve stay sharp"
        );
        assert_eq!(pixel(&bytes, 70, 70), &[128, 0, 0, 128]);
    }
}
