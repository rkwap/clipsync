//! A plain file as the clipboard (`CLIPSYNC_CLIPBOARD_FILE`): replacing the
//! file is a copy, reading it is a paste. Used by the tests, so they never
//! touch the real clipboard; also handy for scripting. Writers should replace
//! the file atomically (write elsewhere, then rename) so it's never read half-written.

use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

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

pub fn set(text: &str) -> io::Result<()> {
    let tmp = path().with_extension("clipsync-tmp");
    fs::write(&tmp, text)?;
    fs::rename(tmp, path())
}

pub fn watch(mut on_change: impl FnMut(String, bool)) -> io::Result<()> {
    let read = || fs::read_to_string(path()).unwrap_or_default();
    let stamp = || fs::metadata(path()).ok().map(|m| (m.modified().ok(), m.len()));
    let mut last = stamp();
    on_change(read(), true);
    loop {
        thread::sleep(Duration::from_millis(50));
        let now = stamp();
        if now != last {
            last = now;
            let text = read();
            if !text.is_empty() {
                on_change(text, false);
            }
        }
    }
}
