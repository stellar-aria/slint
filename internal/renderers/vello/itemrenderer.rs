// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

use std::cell::RefCell;
use std::collections::HashMap;
use std::pin::Pin;

use i_slint_core::item_rendering::{
    CachedRenderingData, ItemRenderer, RenderBorderRectangle, RenderImage, RenderRectangle,
    RenderText,
};
use i_slint_core::items::ItemRc;
use i_slint_core::lengths::{
    LogicalBorderRadius, LogicalLength, LogicalPoint, LogicalRect, LogicalSize, LogicalVector,
    PhysicalPx, RectLengths,
};
use i_slint_core::textlayout::sharedparley::{self, GlyphRenderer, PhysicalLength, PhysicalRect};
use i_slint_core::window::WindowInner;
use i_slint_core::{Brush, Color};

use peniko::kurbo::{Affine, Rect, RoundedRect};
use peniko::Fill;
use vello::Scene;

// Re-export parley types for use in GlyphRenderer
use parley;
use vello::peniko;

/// Convert a Slint Color to a Peniko Color
pub fn to_peniko_color(color: &Color) -> peniko::Color {
    peniko::Color::from_rgba8(color.red(), color.green(), color.blue(), color.alpha())
}

/// Convert a Slint Brush to a Peniko Color (simplified - only handles solid colors for now)
pub fn brush_to_peniko_color(brush: &Brush) -> Option<peniko::Color> {
    match brush {
        Brush::SolidColor(color) => Some(to_peniko_color(color)),
        _ => None, // Gradients handled separately
    }
}

/// Convert color stops from Slint to Peniko format
fn convert_color_stops(
    stops: impl Iterator<Item = i_slint_core::graphics::GradientStop>,
) -> Vec<peniko::ColorStop> {
    stops
        .map(|stop| {
            let color = to_peniko_color(&stop.color);
            peniko::ColorStop {
                offset: stop.position,
                color: peniko::color::DynamicColor::from_alpha_color(color),
            }
        })
        .collect()
}

/// Convert a Slint Brush to an owned Peniko Brush (handles gradients)
pub fn brush_to_peniko_brush_owned(brush: &Brush, bounds: Rect) -> peniko::Brush {
    match brush {
        Brush::SolidColor(color) => peniko::Brush::Solid(to_peniko_color(color)),
        Brush::LinearGradient(gradient) => {
            let stops = convert_color_stops(gradient.stops().copied());

            // Calculate start and end points based on angle and bounds
            let width = bounds.width();
            let height = bounds.height();

            // Use the line_for_angle helper from Slint
            let (start_point, end_point) = i_slint_core::graphics::line_for_angle(
                gradient.angle(),
                [width as f32, height as f32].into(),
            );

            let start = peniko::kurbo::Point::new(
                bounds.x0 + start_point.x as f64,
                bounds.y0 + start_point.y as f64,
            );
            let end = peniko::kurbo::Point::new(
                bounds.x0 + end_point.x as f64,
                bounds.y0 + end_point.y as f64,
            );

            let grad = peniko::Gradient {
                kind: peniko::LinearGradientPosition::new(start, end).into(),
                extend: peniko::Extend::Pad,
                interpolation_cs: peniko::color::ColorSpaceTag::Srgb,
                hue_direction: peniko::color::HueDirection::default(),
                interpolation_alpha_space: peniko::InterpolationAlphaSpace::default(),
                stops: peniko::ColorStops::from(stops.as_slice()),
            };

            peniko::Brush::Gradient(grad)
        }
        Brush::RadialGradient(gradient) => {
            let stops = convert_color_stops(gradient.stops().copied());

            let width = bounds.width();
            let height = bounds.height();

            // Center of the gradient
            let center =
                peniko::kurbo::Point::new(bounds.x0 + width / 2.0, bounds.y0 + height / 2.0);

            // Radius is half the diagonal
            let radius = ((width * width + height * height).sqrt() / 2.0) as f32;

            let grad = peniko::Gradient {
                kind: peniko::RadialGradientPosition::new(center, radius).into(),
                extend: peniko::Extend::Pad,
                interpolation_cs: peniko::color::ColorSpaceTag::Srgb,
                hue_direction: peniko::color::HueDirection::default(),
                interpolation_alpha_space: peniko::InterpolationAlphaSpace::default(),
                stops: peniko::ColorStops::from(stops.as_slice()),
            };

            peniko::Brush::Gradient(grad)
        }
        _ => {
            // Unknown brush type, fallback to transparent
            peniko::Brush::Solid(peniko::Color::TRANSPARENT)
        }
    }
}

/// Apply alpha multiplier to a peniko Color
fn apply_alpha(color: peniko::Color, alpha: f32) -> peniko::Color {
    // peniko::Color is color::AlphaColor<Srgb>
    // We need to multiply the alpha component
    let rgba = color.to_rgba8();
    peniko::Color::from_rgba8(rgba.r, rgba.g, rgba.b, (rgba.a as f32 * alpha) as u8)
}

/// Rendering state that can be saved and restored
#[derive(Clone)]
struct RenderState {
    clip: LogicalRect,
    transform: Affine,
    alpha: f32,
    /// Number of Vello clip layers pushed in this state
    clip_layer_count: u32,
}

pub struct VelloItemRenderer<'a> {
    scene: &'a mut Scene,
    window_inner: &'a WindowInner,
    scale_factor: f32,
    state_stack: Vec<RenderState>,
    wgpu_texture_reader: Option<&'a dyn Fn(&wgpu_26::Texture) -> Option<(u32, u32, Vec<u8>)>>,
    image_cache: &'a RefCell<HashMap<usize, std::sync::Arc<Vec<u8>>>>,
}

impl<'a> VelloItemRenderer<'a> {
    pub fn new(
        scene: &'a mut Scene,
        window_inner: &'a WindowInner,
        logical_size: LogicalSize,
        image_cache: &'a RefCell<HashMap<usize, std::sync::Arc<Vec<u8>>>>,
    ) -> Self {
        Self::new_with_transform(scene, window_inner, logical_size, image_cache, Affine::IDENTITY)
    }

    pub fn new_with_transform(
        scene: &'a mut Scene,
        window_inner: &'a WindowInner,
        logical_size: LogicalSize,
        image_cache: &'a RefCell<HashMap<usize, std::sync::Arc<Vec<u8>>>>,
        initial_transform: Affine,
    ) -> Self {
        let scale_factor = window_inner.scale_factor();
        let initial_state = RenderState {
            clip: LogicalRect::new(Default::default(), logical_size),
            transform: initial_transform,
            alpha: 1.0,
            clip_layer_count: 0,
        };

        Self {
            scene,
            window_inner,
            scale_factor,
            state_stack: vec![initial_state],
            wgpu_texture_reader: None,
            image_cache,
        }
    }

