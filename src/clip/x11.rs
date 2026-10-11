//! X11 (and XWayland), spoken natively via x11rb. The XFixes extension tells
//! us whenever the clipboard owner changes, so idle cost is zero.
//!
//! X11 doesn't store clipboard contents: the app that copied keeps the data
//! and hands it to whoever asks. To read, we ask the owner to convert the
//! CLIPBOARD selection into a property on our (invisible) window. To write,
//! we become the owner and answer other apps' requests ourselves. Large
//! text moves in chunks both ways (the ICCCM "INCR" protocol).

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

use crate::lock;
use crate::net::MAX_TEXT;

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

/// Text larger than this is sent to other apps in chunks of this size.
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
        CLIPSYNC: b"CLIPSYNC_DATA",
    }
}

struct X11 {
    conn: RustConnection,
    win: Window,
    atoms: Atoms,
    /// The UTF-8 text we serve to other apps while we own the clipboard.
    owned: Mutex<Option<Arc<[u8]>>>,
}

static X: OnceLock<X11> = OnceLock::new();

fn x() -> &'static X11 {
    X.get().expect("x11::connect must be called first")
}

/// State of an in-progress read from another app. The flag marks the read
/// of the clipboard's contents at startup.
enum Read {
    Idle,
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

pub fn set(text: &str) -> io::Result<()> {
    try_set(text).map_err(io::Error::other)
}

fn try_set(text: &str) -> Result<()> {
    let x = x();
    *lock(&x.owned) = Some(text.as_bytes().into());
    x.conn.set_selection_owner(x.win, x.atoms.CLIPBOARD, CURRENT_TIME)?;
    let owner = x.conn.get_selection_owner(x.atoms.CLIPBOARD)?.reply()?.owner;
    if owner != x.win {
        return Err("could not take ownership of the clipboard".into());
    }
    Ok(())
}

pub fn watch(mut on_change: impl FnMut(String, bool)) -> io::Result<()> {
    try_watch(&mut on_change).map_err(|e| io::Error::other(format!("X11: {e}")))
}

fn try_watch(on_change: &mut dyn FnMut(String, bool)) -> Result<()> {
    let x = x();
    let a = &x.atoms;
    // Start with what's on the clipboard now.
    let mut read = x.request(a.UTF8_STRING, true)?;
    let mut sends: Vec<Outgoing> = Vec::new();
    loop {
        match x.conn.wait_for_event()? {
            Event::XfixesSelectionNotify(e) if e.selection == a.CLIPBOARD => {
                // Skip our own writes, and the clipboard being emptied.
                if e.owner != x.win && e.owner != NONE {
                    read = x.request(a.UTF8_STRING, false)?;
                }
            }
            Event::SelectionNotify(e) if e.requestor == x.win && e.selection == a.CLIPBOARD => {
                read = match read {
                    // The owner can't provide UTF-8; very old apps may still offer Latin-1.
                    Read::Waiting(target, initial) if e.property == NONE => {
                        if target == a.UTF8_STRING {
                            x.request(AtomEnum::STRING.into(), initial)?
                        } else {
                            if initial {
                                on_change(String::new(), true); // no text on the clipboard
                            }
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
                            deliver(target, reply.value, initial, on_change);
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
                    deliver(target, buf, initial, on_change);
                } else if buf.len() + chunk.len() <= MAX_TEXT {
                    buf.extend_from_slice(&chunk);
                    read = Read::Incr(target, buf, initial);
                } else {
                    log!("ignoring clipboard larger than {} MB", MAX_TEXT >> 20);
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

fn deliver(target: Atom, bytes: Vec<u8>, initial: bool, on_change: &mut dyn FnMut(String, bool)) {
    let text = if target == Atom::from(AtomEnum::STRING) {
        bytes.iter().map(|&b| char::from(b)).collect() // Latin-1
    } else {
        String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
    };
    if initial || !text.is_empty() {
        on_change(text, initial);
    }
}

impl X11 {
    fn request(&self, target: Atom, initial: bool) -> Result<Read> {
        self.conn.convert_selection(self.win, self.atoms.CLIPBOARD, target, self.atoms.CLIPSYNC, CURRENT_TIME)?;
        self.conn.flush()?;
        Ok(Read::Waiting(target, initial))
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

        let data = match owned {
            None => None,
            Some(_) if e.target == a.TARGETS => {
                let targets = [a.TARGETS, a.UTF8_STRING, a.TEXT_PLAIN, a.TEXT, string];
                self.conn.change_property32(PropMode::REPLACE, e.requestor, property, AtomEnum::ATOM, &targets)?;
                self.notify(&e, property)?;
                return Ok(());
            }
            Some(text) if [a.UTF8_STRING, a.TEXT_PLAIN, a.TEXT].contains(&e.target) => {
                let ty = if e.target == a.TEXT { a.UTF8_STRING } else { e.target };
                Some((ty, text))
            }
            Some(text) if e.target == string => {
                let latin1: Vec<u8> =
                    String::from_utf8_lossy(&text).chars().map(|c| u8::try_from(c).unwrap_or(b'?')).collect();
                Some((string, latin1.into()))
            }
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
