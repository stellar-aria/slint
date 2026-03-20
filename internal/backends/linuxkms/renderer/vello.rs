// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

use i_slint_core::platform::PlatformError;
use i_slint_core::{api::PhysicalSize as PhysicalWindowSize, item_rendering::ItemRenderer};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use std::rc::Rc;

use crate::display::{Presenter, RenderingRotation, gbmdisplay::GbmDisplay};
use crate::drmoutput::DrmOutput;
use crate::fullscreenwindowadapter::FullscreenRenderer;

pub struct VelloRendererAdapter {
    renderer: i_slint_renderer_vello::VelloRenderer<i_slint_renderer_vello::wgpu::WgpuBackend>,
    presenter: Rc<GbmDisplay>,
}

/// Newtype wrapper around the raw GBM handles that implements `Send + Sync`.
///
/// `wgpu`'s `WindowHandle` trait requires `Send + Sync`, but `GbmDisplay` contains
/// `Rc` and `Cell` fields and is intentionally single-threaded. We satisfy the bound
/// by capturing the raw pointer values at construction time and asserting the impls.
struct GbmWindowHandleSendable {
    raw_window: raw_window_handle::RawWindowHandle,
    raw_display: raw_window_handle::RawDisplayHandle,
}

// SAFETY: The linuxkms backend is single-threaded and never moves `GbmDisplay` across
// threads. The raw GBM surface/device pointers remain valid for the lifetime of the
// `Rc<GbmDisplay>` held by `VelloRendererAdapter`, which outlives the wgpu surface.
unsafe impl Send for GbmWindowHandleSendable {}
unsafe impl Sync for GbmWindowHandleSendable {}

impl raw_window_handle::HasWindowHandle for GbmWindowHandleSendable {
    fn window_handle(
        &self,
    ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        Ok(unsafe { raw_window_handle::WindowHandle::borrow_raw(self.raw_window) })
    }
}

impl raw_window_handle::HasDisplayHandle for GbmWindowHandleSendable {
    fn display_handle(
        &self,
    ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        Ok(unsafe { raw_window_handle::DisplayHandle::borrow_raw(self.raw_display) })
    }
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

        let raw_window = presenter
            .window_handle()
            .map_err(|e| PlatformError::from(format!("Failed to get GBM window handle: {e}")))?
            .as_raw();
        let raw_display = presenter
            .display_handle()
            .map_err(|e| PlatformError::from(format!("Failed to get GBM display handle: {e}")))?
            .as_raw();

        let renderer = i_slint_renderer_vello::VelloRenderer::new(
            i_slint_renderer_vello::wgpu::WgpuBackend::new(),
        );

        renderer
            .graphics_backend()
            .set_window_handle(
                Box::new(GbmWindowHandleSendable { raw_window, raw_display }),
                size,
                None,
            )
            .map_err(|e| PlatformError::from(format!("Failed to initialize Vello backend: {e}")))?;

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
            rotation.translation_after_rotation(size).into(),
            size,
            Some(draw_mouse_cursor_callback),
        )?;

        self.presenter.present()?;
        Ok(())
    }

    fn size(&self) -> PhysicalWindowSize {
        PhysicalWindowSize {
            width: self.presenter.drm_output.size().0,
            height: self.presenter.drm_output.size().1,
        }
    }
}
