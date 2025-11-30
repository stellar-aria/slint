// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

use i_slint_core::graphics::RequestedGraphicsAPI;
use i_slint_core::platform::PlatformError;
use i_slint_core::{api::PhysicalSize as PhysicalWindowSize, item_rendering::ItemRenderer};
use std::rc::Rc;

use crate::display::{gbmdisplay::GbmDisplay, Presenter, RenderingRotation};
use crate::drmoutput::DrmOutput;
use crate::fullscreenwindowadapter::FullscreenRenderer;

pub struct VelloRendererAdapter {
    renderer: i_slint_renderer_vello::VelloRenderer<i_slint_renderer_vello::WgpuBackend>,
    presenter: Rc<GbmDisplay>,
}

impl VelloRendererAdapter {
    pub fn new(
        device_opener: &crate::DeviceOpener,
    ) -> Result<Box<dyn FullscreenRenderer>, PlatformError> {
        let drm_output = DrmOutput::new(device_opener)?;
        let presenter = Rc::new(GbmDisplay::new(drm_output)?);

        let size = PhysicalWindowSize {
            width: presenter.drm_output.size().0,
            height: presenter.drm_output.size().1,
        };

        let renderer = i_slint_renderer_vello::VelloRenderer::new(
            i_slint_renderer_vello::WgpuBackend::new(),
        )?;

        // Initialize Vello with GbmDisplay as window handle
        // GbmDisplay implements HasWindowHandle and HasDisplayHandle traits
        renderer.graphics_backend().set_window_handle(
            Box::new(presenter.clone()),
            size,
            None, // Use automatic graphics API selection
        )
        .map_err(|e| PlatformError::from(format!("Failed to initialize Vello backend: {}", e)))?;

        Ok(Box::new(VelloRendererAdapter { renderer, presenter }))
    }
}

impl FullscreenRenderer for VelloRendererAdapter {
    fn as_core_renderer(&self) -> &dyn i_slint_core::renderer::Renderer {
        &self.renderer
    }

    fn render_and_present(
        &self,
        rotation: RenderingRotation,
        draw_mouse_cursor_callback: &dyn Fn(&mut dyn ItemRenderer),
    ) -> Result<(), PlatformError> {
        let size = self.size();
        self.renderer.render_transformed_with_post_callback(
            rotation.degrees(),
            rotation.translation_after_rotation(size),
            size,
            Some(draw_mouse_cursor_callback),
        )?;

        self.presenter
            .present()
            .map_err(|e| format!("Error presenting Vello rendering: {}", e).into())
    }

    fn size(&self) -> PhysicalWindowSize {
        PhysicalWindowSize {
            width: self.presenter.drm_output.size().0,
            height: self.presenter.drm_output.size().1,
        }
    }
}

// Implement WindowHandle trait for GbmDisplay by using its raw-window-handle implementation
impl wgpu_26::WindowHandle for Rc<GbmDisplay> {
    fn window_handle(
        &self,
    ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        raw_window_handle::HasWindowHandle::window_handle(self.as_ref())
    }

    fn display_handle(
        &self,
    ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        raw_window_handle::HasDisplayHandle::display_handle(self.as_ref())
    }
}
