// todo("windows"): remove
#![cfg_attr(windows, allow(dead_code))]

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    AtlasTextureId, AtlasTile, Background, Bounds, ContentMask, Corners, Edges, Hsla, Pixels,
    Point, Radians, ScaledPixels, Size, bounds_tree::BoundsTree, point,
};
use std::{
    fmt::Debug,
    iter::Peekable,
    ops::{Add, Range, Sub},
    slice,
    sync::Arc,
    time::{Duration, Instant},
};

#[allow(non_camel_case_types, unused)]
pub(crate) type PathVertex_ScaledPixels = PathVertex<ScaledPixels>;

pub(crate) type DrawOrder = u32;

#[derive(Clone, Default, Debug)]
pub(crate) struct Scene {
    pub(crate) paint_operations: Vec<PaintOperation>,
    primitive_bounds: BoundsTree<ScaledPixels>,
    layer_stack: Vec<DrawOrder>,
    pub(crate) shadows: Vec<Shadow>,
    pub(crate) quads: Vec<Quad>,
    pub(crate) paths: Vec<Path<ScaledPixels>>,
    pub(crate) underlines: Vec<Underline>,
    pub(crate) monochrome_sprites: Vec<MonochromeSprite>,
    pub(crate) polychrome_sprites: Vec<PolychromeSprite>,
    pub(crate) surfaces: Vec<PaintSurface>,
    pub(crate) blur_layers: Vec<BlurLayer>,
    blur_stack: Vec<usize>,
    sprite_animations: Vec<SpriteAnimation>,
    animated_sprites: Vec<(usize, usize)>,
}

impl Scene {
    pub fn clear(&mut self) {
        self.paint_operations.clear();
        self.primitive_bounds.clear();
        self.layer_stack.clear();
        self.paths.clear();
        self.shadows.clear();
        self.quads.clear();
        self.underlines.clear();
        self.monochrome_sprites.clear();
        self.polychrome_sprites.clear();
        self.surfaces.clear();
        self.blur_layers.clear();
        self.blur_stack.clear();
        self.sprite_animations.clear();
        self.animated_sprites.clear();
    }

    pub fn len(&self) -> usize {
        self.paint_operations.len()
    }

    pub fn push_layer(&mut self, bounds: Bounds<ScaledPixels>) {
        if self.blur_stack.is_empty() {
            let order = self.primitive_bounds.insert(bounds);
            self.layer_stack.push(order);
        }
        self.paint_operations
            .push(PaintOperation::StartLayer(bounds));
    }

    pub fn pop_layer(&mut self) {
        if self.blur_stack.is_empty() {
            self.layer_stack.pop();
        }
        self.paint_operations.push(PaintOperation::EndLayer);
    }

    pub fn insert_primitive(&mut self, primitive: impl Into<Primitive>) {
        self.insert_with_animation(primitive.into(), None);
    }

    pub(crate) fn insert_with_animation(
        &mut self,
        mut primitive: Primitive,
        animation: Option<SpriteAnimation>,
    ) {
        if let Primitive::MonochromeSprite(sprite) = &mut primitive {
            // The GPU ignores this padding word. Use it to find animated glyphs
            // after batching sorts the sprite array, without changing its ABI.
            sprite.pad = if let Some(animation) = animation {
                self.sprite_animations.push(animation);
                self.sprite_animations.len() as u32
            } else {
                0
            };
        }
        if self.blur_stack.is_empty() && !self.insert_render_primitive(&mut primitive) {
            return;
        }
        self.paint_operations
            .push(PaintOperation::Primitive(primitive, animation));
    }

    pub(crate) fn has_animations(&self) -> bool {
        !self.animated_sprites.is_empty()
            || self.paths.iter().any(|path| path.animation.is_some())
            || self.blur_layers.iter().any(|layer| {
                layer.reveal.is_some()
                    || matches!(&layer.source, BlurSource::Scene(scene) if scene.has_animations())
            })
    }

    /// Advance only retained paint parameters. Layout, hitboxes, handlers, native
    /// view coverage and text shaping remain exactly those of the rendered frame.
    pub(crate) fn animate(&mut self, now: Instant) {
        for &(sprite, animation) in &self.animated_sprites {
            self.sprite_animations[animation].apply(&mut self.monochrome_sprites[sprite], now);
        }
        for path in &mut self.paths {
            if let Some(animation) = &path.animation {
                // Batching reserves the full animation region. Rendering uses
                // each frame's tight bounds for correct gradients and compositing.
                path.vertices = (animation.draw)(now).map_or_else(Vec::new, |mut frame| {
                    path.bounds =
                        Bounds::new(frame.bounds.origin + animation.offset, frame.bounds.size);
                    path.color = frame.color;
                    for vertex in &mut frame.vertices {
                        vertex.xy_position += animation.offset;
                    }
                    frame.vertices
                });
            }
        }
        for layer in &mut self.blur_layers {
            if let Some(reveal) = layer.reveal {
                let elapsed = now.saturating_duration_since(reveal.started);
                let remaining =
                    (1. - (elapsed.as_secs_f32() / reveal.duration.as_secs_f32()).min(1.)).powi(2);
                layer.radius = reveal.radius * remaining;
                layer.opacity = reveal.opacity * (1. - remaining);
                if elapsed >= reveal.duration {
                    layer.reveal = None;
                }
            }
            if let BlurSource::Scene(scene) = &mut layer.source
                && scene.has_animations()
            {
                Arc::make_mut(scene).animate(now);
            }
        }
    }

    fn insert_render_primitive(&mut self, primitive: &mut Primitive) -> bool {
        let bounds = match primitive {
            Primitive::MonochromeSprite(sprite) => sprite
                .pad
                .checked_sub(1)
                .and_then(|index| self.sprite_animations[index as usize].rotation_bounds(sprite))
                .unwrap_or(sprite.bounds),
            _ => *primitive.bounds(),
        };
        let clipped_bounds = bounds.intersect(&primitive.content_mask().bounds);

        if clipped_bounds.is_empty() {
            return false;
        }

        // Blur can overlap otherwise disjoint geometry in a paint_layer. Once
        // effects enter this scene, use actual primitive bounds for ordering.
        let order = if matches!(primitive, Primitive::Blur(_)) || !self.blur_layers.is_empty() {
            // A backdrop also depends on earlier pixels in its sampling halo.
            let ordering_bounds = match primitive {
                Primitive::Blur(layer) if matches!(layer.source, BlurSource::Backdrop) => {
                    layer.bounds
                }
                _ => clipped_bounds,
            };
            self.primitive_bounds.insert(ordering_bounds)
        } else {
            self.layer_stack
                .last()
                .copied()
                .unwrap_or_else(|| self.primitive_bounds.insert(clipped_bounds))
        };
        match primitive {
            Primitive::Shadow(shadow) => {
                shadow.order = order;
                self.shadows.push(shadow.clone());
            }
            Primitive::Quad(quad) => {
                quad.order = order;
                self.quads.push(quad.clone());
            }
            Primitive::Path(path) => {
                path.order = order;
                path.id = PathId(self.paths.len());
                self.paths.push(path.clone());
            }
            Primitive::Underline(underline) => {
                underline.order = order;
                self.underlines.push(underline.clone());
            }
            Primitive::MonochromeSprite(sprite) => {
                sprite.order = order;
                self.monochrome_sprites.push(sprite.clone());
            }
            Primitive::PolychromeSprite(sprite) => {
                sprite.order = order;
                self.polychrome_sprites.push(sprite.clone());
            }
            Primitive::Surface(surface) => {
                surface.order = order;
                self.surfaces.push(surface.clone());
            }
            Primitive::Blur(layer) => {
                layer.order = order;
                self.blur_layers.push(layer.clone());
            }
        }
        true
    }

