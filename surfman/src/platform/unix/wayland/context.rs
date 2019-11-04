// surfman/surfman/src/platform/unix/wayland/context.rs
//
//! A wrapper around Wayland `EGLContext`s.

use crate::context::{CREATE_CONTEXT_MUTEX, ContextID};
use crate::egl::types::{EGLConfig, EGLint};
use crate::egl;
use crate::gl::Gl;
use crate::platform::generic::egl::context::{self, CurrentContextGuard, NativeContext};
use crate::platform::generic::egl::context::{OwnedEGLContext, UnsafeEGLContextRef};
use crate::platform::generic::egl::device::EGL_FUNCTIONS;
use crate::platform::generic::egl::error::ToWindowingApiError;
use crate::surface::Framebuffer;
use crate::{ContextAttributes, Error, SurfaceAccess, SurfaceType};
use super::device::Device;
use super::surface::{NativeWidget, Surface, WaylandObjects};

use std::mem;
use std::os::raw::c_void;
use std::thread;

pub use crate::platform::generic::egl::context::ContextDescriptor;

thread_local! {
    pub static GL_FUNCTIONS: Gl = Gl::load_with(context::get_proc_address);
}

pub struct Context {
    pub(crate) native_context: Box<dyn NativeContext>,
    pub(crate) id: ContextID,
    framebuffer: Framebuffer<Surface>,
}

impl Drop for Context {
    #[inline]
    fn drop(&mut self) {
        if !self.native_context.is_destroyed() && !thread::panicking() {
            panic!("Contexts must be destroyed explicitly with `destroy_context`!")
        }
    }
}

impl Device {
    #[inline]
    pub fn create_context_descriptor(&self, attributes: &ContextAttributes)
                                     -> Result<ContextDescriptor, Error> {
        unsafe {
            ContextDescriptor::new(self.native_connection.egl_display(), attributes, &[
                egl::SURFACE_TYPE as EGLint,    egl::WINDOW_BIT as EGLint,
                egl::RENDERABLE_TYPE as EGLint, egl::OPENGL_BIT as EGLint,
            ])
        }
    }

    /// Opens the device and context corresponding to the current EGL context.
    ///
    /// The native context is not retained, as there is no way to do this in the EGL API. It is
    /// the caller's responsibility to keep it alive for the duration of this context. Be careful
    /// when using this method; it's essentially a last resort.
    ///
    /// This method is designed to allow `surfman` to deal with contexts created outside the
    /// library; for example, by Glutin. It's legal to use this method to wrap a context rendering
    /// to any target: either a window or a pbuffer. The target is opaque to `surfman`; the
    /// library will not modify or try to detect the render target. This means that any of the
    /// methods that query or replace the surface—e.g. `replace_context_surface`—will fail if
    /// called with a context object created via this method.
    ///
    /// TODO(pcwalton)
    pub unsafe fn from_current_context() -> Result<(Device, Context), Error> {
        /*
        let mut next_context_id = CREATE_CONTEXT_MUTEX.lock().unwrap();

        EGL_FUNCTIONS.with(|egl| {
            // Grab the current EGL display and EGL context.
            let egl_display = egl.GetCurrentDisplay();
            if egl_display == egl::NO_DISPLAY {
                return Err(Error::NoCurrentContext);
            }
            let egl_context = egl.GetCurrentContext();
            if egl_context == egl::NO_CONTEXT {
                return Err(Error::NoCurrentContext);
            }
            let native_context = Box::new(UnsafeEGLContextRef { egl_context });

            // Create the context.
            let mut context = Context {
                native_context,
                id: *next_context_id,
                framebuffer: Framebuffer::External,
            };
            next_context_id.0 += 1;

            Ok((device, context))
        });
        */
        unimplemented!()
    }

    pub fn create_context(&mut self, descriptor: &ContextDescriptor) -> Result<Context, Error> {
        let mut next_context_id = CREATE_CONTEXT_MUTEX.lock().unwrap();
        unsafe {
            // Create the context.
            let egl_display = self.native_connection.egl_display();
            let egl_context = context::create_context(egl_display, descriptor)?;

            // Wrap and return it.
            let context = Context {
                native_context: Box::new(OwnedEGLContext { egl_context }),
                id: *next_context_id,
                framebuffer: Framebuffer::None,
            };
            next_context_id.0 += 1;
            Ok(context)
        }
    }

