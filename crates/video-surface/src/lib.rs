//! Binds the mpv render context into a Slint window.
//!
//! # Layer 2 of the isolation ladder
//!
//! Slint draws its scene on top of whatever is already in the framebuffer.
//! That makes it possible to let mpv draw the video first and the user
//! interface second, which is what this crate arranges.
//!
//! This is the one place in Flatline where two capabilities meet: the
//! renderer belongs to the playback engine, but it can only be created while
//! a UI toolkit has an OpenGL context. The coupling is deliberate, narrow
//! and documented — everywhere else the capabilities stay apart.

pub mod gl;

use std::fmt;

use playback_engine::{Engine, VideoRenderer};
use slint::{GraphicsAPI, RenderingState};

/// Everything that can go wrong while attaching the renderer.
#[derive(Debug)]
pub enum AttachError {
    /// The UI toolkit does not support rendering notifiers.
    NotSupported,
    /// The renderer could not be created.
    Renderer(String),
}

impl fmt::Display for AttachError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AttachError::NotSupported => {
                write!(f, "the user interface toolkit cannot host an OpenGL underlay")
            }
            AttachError::Renderer(message) => write!(f, "the video renderer failed: {message}"),
        }
    }
}

impl std::error::Error for AttachError {}

/// Draws the video of an [`Engine`] inside a Slint window.
///
/// The window draws the video first and the scene on top of it, so the
/// parts of the scene that stay transparent show the video.
///
/// `video_area` reports the size of the area the video is drawn into, in
/// physical pixels. It is called for every frame, which lets the video
/// follow the window when it is resized.
///
/// `request_redraw` asks the user interface for another frame. It is called
/// from mpv's own thread whenever a new video frame is ready, so it has to
/// be cheap and must not block.
pub fn attach(
    window: &slint::Window,
    engine: Engine,
    video_area: impl Fn() -> (i32, i32) + Send + Sync + 'static,
    request_redraw: impl Fn() + Send + Sync + 'static,
) -> Result<(), AttachError> {
    let mut renderer: Option<VideoRenderer> = None;
    let request_redraw = std::sync::Arc::new(request_redraw);

    window
        .set_rendering_notifier(move |state, api| {
            // The renderer only exists when the toolkit draws with OpenGL.
            if !matches!(api, GraphicsAPI::NativeOpenGL { .. }) {
                return;
            }

            match state {
                RenderingState::RenderingSetup => {
                    if renderer.is_some() {
                        return;
                    }
                    // SAFETY: the rendering notifier runs while the OpenGL
                    // context of the window is current, which is exactly what
                    // the renderer needs.
                    match unsafe { engine.create_renderer(gl::resolver()) } {
                        Ok(mut created) => {
                            let request_redraw = std::sync::Arc::clone(&request_redraw);
                            created.set_update_callback(move || {
                                request_redraw();
                            });
                            renderer = Some(created);
                        }
                        Err(error) => {
                            eprintln!("flatline: the video renderer is unavailable: {error}");
                        }
                    }
                }
                RenderingState::BeforeRendering => {
                    if let Some(renderer) = renderer.as_mut() {
                        let (width, height) = video_area();
                        if let Err(error) = renderer.draw(width, height) {
                            eprintln!("flatline: drawing a frame failed: {error}");
                        }
                    }
                }
                RenderingState::RenderingTeardown => {
                    renderer = None;
                }
                _ => {}
            }
        })
        .map_err(|_| AttachError::NotSupported)
}