    /// Set WGPU texture reader callback for reading textures back to CPU
    #[allow(dead_code)]
    pub fn set_wgpu_texture_reader(
        &mut self,
        reader: &'a dyn Fn(&wgpu_26::Texture) -> Option<(u32, u32, Vec<u8>)>,
    ) {
        self.wgpu_texture_reader = Some(reader);
    }

    fn current_state(&self) -> &RenderState {
        self.state_stack.last().unwrap()
    }

    fn current_state_mut(&mut self) -> &mut RenderState {
        self.state_stack.last_mut().unwrap()
    }

    fn to_physical(&self, logical: f32) -> f32 {
        logical * self.scale_factor
    }

    fn to_kurbo_rect(&self, rect: LogicalRect) -> Rect {
        Rect::new(
            self.to_physical(rect.origin.x) as f64,
            self.to_physical(rect.origin.y) as f64,
            self.to_physical(rect.origin.x + rect.size.width) as f64,
            self.to_physical(rect.origin.y + rect.size.height) as f64,
        )
    }

    fn current_transform(&self) -> Affine {
        // Transform is already in physical coordinates (translate/rotate/scale convert to physical)
        self.current_state().transform
    }

    /// Get or create cached image data from SharedImageBuffer
    /// Returns (Arc<Vec<u8>>, format, alpha_type, width, height)
    fn get_or_create_image_data(
        &self,
        buffer: &i_slint_core::graphics::SharedImageBuffer,
    ) -> (std::sync::Arc<Vec<u8>>, peniko::ImageFormat, peniko::ImageAlphaType, u32, u32) {
        use i_slint_core::graphics::SharedImageBuffer;

        // Use the buffer's data pointer as cache key
        let cache_key = match buffer {
            SharedImageBuffer::RGB8(b) => b.as_slice().as_ptr() as usize,
            SharedImageBuffer::RGBA8(b) => b.as_slice().as_ptr() as usize,
            SharedImageBuffer::RGBA8Premultiplied(b) => b.as_slice().as_ptr() as usize,
        };

        // Check if already cached
        let mut cache = self.image_cache.borrow_mut();

        let (format, alpha_type) = match buffer {
            SharedImageBuffer::RGB8(_) => {
                (peniko::ImageFormat::Rgba8, peniko::ImageAlphaType::Alpha)
            }
            SharedImageBuffer::RGBA8(_) => {
                (peniko::ImageFormat::Rgba8, peniko::ImageAlphaType::Alpha)
            }
            SharedImageBuffer::RGBA8Premultiplied(_) => {
                (peniko::ImageFormat::Rgba8, peniko::ImageAlphaType::AlphaPremultiplied)
            }
        };

        let arc_data = if let Some(data) = cache.get(&cache_key) {
            data.clone()
        } else {
            // Not cached - create new Arc'd byte vector
            // Convert RGB8 to RGBA8 if needed
            let rgba_bytes: Vec<u8> = match buffer {
                SharedImageBuffer::RGB8(b) => {
                    // Convert RGB to RGBA by adding opaque alpha channel
                    // Use the same conversion as Slint core: RGB8.into() -> RGBA8
                    use i_slint_core::graphics::Rgba8Pixel;
                    let mut rgba_data = Vec::with_capacity(b.as_slice().len() * 4);
                    for rgb_pixel in b.as_slice() {
                        let rgba_pixel: Rgba8Pixel = (*rgb_pixel).into();
                        rgba_data.push(rgba_pixel.r);
                        rgba_data.push(rgba_pixel.g);
                        rgba_data.push(rgba_pixel.b);
                        rgba_data.push(rgba_pixel.a);
                    }
                    rgba_data
                }
                SharedImageBuffer::RGBA8(b) => b.as_bytes().to_vec(),
                SharedImageBuffer::RGBA8Premultiplied(b) => b.as_bytes().to_vec(),
            };
            let arc_data = std::sync::Arc::new(rgba_bytes);
            cache.insert(cache_key, arc_data.clone());
            arc_data
        };

        (arc_data, format, alpha_type, buffer.width(), buffer.height())
    }
}

impl<'a> ItemRenderer for VelloItemRenderer<'a> {
    fn draw_rectangle(
        &mut self,
        rect: Pin<&dyn RenderRectangle>,
        _item_rc: &ItemRc,
        size: LogicalSize,
        _cache: &CachedRenderingData,
    ) {
        let rect_geom = LogicalRect::new(Default::default(), size);
        let brush = rect.background();

        let kurbo_rect = self.to_kurbo_rect(rect_geom);
        let peniko_brush = brush_to_peniko_brush_owned(&brush, kurbo_rect);

        // Apply alpha to the brush
        let peniko_brush = match peniko_brush {
            peniko::Brush::Solid(color) => {
                peniko::Brush::Solid(apply_alpha(color, self.current_state().alpha))
            }
            other => other, // Gradients don't support alpha multiplication yet
        };

        self.scene.fill(Fill::NonZero, self.current_transform(), &peniko_brush, None, &kurbo_rect);
    }

