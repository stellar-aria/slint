// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

#![doc = include_str!("README.md")]
#![doc(html_logo_url = "https://slint.dev/logo/slint-logo-square-light.svg")]

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::pin::Pin;
use std::rc::{Rc, Weak};

use i_slint_common::sharedfontique;
use i_slint_core::api::{
    PhysicalSize as PhysicalWindowSize, RenderingNotifier, RenderingState,
    SetRenderingNotifierError,
};
use i_slint_core::graphics::euclid;
use i_slint_core::graphics::rendering_metrics_collector::{
    RenderingMetrics, RenderingMetricsCollector,
};
use i_slint_core::graphics::FontRequest;
use i_slint_core::item_rendering::ItemRenderer;
use i_slint_core::item_tree::ItemTreeWeak;
use i_slint_core::items::TextWrap;
use i_slint_core::lengths::{LogicalLength, LogicalPoint, LogicalRect, LogicalSize, ScaleFactor};
use i_slint_core::platform::PlatformError;
use i_slint_core::renderer::RendererSealed;
use i_slint_core::textlayout::sharedparley;
use i_slint_core::window::{WindowAdapter, WindowInner};
use i_slint_core::Brush;

mod itemrenderer;

pub use peniko;
pub use vello;

pub mod wgpu;

/// Graphics backend trait that must be implemented to use the Vello renderer
pub trait GraphicsBackend {
    /// The name of the backend for debugging purposes
    const NAME: &'static str;

    /// Create a new suspended backend instance
    fn new_suspended() -> Self;

    /// Clear the graphics context
    fn clear_graphics_context(&self);

