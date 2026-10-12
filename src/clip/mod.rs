//! Platform clipboard access: `init` picks a backend, `set` puts content on
//! the clipboard, and `watch` calls back with the content whenever the
//! clipboard changes (first with its contents at startup, flagged as
//! `initial`; empty text there means nothing usable). `stop` ends any helper
//! processes before a restart.
//!
//! Content is text, or an image as PNG: every platform's clipboard takes PNG.
//! When a copy offers both (spreadsheet cells, say), the text is used.
//!
//! Setting `CLIPSYNC_CLIPBOARD_FILE` replaces the system clipboard with a
//! plain file, for tests and scripting.

use std::sync::Arc;
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

/// What's on a clipboard.
#[derive(Clone, PartialEq, Debug)]
pub enum Content {
    Text(Arc<str>),
    /// PNG-encoded.
    Image(Arc<[u8]>),
}

const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

pub fn is_png(bytes: &[u8]) -> bool {
    bytes.starts_with(PNG_SIGNATURE)
}

impl Content {
    pub fn is_empty(&self) -> bool {
        match self {
            Content::Text(t) => t.is_empty(),
            Content::Image(i) => i.is_empty(),
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Content::Text(t) => t.len(),
            Content::Image(i) => i.len(),
        }
    }

    /// Identifies the content; text and an image never collide.
    pub fn hash(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        match self {
            Content::Text(t) => h.update(b"t").update(t.as_bytes()),
            Content::Image(i) => h.update(b"i").update(i),
        };
        *h.finalize().as_bytes()
    }

    /// For logs: "18 bytes of text", "an image (1.4 MB)".
    pub fn describe(&self) -> String {
        match self {
            Content::Text(t) => format!("{} bytes of text", t.len()),
            Content::Image(i) if i.len() >= 1 << 20 => format!("an image ({:.1} MB)", i.len() as f64 / (1 << 20) as f64),
            Content::Image(i) => format!("an image ({} KB)", i.len().div_ceil(1024)),
        }
    }
}

pub fn set(content: &Content) -> io::Result<()> {
    if file::active() { file::set(content) } else { platform::set(content) }
}

pub fn watch(on_change: impl FnMut(Content, bool)) -> io::Result<()> {
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

    use super::{Content, wayland, x11};

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

    pub fn set(content: &Content) -> io::Result<()> {
        match backend() {
            Backend::Wayland => wayland::set(content),
            Backend::X11 => x11::set(content),
        }
    }

    pub fn watch(on_change: impl FnMut(Content, bool)) -> io::Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_and_images_hash_apart() {
        let text = Content::Text("abc".into());
        let image = Content::Image(b"abc".to_vec().into());
        assert_ne!(text.hash(), image.hash());
        assert_eq!(text.hash(), Content::Text("abc".into()).hash());
    }

    #[test]
    fn describes_content() {
        assert_eq!(Content::Text("hello".into()).describe(), "5 bytes of text");
        assert_eq!(Content::Image(vec![0; 1500].into()).describe(), "an image (2 KB)");
        assert_eq!(Content::Image(vec![0; 3 << 20].into()).describe(), "an image (3.0 MB)");
    }

    #[test]
    fn recognizes_png() {
        assert!(is_png(b"\x89PNG\r\n\x1a\nrest"));
        assert!(!is_png(b"GIF89a"));
    }
}