    fn draw_border_rectangle(
        &mut self,
        rect: Pin<&dyn RenderBorderRectangle>,
        _item_rc: &ItemRc,
        size: LogicalSize,
        _cache: &CachedRenderingData,
    ) {
        let mut rect_geom = LogicalRect::new(Default::default(), size);
        if rect_geom.size.width <= 0. || rect_geom.size.height <= 0. {
            return;
        }

        let border_color = rect.border_color();
        let border_width = if border_color.is_transparent() {
            LogicalLength::new(0.)
        } else {
            rect.border_width()
        };

        let opaque_border = border_color.is_opaque();
        let border_radius_logical = rect.border_radius();

        // Calculate border radius clamped to half the smallest dimension
        let mut fill_radius = border_radius_logical
            .top_left
            .min(rect_geom.size.width / 2.)
            .min(rect_geom.size.height / 2.);

        // Adjust radius for stroke positioning (stroke is centered on the path)
        // We want the radius to be on the outer edge of the rectangle
        fill_radius = fill_radius + border_width.get() / 2.;
        let stroke_border_radius = if fill_radius > border_width.get() / 2. {
            fill_radius - border_width.get() / 2.
        } else {
            fill_radius
        };

        let (background_shape, maybe_border_shape) = if opaque_border {
            // When border is opaque, draw background adjusted inward
            // Adjust rect for inner drawing (CSS box model: border is inside)
            rect_geom = LogicalRect::new(
                LogicalPoint::new(border_width.get(), border_width.get()),
                LogicalSize::new(
                    rect_geom.size.width - 2. * border_width.get(),
                    rect_geom.size.height - 2. * border_width.get(),
                ),
            );

            let shape = RoundedRect::from_rect(
                self.to_kurbo_rect(rect_geom),
                self.to_physical(stroke_border_radius) as f64,
            );
            (shape, None)
        } else {
            // When border is transparent, draw background at full size
            let background_shape = RoundedRect::from_rect(
                self.to_kurbo_rect(rect_geom),
                self.to_physical(fill_radius) as f64,
            );

            // Adjust rect for border drawing
            let border_rect = LogicalRect::new(
                LogicalPoint::new(border_width.get(), border_width.get()),
                LogicalSize::new(
                    rect_geom.size.width - 2. * border_width.get(),
                    rect_geom.size.height - 2. * border_width.get(),
                ),
            );

            let border_shape = RoundedRect::from_rect(
                self.to_kurbo_rect(border_rect),
                self.to_physical(stroke_border_radius) as f64,
            );

            (background_shape, Some(border_shape))
        };

        // Draw background
        let background_brush = rect.background();
        let bounds = background_shape.rect();
        let peniko_brush = brush_to_peniko_brush_owned(&background_brush, bounds);

        // Apply alpha to the brush
        let peniko_brush = match peniko_brush {
            peniko::Brush::Solid(color) => {
                peniko::Brush::Solid(apply_alpha(color, self.current_state().alpha))
            }
            other => other,
        };

        self.scene.fill(
            Fill::NonZero,
            self.current_transform(),
            &peniko_brush,
            None,
            &background_shape,
        );

        // Draw border if visible
        if !border_color.is_transparent() && border_width.get() > 0. {
            let border_shape_to_stroke = maybe_border_shape.as_ref().unwrap_or(&background_shape);
            let border_bounds = border_shape_to_stroke.rect();
            let border_peniko_brush = brush_to_peniko_brush_owned(&border_color, border_bounds);

            // Apply alpha
            let border_peniko_brush = match border_peniko_brush {
                peniko::Brush::Solid(color) => {
                    peniko::Brush::Solid(apply_alpha(color, self.current_state().alpha))
                }
                other => other,
            };

            // Create stroke style
            let stroke_width = self.to_physical(border_width.get()) as f64;
            let stroke_style = peniko::kurbo::Stroke::new(stroke_width)
                .with_caps(peniko::kurbo::Cap::Butt)
                .with_join(peniko::kurbo::Join::Miter);

            self.scene.stroke(
                &stroke_style,
                self.current_transform(),
                &border_peniko_brush,
                None,
                border_shape_to_stroke,
            );
        }
    }

    fn draw_window_background(
        &mut self,
        rect: Pin<&dyn RenderRectangle>,
        self_rc: &ItemRc,
        size: LogicalSize,
        cache: &CachedRenderingData,
    ) {
        // Just delegate to draw_rectangle for now
        self.draw_rectangle(rect, self_rc, size, cache);
    }