    /// Render a scene to the surface
    fn render_scene(
        &self,
        scene: &vello::Scene,
        width: u32,
        height: u32,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Get the graphics API for rendering notifier callbacks
    fn with_graphics_api<R>(
        &self,
        callback: impl FnOnce(Option<i_slint_core::api::GraphicsAPI<'_>>) -> R,
    ) -> Result<R, i_slint_core::platform::PlatformError>;

    /// Resize the surface
    fn resize(
        &self,
        width: NonZeroU32,
        height: NonZeroU32,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Read a WGPU 26 texture back to CPU as RGBA8 data  
    /// Returns (width, height, rgba_data) if successful
    ///
    /// Note: Reading GPU textures back to CPU is inherently slow and should be avoided.
    /// This is provided for compatibility but may not be implemented by all backends.
    fn read_wgpu_26_texture(&self, _texture: &wgpu_26::Texture) -> Option<(u32, u32, Vec<u8>)> {
        // Default implementation returns None - backends must override if they support this
        None
    }
}

/// Use the Vello renderer when implementing a custom Slint platform where you deliver events to
/// Slint and want the scene to be rendered using GPU acceleration via Vello.
/// Cache key for images - uses pointer address as unique identifier
type ImageCacheKey = usize;

pub struct VelloRenderer<B: GraphicsBackend> {
    maybe_window_adapter: RefCell<Option<Weak<dyn WindowAdapter>>>,
    rendering_notifier: RefCell<Option<Box<dyn RenderingNotifier>>>,
    scene: RefCell<vello::Scene>,
    rendering_metrics_collector: RefCell<Option<Rc<RenderingMetricsCollector>>>,
    rendering_first_time: Cell<bool>,
    graphics_backend: B,
    /// Cache of image data to avoid recreating them every frame
    image_cache: RefCell<HashMap<ImageCacheKey, std::sync::Arc<Vec<u8>>>>,
}

impl<B: GraphicsBackend> VelloRenderer<B> {
    /// Create a new Vello renderer with the specified backend
    pub fn new(backend: B) -> Self {
        Self {
            maybe_window_adapter: RefCell::default(),
            rendering_notifier: RefCell::default(),
            scene: RefCell::new(vello::Scene::new()),
            rendering_metrics_collector: RefCell::default(),
            rendering_first_time: Cell::new(true),
            graphics_backend: backend,
            image_cache: RefCell::new(HashMap::new()),
        }
    }

    /// Get a reference to the graphics backend
    pub fn graphics_backend(&self) -> &B {
        &self.graphics_backend
    }

    /// Clear graphics resources (surface, device, etc.). This should be called
    /// when suspending or before dropping the renderer to release window references.
    pub fn clear_graphics_context(&self) {
        self.image_cache.borrow_mut().clear();
        self.graphics_backend.clear_graphics_context();
    }

    /// Render the scene using the Vello renderer.
    pub fn render(&self) -> Result<(), i_slint_core::platform::PlatformError> {
        if self.rendering_first_time.take() {
            *self.rendering_metrics_collector.borrow_mut() =
                RenderingMetricsCollector::new(&format!("Vello renderer with {} backend", B::NAME));

            if let Some(callback) = self.rendering_notifier.borrow_mut().as_mut() {
                self.with_graphics_api(|api| {
                    callback.notify(RenderingState::RenderingSetup, &api)
                })?;
            }
        }

        let window_adapter = self.window_adapter()?;
        let window = window_adapter.window();
        let window_size = window.size();

        let Some((width, height)): Option<(NonZeroU32, NonZeroU32)> =
            window_size.width.try_into().ok().zip(window_size.height.try_into().ok())
        else {
            // Nothing to render
            return Ok(());
        };

        let window_inner = WindowInner::from_pub(window);

        window_inner
            .draw_contents(|components| -> Result<(), PlatformError> {
                let mut scene = self.scene.borrow_mut();
                scene.reset();

                let window_background_brush =
                    window_inner.window_item().map(|w| w.as_pin_ref().background());

                // Clear with window background if it is a solid color
                if let Some(Brush::SolidColor(clear_color)) = window_background_brush {
                    let color = peniko::Color::from_rgba8(
                        clear_color.red(),
                        clear_color.green(),
                        clear_color.blue(),
                        clear_color.alpha(),
                    );
                    scene.fill(
                        peniko::Fill::NonZero,
                        vello::kurbo::Affine::IDENTITY,
                        color,
                        None,
                        &vello::kurbo::Rect::new(0.0, 0.0, width.get() as f64, height.get() as f64),
                    );
                }

                if let Some(notifier_fn) = self.rendering_notifier.borrow_mut().as_mut() {
                    drop(scene);
                    self.with_graphics_api(|api| {
                        notifier_fn.notify(RenderingState::BeforeRendering, &api)
                    })?;
                    scene = self.scene.borrow_mut();
                }

                let logical_size = LogicalSize::new(width.get() as f32, height.get() as f32);
                let mut item_renderer = self::itemrenderer::VelloItemRenderer::new(
                    &mut scene,
                    &window_inner,
                    logical_size,
                    &self.image_cache,
                );

                // WGPU texture rendering is not currently supported in the Vello renderer
                // due to the complexity of reading textures back from GPU to CPU.
                // Applications should use other image formats or render WGPU textures
                // via the rendering notifier callback.
                {
                    // Texture reader is not set - WGPU textures will be skipped
                    // item_renderer.set_wgpu_texture_reader(...);
                }

                // Render window item background if not solid color
                if let Some(window_item_rc) = window_inner.window_item_rc() {
                    let window_item =
                        window_item_rc.downcast::<i_slint_core::items::WindowItem>().unwrap();
                    match window_item.as_pin_ref().background() {
                        Brush::SolidColor(..) => {
                            // Already cleared earlier
                        }
                        _ => {
                            // Draws the window background as gradient
                            item_renderer.draw_rectangle(
                                window_item.as_pin_ref(),
                                &window_item_rc,
                                i_slint_core::lengths::logical_size_from_api(
                                    window.size().to_logical(window_inner.scale_factor()),
                                ),
                                &window_item.as_pin_ref().cached_rendering_data,
                            );
                        }
                    }
                }

                // Render all component items
                for (component, origin) in components {
                    if let Some(component) = ItemTreeWeak::upgrade(component) {
                        i_slint_core::item_rendering::render_component_items(
                            &component,
                            &mut item_renderer,
                            *origin,
                            &self.window_adapter()?,
                        );
                    }
                }

                if let Some(collector) = &self.rendering_metrics_collector.borrow().as_ref() {
                    let metrics = RenderingMetrics::default();
                    collector.measure_frame_rendered(&mut item_renderer, metrics);
                }

                drop(item_renderer);

                Ok(())
            })
            .unwrap_or(Ok(()))?;

        // Render the scene to the surface
        let scene = self.scene.borrow();
        self.graphics_backend.render_scene(&scene, width.get(), height.get())?;
        drop(scene);

        if let Some(callback) = self.rendering_notifier.borrow_mut().as_mut() {
            self.with_graphics_api(|api| callback.notify(RenderingState::AfterRendering, &api))?;
        }

        Ok(())
    }

    /// Render with rotation and post-rendering callback support for embedded displays
    pub fn render_transformed_with_post_callback(
        &self,
        rotation_degrees: f32,
        translation: euclid::Vector2D<f32, i_slint_core::lengths::LogicalPx>,
        size: PhysicalWindowSize,
        post_render_callback: Option<&dyn Fn(&mut dyn ItemRenderer)>,
    ) -> Result<(), PlatformError> {
        if self.rendering_first_time.take() {
            *self.rendering_metrics_collector.borrow_mut() =
                RenderingMetricsCollector::new(&format!("Vello renderer with {} backend", B::NAME));

            if let Some(callback) = self.rendering_notifier.borrow_mut().as_mut() {
                self.with_graphics_api(|api| {
                    callback.notify(RenderingState::RenderingSetup, &api)
                })?;
            }
        }

        let Some((width, height)): Option<(NonZeroU32, NonZeroU32)> =
            size.width.try_into().ok().zip(size.height.try_into().ok())
        else {
            // Nothing to render
            return Ok(());
        };

        let window_adapter = self.window_adapter()?;
        let window = window_adapter.window();
        let window_inner = WindowInner::from_pub(window);

        window_inner
            .draw_contents(|components| -> Result<(), PlatformError> {
                let mut scene = self.scene.borrow_mut();
                scene.reset();

                let window_background_brush =
                    window_inner.window_item().map(|w| w.as_pin_ref().background());

                // Clear with window background if it is a solid color
                if let Some(Brush::SolidColor(clear_color)) = window_background_brush {
                    let color = peniko::Color::from_rgba8(
                        clear_color.red(),
                        clear_color.green(),
                        clear_color.blue(),
                        clear_color.alpha(),
                    );
                    scene.fill(
                        peniko::Fill::NonZero,
                        vello::kurbo::Affine::IDENTITY,
                        color,
                        None,
                        &vello::kurbo::Rect::new(0.0, 0.0, width.get() as f64, height.get() as f64),
                    );
                }

                if let Some(notifier_fn) = self.rendering_notifier.borrow_mut().as_mut() {
                    drop(scene);
                    self.with_graphics_api(|api| {
                        notifier_fn.notify(RenderingState::BeforeRendering, &api)
                    })?;
                    scene = self.scene.borrow_mut();
                }

                // Create initial transform with rotation and translation
                let initial_transform = if rotation_degrees != 0.0 {
                    // Rotation is applied around origin, then translation
                    vello::kurbo::Affine::translate((translation.x as f64, translation.y as f64))
                        * vello::kurbo::Affine::rotate((rotation_degrees as f64).to_radians())
                } else {
                    vello::kurbo::Affine::IDENTITY
                };

                let logical_size = LogicalSize::new(width.get() as f32, height.get() as f32);
                let mut item_renderer = self::itemrenderer::VelloItemRenderer::new_with_transform(
                    &mut scene,
                    &window_inner,
                    logical_size,
                    &self.image_cache,
                    initial_transform,
                );

                // Render window item background if not solid color
                if let Some(window_item_rc) = window_inner.window_item_rc() {
                    let window_item =
                        window_item_rc.downcast::<i_slint_core::items::WindowItem>().unwrap();
                    match window_item.as_pin_ref().background() {
                        Brush::SolidColor(..) => {
                            // Already cleared earlier
                        }
                        _ => {
                            // Draws the window background as gradient
                            item_renderer.draw_rectangle(
                                window_item.as_pin_ref(),
                                &window_item_rc,
                                i_slint_core::lengths::logical_size_from_api(
                                    window.size().to_logical(window_inner.scale_factor()),
                                ),
                                &window_item.as_pin_ref().cached_rendering_data,
                            );
                        }
                    }
                }

                // Render all component items
                for (component, origin) in components {
                    if let Some(component) = ItemTreeWeak::upgrade(component) {
                        i_slint_core::item_rendering::render_component_items(
                            &component,
                            &mut item_renderer,
                            *origin,
                            &self.window_adapter()?,
                        );
                    }
                }

                // Call post-render callback (e.g., for drawing mouse cursor)
                if let Some(callback) = post_render_callback {
                    callback(&mut item_renderer);
                }

                if let Some(collector) = &self.rendering_metrics_collector.borrow().as_ref() {
                    let metrics = RenderingMetrics::default();
                    collector.measure_frame_rendered(&mut item_renderer, metrics);
                }

                drop(item_renderer);

                Ok(())
            })
            .unwrap_or(Ok(()))?;

        // Render the scene to the surface
        let scene = self.scene.borrow();
        self.graphics_backend.render_scene(&scene, width.get(), height.get())?;
        drop(scene);

        if let Some(callback) = self.rendering_notifier.borrow_mut().as_mut() {
            self.with_graphics_api(|api| callback.notify(RenderingState::AfterRendering, &api))?;
        }

        Ok(())
    }

    fn with_graphics_api(
        &self,
        callback: impl FnOnce(i_slint_core::api::GraphicsAPI<'_>),
    ) -> Result<(), PlatformError> {
        self.graphics_backend.with_graphics_api(|api| callback(api.unwrap()))
    }

    fn window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        self.maybe_window_adapter.borrow().as_ref().and_then(|w| w.upgrade()).ok_or_else(|| {
            "Renderer must be associated with component before use".to_string().into()
        })
    }
}

impl<B: GraphicsBackend> Drop for VelloRenderer<B> {
    fn drop(&mut self) {
        // Notify teardown before clearing resources
        if !self.rendering_first_time.get() {
            if let Some(callback) = self.rendering_notifier.borrow_mut().as_mut() {
                let _ = self.graphics_backend.with_graphics_api(|api| {
                    if let Some(api) = api {
                        let _ = callback.notify(RenderingState::RenderingTeardown, &api);
                    }
                });
            }
        }

        // Clear graphics context to release window references
        self.clear_graphics_context();
    }
}

#[doc(hidden)]
impl<B: GraphicsBackend> RendererSealed for VelloRenderer<B> {
    fn text_size(
        &self,
        font_request: i_slint_core::graphics::FontRequest,
        text: &str,
        max_width: Option<LogicalLength>,
        scale_factor: ScaleFactor,
        text_wrap: TextWrap,
    ) -> LogicalSize {
        sharedparley::text_size(font_request, text, max_width, scale_factor, text_wrap)
    }

    fn font_metrics(
        &self,
        font_request: i_slint_core::graphics::FontRequest,
        _scale_factor: ScaleFactor,
    ) -> i_slint_core::items::FontMetrics {
        sharedparley::font_metrics(font_request)
    }

    fn text_input_byte_offset_for_position(
        &self,
        text_input: Pin<&i_slint_core::items::TextInput>,
        pos: LogicalPoint,
        font_request: FontRequest,
        scale_factor: ScaleFactor,
    ) -> usize {
        sharedparley::text_input_byte_offset_for_position(
            text_input,
            pos,
            font_request,
            scale_factor,
        )
    }

    fn text_input_cursor_rect_for_byte_offset(
        &self,
        text_input: Pin<&i_slint_core::items::TextInput>,
        byte_offset: usize,
        font_request: FontRequest,
        scale_factor: ScaleFactor,
    ) -> LogicalRect {
        sharedparley::text_input_cursor_rect_for_byte_offset(
            text_input,
            byte_offset,
            font_request,
            scale_factor,
        )
    }

    fn register_font_from_memory(
        &self,
        data: &'static [u8],
    ) -> Result<(), Box<dyn std::error::Error>> {
        sharedfontique::get_collection().register_fonts(data.to_vec().into(), None);
        Ok(())
    }

    fn register_font_from_path(
        &self,
        path: &std::path::Path,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let requested_path = path.canonicalize().unwrap_or_else(|_| path.into());
        let contents = std::fs::read(requested_path)?;
        sharedfontique::get_collection().register_fonts(contents.into(), None);
        Ok(())
    }

    fn default_font_size(&self) -> LogicalLength {
        sharedparley::DEFAULT_FONT_SIZE
    }

    fn supports_transformations(&self) -> bool {
        true // Transforms are fully implemented (translate, rotate, scale)
    }

    fn set_rendering_notifier(
        &self,
        callback: Box<dyn i_slint_core::api::RenderingNotifier>,
    ) -> Result<(), i_slint_core::api::SetRenderingNotifierError> {
        let mut notifier = self.rendering_notifier.borrow_mut();
        if notifier.replace(callback).is_some() {
            Err(SetRenderingNotifierError::AlreadySet)
        } else {
            Ok(())
        }
    }

    fn free_graphics_resources(
        &self,
        _component: i_slint_core::item_tree::ItemTreeRef,
        _items: &mut dyn Iterator<Item = Pin<i_slint_core::items::ItemRef<'_>>>,
    ) -> Result<(), i_slint_core::platform::PlatformError> {
        Ok(())
    }

    fn set_window_adapter(&self, window_adapter: &Rc<dyn WindowAdapter>) {
        *self.maybe_window_adapter.borrow_mut() = Some(Rc::downgrade(window_adapter));
    }

    fn resize(&self, size: i_slint_core::api::PhysicalSize) -> Result<(), PlatformError> {
        if let Some((width, height)) = size.width.try_into().ok().zip(size.height.try_into().ok()) {
            self.graphics_backend.resize(width, height)?;
        };
        Ok(())
    }
}
