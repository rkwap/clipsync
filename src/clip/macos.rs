//! AppKit's NSPasteboard. macOS has no clipboard-change notification, so
//! we poll `changeCount` (a cheap integer read) and only read the contents
//! when it changes. Images go out as PNG (converted from TIFF, which most Mac
//! apps copy), and come in as both PNG and TIFF.
#![allow(unused_unsafe)]

use std::io;
use std::thread;
use std::time::Duration;

use objc2::rc::autoreleasepool;
use objc2_app_kit::{
    NSBitmapImageFileType, NSBitmapImageRep, NSPasteboard, NSPasteboardTypePNG, NSPasteboardTypeString,
    NSPasteboardTypeTIFF,
};
use objc2_foundation::{NSData, NSDictionary, NSString};

use super::{Content, is_png};

const POLL: Duration = Duration::from_millis(250);

pub fn init(backend: Option<&str>) -> io::Result<()> {
    match backend {
        Some(_) => Err(io::Error::new(io::ErrorKind::InvalidInput, "--backend only applies on Linux")),
        None => Ok(()),
    }
}

pub fn stop() {}

pub fn set(content: &Content) -> io::Result<()> {
    autoreleasepool(|_| unsafe {
        let pb = NSPasteboard::generalPasteboard();
        pb.clearContents();
        let ok = match content {
            Content::Text(text) => pb.setString_forType(&NSString::from_str(text), NSPasteboardTypeString),
            Content::Image(png) => {
                let data = NSData::with_bytes(png);
                let mut ok = pb.setData_forType(Some(&data), NSPasteboardTypePNG);
                // Plenty of Mac apps only paste TIFF, so offer that too.
                if let Some(tiff) = NSBitmapImageRep::imageRepWithData(&data).and_then(|rep| rep.TIFFRepresentation()) {
                    ok &= pb.setData_forType(Some(&tiff), NSPasteboardTypeTIFF);
                }
                ok
            }
        };
        if ok { Ok(()) } else { Err(io::Error::other("NSPasteboard rejected the write")) }
    })
}

/// The pasteboard's text if it has any, otherwise its image as PNG.
fn read(pb: &NSPasteboard) -> Option<Content> {
    unsafe {
        if let Some(text) = pb.stringForType(NSPasteboardTypeString) {
            return Some(Content::Text(text.to_string().into()));
        }
        if let Some(png) = pb.dataForType(NSPasteboardTypePNG).map(|d| d.to_vec()).filter(|b| is_png(b)) {
            return Some(Content::Image(png.into()));
        }
        // Most Mac apps copy images as TIFF; convert it.
        let tiff = pb.dataForType(NSPasteboardTypeTIFF)?;
        let rep = NSBitmapImageRep::imageRepWithData(&tiff)?;
        let png = rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())?;
        Some(Content::Image(png.to_vec().into()))
    }
}

pub fn watch(mut on_change: impl FnMut(Content, bool)) -> io::Result<()> {
    let (mut last, initial) = autoreleasepool(|_| unsafe {
        let pb = NSPasteboard::generalPasteboard();
        (pb.changeCount(), read(&pb))
    });
    on_change(initial.unwrap_or(Content::Text("".into())), true);
    loop {
        thread::sleep(POLL);
        let content = autoreleasepool(|_| unsafe {
            let pb = NSPasteboard::generalPasteboard();
            let count = pb.changeCount();
            if count == last {
                return None;
            }
            last = count;
            read(&pb)
        });
        if let Some(content) = content {
            on_change(content, false);
        }
    }
}
