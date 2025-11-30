// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

//! WGPU backend for Vello renderer

use std::cell::RefCell;
use std::num::NonZeroU32;

use i_slint_core::api::PhysicalSize as PhysicalWindowSize;
use i_slint_core::graphics::RequestedGraphicsAPI;

use wgpu_26 as wgpu;

/// WGPU backend for Vello rendering
pub struct WgpuBackend {
    instance: RefCell<Option<wgpu::Instance>>,
    device: RefCell<Option<wgpu::Device>>,
    queue: RefCell<Option<wgpu::Queue>>,
    surface_config: RefCell<Option<wgpu::SurfaceConfiguration>>,
    surface: RefCell<Option<wgpu::Surface<'static>>>,
    vello_renderer: RefCell<Option<vello::Renderer>>,
    // Intermediate texture for Vello rendering (always RGBA8)
    target_texture: RefCell<Option<wgpu::Texture>>,
    target_view: RefCell<Option<wgpu::TextureView>>,
    // Blitter to copy from intermediate texture to surface
    blitter: RefCell<Option<wgpu::util::TextureBlitter>>,
}

impl WgpuBackend {
    /// Create a new suspended WGPU backend
    pub fn new() -> Self {
        Self {
            instance: RefCell::default(),
            device: RefCell::default(),
            queue: RefCell::default(),
            surface_config: RefCell::default(),
            surface: RefCell::default(),
            vello_renderer: RefCell::default(),
            target_texture: RefCell::default(),
            target_view: RefCell::default(),
            blitter: RefCell::default(),
        }
    }

    /// Create intermediate render targets (RGBA8 texture for Vello)
    fn create_targets(
        &self,
        width: u32,
        height: u32,
        device: &wgpu::Device,
        surface_format: wgpu::TextureFormat,
    ) {
        let target_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("Vello intermediate texture"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // Vello requires STORAGE_BINDING for compute shaders
            usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING,
            format: wgpu::TextureFormat::Rgba8Unorm,
            view_formats: &[],
        });
        let target_view = target_texture.create_view(&wgpu::TextureViewDescriptor::default());

        *self.target_texture.borrow_mut() = Some(target_texture);
        *self.target_view.borrow_mut() = Some(target_view);
        
        // Create blitter to copy from RGBA8 to surface format
        *self.blitter.borrow_mut() = Some(wgpu::util::TextureBlitter::new(device, surface_format));
    }

    /// Get a reference to the WGPU device (if initialized)
    pub fn device(&self) -> std::cell::Ref<'_, Option<wgpu::Device>> {
        self.device.borrow()
    }

    /// Get a reference to the WGPU queue (if initialized)
    pub fn queue(&self) -> std::cell::Ref<'_, Option<wgpu::Queue>> {
        self.queue.borrow()
    }

    /// Initialize the WGPU backend with a window handle
    pub fn set_window_handle(
        &self,
        window_handle: Box<dyn wgpu::WindowHandle>,
        size: PhysicalWindowSize,
        requested_graphics_api: Option<RequestedGraphicsAPI>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let (instance, adapter, device, queue, surface) =
            i_slint_core::graphics::wgpu_26::init_instance_adapter_device_queue_surface(
                window_handle,
                requested_graphics_api,
                wgpu::Backends::empty(),  // Don't avoid any backends
            )?;

        let mut surface_config =
            surface.get_default_config(&adapter, size.width, size.height).unwrap();

        let swapchain_capabilities = surface.get_capabilities(&adapter);
        // Prefer RGBA8 or BGRA8 for blitting compatibility
        let swapchain_format = swapchain_capabilities
            .formats
            .iter()
            .find(|f| matches!(f, wgpu::TextureFormat::Rgba8Unorm | wgpu::TextureFormat::Bgra8Unorm))
            .or_else(|| swapchain_capabilities.formats.iter().find(|f| !f.is_srgb()))
            .copied()
            .unwrap_or_else(|| swapchain_capabilities.formats[0]);
        surface_config.format = swapchain_format;
        
        // Surface needs RENDER_ATTACHMENT for blitting
        surface_config.usage = wgpu::TextureUsages::RENDER_ATTACHMENT;
        
        // Prefer FIFO for better frame pacing
        surface_config.present_mode = wgpu::PresentMode::AutoVsync;
        
        surface.configure(&device, &surface_config);

        // Create Vello renderer
        let vello_renderer = vello::Renderer::new(
            &device,
            vello::RendererOptions {
                use_cpu: false,
                antialiasing_support: vello::AaSupport::all(),
                num_init_threads: None,
                pipeline_cache: None,
            },
        )
        .map_err(|e| format!("Failed to create Vello renderer: {}", e))?;

        *self.instance.borrow_mut() = Some(instance);
        *self.device.borrow_mut() = Some(device.clone());
        *self.queue.borrow_mut() = Some(queue);
        *self.surface_config.borrow_mut() = Some(surface_config.clone());
        *self.surface.borrow_mut() = Some(surface);
        *self.vello_renderer.borrow_mut() = Some(vello_renderer);

        // Create intermediate render targets
        self.create_targets(size.width, size.height, &device, swapchain_format);

        Ok(())
    }
}

impl Default for WgpuBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::GraphicsBackend for WgpuBackend {
    const NAME: &'static str = "WGPU";

    fn new_suspended() -> Self {
        Self::new()
    }

    fn read_wgpu_26_texture(&self, _texture: &wgpu::Texture) -> Option<(u32, u32, Vec<u8>)> {
        // Texture readback to CPU is not implemented as it's inefficient.
        // Instead, WGPU textures should be rendered directly using GPU operations.
        // See WgpuBackend::render_wgpu_texture_to_target() for a potential GPU-based approach.
        None
    }