    pub fn push_blur(
        &mut self,
        radius: ScaledPixels,
        opacity: f32,
        mask: ContentMask<ScaledPixels>,
        reveal: Option<(Instant, Duration)>,
    ) {
        self.blur_stack.push(self.len());
        self.paint_operations.push(PaintOperation::StartBlur {
            radius,
            opacity,
            mask,
            reveal,
        });
    }

    pub fn pop_blur(&mut self) {
        let start = self.blur_stack.pop().expect("unbalanced blur layer");
        if self.blur_stack.is_empty() {
            let PaintOperation::StartBlur {
                radius,
                opacity,
                ref mask,
                reveal,
            } = self.paint_operations[start]
            else {
                unreachable!();
            };
            let mask = mask.clone();
            let mut scene = Scene::default();
            scene.replay(start + 1..self.len(), self);
            scene.finish();
            let bounds = scene
                .paint_operations
                .iter()
                .filter_map(|op| match op {
                    PaintOperation::Primitive(p, animation) => {
                        let bounds = match p {
                            Primitive::MonochromeSprite(sprite) => animation
                                .and_then(|animation| animation.rotation_bounds(sprite))
                                .unwrap_or_else(|| p.paint_bounds()),
                            _ => p.paint_bounds(),
                        };
                        Some(bounds.intersect(&p.content_mask().bounds))
                    }
                    _ => None,
                })
                .chain(
                    scene
                        .blur_layers
                        .iter()
                        .map(|layer| layer.bounds.intersect(&layer.content_mask.bounds)),
                )
                .filter(|bounds| !bounds.is_empty())
                .reduce(|a, b| a.union(&b));
            if let Some(bounds) = bounds {
                // Three standard deviations contain 99.7% of the Gaussian kernel.
                let bounds = bounds.dilate(radius * 3.0);
                self.insert_render_primitive(&mut Primitive::Blur(BlurLayer {
                    order: 0,
                    bounds,
                    content_mask: mask,
                    radius,
                    opacity,
                    source: BlurSource::Scene(Arc::new(scene)),
                    corners: None,
                    fade: None,
                    reveal: reveal.map(|(started, duration)| BlurReveal {
                        started,
                        duration,
                        radius,
                        opacity,
                    }),
                }));
            }
        }
        self.paint_operations.push(PaintOperation::EndBlur);
    }

    pub fn replay(&mut self, range: Range<usize>, prev_scene: &Scene) {
        for operation in &prev_scene.paint_operations[range] {
            match operation {
                PaintOperation::Primitive(primitive, animation) => {
                    self.insert_with_animation(primitive.clone(), *animation)
                }
                PaintOperation::StartLayer(bounds) => self.push_layer(*bounds),
                PaintOperation::EndLayer => self.pop_layer(),
                PaintOperation::StartBlur {
                    radius,
                    opacity,
                    mask,
                    reveal,
                } => self.push_blur(*radius, *opacity, mask.clone(), *reveal),
                PaintOperation::EndBlur => self.pop_blur(),
            }
        }
    }

    pub fn finish(&mut self) {
        self.shadows.sort_by_key(|shadow| shadow.order);
        self.quads.sort_by_key(|quad| quad.order);
        self.paths.sort_by_key(|path| path.order);
        self.underlines.sort_by_key(|underline| underline.order);
        self.monochrome_sprites
            .sort_by_key(|sprite| (sprite.order, sprite.tile.tile_id));
        self.polychrome_sprites
            .sort_by_key(|sprite| (sprite.order, sprite.tile.tile_id));
        self.surfaces.sort_by_key(|surface| surface.order);
        self.blur_layers.sort_by_key(|layer| layer.order);
        self.animated_sprites.clear();
        self.animated_sprites
            .extend(
                self.monochrome_sprites
                    .iter()
                    .enumerate()
                    .filter_map(|(index, sprite)| {
                        sprite
                            .pad
                            .checked_sub(1)
                            .map(|animation| (index, animation as usize))
                    }),
            );
    }

    /// Rebase a captured scene into a small offscreen render target. Replay keeps
    /// nested effects and cached paint ranges independent of render-target coordinates.
    pub fn translated(&self, offset: Point<ScaledPixels>) -> Self {
        let mut scene = Self::default();
        for operation in &self.paint_operations {
            match operation {
                PaintOperation::Primitive(primitive, animation) => {
                    let mut primitive = primitive.clone();
                    match &mut primitive {
                        Primitive::Shadow(p) => {
                            p.bounds.origin += offset;
                            p.content_mask.bounds.origin += offset;
                        }
                        Primitive::Quad(p) => {
                            p.bounds.origin += offset;
                            p.content_mask.bounds.origin += offset;
                        }
                        Primitive::Underline(p) => {
                            p.bounds.origin += offset;
                            p.content_mask.bounds.origin += offset;
                        }
                        Primitive::PolychromeSprite(p) => {
                            p.bounds.origin += offset;
                            p.content_mask.bounds.origin += offset;
                        }
                        Primitive::Surface(p) => {
                            p.bounds.origin += offset;
                            p.content_mask.bounds.origin += offset;
                        }
                        Primitive::MonochromeSprite(p) => {
                            p.bounds.origin += offset;
                            p.content_mask.bounds.origin += offset;
                            p.shimmer.center += offset.x.0;
                            let [x, y] = [offset.x.0, offset.y.0];
                            let m = p.transformation.rotation_scale;
                            p.transformation.translation[0] += x - m[0][0] * x - m[0][1] * y;
                            p.transformation.translation[1] += y - m[1][0] * x - m[1][1] * y;
                        }
                        Primitive::Path(p) => {
                            if let Some(animation) = &mut p.animation {
                                animation.offset += offset;
                            }
                            p.bounds.origin += offset;
                            p.content_mask.bounds.origin += offset;
                            for vertex in &mut p.vertices {
                                vertex.xy_position += offset;
                            }
                        }
                        Primitive::Blur(p) => {
                            assert!(matches!(p.source, BlurSource::Backdrop));
                            p.bounds.origin += offset;
                            p.content_mask.bounds.origin += offset;
                            if let Some((bounds, _)) = &mut p.corners {
                                bounds.origin += offset;
                            }
                            if let Some(fade) = &mut p.fade {
                                fade.origin += offset;
                            }
                        }
                    }
                    scene.insert_with_animation(
                        primitive,
                        animation.map(|animation| animation.translated(offset)),
                    );
                }
                PaintOperation::StartLayer(bounds) => {
                    scene.push_layer(Bounds::new(bounds.origin + offset, bounds.size))
                }
                PaintOperation::EndLayer => scene.pop_layer(),
                PaintOperation::StartBlur {
                    radius,
                    opacity,
                    mask,
                    reveal,
                } => scene.push_blur(
                    *radius,
                    *opacity,
                    ContentMask {
                        bounds: Bounds::new(mask.bounds.origin + offset, mask.bounds.size),
                    },
                    *reveal,
                ),
                PaintOperation::EndBlur => scene.pop_blur(),
            }
        }
        scene.finish();
        scene.animate(Instant::now());
        scene
    }

