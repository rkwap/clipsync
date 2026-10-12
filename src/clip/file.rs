//! A plain file as the clipboard (`CLIPSYNC_CLIPBOARD_FILE`): replacing the
//! file is a copy, reading it is a paste. A file starting with the PNG
//! signature holds an image; anything else is UTF-8 text. Used by the tests, so they never
//! touch the real clipboard; also handy for scripting. Writers should replace
//! the file atomically (write elsewhere, then rename) so it's never read half-written.

use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

use super::{Content, is_png};

static PATH: OnceLock<PathBuf> = OnceLock::new();

pub fn init(path: PathBuf) {
    let _ = PATH.set(path);
}

pub fn active() -> bool {
    PATH.get().is_some()
}

fn path() -> &'static PathBuf {
    PATH.get().expect("file clipboard not initialized")
}

pub fn set(content: &Content) -> io::Result<()> {
    let tmp = path().with_extension("clipsync-tmp");
    match content {
        Content::Text(t) => fs::write(&tmp, t.as_bytes())?,
        Content::Image(i) => fs::write(&tmp, i)?,
    }
    fs::rename(tmp, path())
}

fn read() -> Content {
    let bytes = fs::read(path()).unwrap_or_default();
    if is_png(&bytes) {
        return Content::Image(bytes.into());
    }
    Content::Text(String::from_utf8(bytes).unwrap_or_default().into())
}

pub fn watch(mut on_change: impl FnMut(Content, bool)) -> io::Result<()> {
    let stamp = || fs::metadata(path()).ok().map(|m| (m.modified().ok(), m.len()));
    let mut last = stamp();
    on_change(read(), true);
    loop {
        thread::sleep(Duration::from_millis(50));
        let now = stamp();
        if now != last {
            last = now;
            let content = read();
            if !content.is_empty() {
                on_change(content, false);
            }
        }
    }
}