    fn draw_image(
        &mut self,
        image: Pin<&dyn RenderImage>,
        _item_rc: &ItemRc,
        size: LogicalSize,
        _cache: &CachedRenderingData,
    ) {
        use i_slint_core::graphics::ImageInner;

        if size.width <= 0. || size.height <= 0. {
            return;
        }

        let source_image = image.source();
        if source_image.size().width == 0 || source_image.size().height == 0 {
            return;
        }

        let image_inner: &ImageInner = (&source_image).into();

        // Calculate proper image fitting
        use i_slint_core::graphics::{fit, IntRect};
        use i_slint_core::lengths::ScaleFactor;

        // Convert to physical size for the fit function
        let scale: ScaleFactor = ScaleFactor::new(self.scale_factor);
        let phys_size = size.cast() * scale;

        let source_size = source_image.size();
        let source_clip =
            image.source_clip().unwrap_or_else(|| IntRect::from_size(source_size.cast()));

        let fit_result = fit(
            image.image_fit(),
            phys_size,
            source_clip,
            scale,
            image.alignment(),
            image.tiling(),
        );

        // Handle all supported image types: bitmaps, SVG, nine-slice, static textures
        match image_inner {
            ImageInner::EmbeddedImage { buffer, .. } => {
                // Use cached image data to avoid recreating Arc<Vec<u8>> every frame
                let (arc_data, format, alpha_type, width, height) =
                    self.get_or_create_image_data(buffer);

                let image_data = peniko::ImageData {
                    data: peniko::Blob::new(arc_data),
                    format,
                    alpha_type,
                    width,
                    height,
                };

                // Create image sampler with alpha
                let sampler =
                    peniko::ImageSampler::default().with_alpha(self.current_state().alpha);

                // Create image brush
                let image_brush = peniko::ImageBrush { image: image_data.into(), sampler };

                // Calculate the scaling and positioning transform
                // The image is width×height pixels, and we need to scale it to fit the target
                let scale_x = fit_result.source_to_target_x as f64;
                let scale_y = fit_result.source_to_target_y as f64;

                // Create a transform that properly handles source clipping by translating the source image
                // so that the clipped region appears at the correct position
                let source_translate_x = -source_clip.min_x() as f64;
                let source_translate_y = -source_clip.min_y() as f64;

                let image_transform = self.current_transform()
                    * Affine::translate((fit_result.offset.x as f64, fit_result.offset.y as f64))
                    * Affine::scale_non_uniform(scale_x, scale_y)
                    * Affine::translate((source_translate_x, source_translate_y));

                // Draw the image at its natural size (the transform handles scaling and clipping)
                let target_rect = peniko::kurbo::Rect::new(0.0, 0.0, width as f64, height as f64);

                // Use clipping to show only the desired region of the image
                let clip_rect = peniko::kurbo::Rect::new(
                    fit_result.offset.x as f64,
                    fit_result.offset.y as f64,
                    (fit_result.offset.x + fit_result.size.width) as f64,
                    (fit_result.offset.y + fit_result.size.height) as f64,
                );

                // Push a clipping layer to restrict drawing to the intended region
                self.scene.push_layer(
                    peniko::BlendMode::default(),
                    self.current_state().alpha,
                    self.current_transform(),
                    &clip_rect,
                );

                // Draw the image
                self.scene.fill(Fill::NonZero, image_transform, &image_brush, None, &target_rect);

                // Pop the clipping layer
                self.scene.pop_layer();
            }

            ImageInner::StaticTextures(st) => {
                // Static textures contain pre-processed image data
                // Convert to RGBA8 format and render
                let image_data = peniko::ImageData {
                    data: peniko::Blob::new(std::sync::Arc::new(st.data.as_slice().to_vec())),
                    format: peniko::ImageFormat::Rgba8,
                    alpha_type: peniko::ImageAlphaType::AlphaPremultiplied,
                    width: st.size.width,
                    height: st.size.height,
                };

                let sampler =
                    peniko::ImageSampler::default().with_alpha(self.current_state().alpha);

                let image_brush = peniko::ImageBrush { image: image_data.into(), sampler };

                let target_rect = peniko::kurbo::Rect::new(
                    0.0,
                    0.0,
                    self.to_physical(size.width) as f64,
                    self.to_physical(size.height) as f64,
                );

                self.scene.fill(
                    Fill::NonZero,
                    self.current_transform(),
                    &image_brush,
                    None,
                    &target_rect,
                );
            }
            ImageInner::BackendStorage(_storage) => {
                // Backend-specific storage not currently supported
                // This would require integration with the backend's texture caching system
            }
            ImageInner::BorrowedOpenGLTexture { .. } => {
                // Not applicable for Vello
            }
            #[cfg(target_arch = "wasm32")]
            ImageInner::HTMLImage(html_image) => {
                // Extract pixel data from HTML image element
                if let Some(img_size) = html_image.size() {
                    use wasm_bindgen::JsCast;

                    let dom_element = &html_image.dom_element;
                    let width = img_size.width;
                    let height = img_size.height;

                    // Create an offscreen canvas to extract pixel data
                    let window = web_sys::window().unwrap();
                    let document = window.document().unwrap();
                    let canvas = document
                        .create_element("canvas")
                        .unwrap()
                        .dyn_into::<web_sys::HtmlCanvasElement>()
                        .unwrap();
                    canvas.set_width(width);
                    canvas.set_height(height);

                    let ctx = canvas
                        .get_context("2d")
                        .unwrap()
                        .unwrap()
                        .dyn_into::<web_sys::CanvasRenderingContext2d>()
                        .unwrap();

                    // Draw image to canvas
                    let _ = ctx.draw_image_with_html_image_element(dom_element, 0.0, 0.0);

                    // Extract pixel data
                    let image_data =
                        ctx.get_image_data(0.0, 0.0, width as f64, height as f64).unwrap();
                    let pixels = image_data.data().0;

                    // Create image data for Vello (RGBA8 with premultiplied alpha for HTML images)
                    let image_data = peniko::ImageData {
                        data: peniko::Blob::new(std::sync::Arc::new(pixels)),
                        format: peniko::ImageFormat::Rgba8,
                        alpha_type: peniko::ImageAlphaType::AlphaPremultiplied,
                        width,
                        height,
                    };

                    let sampler =
                        peniko::ImageSampler::default().with_alpha(self.current_state().alpha);

                    let image_brush = peniko::ImageBrush { image: image_data.into(), sampler };

                    let target_rect = peniko::kurbo::Rect::new(
                        0.0,
                        0.0,
                        self.to_physical(size.width) as f64,
                        self.to_physical(size.height) as f64,
                    );

                    self.scene.fill(
                        Fill::NonZero,
                        self.current_transform(),
                        &image_brush,
                        None,
                        &target_rect,
                    );
                }
            }
            ImageInner::NineSlice(nine_slice) => {
                // Get the inner image and nine-slice borders
                let (inner_image, borders) = (&nine_slice.0, nine_slice.1);
                let inner_image_inner: &ImageInner = inner_image.into();

                // Get the source image buffer
                if let ImageInner::EmbeddedImage { buffer, .. } = inner_image_inner {
                    use i_slint_core::graphics::SharedImageBuffer;

                    // Extract bytes and format from the buffer
                    let (_bytes, _format, _alpha_type) = match buffer {
                        SharedImageBuffer::RGB8(b) => (
                            b.as_bytes(),
                            peniko::ImageFormat::Rgba8,
                            peniko::ImageAlphaType::Alpha,
                        ),
                        SharedImageBuffer::RGBA8(b) => (
                            b.as_bytes(),
                            peniko::ImageFormat::Rgba8,
                            peniko::ImageAlphaType::Alpha,
                        ),
                        SharedImageBuffer::RGBA8Premultiplied(b) => (
                            b.as_bytes(),
                            peniko::ImageFormat::Rgba8,
                            peniko::ImageAlphaType::AlphaPremultiplied,
                        ),
                    };

                    // Get nine-slice fit information
                    use i_slint_core::lengths::ScaleFactor;
                    let physical_size: i_slint_core::graphics::euclid::Size2D<f32, PhysicalPx> =
                        i_slint_core::graphics::euclid::Size2D::new(
                            self.to_physical(size.width),
                            self.to_physical(size.height),
                        );
                    let fits = i_slint_core::graphics::fit9slice(
                        inner_image.size(),
                        borders,
                        physical_size,
                        ScaleFactor::new(self.scale_factor),
                        image.alignment(),
                        image.tiling(),
                    );

                    // Use cached image data
                    let (arc_data, format, alpha_type, img_width, img_height) =
                        self.get_or_create_image_data(buffer);

                    // Render each slice with proper source region
                    for fit in fits {
                        if fit.clip_rect.is_empty() {
                            continue;
                        }

                        // Create full source image data
                        let image_data = peniko::ImageData {
                            data: peniko::Blob::new(arc_data.clone()),
                            format,
                            alpha_type,
                            width: img_width,
                            height: img_height,
                        };

                        let sampler =
                            peniko::ImageSampler::default().with_alpha(self.current_state().alpha);

                        let image_brush = peniko::ImageBrush { image: image_data.into(), sampler };

                        // Calculate target rectangle for this slice
                        let target_x = fit.offset.x as f64;
                        let target_y = fit.offset.y as f64;
                        let target_width = fit.size.width as f64;
                        let target_height = fit.size.height as f64;

                        // Transform that positions the image so that the clip_rect region
                        // aligns with the target position, with proper scaling
                        let scale_x = target_width / fit.clip_rect.width() as f64;
                        let scale_y = target_height / fit.clip_rect.height() as f64;

                        // Translate so that clip_rect.origin maps to target position
                        let translate_x = target_x - (fit.clip_rect.min_x() as f64 * scale_x);
                        let translate_y = target_y - (fit.clip_rect.min_y() as f64 * scale_y);

                        let slice_transform = self.current_transform()
                            * Affine::translate((translate_x, translate_y))
                            * Affine::scale_non_uniform(scale_x, scale_y);

                        // Render the slice with clipping to target rectangle
                        let clip_rect = peniko::kurbo::Rect::new(
                            target_x,
                            target_y,
                            target_x + target_width,
                            target_y + target_height,
                        );

                        self.scene.push_layer(
                            peniko::BlendMode::default(),
                            self.current_state().alpha,
                            slice_transform,
                            &clip_rect,
                        );

                        // Fill with the full image (but clipped to show only the desired region)
                        let full_image_rect =
                            peniko::kurbo::Rect::new(0.0, 0.0, img_width as f64, img_height as f64);

                        self.scene.fill(
                            Fill::NonZero,
                            Affine::IDENTITY, // Already transformed by layer
                            &image_brush,
                            None,
                            &full_image_rect,
                        );

                        self.scene.pop_layer();
                    }
                }
            }
            ImageInner::Svg(svg) => {
                // Render SVG to a raster image first, then draw it
                let svg_size = svg.size();

                // Convert logical size to physical pixels
                let physical_size = i_slint_core::graphics::euclid::Size2D::<f32, PhysicalPx>::new(
                    self.to_physical(size.width),
                    self.to_physical(size.height),
                );

                // Calculate the target size based on the image fit
                let fit = i_slint_core::graphics::fit(
                    image.image_fit(),
                    physical_size,
                    i_slint_core::graphics::IntRect::from_size(svg_size.cast()),
                    i_slint_core::lengths::ScaleFactor::new(self.scale_factor),
                    image.alignment(),
                    image.tiling(),
                );

                let target_size = i_slint_core::graphics::euclid::Size2D::<u32, PhysicalPx>::new(
                    (svg_size.cast::<f32>().width * fit.source_to_target_x) as u32,
                    (svg_size.cast::<f32>().height * fit.source_to_target_y) as u32,
                );

                // Render the SVG to a raster buffer
                if let Ok(buffer) = svg.render(Some(target_size)) {
                    use i_slint_core::graphics::SharedImageBuffer;

                    // Extract bytes and format from the rendered buffer, copying the data
                    let (bytes_vec, format, alpha_type, width, height) = match &buffer {
                        SharedImageBuffer::RGB8(b) => (
                            b.as_bytes().to_vec(),
                            peniko::ImageFormat::Rgba8,
                            peniko::ImageAlphaType::Alpha,
                            b.width(),
                            b.height(),
                        ),
                        SharedImageBuffer::RGBA8(b) => (
                            b.as_bytes().to_vec(),
                            peniko::ImageFormat::Rgba8,
                            peniko::ImageAlphaType::Alpha,
                            b.width(),
                            b.height(),
                        ),
                        SharedImageBuffer::RGBA8Premultiplied(b) => (
                            b.as_bytes().to_vec(),
                            peniko::ImageFormat::Rgba8,
                            peniko::ImageAlphaType::AlphaPremultiplied,
                            b.width(),
                            b.height(),
                        ),
                    };

                    // Create image data
                    let image_data = peniko::ImageData {
                        data: peniko::Blob::new(std::sync::Arc::new(bytes_vec)),
                        format,
                        alpha_type,
                        width,
                        height,
                    };

                    let sampler =
                        peniko::ImageSampler::default().with_alpha(self.current_state().alpha);

                    let image_brush = peniko::ImageBrush { image: image_data.into(), sampler };

                    // Calculate the destination rectangle
                    let dest_rect = peniko::kurbo::Rect::new(
                        fit.offset.x as f64,
                        fit.offset.y as f64,
                        (fit.offset.x + fit.size.width) as f64,
                        (fit.offset.y + fit.size.height) as f64,
                    );

                    self.scene.fill(
                        Fill::NonZero,
                        self.current_transform(),
                        &image_brush,
                        None,
                        &dest_rect,
                    );
                }
            }
            ImageInner::WGPUTexture(wgpu_texture) => {
                use i_slint_core::graphics::WGPUTexture;

                // Only handle WGPU 26 textures (the version we use for Vello backend)
                if let WGPUTexture::WGPU26Texture(texture) = wgpu_texture {
                    if let Some(reader) = self.wgpu_texture_reader {
                        // Read texture back to CPU using the backend's reader
                        if let Some((width, height, rgba_data)) = reader(texture) {
                            let image_data = peniko::ImageData {
                                data: peniko::Blob::new(std::sync::Arc::new(rgba_data)),
                                format: peniko::ImageFormat::Rgba8,
                                alpha_type: peniko::ImageAlphaType::AlphaPremultiplied,
                                width,
                                height,
                            };

                            let sampler = peniko::ImageSampler::default()
                                .with_alpha(self.current_state().alpha);

                            let image_brush =
                                peniko::ImageBrush { image: image_data.into(), sampler };

                            let target_rect = peniko::kurbo::Rect::new(
                                0.0,
                                0.0,
                                self.to_physical(size.width) as f64,
                                self.to_physical(size.height) as f64,
                            );

                            self.scene.fill(
                                Fill::NonZero,
                                self.current_transform(),
                                &image_brush,
                                None,
                                &target_rect,
                            );
                        }
                    }
                }
            }
            ImageInner::None => {
                // Empty image, nothing to render
            }
        }
    }

