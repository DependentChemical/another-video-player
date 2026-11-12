//! Resolving OpenGL entry points.
//!
//! mpv needs the addresses of the OpenGL functions it uses. It asks for them
//! through a callback while the render context is created, and the callback
//! has to stay valid for as long as the context lives — so it cannot borrow
//! anything that belongs to the UI toolkit.
//!
//! This module therefore resolves the symbols through the operating system
//! loader, which always points at the same driver the toolkit is using:
//!
//! * Windows: `GetProcAddress` on `opengl32.dll`, plus `wglGetProcAddress`
//!   for everything that is not part of the core profile.
//! * Unix: `glXGetProcAddressARB` or `eglGetProcAddress`, falling back to
//!   `dlsym`.

use std::ffi::{CString, c_char, c_void};

/// Builds a resolver that can be handed to mpv.
pub fn resolver() -> Box<dyn FnMut(&str) -> *mut c_void + Send + 'static> {
    Box::new(|name: &str| resolve(name))
}

/// Resolves a single OpenGL entry point.
pub fn resolve(name: &str) -> *mut c_void {
    let Ok(name) = CString::new(name) else {
        return std::ptr::null_mut();
    };
    platform::resolve(name.as_ptr())
}

#[cfg(windows)]
mod platform {
    use super::{c_char, c_void};

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetModuleHandleA(module_name: *const c_char) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const c_char) -> *mut c_void;
    }

    type WglGetProcAddress = unsafe extern "system" fn(*const c_char) -> *mut c_void;

    /// The `wglGetProcAddress` of the current driver, looked up once.
    fn wgl_get_proc_address() -> Option<WglGetProcAddress> {
        // SAFETY: both calls are plain lookups; a missing module or symbol
        // simply yields null.
        unsafe {
            let module = GetModuleHandleA(c"opengl32.dll".as_ptr());
            if module.is_null() {
                return None;
            }
            let symbol = GetProcAddress(module, c"wglGetProcAddress".as_ptr());
            if symbol.is_null() {
                return None;
            }
            Some(std::mem::transmute::<*mut c_void, WglGetProcAddress>(symbol))
        }
    }

    pub fn resolve(name: *const c_char) -> *mut c_void {
        // SAFETY: the module handle and the symbol lookup are safe to call at
        // any time; the returned pointer is only used while a GL context is
        // current, which is the case whenever mpv asks for it.
        unsafe {
            let module = GetModuleHandleA(c"opengl32.dll".as_ptr());
            if module.is_null() {
                return std::ptr::null_mut();
            }
            let mut pointer = GetProcAddress(module, name);
            if pointer.is_null()
                && let Some(wgl) = wgl_get_proc_address()
            {
                pointer = wgl(name);
                // Drivers report unsupported functions with 1, 2, 3 or -1.
                if (pointer as isize) <= 3 {
                    pointer = std::ptr::null_mut();
                }
            }
            pointer
        }
    }
}

#[cfg(unix)]
mod platform {
    use super::{c_char, c_void};
    use std::sync::OnceLock;

    extern "C" {
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    }

    #[cfg(target_os = "linux")]
    const RTLD_DEFAULT: *mut c_void = std::ptr::null_mut();
    #[cfg(not(target_os = "linux"))]
    const RTLD_DEFAULT: *mut c_void = (-2isize) as *mut c_void;

    type Loader = unsafe extern "C" fn(*const c_char) -> *mut c_void;

    /// `glXGetProcAddressARB`, when the process uses GLX.
    fn glx_loader() -> Option<Loader> {
        static LOADER: OnceLock<Option<Loader>> = OnceLock::new();
        *LOADER.get_or_init(|| unsafe {
            let pointer = dlsym(RTLD_DEFAULT, c"glXGetProcAddressARB".as_ptr());
            (!pointer.is_null()).then(|| std::mem::transmute::<*mut c_void, Loader>(pointer))
        })
    }

    /// `eglGetProcAddress`, when the process uses EGL.
    fn egl_loader() -> Option<Loader> {
        static LOADER: OnceLock<Option<Loader>> = OnceLock::new();
        *LOADER.get_or_init(|| unsafe {
            let pointer = dlsym(RTLD_DEFAULT, c"eglGetProcAddress".as_ptr());
            (!pointer.is_null()).then(|| std::mem::transmute::<*mut c_void, Loader>(pointer))
        })
    }

    pub fn resolve(name: *const c_char) -> *mut c_void {
        // SAFETY: `dlsym` with a valid, null terminated symbol name is safe;
        // the returned pointer is only used while a GL context is current.
        unsafe {
            if let Some(loader) = glx_loader() {
                let pointer = loader(name);
                if !pointer.is_null() {
                    return pointer;
                }
            }
            if let Some(loader) = egl_loader() {
                let pointer = loader(name);
                if !pointer.is_null() {
                    return pointer;
                }
            }
            dlsym(RTLD_DEFAULT, name)
        }
    }
}

#[cfg(not(any(windows, unix)))]
mod platform {
    use super::{c_char, c_void};

    pub fn resolve(_name: *const c_char) -> *mut c_void {
        std::ptr::null_mut()
    }
}
