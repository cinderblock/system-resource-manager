//! The local endpoint, and the guarantee that only one daemon owns it.

use anyhow::{Context, Result};

#[cfg(windows)]
mod imp {
    use super::*;
    use std::ffi::c_void;
    use tokio::net::windows::named_pipe::{NamedPipeServer, PipeMode, ServerOptions};
    use windows_sys::Win32::Foundation::{LocalFree, ERROR_ACCESS_DENIED};
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;

    /// The default DACL grants access to whoever created the pipe, so a daemon first
    /// started from an elevated shell would lock out every unelevated client. Name
    /// the access explicitly: full control for SYSTEM, administrators and the owner;
    /// read and write, but not creating new instances, for any authenticated user.
    const SDDL: &str = "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;OW)(A;;0x12019b;;;AU)";

    pub struct Listener {
        endpoint: String,
        next: NamedPipeServer,
    }

    struct Descriptor(*mut c_void);

    impl Descriptor {
        fn new() -> Result<Self> {
            let sddl: Vec<u16> = SDDL.encode_utf16().chain(Some(0)).collect();
            let mut descriptor = std::ptr::null_mut();
            // SAFETY: `sddl` is NUL-terminated and outlives the call; on success the
            // out-pointer owns a LocalAlloc'd descriptor, which Drop frees.
            let ok = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(std::io::Error::last_os_error()).context("building pipe DACL");
            }
            Ok(Self(descriptor))
        }
    }

    impl Drop for Descriptor {
        fn drop(&mut self) {
            // SAFETY: allocated by the conversion above and freed only here.
            unsafe { LocalFree(self.0) };
        }
    }

    fn create(endpoint: &str, first: bool) -> std::io::Result<NamedPipeServer> {
        let descriptor = Descriptor::new().map_err(std::io::Error::other)?;
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };
        // SAFETY: `attributes` points at a live descriptor for the whole call, and
        // the system copies it into the pipe.
        unsafe {
            ServerOptions::new()
                .pipe_mode(PipeMode::Byte)
                .first_pipe_instance(first)
                .create_with_security_attributes_raw(
                    endpoint,
                    (&raw mut attributes).cast::<c_void>(),
                )
        }
    }

    impl Listener {
        /// `None` means another daemon already owns the endpoint.
        pub fn bind(endpoint: &str) -> Result<Option<Self>> {
            match create(endpoint, true) {
                Ok(next) => Ok(Some(Self {
                    endpoint: endpoint.to_string(),
                    next,
                })),
                Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => Ok(None),
                Err(e) => Err(e).with_context(|| format!("binding {endpoint}")),
            }
        }

        pub async fn accept(&mut self) -> Result<NamedPipeServer> {
            self.next.connect().await.context("accepting")?;
            let fresh = create(&self.endpoint, false).context("creating pipe instance")?;
            Ok(std::mem::replace(&mut self.next, fresh))
        }
    }
}

#[cfg(unix)]
mod imp {
    use super::*;
    use std::fs::File;
    use tokio::net::{UnixListener, UnixStream};

    pub struct Listener {
        inner: UnixListener,
        // Held for the life of the process. A socket file can outlive its daemon, so
        // it can't be what proves ownership.
        _lock: File,
    }

    impl Listener {
        /// `None` means another daemon already owns the endpoint.
        pub fn bind(endpoint: &str) -> Result<Option<Self>> {
            let lock = File::create(format!("{endpoint}.lock")).context("creating lock file")?;
            if lock.try_lock().is_err() {
                return Ok(None);
            }
            let _ = std::fs::remove_file(endpoint);
            let inner =
                UnixListener::bind(endpoint).with_context(|| format!("binding {endpoint}"))?;
            Ok(Some(Self { inner, _lock: lock }))
        }

        pub async fn accept(&mut self) -> Result<UnixStream> {
            Ok(self.inner.accept().await.context("accepting")?.0)
        }
    }
}

pub use imp::Listener;