    fn draw_text(
        &mut self,
        text: Pin<&dyn RenderText>,
        item_rc: &ItemRc,
        size: LogicalSize,
        _cache: &CachedRenderingData,
    ) {
        sharedparley::draw_text(self, text, Some(text.font_request(item_rc)), size);
    }

    fn draw_text_input(
        &mut self,
        text_input: Pin<&i_slint_core::items::TextInput>,
        item_rc: &ItemRc,
        size: LogicalSize,
    ) {
        sharedparley::draw_text_input(
            self,
            text_input,
            Some(text_input.font_request(item_rc)),
            size,
            None,
        );
    }

    fn draw_path(
        &mut self,
        path: Pin<&i_slint_core::items::Path>,
        item_rc: &ItemRc,
        _size: LogicalSize,
    ) {
        use i_slint_core::items::{FillRule, LineCap};
        use peniko::kurbo::{BezPath, PathEl};

        let (offset, path_events) = match path.fitted_path_events(item_rc) {
            Some(offset_and_events) => offset_and_events,
            None => return,
        };

        // Convert lyon_path events to kurbo BezPath
        let mut kurbo_path = BezPath::new();

        for event in path_events.iter() {
            match event {
                lyon_path::Event::Begin { at } => {
                    kurbo_path.push(PathEl::MoveTo(peniko::kurbo::Point::new(
                        self.to_physical(at.x + offset.x) as f64,
                        self.to_physical(at.y + offset.y) as f64,
                    )));
                }
                lyon_path::Event::Line { from: _, to } => {
                    kurbo_path.push(PathEl::LineTo(peniko::kurbo::Point::new(
                        self.to_physical(to.x + offset.x) as f64,
                        self.to_physical(to.y + offset.y) as f64,
                    )));
                }
                lyon_path::Event::Quadratic { from: _, ctrl, to } => {
                    kurbo_path.push(PathEl::QuadTo(
                        peniko::kurbo::Point::new(
                            self.to_physical(ctrl.x + offset.x) as f64,
                            self.to_physical(ctrl.y + offset.y) as f64,
                        ),
                        peniko::kurbo::Point::new(
                            self.to_physical(to.x + offset.x) as f64,
                            self.to_physical(to.y + offset.y) as f64,
                        ),
                    ));
                }
                lyon_path::Event::Cubic { from: _, ctrl1, ctrl2, to } => {
                    kurbo_path.push(PathEl::CurveTo(
                        peniko::kurbo::Point::new(
                            self.to_physical(ctrl1.x + offset.x) as f64,
                            self.to_physical(ctrl1.y + offset.y) as f64,
                        ),
                        peniko::kurbo::Point::new(
                            self.to_physical(ctrl2.x + offset.x) as f64,
                            self.to_physical(ctrl2.y + offset.y) as f64,
                        ),
                        peniko::kurbo::Point::new(
                            self.to_physical(to.x + offset.x) as f64,
                            self.to_physical(to.y + offset.y) as f64,
                        ),
                    ));
                }
                lyon_path::Event::End { last: _, first: _, close } => {
                    if close {
                        kurbo_path.push(PathEl::ClosePath);
                    }
                }
            }
        }

        // Get path bounds for gradient calculation
        // Use kurbo's PathSeg trait to calculate bounds
        use peniko::kurbo::Shape;
        let path_bounds = kurbo_path.bounding_box();

        // Draw fill
        let fill_brush = path.fill();
        let peniko_fill = brush_to_peniko_brush_owned(&fill_brush, path_bounds);

        // Apply alpha to solid brushes
        let peniko_fill = match peniko_fill {
            peniko::Brush::Solid(color) => {
                peniko::Brush::Solid(apply_alpha(color, self.current_state().alpha))
            }
            other => other,
        };

        let fill_style = match path.fill_rule() {
            FillRule::Nonzero => Fill::NonZero,
            FillRule::Evenodd => Fill::EvenOdd,
        };

        self.scene.fill(fill_style, self.current_transform(), &peniko_fill, None, &kurbo_path);

        // Draw stroke
        let stroke_brush = path.stroke();
        if !matches!(stroke_brush, Brush::SolidColor(c) if c.alpha() == 0) {
            let peniko_stroke = brush_to_peniko_brush_owned(&stroke_brush, path_bounds);

            // Apply alpha to solid brushes
            let peniko_stroke = match peniko_stroke {
                peniko::Brush::Solid(color) => {
                    peniko::Brush::Solid(apply_alpha(color, self.current_state().alpha))
                }
                other => other,
            };

            let stroke_width = self.to_physical(path.stroke_width().get());

            let line_cap = match path.stroke_line_cap() {
                LineCap::Butt => peniko::kurbo::Cap::Butt,
                LineCap::Round => peniko::kurbo::Cap::Round,
                LineCap::Square => peniko::kurbo::Cap::Square,
            };

            // Use miter join with standard miter limit
            let stroke_style = peniko::kurbo::Stroke::new(stroke_width as f64)
                .with_caps(line_cap)
                .with_join(peniko::kurbo::Join::Miter)
                .with_miter_limit(4.0);

            self.scene.stroke(
                &stroke_style,
                self.current_transform(),
                &peniko_stroke,
                None,
                &kurbo_path,
            );
        }
    }