    pub fn destroy_context(&self, context: &mut Context) -> Result<(), Error> {
        if context.native_context.is_destroyed() {
            return Ok(());
        }

        if let Ok(Some(surface)) = self.unbind_surface_from_context(context) {
            self.destroy_surface(context, surface)?;
        }

        unsafe {
            context.native_context.destroy(self.native_connection.egl_display());
        }

        Ok(())
    }

    pub fn context_descriptor(&self, context: &Context) -> ContextDescriptor {
        unsafe {
            ContextDescriptor::from_egl_context(self.native_connection.egl_display(),
                                                context.native_context.egl_context())
        }
    }

    pub fn make_context_current(&self, context: &Context) -> Result<(), Error> {
        unsafe {
            let egl_surface = match context.framebuffer {
                Framebuffer::Surface(Surface {
                    wayland_objects: WaylandObjects::Window { egl_surface, .. },
                    ..
                }) => egl_surface,
                Framebuffer::Surface(Surface {
                    wayland_objects: WaylandObjects::TextureImage { .. },
                    ..
                }) | Framebuffer::None => egl::NO_SURFACE,
                Framebuffer::External => {
                    return Err(Error::ExternalRenderTarget)
                }
            };

            EGL_FUNCTIONS.with(|egl| {
                let result = egl.MakeCurrent(self.native_connection.egl_display(),
                                             egl_surface,
                                             egl_surface,
                                             context.native_context.egl_context());
                if result == egl::FALSE {
                    let err = egl.GetError().to_windowing_api_error();
                    return Err(Error::MakeCurrentFailed(err));
                }
                Ok(())
            })
        }
    }

    pub fn make_no_context_current(&self) -> Result<(), Error> {
        unsafe {
            context::make_no_context_current(self.native_connection.egl_display())
        }
    }

    pub(crate) fn temporarily_make_context_current(&self, context: &Context)
                                                   -> Result<CurrentContextGuard, Error> {
        let guard = CurrentContextGuard::new();
        self.make_context_current(context)?;
        Ok(guard)
    }

    pub(crate) fn context_is_current(&self, context: &Context) -> bool {
        EGL_FUNCTIONS.with(|egl| {
            unsafe {
                egl.GetCurrentContext() == context.native_context.egl_context()
            }
        })
    }

    pub fn context_surface<'c>(&self, context: &'c Context) -> Result<Option<&'c Surface>, Error> {
        match context.framebuffer {
            Framebuffer::None => Ok(None),
            Framebuffer::External => Err(Error::ExternalRenderTarget),
            Framebuffer::Surface(ref surface) => Ok(Some(surface)),
        }
    }

    #[inline]
    pub fn context_descriptor_attributes(&self, context_descriptor: &ContextDescriptor)
                                         -> ContextAttributes {
        unsafe {
            context_descriptor.attributes(self.native_connection.egl_display())
        }
    }

    #[inline]
    pub fn get_proc_address(&self, _: &Context, symbol_name: &str) -> *const c_void {
        context::get_proc_address(symbol_name)
    }

    #[inline]
    pub(crate) fn context_descriptor_to_egl_config(&self, context_descriptor: &ContextDescriptor)
                                                   -> EGLConfig {
        unsafe {
            context::egl_config_from_id(self.native_connection.egl_display(),
                                        context_descriptor.egl_config_id)
        }
    }

    pub fn bind_surface_to_context(&self, context: &mut Context, surface: Surface)
                                   -> Result<(), Error> {
        if context.id != surface.context_id {
            return Err(Error::IncompatibleSurface);
        }

        match context.framebuffer {
            Framebuffer::None => {
                let is_current = self.context_is_current(context);
                context.framebuffer = Framebuffer::Surface(surface);

                if is_current {
                    // We need to make ourselves current again, because the surface changed.
                    self.make_context_current(context)?;
                }

                Ok(())
            }
            Framebuffer::External => Err(Error::ExternalRenderTarget),
            Framebuffer::Surface(_) => Err(Error::SurfaceAlreadyBound),
        }
    }

    pub fn unbind_surface_from_context(&self, context: &mut Context)
                                       -> Result<Option<Surface>, Error> {
        match context.framebuffer {
            Framebuffer::None => return Ok(None),
            Framebuffer::Surface(_) => {}
            Framebuffer::External => return Err(Error::ExternalRenderTarget),
        }

        match mem::replace(&mut context.framebuffer, Framebuffer::None) {
            Framebuffer::Surface(surface) => Ok(Some(surface)),
            Framebuffer::None | Framebuffer::External => unreachable!(),
        }
    }

    #[inline]
    pub fn context_id(&self, context: &Context) -> ContextID {
        context.id
    }
}