//! AppKit's NSPasteboard. macOS has no clipboard-change notification, so
//! we poll `changeCount` (a cheap integer read) and only read the text
//! when it changes.
#![allow(unused_unsafe)]

use std::io;
use std::thread;
use std::time::Duration;

use objc2::rc::autoreleasepool;
use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
use objc2_foundation::NSString;

const POLL: Duration = Duration::from_millis(250);

pub fn init(backend: Option<&str>) -> io::Result<()> {
    match backend {
        Some(_) => Err(io::Error::new(io::ErrorKind::InvalidInput, "--backend only applies on Linux")),
        None => Ok(()),
    }
}

pub fn stop() {}

pub fn set(text: &str) -> io::Result<()> {
    autoreleasepool(|_| unsafe {
        let pb = NSPasteboard::generalPasteboard();
        pb.clearContents();
        if pb.setString_forType(&NSString::from_str(text), NSPasteboardTypeString) {
            Ok(())
        } else {
            Err(io::Error::other("NSPasteboard rejected the write"))
        }
    })
}

pub fn watch(mut on_change: impl FnMut(String, bool)) -> io::Result<()> {
    let (mut last, initial) = autoreleasepool(|_| unsafe {
        let pb = NSPasteboard::generalPasteboard();
        (pb.changeCount(), pb.stringForType(NSPasteboardTypeString).map(|s| s.to_string()))
    });
    on_change(initial.unwrap_or_default(), true);
    loop {
        thread::sleep(POLL);
        let text = autoreleasepool(|_| unsafe {
            let pb = NSPasteboard::generalPasteboard();
            let count = pb.changeCount();
            if count == last {
                return None;
            }
            last = count;
            pb.stringForType(NSPasteboardTypeString).map(|s| s.to_string())
        });
        if let Some(text) = text {
            on_change(text, false);
        }
    }
}