    fn draw_box_shadow(
        &mut self,
        box_shadow: Pin<&i_slint_core::items::BoxShadow>,
        item_rc: &ItemRc,
        _size: LogicalSize,
    ) {
        // Skip if shadow is invisible
        if box_shadow.color().alpha() == 0
            || (box_shadow.blur().get() == 0.0
                && box_shadow.offset_x().get() == 0.0
                && box_shadow.offset_y().get() == 0.0)
        {
            return;
        }

        // Get size from the item geometry
        let geometry = item_rc.geometry();
        let width = geometry.width_length().get();
        let height = geometry.height_length().get();
        let offset_x = box_shadow.offset_x().get();
        let offset_y = box_shadow.offset_y().get();
        let blur_radius = box_shadow.blur().get();
        let border_radius = box_shadow.border_radius().get();

        let mut shadow_color = to_peniko_color(&box_shadow.color());
        shadow_color = apply_alpha(shadow_color, self.current_state().alpha);

        // Convert blur radius to standard deviation for gaussian blur
        // CSS blur radius is approximately 2 * standard deviation
        let std_dev = self.to_physical(blur_radius) as f64 / 2.0;

        if std_dev > 0.0 {
            // Use Vello's built-in blurred rounded rectangle
            let shadow_rect = peniko::kurbo::Rect::new(
                self.to_physical(offset_x) as f64,
                self.to_physical(offset_y) as f64,
                self.to_physical(offset_x + width) as f64,
                self.to_physical(offset_y + height) as f64,
            );

            self.scene.draw_blurred_rounded_rect(
                self.current_transform(),
                shadow_rect,
                shadow_color,
                self.to_physical(border_radius) as f64,
                std_dev,
            );
        } else {
            // No blur - use simple filled rectangle
            let shadow_rect = peniko::kurbo::RoundedRect::from_rect(
                peniko::kurbo::Rect::new(
                    self.to_physical(offset_x) as f64,
                    self.to_physical(offset_y) as f64,
                    self.to_physical(offset_x + width) as f64,
                    self.to_physical(offset_y + height) as f64,
                ),
                self.to_physical(border_radius) as f64,
            );

            self.scene.fill(
                Fill::NonZero,
                self.current_transform(),
                shadow_color,
                None,
                &shadow_rect,
            );
        }
    }

