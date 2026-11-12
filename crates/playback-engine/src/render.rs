//! Drawing the video with the mpv render API.
//!
//! mpv renders into an OpenGL framebuffer that the embedder owns. The
//! renderer here wraps that: it hands mpv a framebuffer description and
//! asks it to draw the current frame.

use std::ffi::c_void;
use std::sync::Arc;

use rsmpv::Mpv;
use rsmpv::render::{OpenGlFbo, OwnedRenderContext};

use crate::Error;

/// Resolves OpenGL entry points by name.
///
/// The resolver has to stay valid for as long as the renderer exists, which
/// is why it is boxed and requires `Send`.
pub type GlProcAddress = Box<dyn FnMut(&str) -> *mut c_void + Send + 'static>;

/// Draws the video of an mpv player.
pub struct VideoRenderer {
    context: OwnedRenderContext,
}

impl VideoRenderer {
    /// Creates a renderer for an mpv core.
    ///
    /// # Safety
    ///
    /// The call has to happen while an OpenGL context is current, and that
    /// context has to stay current for every later use of the renderer.
    pub unsafe fn create(core: Arc<Mpv>, gl: GlProcAddress) -> Result<VideoRenderer, Error> {
        let context = unsafe { OwnedRenderContext::new_opengl(core, false, gl) }?;
        Ok(VideoRenderer { context })
    }

    /// Draws the current frame into the default framebuffer.
    ///
    /// `width` and `height` describe the size of the area the video is
    /// drawn into. The video keeps its aspect ratio and is centred inside
    /// that area.
    pub fn draw(&mut self, width: i32, height: i32) -> Result<(), Error> {
        if width <= 0 || height <= 0 {
            return Ok(());
        }
        self.context.render_opengl(
            OpenGlFbo { fbo: 0, width, height, internal_format: 0 },
            true,
            true,
        )?;
        Ok(())
    }

    /// Registers a callback that fires whenever a new frame is ready.
    pub fn set_update_callback(&mut self, callback: impl Fn() + Send + Sync + 'static) {
        self.context.set_update_callback(callback);
    }

    /// Tells mpv that a frame was presented.
    pub fn report_swap(&mut self) {
        self.context.report_swap();
    }
}
