//! Platform clipboard access: `init` picks a backend, `set` writes text, and
//! `watch` calls back with the text whenever the clipboard changes (first
//! with its contents at startup, flagged as `initial`). `stop` ends any helper
//! processes before a restart.
//!
//! Setting `CLIPSYNC_CLIPBOARD_FILE` replaces the system clipboard with a
//! plain file, for tests and scripting.

use std::{env, io};

mod file;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(target_os = "linux")]
mod wayland;
#[cfg(target_os = "linux")]
mod x11;
#[cfg(target_os = "linux")]
use linux as platform;

pub fn init(backend: Option<&str>) -> io::Result<()> {
    if let Some(path) = env::var_os("CLIPSYNC_CLIPBOARD_FILE") {
        log!("using {} as the clipboard", path.to_string_lossy());
        file::init(path.into());
        return Ok(());
    }
    platform::init(backend)
}

pub fn set(text: &str) -> io::Result<()> {
    if file::active() { file::set(text) } else { platform::set(text) }
}

pub fn watch(on_change: impl FnMut(String, bool)) -> io::Result<()> {
    if file::active() { file::watch(on_change) } else { platform::watch(on_change) }
}

pub fn stop() {
    platform::stop();
}

#[cfg(target_os = "linux")]
mod linux {
    //! Prefer native Wayland when the compositor supports data-control,
    //! otherwise use X11. On Wayland desktops without data-control (GNOME),
    //! X11 still works through XWayland, which mirrors the clipboard.

    use std::io;
    use std::sync::OnceLock;
    use std::env;

    use super::{wayland, x11};

    #[derive(Clone, Copy)]
    enum Backend {
        Wayland,
        X11,
    }

    static BACKEND: OnceLock<Backend> = OnceLock::new();

    pub fn init(choice: Option<&str>) -> io::Result<()> {
        let backend = match choice {
            Some("wayland") => Backend::Wayland,
            Some("x11") => Backend::X11,
            Some(other) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown backend {other:?} (expected wayland or x11)"),
                ));
            }
            None => detect()?,
        };
        if let Backend::X11 = backend {
            x11::connect()?;
        }
        let _ = BACKEND.set(backend);
        log!("using {} clipboard", match backend {
            Backend::Wayland => "Wayland",
            Backend::X11 => "X11",
        });
        Ok(())
    }

    fn detect() -> io::Result<Backend> {
        if env::var_os("WAYLAND_DISPLAY").is_some() && wayland::supported() {
            return Ok(Backend::Wayland);
        }
        if env::var_os("DISPLAY").is_some() {
            if env::var_os("WAYLAND_DISPLAY").is_some() {
                log!(
                    "compositor lacks data-control (or wl-clipboard is missing); using XWayland. \
                     This relies on the compositor mirroring the clipboard to X11 for unfocused \
                     apps, which wlroots-based compositors don't do"
                );
            }
            return Ok(Backend::X11);
        }
        Err(io::Error::other(
            "no usable clipboard: need X11 (or XWayland), or a Wayland compositor \
             with data-control support and wl-clipboard installed",
        ))
    }

    fn backend() -> Backend {
        *BACKEND.get().expect("clip::init must be called first")
    }

    pub fn set(text: &str) -> io::Result<()> {
        match backend() {
            Backend::Wayland => wayland::set(text),
            Backend::X11 => x11::set(text),
        }
    }

    pub fn watch(on_change: impl FnMut(String, bool)) -> io::Result<()> {
        match backend() {
            Backend::Wayland => wayland::watch(on_change),
            Backend::X11 => x11::watch(on_change),
        }
    }

    pub fn stop() {
        wayland::stop();
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("clipsync supports Linux and macOS");
