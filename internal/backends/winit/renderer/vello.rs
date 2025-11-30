// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

use std::rc::Rc;
use std::sync::Arc;

use i_slint_core::renderer::Renderer;
use i_slint_core::platform::PlatformError;
use i_slint_renderer_vello::VelloRenderer;

use winit::event_loop::ActiveEventLoop;

use super::WinitCompatibleRenderer;

pub struct WGPUVelloRenderer {
    renderer: VelloRenderer<i_slint_renderer_vello::wgpu::WgpuBackend>,
    requested_graphics_api: Option<i_slint_core::graphics::RequestedGraphicsAPI>,
}

impl WGPUVelloRenderer {
    pub fn new_suspended(
        shared_backend_data: &Rc<crate::SharedBackendData>,
    ) -> Result<Box<dyn WinitCompatibleRenderer>, PlatformError> {
        if !i_slint_core::graphics::wgpu_26::any_wgpu26_adapters_with_gpu(
            shared_backend_data._requested_graphics_api.clone(),
        ) {
            return Err(PlatformError::from("Vello/WGPU: No GPU adapters found"));
        }

        let backend = i_slint_renderer_vello::wgpu::WgpuBackend::new();
        
        Ok(Box::new(Self {
            renderer: VelloRenderer::new(backend),
            requested_graphics_api: shared_backend_data._requested_graphics_api.clone(),
        }))
    }
}

impl WinitCompatibleRenderer for WGPUVelloRenderer {
    fn render(&self, _window: &i_slint_core::api::Window) -> Result<(), PlatformError> {
        self.renderer.render()
    }

    fn as_core_renderer(&self) -> &dyn Renderer {
        &self.renderer
    }

    fn suspend(&self) -> Result<(), PlatformError> {
        // Clear graphics context to release window references
        self.renderer.clear_graphics_context();
        Ok(())
    }

    fn resume(
        &self,
        active_event_loop: &ActiveEventLoop,
        window_attributes: winit::window::WindowAttributes,
    ) -> Result<Arc<winit::window::Window>, PlatformError> {
        let winit_window = Arc::new(active_event_loop.create_window(window_attributes).map_err(
            |winit_os_error| {
                PlatformError::from(format!(
                    "Error creating native window for Vello rendering: {}",
                    winit_os_error
                ))
            },
        )?);

        let size = winit_window.inner_size();

        // Initialize Vello with the window handle and size
        self.renderer.graphics_backend().set_window_handle(
            Box::new(winit_window.clone()),
            crate::winitwindowadapter::physical_size_to_slint(&size),
            self.requested_graphics_api.clone(),
        ).map_err(|e| PlatformError::from(format!("Failed to initialize Vello: {}", e)))?;

        Ok(winit_window)
    }
}