    fn combine_clip(
        &mut self,
        rect: LogicalRect,
        radius: LogicalBorderRadius,
        _border_width: LogicalLength,
    ) -> bool {
        // Compute values before borrowing state mutably
        let border_radius = radius.top_left.min(rect.size.width / 2.).min(rect.size.height / 2.);
        let current_transform = self.current_transform();

        // Update the clip state
        let state = self.current_state_mut();
        let clip_valid = match state.clip.intersection(&rect) {
            Some(r) => {
                state.clip = r;
                true
            }
            None => {
                state.clip = LogicalRect::default();
                false
            }
        };

        // Track the pushed clip layer
        state.clip_layer_count += 1;

        // End the mutable borrow before accessing other methods
        let _ = state;

        // Apply actual clipping using Vello's layer system
        if border_radius > 0. {
            // Use rounded rectangle for clipping
            let clip_shape = RoundedRect::from_rect(
                self.to_kurbo_rect(rect),
                self.to_physical(border_radius) as f64,
            );

            // Push a clip layer
            self.scene.push_layer(
                peniko::BlendMode::default(),
                1.0,
                current_transform,
                &clip_shape,
            );
        } else {
            // Use simple rectangle for clipping
            let clip_shape = self.to_kurbo_rect(rect);

            self.scene.push_layer(
                peniko::BlendMode::default(),
                1.0,
                current_transform,
                &clip_shape,
            );
        }

        clip_valid
    }

    fn get_current_clip(&self) -> LogicalRect {
        self.current_state().clip
    }

    fn translate(&mut self, distance: LogicalVector) {
        // Convert logical distance to physical before applying transform
        let physical_distance =
            (self.to_physical(distance.x) as f64, self.to_physical(distance.y) as f64);

        let state = self.current_state_mut();
        // Update the transform (in physical space)
        state.transform = state.transform * Affine::translate(physical_distance);
        // Update the clip rect (still in logical space)
        state.clip = state.clip.translate(-distance);
    }

    fn rotate(&mut self, angle_in_degrees: f32) {
        let angle_in_radians = angle_in_degrees.to_radians();
        let state = self.current_state_mut();
        // Update the transform
        state.transform = state.transform * Affine::rotate(angle_in_radians as f64);

        // Compute the bounding box of the rotated clip rectangle
        let clip = &state.clip;
        let (sin, cos) = (angle_in_radians.sin(), angle_in_radians.cos());
        let rotate_point =
            |p: i_slint_core::lengths::LogicalPoint| (p.x * cos - p.y * sin, p.x * sin + p.y * cos);

        use i_slint_core::lengths::LogicalVector;

        let corners = [
            rotate_point(clip.origin),
            rotate_point(clip.origin + LogicalVector::new(clip.width(), 0.)),
            rotate_point(clip.origin + LogicalVector::new(0., clip.height())),
            rotate_point(clip.origin + clip.size),
        ];

        let origin: i_slint_core::lengths::LogicalPoint = (
            corners.iter().fold(f32::MAX, |a, b| b.0.min(a)),
            corners.iter().fold(f32::MAX, |a, b| b.1.min(a)),
        )
            .into();

        let end: i_slint_core::lengths::LogicalPoint = (
            corners.iter().fold(f32::MIN, |a, b| b.0.max(a)),
            corners.iter().fold(f32::MIN, |a, b| b.1.max(a)),
        )
            .into();

        state.clip = LogicalRect::new(origin, (end - origin).into());
    }

    fn scale(&mut self, x_factor: f32, y_factor: f32) {
        let state = self.current_state_mut();
        // Update the transform
        state.transform =
            state.transform * Affine::scale_non_uniform(x_factor as f64, y_factor as f64);
        // Update the clip rect
        state.clip.size.width /= x_factor;
        state.clip.size.height /= y_factor;
    }

    fn apply_opacity(&mut self, opacity: f32) {
        let state = self.current_state_mut();
        state.alpha *= opacity;
    }

    fn save_state(&mut self) {
        let mut current = self.current_state().clone();
        // Reset clip layer count for the new state
        // (the count represents layers added in THIS state level)
        current.clip_layer_count = 0;
        self.state_stack.push(current);
    }

    fn restore_state(&mut self) {
        if self.state_stack.len() > 1 {
            // Pop any clip layers that were pushed during this state
            let old_state = self.state_stack.pop().unwrap();
            for _ in 0..old_state.clip_layer_count {
                self.scene.pop_layer();
            }
        }
    }

    fn scale_factor(&self) -> f32 {
        self.scale_factor
    }

    fn draw_cached_pixmap(
        &mut self,
        _item_cache: &ItemRc,
        update_fn: &dyn Fn(&mut dyn FnMut(u32, u32, &[u8])),
    ) {
        // Call the update function to get the pixmap data
        let mut image_data_opt: Option<(u32, u32, Vec<u8>)> = None;

        update_fn(&mut |width: u32, height: u32, data: &[u8]| {
            // Store the image data for rendering
            image_data_opt = Some((width, height, data.to_vec()));
        });

        if let Some((width, height, data)) = image_data_opt {
            // Create peniko image from the RGBA data
            let image_data = peniko::ImageData {
                data: peniko::Blob::new(std::sync::Arc::new(data)),
                format: peniko::ImageFormat::Rgba8,
                alpha_type: peniko::ImageAlphaType::AlphaPremultiplied,
                width,
                height,
            };

            let sampler = peniko::ImageSampler::default().with_alpha(self.current_state().alpha);

            let image_brush = peniko::ImageBrush { image: image_data.into(), sampler };

            // Draw the cached image
            let target_rect = peniko::kurbo::Rect::new(
                0.0,
                0.0,
                self.to_physical(width as f32) as f64,
                self.to_physical(height as f32) as f64,
            );

            self.scene.fill(
                Fill::NonZero,
                self.current_transform(),
                &image_brush,
                None,
                &target_rect,
            );
        }
    }

    fn draw_string(&mut self, string: &str, color: Color) {
        use i_slint_core::SharedString;

        // Use sharedparley to draw the debug string
        sharedparley::draw_text(
            self,
            std::pin::pin!((SharedString::from(string), Brush::from(color))),
            None,
            LogicalSize::new(1000., 1000.), // Large size to avoid early return
        );
    }

