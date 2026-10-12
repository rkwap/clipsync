//! X11 (and XWayland), spoken natively via x11rb. The XFixes extension tells
//! us whenever the clipboard owner changes, so idle cost is zero.
//!
//! X11 doesn't store clipboard contents: the app that copied keeps the data
//! and hands it to whoever asks. To read, we ask the owner to convert the
//! CLIPBOARD selection into a property on our (invisible) window. To write,
//! we become the owner and answer other apps' requests ourselves. Large
//! transfers move in chunks both ways (the ICCCM "INCR" protocol).
//!
//! Reading starts by asking the owner which formats it offers (TARGETS):
//! text if there's any, otherwise a PNG image.

use std::error::Error;
use std::io;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xfixes::{ConnectionExt as _, SelectionEventMask};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, CreateWindowAux, EventMask, GetPropertyReply,
    PropMode, Property, SELECTION_NOTIFY_EVENT, SelectionNotifyEvent, SelectionRequestEvent, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT, CURRENT_TIME, NONE};

use super::{Content, is_png};
use crate::lock;
use crate::net::MAX_CONTENT;

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

/// Anything larger than this is sent to other apps in chunks of this size.
/// Many apps refuse to read one huge property.
const CHUNK: usize = 256 * 1024;

/// Give up on chunked sends to apps that stop reading.
const SEND_TIMEOUT: Duration = Duration::from_secs(30);

x11rb::atom_manager! {
    Atoms: AtomsCookie {
        CLIPBOARD,
        UTF8_STRING,
        TARGETS,
        TEXT,
        INCR,
        TEXT_PLAIN: b"text/plain;charset=utf-8",
        IMAGE_PNG: b"image/png",
        CLIPSYNC: b"CLIPSYNC_DATA",
    }
}

struct X11 {
    conn: RustConnection,
    win: Window,
    atoms: Atoms,
    /// What we serve to other apps while we own the clipboard.
    owned: Mutex<Option<Content>>,
}

static X: OnceLock<X11> = OnceLock::new();

fn x() -> &'static X11 {
    X.get().expect("x11::connect must be called first")
}

/// State of an in-progress read from another app. The flag marks the read
/// of the clipboard's contents at startup.
enum Read {
    Idle,
    /// Asked the owner which formats it offers.
    Targets(bool),
    /// Asked the owner for `target`, waiting for its SelectionNotify.
    Waiting(Atom, bool),
    /// Large transfer, arriving in chunks.
    Incr(Atom, Vec<u8>, bool),
}

/// A chunked send to another app. Each time it deletes the property, we
/// write the next chunk; an empty chunk ends the transfer.
struct Outgoing {
    requestor: Window,
    property: Atom,
    ty: Atom,
    data: Arc<[u8]>,
    offset: usize,
    started: Instant,
}

pub fn connect() -> io::Result<()> {
    try_connect().map_err(|e| io::Error::other(format!("X11: {e}")))
}

fn try_connect() -> Result<()> {
    let (conn, screen) = x11rb::connect(None)?;
    let root = conn.setup().roots[screen].root;
    let win = conn.generate_id()?;
    conn.create_window(
        COPY_DEPTH_FROM_PARENT,
        win,
        root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_OUTPUT,
        COPY_FROM_PARENT,
        // Property changes drive chunked reads.
        &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )?;
    let atoms = Atoms::new(&conn)?.reply()?;
    conn.xfixes_query_version(5, 0)?.reply()?;
    conn.xfixes_select_selection_input(
        win,
        atoms.CLIPBOARD,
        SelectionEventMask::SET_SELECTION_OWNER
            | SelectionEventMask::SELECTION_WINDOW_DESTROY
            | SelectionEventMask::SELECTION_CLIENT_CLOSE,
    )?;
    conn.flush()?;
    let _ = X.set(X11 { conn, win, atoms, owned: Mutex::new(None) });
    Ok(())
}

pub fn set(content: &Content) -> io::Result<()> {
    try_set(content).map_err(io::Error::other)
}

fn try_set(content: &Content) -> Result<()> {
    let x = x();
    *lock(&x.owned) = Some(content.clone());
    x.conn.set_selection_owner(x.win, x.atoms.CLIPBOARD, CURRENT_TIME)?;
    let owner = x.conn.get_selection_owner(x.atoms.CLIPBOARD)?.reply()?.owner;
    if owner != x.win {
        return Err("could not take ownership of the clipboard".into());
    }
    Ok(())
}

pub fn watch(mut on_change: impl FnMut(Content, bool)) -> io::Result<()> {
    try_watch(&mut on_change).map_err(|e| io::Error::other(format!("X11: {e}")))
}