    #[cfg_attr(
        all(
            any(target_os = "linux", target_os = "freebsd"),
            not(any(feature = "x11", feature = "wayland"))
        ),
        allow(dead_code)
    )]
    pub(crate) fn batches(&self) -> impl Iterator<Item = PrimitiveBatch<'_>> {
        BatchIterator {
            shadows: &self.shadows,
            shadows_start: 0,
            shadows_iter: self.shadows.iter().peekable(),
            quads: &self.quads,
            quads_start: 0,
            quads_iter: self.quads.iter().peekable(),
            paths: &self.paths,
            paths_start: 0,
            paths_iter: self.paths.iter().peekable(),
            underlines: &self.underlines,
            underlines_start: 0,
            underlines_iter: self.underlines.iter().peekable(),
            monochrome_sprites: &self.monochrome_sprites,
            monochrome_sprites_start: 0,
            monochrome_sprites_iter: self.monochrome_sprites.iter().peekable(),
            polychrome_sprites: &self.polychrome_sprites,
            polychrome_sprites_start: 0,
            polychrome_sprites_iter: self.polychrome_sprites.iter().peekable(),
            surfaces: &self.surfaces,
            surfaces_start: 0,
            surfaces_iter: self.surfaces.iter().peekable(),
            blur_layers: self.blur_layers.iter().peekable(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Default)]
#[cfg_attr(
    all(
        any(target_os = "linux", target_os = "freebsd"),
        not(any(feature = "x11", feature = "wayland"))
    ),
    allow(dead_code)
)]
pub(crate) enum PrimitiveKind {
    Shadow,
    #[default]
    Quad,
    Path,
    Underline,
    MonochromeSprite,
    PolychromeSprite,
    Surface,
    Blur,
}

#[derive(Clone, Debug)]
pub(crate) enum PaintOperation {
    Primitive(Primitive, Option<SpriteAnimation>),
    StartLayer(Bounds<ScaledPixels>),
    EndLayer,
    StartBlur {
        radius: ScaledPixels,
        opacity: f32,
        mask: ContentMask<ScaledPixels>,
        reveal: Option<(Instant, Duration)>,
    },
    EndBlur,
}

#[derive(Clone, Debug)]
pub(crate) enum Primitive {
    Shadow(Shadow),
    Quad(Quad),
    Path(Path<ScaledPixels>),
    Underline(Underline),
    MonochromeSprite(MonochromeSprite),
    PolychromeSprite(PolychromeSprite),
    Surface(PaintSurface),
    Blur(BlurLayer),
}

/// The pixels a blur samples: isolated foreground content or the current target.
#[derive(Clone, Debug)]
pub(crate) enum BlurSource {
    Scene(Arc<Scene>),
    Backdrop,
}

/// A finite reveal retains its original paint values while presentation advances.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BlurReveal {
    started: Instant,
    duration: Duration,
    radius: ScaledPixels,
    opacity: f32,
}

/// A bounded region, composited after a separable Gaussian blur.
#[derive(Clone, Debug)]
pub(crate) struct BlurLayer {
    pub reveal: Option<BlurReveal>,
    pub order: DrawOrder,
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub radius: ScaledPixels,
    pub opacity: f32,
    pub source: BlurSource,
    /// Rounds the composited region to these bounds and corner radius, taken before clipping.
    pub corners: Option<(Bounds<ScaledPixels>, ScaledPixels)>,
    /// Unclipped fade bounds: sharp at the top and sides, blurred toward the bottom center.
    pub fade: Option<Bounds<ScaledPixels>>,
}

impl Primitive {
    /// Extents emitted by the shaders, including shadow spread and SVG transforms.
    fn paint_bounds(&self) -> Bounds<ScaledPixels> {
        match self {
            Primitive::Shadow(shadow) => shadow.bounds.dilate(shadow.blur_radius * 3.0),
            Primitive::MonochromeSprite(sprite) => {
                let bounds = sprite.bounds;
                [
                    bounds.origin,
                    bounds.top_right(),
                    bounds.bottom_left(),
                    bounds.bottom_right(),
                ]
                .into_iter()
                .map(|corner| {
                    let transformed = sprite
                        .transformation
                        .apply(point(Pixels(corner.x.0), Pixels(corner.y.0)));
                    Bounds::new(
                        point(ScaledPixels(transformed.x.0), ScaledPixels(transformed.y.0)),
                        Size::default(),
                    )
                })
                .reduce(|a, b| a.union(&b))
                .unwrap()
            }
            _ => *self.bounds(),
        }
    }

    pub fn bounds(&self) -> &Bounds<ScaledPixels> {
        match self {
            Primitive::Shadow(shadow) => &shadow.bounds,
            Primitive::Quad(quad) => &quad.bounds,
            Primitive::Path(path) => &path.bounds,
            Primitive::Underline(underline) => &underline.bounds,
            Primitive::MonochromeSprite(sprite) => &sprite.bounds,
            Primitive::PolychromeSprite(sprite) => &sprite.bounds,
            Primitive::Surface(surface) => &surface.bounds,
            Primitive::Blur(layer) => &layer.bounds,
        }
    }