    fn draw_image_direct(&mut self, image: i_slint_core::graphics::Image) {
        use i_slint_core::graphics::{ImageInner, SharedImageBuffer};

        let image_size = image.size();
        if image_size.width == 0 || image_size.height == 0 {
            return;
        }

        let image_inner: &ImageInner = (&image).into();

        match image_inner {
            ImageInner::EmbeddedImage { buffer, .. } => {
                // Extract bytes and convert to RGBA8 if needed
                let (rgba_bytes, alpha_type): (Vec<u8>, _) = match buffer {
                    SharedImageBuffer::RGB8(b) => {
                        // Convert RGB to RGBA by adding opaque alpha channel
                        // Use the same conversion as Slint core: RGB8.into() -> RGBA8
                        use i_slint_core::graphics::Rgba8Pixel;
                        let mut rgba_data = Vec::with_capacity(b.as_slice().len() * 4);
                        for rgb_pixel in b.as_slice() {
                            let rgba_pixel: Rgba8Pixel = (*rgb_pixel).into();
                            rgba_data.push(rgba_pixel.r);
                            rgba_data.push(rgba_pixel.g);
                            rgba_data.push(rgba_pixel.b);
                            rgba_data.push(rgba_pixel.a);
                        }
                        (rgba_data, peniko::ImageAlphaType::Alpha)
                    }
                    SharedImageBuffer::RGBA8(b) => {
                        (b.as_bytes().to_vec(), peniko::ImageAlphaType::Alpha)
                    }
                    SharedImageBuffer::RGBA8Premultiplied(b) => {
                        (b.as_bytes().to_vec(), peniko::ImageAlphaType::AlphaPremultiplied)
                    }
                };

                let image_data = peniko::ImageData {
                    data: peniko::Blob::new(std::sync::Arc::new(rgba_bytes)),
                    format: peniko::ImageFormat::Rgba8,
                    alpha_type,
                    width: buffer.width(),
                    height: buffer.height(),
                };

                let sampler =
                    peniko::ImageSampler::default().with_alpha(self.current_state().alpha);

                let image_brush = peniko::ImageBrush { image: image_data.into(), sampler };

                // Draw at natural size
                let target_rect = peniko::kurbo::Rect::new(
                    0.0,
                    0.0,
                    self.to_physical(image_size.width as f32) as f64,
                    self.to_physical(image_size.height as f32) as f64,
                );

                self.scene.fill(
                    Fill::NonZero,
                    self.current_transform(),
                    &image_brush,
                    None,
                    &target_rect,
                );
            }
            _ => {
                // Other image types not yet supported
            }
        }
    }

    fn window(&self) -> &WindowInner {
        self.window_inner
    }

    fn as_any(&mut self) -> Option<&mut dyn core::any::Any> {
        None
    }
}

/// Vello-specific brush type for text rendering
#[derive(Clone)]
pub enum VelloBrush {
    Fill(peniko::Color),
    Stroke(peniko::Color, f32), // color and stroke width
}

impl<'a> GlyphRenderer for VelloItemRenderer<'a> {
    type PlatformBrush = VelloBrush;

    fn platform_text_fill_brush(
        &mut self,
        brush: Brush,
        _size: LogicalSize,
    ) -> Option<Self::PlatformBrush> {
        brush_to_peniko_color(&brush).map(VelloBrush::Fill)
    }

    fn platform_brush_for_color(&mut self, color: &Color) -> Option<Self::PlatformBrush> {
        if color.alpha() == 0 {
            None
        } else {
            Some(VelloBrush::Fill(to_peniko_color(color)))
        }
    }

    fn platform_text_stroke_brush(
        &mut self,
        brush: Brush,
        physical_stroke_width: f32,
        _size: LogicalSize,
    ) -> Option<Self::PlatformBrush> {
        brush_to_peniko_color(&brush).map(|color| VelloBrush::Stroke(color, physical_stroke_width))
    }

    fn draw_glyph_run(
        &mut self,
        font: &parley::FontData,
        font_size: PhysicalLength,
        brush: Self::PlatformBrush,
        y_offset: PhysicalLength,
        glyphs_it: &mut dyn Iterator<Item = parley::layout::Glyph>,
    ) {
        // Convert Parley glyphs to Vello glyphs
        let glyphs: Vec<vello::Glyph> = glyphs_it
            .map(|g| vello::Glyph { id: g.id as u32, x: g.x, y: g.y + y_offset.get() })
            .collect();

        // Skip if no glyphs to render
        if glyphs.is_empty() {
            return;
        }

        // Create FontData for Vello from Parley's FontData
        // peniko::FontData is what Vello uses
        let vello_font = peniko::FontData { data: font.data.clone(), index: font.index };

        // Save transform and alpha before borrowing scene
        let transform = self.current_transform();
        let alpha = self.current_state().alpha;

        match brush {
            VelloBrush::Fill(color) => {
                // Draw text with fill
                let color_with_alpha = apply_alpha(color, alpha);

                self.scene
                    .draw_glyphs(&vello_font)
                    .font_size(font_size.get())
                    .transform(transform)
                    .brush(color_with_alpha)
                    .draw(Fill::NonZero, glyphs.into_iter());
            }
            VelloBrush::Stroke(color, stroke_width) => {
                // Draw text with stroke
                // Note: Vello's text rendering primarily supports fill, not stroke
                // We'll draw the glyphs slightly larger to simulate stroke effect
                let stroke_offset = self.to_physical(stroke_width);
                let color_with_alpha = apply_alpha(color, alpha);

                self.scene
                    .draw_glyphs(&vello_font)
                    .font_size(font_size.get() + stroke_offset)
                    .transform(transform)
                    .brush(color_with_alpha)
                    .draw(Fill::NonZero, glyphs.into_iter());
            }
        }
    }

    fn fill_rectangle(&mut self, physical_rect: PhysicalRect, brush: Self::PlatformBrush) {
        let color = match brush {
            VelloBrush::Fill(color) => color,
            VelloBrush::Stroke(color, _) => color,
        };

        let rect = Rect::new(
            physical_rect.min_x() as f64,
            physical_rect.min_y() as f64,
            physical_rect.max_x() as f64,
            physical_rect.max_y() as f64,
        );

        self.scene.fill(Fill::NonZero, self.current_transform(), color, None, &rect);
    }
}