/// Nothing usable on the clipboard (only reported for the startup read).
fn nothing(initial: bool, on_change: &mut dyn FnMut(Content, bool)) {
    if initial {
        on_change(Content::Text("".into()), true);
    }
}

fn try_watch(on_change: &mut dyn FnMut(Content, bool)) -> Result<()> {
    let x = x();
    let a = &x.atoms;
    // Start with what's on the clipboard now.
    let mut read = x.request(a.TARGETS, Read::Targets(true))?;
    let mut sends: Vec<Outgoing> = Vec::new();
    loop {
        match x.conn.wait_for_event()? {
            Event::XfixesSelectionNotify(e) if e.selection == a.CLIPBOARD => {
                // Skip our own writes, and the clipboard being emptied.
                if e.owner != x.win && e.owner != NONE {
                    read = x.request(a.TARGETS, Read::Targets(false))?;
                }
            }
            Event::SelectionNotify(e) if e.requestor == x.win && e.selection == a.CLIPBOARD => {
                read = match read {
                    // Very old apps don't answer TARGETS; just ask for text.
                    Read::Targets(initial) if e.property == NONE => x.request(a.UTF8_STRING, Read::Waiting(a.UTF8_STRING, initial))?,
                    Read::Targets(initial) => {
                        let offered: Vec<Atom> = x.take_property()?.value32().map(Iterator::collect).unwrap_or_default();
                        match x.pick(&offered) {
                            Some(target) => x.request(target, Read::Waiting(target, initial))?,
                            None => {
                                nothing(initial, on_change);
                                Read::Idle
                            }
                        }
                    }
                    // The owner can't provide UTF-8; very old apps may still offer Latin-1.
                    Read::Waiting(target, initial) if e.property == NONE => {
                        if target == a.UTF8_STRING {
                            let string = AtomEnum::STRING.into();
                            x.request(string, Read::Waiting(string, initial))?
                        } else {
                            nothing(initial, on_change);
                            Read::Idle
                        }
                    }
                    Read::Waiting(target, initial) => {
                        let reply = x.take_property()?;
                        if reply.type_ == a.INCR {
                            // Deleting the property (done by take_property) tells
                            // the owner to start sending chunks.
                            Read::Incr(target, Vec::new(), initial)
                        } else {
                            x.deliver(target, reply.value, initial, on_change);
                            Read::Idle
                        }
                    }
                    other => other,
                };
            }
            // Each new value of our property is the next incoming chunk. (Outside
            // INCR, the owner also sets it right before its SelectionNotify; ignore that.)
            Event::PropertyNotify(e)
                if e.window == x.win
                    && e.atom == a.CLIPSYNC
                    && e.state == Property::NEW_VALUE
                    && matches!(read, Read::Incr(..)) =>
            {
                let Read::Incr(target, mut buf, initial) = std::mem::replace(&mut read, Read::Idle) else {
                    unreachable!()
                };
                let chunk = x.take_property()?.value;
                if chunk.is_empty() {
                    // A zero-length chunk ends the transfer.
                    x.deliver(target, buf, initial, on_change);
                } else if buf.len() + chunk.len() <= MAX_CONTENT {
                    buf.extend_from_slice(&chunk);
                    read = Read::Incr(target, buf, initial);
                } else {
                    log!("not syncing a copy larger than {} MB", MAX_CONTENT >> 20);
                }
            }
            // An app we're sending to in chunks consumed the last one.
            Event::PropertyNotify(e) if e.state == Property::DELETE => {
                if let Some(i) = sends.iter().position(|s| s.requestor == e.window && s.property == e.atom)
                    && !x.send_chunk(&mut sends[i])?
                {
                    sends.swap_remove(i);
                }
            }
            Event::SelectionRequest(e) => {
                sends.retain(|s| s.started.elapsed() < SEND_TIMEOUT);
                x.serve(e, &mut sends)?;
            }
            _ => {}
        }
    }
}

impl X11 {
    /// Ask the owner to convert the clipboard to `target`; `next` is what we're then waiting for.
    fn request(&self, target: Atom, next: Read) -> Result<Read> {
        self.conn.convert_selection(self.win, self.atoms.CLIPBOARD, target, self.atoms.CLIPSYNC, CURRENT_TIME)?;
        self.conn.flush()?;
        Ok(next)
    }

    /// The format to read, from those the owner offers: text first, then PNG.
    fn pick(&self, offered: &[Atom]) -> Option<Atom> {
        let a = &self.atoms;
        [a.UTF8_STRING, a.TEXT_PLAIN, AtomEnum::STRING.into(), a.IMAGE_PNG].into_iter().find(|t| offered.contains(t))
    }