    pub fn content_mask(&self) -> &ContentMask<ScaledPixels> {
        match self {
            Primitive::Shadow(shadow) => &shadow.content_mask,
            Primitive::Quad(quad) => &quad.content_mask,
            Primitive::Path(path) => &path.content_mask,
            Primitive::Underline(underline) => &underline.content_mask,
            Primitive::MonochromeSprite(sprite) => &sprite.content_mask,
            Primitive::PolychromeSprite(sprite) => &sprite.content_mask,
            Primitive::Surface(surface) => &surface.content_mask,
            Primitive::Blur(layer) => &layer.content_mask,
        }
    }
}

#[cfg_attr(
    all(
        any(target_os = "linux", target_os = "freebsd"),
        not(any(feature = "x11", feature = "wayland"))
    ),
    allow(dead_code)
)]
struct BatchIterator<'a> {
    shadows: &'a [Shadow],
    shadows_start: usize,
    shadows_iter: Peekable<slice::Iter<'a, Shadow>>,
    quads: &'a [Quad],
    quads_start: usize,
    quads_iter: Peekable<slice::Iter<'a, Quad>>,
    paths: &'a [Path<ScaledPixels>],
    paths_start: usize,
    paths_iter: Peekable<slice::Iter<'a, Path<ScaledPixels>>>,
    underlines: &'a [Underline],
    underlines_start: usize,
    underlines_iter: Peekable<slice::Iter<'a, Underline>>,
    monochrome_sprites: &'a [MonochromeSprite],
    monochrome_sprites_start: usize,
    monochrome_sprites_iter: Peekable<slice::Iter<'a, MonochromeSprite>>,
    polychrome_sprites: &'a [PolychromeSprite],
    polychrome_sprites_start: usize,
    polychrome_sprites_iter: Peekable<slice::Iter<'a, PolychromeSprite>>,
    surfaces: &'a [PaintSurface],
    surfaces_start: usize,
    surfaces_iter: Peekable<slice::Iter<'a, PaintSurface>>,
    blur_layers: Peekable<slice::Iter<'a, BlurLayer>>,
}

impl<'a> Iterator for BatchIterator<'a> {
    type Item = PrimitiveBatch<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut orders_and_kinds = [
            (
                self.shadows_iter.peek().map(|s| s.order),
                PrimitiveKind::Shadow,
            ),
            (self.quads_iter.peek().map(|q| q.order), PrimitiveKind::Quad),
            (self.paths_iter.peek().map(|q| q.order), PrimitiveKind::Path),
            (
                self.underlines_iter.peek().map(|u| u.order),
                PrimitiveKind::Underline,
            ),
            (
                self.monochrome_sprites_iter.peek().map(|s| s.order),
                PrimitiveKind::MonochromeSprite,
            ),
            (
                self.polychrome_sprites_iter.peek().map(|s| s.order),
                PrimitiveKind::PolychromeSprite,
            ),
            (
                self.surfaces_iter.peek().map(|s| s.order),
                PrimitiveKind::Surface,
            ),
            (
                self.blur_layers.peek().map(|layer| layer.order),
                PrimitiveKind::Blur,
            ),
        ];
        orders_and_kinds.sort_by_key(|(order, kind)| (order.unwrap_or(u32::MAX), *kind));

        let first = orders_and_kinds[0];
        let second = orders_and_kinds[1];
        let (batch_kind, max_order_and_kind) = if first.0.is_some() {
            (first.1, (second.0.unwrap_or(u32::MAX), second.1))
        } else {
            return None;
        };

        match batch_kind {
            PrimitiveKind::Blur => self.blur_layers.next().map(PrimitiveBatch::Blur),
            PrimitiveKind::Shadow => {
                let shadows_start = self.shadows_start;
                let mut shadows_end = shadows_start + 1;
                self.shadows_iter.next();
                while self
                    .shadows_iter
                    .next_if(|shadow| (shadow.order, batch_kind) < max_order_and_kind)
                    .is_some()
                {
                    shadows_end += 1;
                }
                self.shadows_start = shadows_end;
                Some(PrimitiveBatch::Shadows(
                    &self.shadows[shadows_start..shadows_end],
                ))
            }
            PrimitiveKind::Quad => {
                let quads_start = self.quads_start;
                let mut quads_end = quads_start + 1;
                self.quads_iter.next();
                while self
                    .quads_iter
                    .next_if(|quad| (quad.order, batch_kind) < max_order_and_kind)
                    .is_some()
                {
                    quads_end += 1;
                }
                self.quads_start = quads_end;
                Some(PrimitiveBatch::Quads(&self.quads[quads_start..quads_end]))
            }
            PrimitiveKind::Path => {
                let paths_start = self.paths_start;
                let mut paths_end = paths_start + 1;
                self.paths_iter.next();
                while self
                    .paths_iter
                    .next_if(|path| (path.order, batch_kind) < max_order_and_kind)
                    .is_some()
                {
                    paths_end += 1;
                }
                self.paths_start = paths_end;
                Some(PrimitiveBatch::Paths(&self.paths[paths_start..paths_end]))
            }
            PrimitiveKind::Underline => {
                let underlines_start = self.underlines_start;
                let mut underlines_end = underlines_start + 1;
                self.underlines_iter.next();
                while self
                    .underlines_iter
                    .next_if(|underline| (underline.order, batch_kind) < max_order_and_kind)
                    .is_some()
                {
                    underlines_end += 1;
                }
                self.underlines_start = underlines_end;
                Some(PrimitiveBatch::Underlines(
                    &self.underlines[underlines_start..underlines_end],
                ))
            }
            PrimitiveKind::MonochromeSprite => {
                let texture_id = self.monochrome_sprites_iter.peek().unwrap().tile.texture_id;
                let sprites_start = self.monochrome_sprites_start;
                let mut sprites_end = sprites_start + 1;
                self.monochrome_sprites_iter.next();
                while self
                    .monochrome_sprites_iter
                    .next_if(|sprite| {
                        (sprite.order, batch_kind) < max_order_and_kind
                            && sprite.tile.texture_id == texture_id
                    })
                    .is_some()
                {
                    sprites_end += 1;
                }
                self.monochrome_sprites_start = sprites_end;
                Some(PrimitiveBatch::MonochromeSprites {
                    texture_id,
                    sprites: &self.monochrome_sprites[sprites_start..sprites_end],
                })
            }
            PrimitiveKind::PolychromeSprite => {
                let texture_id = self.polychrome_sprites_iter.peek().unwrap().tile.texture_id;
                let sprites_start = self.polychrome_sprites_start;
                let mut sprites_end = self.polychrome_sprites_start + 1;
                self.polychrome_sprites_iter.next();
                while self
                    .polychrome_sprites_iter
                    .next_if(|sprite| {
                        (sprite.order, batch_kind) < max_order_and_kind
                            && sprite.tile.texture_id == texture_id
                    })
                    .is_some()
                {
                    sprites_end += 1;
                }
                self.polychrome_sprites_start = sprites_end;
                Some(PrimitiveBatch::PolychromeSprites {
                    texture_id,
                    sprites: &self.polychrome_sprites[sprites_start..sprites_end],
                })
            }
            PrimitiveKind::Surface => {
                let surfaces_start = self.surfaces_start;
                let mut surfaces_end = surfaces_start + 1;
                self.surfaces_iter.next();
                while self
                    .surfaces_iter
                    .next_if(|surface| (surface.order, batch_kind) < max_order_and_kind)
                    .is_some()
                {
                    surfaces_end += 1;
                }
                self.surfaces_start = surfaces_end;
                Some(PrimitiveBatch::Surfaces(
                    &self.surfaces[surfaces_start..surfaces_end],
                ))
            }
        }
    }
}