    fn clear_graphics_context(&self) {
        self.blitter.borrow_mut().take();
        self.target_view.borrow_mut().take();
        self.target_texture.borrow_mut().take();
        self.vello_renderer.borrow_mut().take();
        self.surface.borrow_mut().take();
        self.surface_config.borrow_mut().take();
        self.queue.borrow_mut().take();
        self.device.borrow_mut().take();
        self.instance.borrow_mut().take();
    }

    fn render_scene(
        &self,
        scene: &vello::Scene,
        width: u32,
        height: u32,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let device = self.device.borrow();
        let queue = self.queue.borrow();
        let mut vello_renderer = self.vello_renderer.borrow_mut();
        let surface = self.surface.borrow();
        let target_view = self.target_view.borrow();
        let target_texture = self.target_texture.borrow();
        let blitter = self.blitter.borrow();

        let device = device.as_ref().ok_or("Device not initialized")?;
        let queue = queue.as_ref().ok_or("Queue not initialized")?;
        let renderer = vello_renderer.as_mut().ok_or("Vello renderer not initialized")?;
        let surface = surface.as_ref().ok_or("Surface not initialized")?;
        let target_view = target_view.as_ref().ok_or("Target view not initialized")?;
        let _target_texture = target_texture.as_ref().ok_or("Target texture not initialized")?;
        let blitter = blitter.as_ref().ok_or("Blitter not initialized")?;

        // Get the surface texture
        let surface_texture = surface.get_current_texture()?;
        let surface_texture_view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        // Render Vello scene to intermediate RGBA8 texture
        renderer
            .render_to_texture(device, queue, scene, target_view, &vello::RenderParams {
                base_color: peniko::Color::BLACK,
                width,
                height,
                antialiasing_method: vello::AaConfig::Msaa16,
            })
            .map_err(|e| format!("Vello render error: {}", e))?;

        // Blit from intermediate texture to surface
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("Vello blit encoder"),
        });

        blitter.copy(
            device,
            &mut encoder,
            target_view,
            &surface_texture_view,
        );

        queue.submit([encoder.finish()]);
        surface_texture.present();

        Ok(())
    }

    fn with_graphics_api<R>(
        &self,
        callback: impl FnOnce(Option<i_slint_core::api::GraphicsAPI<'_>>) -> R,
    ) -> Result<R, i_slint_core::platform::PlatformError> {
        let instance = self.instance.borrow().clone();
        let device = self.device.borrow().clone();
        let queue = self.queue.borrow().clone();
        
        if let (Some(instance), Some(device), Some(queue)) = (instance, device, queue) {
            Ok(callback(Some(i_slint_core::graphics::create_graphics_api_wgpu_26(
                instance, device, queue,
            ))))
        } else {
            Ok(callback(None))
        }
    }

    fn resize(
        &self,
        width: NonZeroU32,
        height: NonZeroU32,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut surface_config = self.surface_config.borrow_mut();
        let Some(surface_config) = surface_config.as_mut() else {
            // Ignore resize when suspended
            return Ok(());
        };

        surface_config.width = width.get();
        surface_config.height = height.get();

        let device = self.device.borrow();
        let device = device.as_ref().ok_or("Device not initialized")?;

        self.surface
            .borrow_mut()
            .as_mut()
            .ok_or("Surface not initialized")?
            .configure(device, surface_config);

        // Recreate intermediate render targets with new size
        self.create_targets(width.get(), height.get(), device, surface_config.format);

        Ok(())
    }
}

impl WgpuBackend {
    /// Render a WGPU texture directly to the intermediate render target (GPU-only operation)
    /// This is much more efficient than reading back to CPU
    #[allow(dead_code)] // May be used in future for direct texture rendering
    fn render_wgpu_texture_to_target(
        &self,
        texture: &wgpu::Texture,
        _dest_x: f32,
        _dest_y: f32,
        _dest_width: f32,
        _dest_height: f32,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let device = self.device.borrow();
        let queue = self.queue.borrow();
        let target_texture = self.target_texture.borrow();
        
        let device = device.as_ref().ok_or("Device not initialized")?;
        let queue = queue.as_ref().ok_or("Queue not initialized")?;
        let target = target_texture.as_ref().ok_or("Target texture not initialized")?;
        
        // Create a simple blit pipeline if needed, or use copy operations
        // For now, we'll use texture copy which works for same-size textures
        let tex_size = texture.size();
        let target_size = target.size();
        
        // Simple case: if texture fits exactly, copy it directly
        if tex_size.width == target_size.width && tex_size.height == target_size.height {
            let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("WGPU texture blit encoder"),
            });
            
            encoder.copy_texture_to_texture(
                texture.as_image_copy(),
                target.as_image_copy(),
                tex_size,
            );
            
            queue.submit([encoder.finish()]);
            Ok(())
        } else {
            // For scaled/positioned rendering, we'd need a custom render pipeline
            // This is complex and would require shaders, so for now return an error
            Err("WGPU texture rendering with scaling/positioning not yet implemented".into())
        }
    }
}

impl crate::VelloRenderer<WgpuBackend> {
    /// Set the window handle for the renderer
    pub fn set_window_handle(
        &self,
        window_handle: Box<dyn wgpu::WindowHandle>,
        size: PhysicalWindowSize,
        requested_graphics_api: Option<RequestedGraphicsAPI>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.graphics_backend.set_window_handle(window_handle, size, requested_graphics_api)
    }
}