    fn deliver(&self, target: Atom, bytes: Vec<u8>, initial: bool, on_change: &mut dyn FnMut(Content, bool)) {
        let content = if target == self.atoms.IMAGE_PNG {
            if !is_png(&bytes) {
                return nothing(initial, on_change);
            }
            Content::Image(bytes.into())
        } else if target == Atom::from(AtomEnum::STRING) {
            Content::Text(bytes.iter().map(|&b| char::from(b)).collect::<String>().into()) // Latin-1
        } else {
            let text = String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
            Content::Text(text.into())
        };
        if initial || !content.is_empty() {
            on_change(content, initial);
        }
    }

    /// Read and delete our transfer property.
    fn take_property(&self) -> Result<GetPropertyReply> {
        let reply = self
            .conn
            .get_property(true, self.win, self.atoms.CLIPSYNC, AtomEnum::ANY, 0, u32::MAX / 4)?
            .reply()?;
        Ok(reply)
    }

    /// Answer another app asking for the clipboard while we own it.
    fn serve(&self, e: SelectionRequestEvent, sends: &mut Vec<Outgoing>) -> Result<()> {
        let a = &self.atoms;
        // Obsolete clients leave the property unset; ICCCM says to use the target.
        let property = if e.property == NONE { e.target } else { e.property };
        let string = Atom::from(AtomEnum::STRING);
        let owned = lock(&self.owned).clone().filter(|_| e.selection == a.CLIPBOARD);

        let data: Option<(Atom, Arc<[u8]>)> = match owned {
            None => None,
            Some(content) if e.target == a.TARGETS => {
                let targets: &[Atom] = match content {
                    Content::Text(_) => &[a.TARGETS, a.UTF8_STRING, a.TEXT_PLAIN, a.TEXT, string],
                    Content::Image(_) => &[a.TARGETS, a.IMAGE_PNG],
                };
                self.conn.change_property32(PropMode::REPLACE, e.requestor, property, AtomEnum::ATOM, targets)?;
                self.notify(&e, property)?;
                return Ok(());
            }
            Some(Content::Text(text)) if [a.UTF8_STRING, a.TEXT_PLAIN, a.TEXT].contains(&e.target) => {
                let ty = if e.target == a.TEXT { a.UTF8_STRING } else { e.target };
                Some((ty, text.as_bytes().into()))
            }
            Some(Content::Text(text)) if e.target == string => {
                let latin1: Vec<u8> = text.chars().map(|c| u8::try_from(c).unwrap_or(b'?')).collect();
                Some((string, latin1.into()))
            }
            Some(Content::Image(png)) if e.target == a.IMAGE_PNG => Some((a.IMAGE_PNG, png)),
            Some(_) => None,
        };
        let Some((ty, data)) = data else {
            return self.notify(&e, NONE); // refuse
        };

        if data.len() <= CHUNK {
            self.conn.change_property8(PropMode::REPLACE, e.requestor, property, ty, &data)?;
        } else {
            // Watch for the requestor deleting the property, then announce INCR.
            self.conn.change_window_attributes(
                e.requestor,
                &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            )?;
            let size = u32::try_from(data.len()).unwrap_or(u32::MAX);
            self.conn.change_property32(PropMode::REPLACE, e.requestor, property, a.INCR, &[size])?;
            sends.push(Outgoing { requestor: e.requestor, property, ty, data, offset: 0, started: Instant::now() });
        }
        self.notify(&e, property)
    }

    /// Tell the requestor its conversion is ready (or refused, with `NONE`).
    fn notify(&self, e: &SelectionRequestEvent, property: Atom) -> Result<()> {
        let event = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: e.time,
            requestor: e.requestor,
            selection: e.selection,
            target: e.target,
            property,
        };
        self.conn.send_event(false, e.requestor, EventMask::NO_EVENT, event)?;
        self.conn.flush()?;
        Ok(())
    }

    /// Write the next chunk of a send. Returns false once the final, empty
    /// chunk has been written and the transfer is over.
    fn send_chunk(&self, s: &mut Outgoing) -> Result<bool> {
        let end = (s.offset + CHUNK).min(s.data.len());
        self.conn.change_property8(PropMode::REPLACE, s.requestor, s.property, s.ty, &s.data[s.offset..end])?;
        let done = s.offset == end;
        s.offset = end;
        if done {
            self.conn.change_window_attributes(
                s.requestor,
                &ChangeWindowAttributesAux::new().event_mask(EventMask::NO_EVENT),
            )?;
        }
        self.conn.flush()?;
        Ok(!done)
    }
}