#[derive(Debug)]
#[cfg_attr(
    all(
        any(target_os = "linux", target_os = "freebsd"),
        not(any(feature = "x11", feature = "wayland"))
    ),
    allow(dead_code)
)]
pub(crate) enum PrimitiveBatch<'a> {
    Shadows(&'a [Shadow]),
    Quads(&'a [Quad]),
    Paths(&'a [Path<ScaledPixels>]),
    Underlines(&'a [Underline]),
    MonochromeSprites {
        texture_id: AtlasTextureId,
        sprites: &'a [MonochromeSprite],
    },
    PolychromeSprites {
        texture_id: AtlasTextureId,
        sprites: &'a [PolychromeSprite],
    },
    Surfaces(&'a [PaintSurface]),
    Blur(&'a BlurLayer),
}

#[derive(Default, Debug, Clone)]
#[repr(C)]
pub(crate) struct Quad {
    pub order: DrawOrder,
    pub border_style: BorderStyle,
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub background: Background,
    pub border_color: Hsla,
    pub corner_radii: Corners<ScaledPixels>,
    pub border_widths: Edges<ScaledPixels>,
}

impl From<Quad> for Primitive {
    fn from(quad: Quad) -> Self {
        Primitive::Quad(quad)
    }
}

#[derive(Debug, Clone)]
#[repr(C)]
pub(crate) struct Underline {
    pub order: DrawOrder,
    pub pad: u32, // align to 8 bytes
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub color: Hsla,
    pub thickness: ScaledPixels,
    pub wavy: u32,
}

impl From<Underline> for Primitive {
    fn from(underline: Underline) -> Self {
        Primitive::Underline(underline)
    }
}

#[derive(Debug, Clone)]
#[repr(C)]
pub(crate) struct Shadow {
    pub order: DrawOrder,
    pub blur_radius: ScaledPixels,
    pub bounds: Bounds<ScaledPixels>,
    pub corner_radii: Corners<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub color: Hsla,
}

impl From<Shadow> for Primitive {
    fn from(shadow: Shadow) -> Self {
        Primitive::Shadow(shadow)
    }
}

/// The style of a border.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[repr(C)]
pub enum BorderStyle {
    /// A solid border.
    #[default]
    Solid = 0,
    /// A dashed border.
    Dashed = 1,
}

/// A data type representing a 2 dimensional transformation that can be applied to an element.
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(C)]
pub struct TransformationMatrix {
    /// 2x2 matrix containing rotation and scale,
    /// stored row-major
    pub rotation_scale: [[f32; 2]; 2],
    /// translation vector
    pub translation: [f32; 2],
}

impl Eq for TransformationMatrix {}

impl TransformationMatrix {
    /// The unit matrix, has no effect.
    pub fn unit() -> Self {
        Self {
            rotation_scale: [[1.0, 0.0], [0.0, 1.0]],
            translation: [0.0, 0.0],
        }
    }

    /// Move the origin by a given point
    pub fn translate(mut self, point: Point<ScaledPixels>) -> Self {
        self.compose(Self {
            rotation_scale: [[1.0, 0.0], [0.0, 1.0]],
            translation: [point.x.0, point.y.0],
        })
    }

    /// Clockwise rotation in radians around the origin
    pub fn rotate(self, angle: Radians) -> Self {
        self.compose(Self {
            rotation_scale: [
                [angle.0.cos(), -angle.0.sin()],
                [angle.0.sin(), angle.0.cos()],
            ],
            translation: [0.0, 0.0],
        })
    }

    /// Scale around the origin
    pub fn scale(self, size: Size<f32>) -> Self {
        self.compose(Self {
            rotation_scale: [[size.width, 0.0], [0.0, size.height]],
            translation: [0.0, 0.0],
        })
    }

    /// Perform matrix multiplication with another transformation
    /// to produce a new transformation that is the result of
    /// applying both transformations: first, `other`, then `self`.
    #[inline]
    pub fn compose(self, other: TransformationMatrix) -> TransformationMatrix {
        if other == Self::unit() {
            return self;
        }
        // Perform matrix multiplication
        TransformationMatrix {
            rotation_scale: [
                [
                    self.rotation_scale[0][0] * other.rotation_scale[0][0]
                        + self.rotation_scale[0][1] * other.rotation_scale[1][0],
                    self.rotation_scale[0][0] * other.rotation_scale[0][1]
                        + self.rotation_scale[0][1] * other.rotation_scale[1][1],
                ],
                [
                    self.rotation_scale[1][0] * other.rotation_scale[0][0]
                        + self.rotation_scale[1][1] * other.rotation_scale[1][0],
                    self.rotation_scale[1][0] * other.rotation_scale[0][1]
                        + self.rotation_scale[1][1] * other.rotation_scale[1][1],
                ],
            ],
            translation: [
                self.translation[0]
                    + self.rotation_scale[0][0] * other.translation[0]
                    + self.rotation_scale[0][1] * other.translation[1],
                self.translation[1]
                    + self.rotation_scale[1][0] * other.translation[0]
                    + self.rotation_scale[1][1] * other.translation[1],
            ],
        }
    }

    /// Apply transformation to a point, mainly useful for debugging
    pub fn apply(&self, point: Point<Pixels>) -> Point<Pixels> {
        let input = [point.x.0, point.y.0];
        let mut output = self.translation;
        for (i, output_cell) in output.iter_mut().enumerate() {
            for (k, input_cell) in input.iter().enumerate() {
                *output_cell += self.rotation_scale[i][k] * *input_cell;
            }
        }
        Point::new(output[0].into(), output[1].into())
    }
}

impl Default for TransformationMatrix {
    fn default() -> Self {
        Self::unit()
    }
}

/// Paint parameters retained alongside a monochrome sprite without changing its GPU ABI.
#[derive(Clone, Copy, Debug)]
pub(crate) enum SpriteAnimation {
    Shimmer(ShimmerAnimation),
    Rotation {
        started: Instant,
        period: Duration,
        center: Point<ScaledPixels>,
        base: TransformationMatrix,
    },
}

impl SpriteAnimation {
    fn apply(self, sprite: &mut MonochromeSprite, now: Instant) {
        match self {
            Self::Shimmer(shimmer) => sprite.shimmer.center = shimmer.center(now),
            Self::Rotation {
                started,
                period,
                center,
                base,
            } => {
                let phase = (now.saturating_duration_since(started).as_secs_f64()
                    / period.as_secs_f64())
                .fract() as f32;
                sprite.transformation = base
                    .translate(center)
                    .rotate(crate::radians(phase * std::f32::consts::TAU))
                    .translate(point(ScaledPixels(-center.x.0), ScaledPixels(-center.y.0)));
            }
        }
    }

    fn translated(self, offset: Point<ScaledPixels>) -> Self {
        match self {
            Self::Shimmer(mut shimmer) => {
                shimmer.left += offset.x.0;
                Self::Shimmer(shimmer)
            }
            Self::Rotation {
                started,
                period,
                center,
                base,
            } => Self::Rotation {
                started,
                period,
                center: center + offset,
                base: TransformationMatrix::unit()
                    .translate(offset)
                    .compose(base)
                    .translate(point(ScaledPixels(-offset.x.0), ScaledPixels(-offset.y.0))),
            },
        }
    }

    fn rotation_bounds(self, sprite: &MonochromeSprite) -> Option<Bounds<ScaledPixels>> {
        let Self::Rotation { center, base, .. } = self else {
            return None;
        };
        let radius = [
            sprite.bounds.origin,
            sprite.bounds.top_right(),
            sprite.bounds.bottom_left(),
            sprite.bounds.bottom_right(),
        ]
        .into_iter()
        .map(|corner| (corner.x.0 - center.x.0).hypot(corner.y.0 - center.y.0))
        .fold(0., f32::max);
        let center = base.apply(point(Pixels(center.x.0), Pixels(center.y.0)));
        let [x, y] = base.rotation_scale;
        let rx = radius * x[0].hypot(x[1]);
        let ry = radius * y[0].hypot(y[1]);
        Some(Bounds::new(
            point(ScaledPixels(center.x.0 - rx), ScaledPixels(center.y.0 - ry)),
            crate::size(ScaledPixels(2. * rx), ScaledPixels(2. * ry)),
        ))
    }
}

/// Timing and sweep bounds stay on the CPU; only the highlight center changes
/// in the existing glyph shader when a retained scene is presented again.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ShimmerAnimation {
    pub started: Instant,
    pub period: Duration,
    pub left: f32,
    pub width: f32,
}

impl ShimmerAnimation {
    fn center(self, now: Instant) -> f32 {
        let phase = (now.saturating_duration_since(self.started).as_secs_f64()
            / self.period.as_secs_f64())
        .fract() as f32;
        self.left + phase * self.width
    }
}

/// Paint-only highlight parameters in device pixels. A zero spread disables it.
#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
pub(crate) struct GlyphShimmer {
    pub color: Hsla,
    pub center: f32,
    pub spread: f32,
}

#[derive(Clone, Debug)]
#[repr(C)]
pub(crate) struct MonochromeSprite {
    pub order: DrawOrder,
    pub pad: u32, // align to 8 bytes
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub color: Hsla,
    pub tile: AtlasTile,
    pub transformation: TransformationMatrix,
    pub shimmer: GlyphShimmer,
}

impl From<MonochromeSprite> for Primitive {
    fn from(sprite: MonochromeSprite) -> Self {
        Primitive::MonochromeSprite(sprite)
    }
}

#[derive(Clone, Debug)]
#[repr(C)]
pub(crate) struct PolychromeSprite {
    pub order: DrawOrder,
    pub pad: u32, // align to 8 bytes
    pub grayscale: bool,
    pub opacity: f32,
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    pub corner_radii: Corners<ScaledPixels>,
    pub tile: AtlasTile,
}

impl From<PolychromeSprite> for Primitive {
    fn from(sprite: PolychromeSprite) -> Self {
        Primitive::PolychromeSprite(sprite)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PaintSurface {
    pub order: DrawOrder,
    pub bounds: Bounds<ScaledPixels>,
    pub content_mask: ContentMask<ScaledPixels>,
    #[cfg(target_os = "macos")]
    pub image_buffer: core_video::pixel_buffer::CVPixelBuffer,
    /// Clockwise quarter turns applied to the image.
    pub turns: u32,
}

impl From<PaintSurface> for Primitive {
    fn from(surface: PaintSurface) -> Self {
        Primitive::Surface(surface)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PathId(pub(crate) usize);

#[derive(Clone)]
pub(crate) struct PathAnimation {
    pub draw: Arc<dyn Fn(Instant) -> Option<Path<ScaledPixels>> + Send + Sync>,
    pub offset: Point<ScaledPixels>,
}

impl Debug for PathAnimation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PathAnimation")
            .field("offset", &self.offset)
            .finish_non_exhaustive()
    }
}

/// A line made up of a series of vertices and control points.
#[derive(Clone, Debug)]
pub struct Path<P: Clone + Debug + Default + PartialEq> {
    pub(crate) id: PathId,
    pub(crate) order: DrawOrder,
    pub(crate) bounds: Bounds<P>,
    pub(crate) content_mask: ContentMask<P>,
    pub(crate) vertices: Vec<PathVertex<P>>,
    pub(crate) color: Background,
    pub(crate) animation: Option<PathAnimation>,
    start: Point<P>,
    current: Point<P>,
    contour_count: usize,
}

impl Path<Pixels> {
    /// Create a new path with the given starting point.
    pub fn new(start: Point<Pixels>) -> Self {
        Self {
            id: PathId(0),
            order: DrawOrder::default(),
            vertices: Vec::new(),
            start,
            current: start,
            bounds: Bounds {
                origin: start,
                size: Default::default(),
            },
            content_mask: Default::default(),
            color: Default::default(),
            contour_count: 0,
            animation: None,
        }
    }

    /// Scale this path by the given factor.
    pub fn scale(&self, factor: f32) -> Path<ScaledPixels> {
        Path {
            id: self.id,
            order: self.order,
            bounds: self.bounds.scale(factor),
            content_mask: self.content_mask.scale(factor),
            vertices: self
                .vertices
                .iter()
                .map(|vertex| vertex.scale(factor))
                .collect(),
            start: self.start.map(|start| start.scale(factor)),
            current: self.current.scale(factor),
            contour_count: self.contour_count,
            color: self.color,
            animation: self.animation.clone(),
        }
    }

    /// Move the start, current point to the given point.
    pub fn move_to(&mut self, to: Point<Pixels>) {
        self.contour_count += 1;
        self.start = to;
        self.current = to;
    }

    /// Draw a straight line from the current point to the given point.
    pub fn line_to(&mut self, to: Point<Pixels>) {
        self.contour_count += 1;
        if self.contour_count > 1 {
            self.push_triangle(
                (self.start, self.current, to),
                (point(0., 1.), point(0., 1.), point(0., 1.)),
            );
        }
        self.current = to;
    }

    /// Draw a curve from the current point to the given point, using the given control point.
    pub fn curve_to(&mut self, to: Point<Pixels>, ctrl: Point<Pixels>) {
        self.contour_count += 1;
        if self.contour_count > 1 {
            self.push_triangle(
                (self.start, self.current, to),
                (point(0., 1.), point(0., 1.), point(0., 1.)),
            );
        }

        self.push_triangle(
            (self.current, ctrl, to),
            (point(0., 0.), point(0.5, 0.), point(1., 1.)),
        );
        self.current = to;
    }

    /// Push a triangle to the Path.
    pub fn push_triangle(
        &mut self,
        xy: (Point<Pixels>, Point<Pixels>, Point<Pixels>),
        st: (Point<f32>, Point<f32>, Point<f32>),
    ) {
        self.bounds = self
            .bounds
            .union(&Bounds {
                origin: xy.0,
                size: Default::default(),
            })
            .union(&Bounds {
                origin: xy.1,
                size: Default::default(),
            })
            .union(&Bounds {
                origin: xy.2,
                size: Default::default(),
            });

        self.vertices.push(PathVertex {
            xy_position: xy.0,
            st_position: st.0,
            content_mask: Default::default(),
        });
        self.vertices.push(PathVertex {
            xy_position: xy.1,
            st_position: st.1,
            content_mask: Default::default(),
        });
        self.vertices.push(PathVertex {
            xy_position: xy.2,
            st_position: st.2,
            content_mask: Default::default(),
        });
    }
}

impl<T> Path<T>
where
    T: Clone + Debug + Default + PartialEq + PartialOrd + Add<T, Output = T> + Sub<Output = T>,
{
    #[allow(unused)]
    pub(crate) fn clipped_bounds(&self) -> Bounds<T> {
        self.bounds.intersect(&self.content_mask.bounds)
    }
}

impl From<Path<ScaledPixels>> for Primitive {
    fn from(path: Path<ScaledPixels>) -> Self {
        Primitive::Path(path)
    }
}

#[derive(Clone, Debug)]
#[repr(C)]
pub(crate) struct PathVertex<P: Clone + Debug + Default + PartialEq> {
    pub(crate) xy_position: Point<P>,
    pub(crate) st_position: Point<f32>,
    pub(crate) content_mask: ContentMask<P>,
}

impl PathVertex<Pixels> {
    pub fn scale(&self, factor: f32) -> PathVertex<ScaledPixels> {
        PathVertex {
            xy_position: self.xy_position.scale(factor),
            st_position: self.st_position,
            content_mask: self.content_mask.scale(factor),
        }
    }
}

#[cfg(test)]
mod retained_animation_tests {
    use super::*;
    use crate::{AtlasTextureKind, DevicePixels, TileId, size};

    fn glyph(x: f32) -> MonochromeSprite {
        let bounds = Bounds::new(
            point(ScaledPixels(x), ScaledPixels(0.)),
            size(ScaledPixels(10.), ScaledPixels(20.)),
        );
        MonochromeSprite {
            order: 0,
            pad: 0,
            bounds,
            content_mask: ContentMask { bounds },
            color: Hsla::white(),
            tile: AtlasTile {
                texture_id: AtlasTextureId {
                    index: 0,
                    kind: AtlasTextureKind::Monochrome,
                },
                tile_id: TileId(0),
                padding: 0,
                bounds: Bounds::new(Point::default(), size(DevicePixels(10), DevicePixels(20))),
            },
            transformation: TransformationMatrix::unit(),
            shimmer: GlyphShimmer {
                color: Hsla::white(),
                center: -20.,
                spread: 20.,
            },
        }
    }

    #[test]
    fn rotating_svg_keeps_its_center_and_timeline_through_blur_replay_and_translation() {
        let started = Instant::now();
        let mut sprite = glyph(0.);
        sprite.content_mask = ContentMask {
            bounds: Bounds::new(
                point(ScaledPixels(-100.), ScaledPixels(-100.)),
                size(ScaledPixels(200.), ScaledPixels(200.)),
            ),
        };
        let animation = SpriteAnimation::Rotation {
            started,
            period: Duration::from_secs(1),
            center: sprite.bounds.center(),
            base: TransformationMatrix::unit(),
        };
        let mut scene = Scene::default();
        scene.push_blur(ScaledPixels(1.), 1., sprite.content_mask.clone(), None);
        scene.insert_with_animation(sprite.into(), Some(animation));
        scene.pop_blur();
        scene.finish();
        // The offscreen target includes the entire sweep, not just its first frame.
        assert!(scene.blur_layers[0].bounds.size.width > ScaledPixels(20.));
        let mut cached = Scene::default();
        cached.replay(0..scene.len(), &scene);
        cached.finish();
        let offset = point(ScaledPixels(-20.), ScaledPixels(4.));
        let mut translated = cached.translated(offset);
        for (scene, offset) in [(&mut cached, Point::default()), (&mut translated, offset)] {
            assert!(scene.has_animations());
            // The same phase in the next lap must not restart at the cached frame.
            scene.animate(started + Duration::from_millis(1250));
            let BlurSource::Scene(inner) = &scene.blur_layers[0].source else {
                panic!("missing blur")
            };
            let sprite = &inner.monochrome_sprites[0];
            let actual = sprite
                .transformation
                .apply(point(Pixels(10. + offset.x.0), Pixels(10. + offset.y.0)));
            assert!((actual.x.0 - (5. + offset.x.0)).abs() < 0.001);
            assert!((actual.y.0 - (15. + offset.y.0)).abs() < 0.001);
        }
        cached.clear();
        assert!(!cached.has_animations());
    }

    #[test]
    fn delayed_reveal_finishes_after_cache_replay_and_translation() {
        let started = Instant::now() + Duration::from_secs(1);
        let duration = Duration::from_secs(2);
        let mut scene = Scene::default();
        scene.push_blur(
            ScaledPixels(4.),
            0.8,
            glyph(0.).content_mask,
            Some((started, duration)),
        );
        scene.insert_primitive(glyph(0.));
        scene.pop_blur();
        scene.finish();
        let bounds = scene.blur_layers[0].bounds;
        scene.animate(started - Duration::from_millis(100));
        assert_eq!(scene.blur_layers[0].opacity, 0.);
        assert!(scene.has_animations());

        let mut cached = Scene::default();
        cached.replay(0..scene.len(), &scene);
        cached.finish();
        let offset = point(ScaledPixels(30.), ScaledPixels(10.));
        let mut translated = cached.translated(offset);
        for scene in [&mut cached, &mut translated] {
            scene.animate(started + Duration::from_secs(1));
            assert_eq!(scene.blur_layers[0].radius, ScaledPixels(1.));
            assert!((scene.blur_layers[0].opacity - 0.6).abs() < 0.0001);
            assert!(scene.has_animations());
            scene.animate(started + duration);
            assert_eq!(scene.blur_layers[0].radius, ScaledPixels(0.));
            assert_eq!(scene.blur_layers[0].opacity, 0.8);
            assert!(!scene.has_animations());
        }
        assert_eq!(cached.blur_layers[0].bounds, bounds);
        assert_eq!(
            translated.blur_layers[0].bounds.origin,
            bounds.origin + offset
        );
        // A later ordinary draw must not restart a completed cached reveal.
        let mut replayed = Scene::default();
        replayed.replay(0..cached.len(), &cached);
        replayed.finish();
        replayed.animate(started + duration + Duration::from_secs(1));
        assert_eq!(replayed.blur_layers[0].opacity, 0.8);
        assert!(!replayed.has_animations());
    }

    #[test]
    fn retained_path_advances_after_replay_and_translation_with_stable_order() {
        let started = Instant::now();
        let draw = Arc::new(move |now: Instant| {
            let x = now.saturating_duration_since(started).as_secs_f32() * 10.;
            let mut builder = crate::PathBuilder::stroke(crate::px(1.));
            builder.move_to(point(Pixels(x + 5.), Pixels(5.)));
            builder.line_to(point(Pixels(x + 5.), Pixels(15.)));
            builder.build().ok().map(|mut path| {
                path.color = crate::rgb(if x < 5. { 0xff0000 } else { 0x00ff00 }).into();
                path.scale(1.)
            })
        });
        let mut path = draw(started).unwrap();
        path.bounds = Bounds::new(
            Point::default(),
            size(ScaledPixels(100.), ScaledPixels(20.)),
        );
        path.content_mask = ContentMask {
            bounds: path.bounds,
        };
        path.animation = Some(PathAnimation {
            draw,
            offset: Point::default(),
        });
        let initial = path.vertices[0].xy_position;
        let mut scene = Scene::default();
        scene.insert_primitive(path);
        scene.finish();
        let mut cached = Scene::default();
        cached.replay(0..scene.len(), &scene);
        cached.finish();
        let offset = point(ScaledPixels(-20.), ScaledPixels(4.));
        let mut translated = cached.translated(offset);
        assert!(cached.has_animations());
        cached.animate(started + Duration::from_millis(500));
        translated.animate(started + Duration::from_millis(500));
        assert_eq!(
            cached.paths[0].vertices[0].xy_position,
            initial + point(ScaledPixels(5.), ScaledPixels(0.))
        );
        assert_eq!(
            translated.paths[0].vertices[0].xy_position,
            cached.paths[0].vertices[0].xy_position + offset
        );
        assert_eq!(cached.paths[0].order, scene.paths[0].order);
        assert_eq!(
            cached.paths[0].bounds.intersect(&scene.paths[0].bounds),
            cached.paths[0].bounds
        );
        assert_eq!(cached.paths[0].color, crate::rgb(0x00ff00).into());
        assert_eq!(translated.paths[0].color, cached.paths[0].color);
        assert_eq!(
            translated.paths[0].bounds.origin,
            cached.paths[0].bounds.origin + offset
        );
        cached.clear();
        assert!(!cached.has_animations());
    }

    #[test]
    fn retained_shimmer_survives_batching_cache_replay_and_translation() {
        let started = Instant::now();
        let animation = ShimmerAnimation {
            started,
            period: Duration::from_secs(2),
            left: -20.,
            width: 140.,
        };
        let mut scene = Scene::default();
        scene.insert_with_animation(glyph(0.).into(), Some(SpriteAnimation::Shimmer(animation)));
        scene.insert_primitive(glyph(100.));
        scene.insert_with_animation(glyph(20.).into(), Some(SpriteAnimation::Shimmer(animation)));
        scene.finish();
        let mut cached = Scene::default();
        cached.replay(0..scene.len(), &scene);
        cached.finish();
        let mut translated = cached.translated(point(ScaledPixels(-10.), ScaledPixels(0.)));
        cached.animate(started + Duration::from_millis(500));
        translated.animate(started + Duration::from_millis(500));
        for sprite in &cached.monochrome_sprites {
            assert_eq!(
                sprite.shimmer.center,
                if sprite.bounds.left().0 == 100. {
                    -20.
                } else {
                    15.
                }
            );
        }
        for sprite in &translated.monochrome_sprites {
            assert_eq!(
                sprite.shimmer.center,
                if sprite.bounds.left().0 == 90. {
                    -30.
                } else {
                    5.
                }
            );
        }
        cached.animate(started + Duration::from_millis(2500));
        assert_eq!(cached.monochrome_sprites[0].shimmer.center, 15.);
        cached.clear();
        assert!(!cached.has_animations());
    }

    #[test]
    fn shimmer_in_blurred_content_advances_and_clipped_glyphs_stay_idle() {
        let started = Instant::now();
        let animation = ShimmerAnimation {
            started,
            period: Duration::from_secs(2),
            left: -20.,
            width: 140.,
        };
        let mut scene = Scene::default();
        scene.push_blur(ScaledPixels(2.), 1., glyph(0.).content_mask, None);
        scene.insert_with_animation(glyph(0.).into(), Some(SpriteAnimation::Shimmer(animation)));
        scene.pop_blur();
        scene.finish();
        assert!(scene.has_animations());
        scene.animate(started + Duration::from_secs(1));
        let BlurSource::Scene(inner) = &scene.blur_layers[0].source else {
            panic!("missing content layer")
        };
        assert_eq!(inner.monochrome_sprites[0].shimmer.center, 50.);
        let mut clipped = glyph(0.);
        clipped.content_mask = glyph(100.).content_mask;
        scene.clear();
        scene.insert_with_animation(clipped.into(), Some(SpriteAnimation::Shimmer(animation)));
        scene.finish();
        assert!(!scene.has_animations());
    }
}
