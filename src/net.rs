//! Connections between members.
//!
//! First each side sends `"clipsync" || u8 protocol || u8 length || version`,
//! so a device running an incompatible version gets a clear message. Then the
//! two sides run a Noise XX handshake
//! (X25519, ChaCha20-Poly1305, BLAKE2s) with their device keys. That proves
//! each side holds its key; the cluster id is mixed in as the prologue, so
//! devices from another cluster fail the handshake. Each side then checks the
//! other's key against its member list.
//!
//! Everything after that is encrypted. A message is `u32 length || type ||
//! payload`, split into Noise messages of at most 64 KiB, each sent as
//! `u16 length || ciphertext`. Messages:
//!
//! - HELLO   `u16 listen port || latest event id`; always first
//! - EVENT   `event id || UTF-8 text`: a copy of text
//! - IMAGE   `event id || PNG`: a copy of an image (0.1.0 ignores these)
//! - MEMBERS the member list (see cluster.rs)
//! - WANT / NOPE `event id`: ask for a missed copy / don't have its text
//! - ADDRS   `device key (hex) address` lines: where the sender reaches other
//!   members, so devices can find each other without mDNS
//! - PING    keepalive
//!
//! An event id is `u64 hybrid-logical-clock time || origin device key (32)`.

use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use snow::StatelessTransportState;

use crate::clip::{Content, is_png};
use crate::cluster::Membership;
use crate::hub::{Event, Hello, Hub, Out, now_ms};
use crate::store::{DeviceId, EventId};

pub const NOISE: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";
/// Bumped when a change means devices on different versions can't talk to
/// each other. Versions with the same protocol work together.
pub const PROTOCOL: u8 = 1;
const MAGIC: &[u8; 8] = b"clipsync";
/// The largest copy synced, text or image.
pub const MAX_CONTENT: usize = 50 << 20;
const MAX_CHUNK: usize = 65535 - 16;
const PING_EVERY: Duration = Duration::from_secs(5);
const DEAD_AFTER: Duration = Duration::from_secs(15);
/// While connected to a peer, how often its dialer checks whether it still is.
const IDLE_CHECK: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// How long the device whose connection would lose the tie-break waits
/// before dialing, so both sides don't usually connect at once.
const HEAD_START: Duration = Duration::from_secs(1);

const HELLO: u8 = 0;
const EVENT: u8 = 1;
const PING: u8 = 2;
const MEMBERS: u8 = 3;
const WANT: u8 = 4;
const NOPE: u8 = 5;
const ADDRS: u8 = 6;
const IMAGE: u8 = 7;

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

fn noise_err(e: snow::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

/// One direction of an established connection.
struct Cipher {
    noise: Arc<StatelessTransportState>,
    nonce: u64,
}

fn send_msg(mut w: &TcpStream, c: &mut Cipher, kind: u8, payload: &[u8]) -> io::Result<()> {
    let mut plain = Vec::with_capacity(5 + payload.len());
    plain.extend_from_slice(&(1 + payload.len() as u32).to_be_bytes());
    plain.push(kind);
    plain.extend_from_slice(payload);
    let mut out = Vec::with_capacity(plain.len() + (plain.len() / MAX_CHUNK + 1) * 18);
    let mut ct = vec![0u8; 65535];
    for chunk in plain.chunks(MAX_CHUNK) {
        let n = c.noise.write_message(c.nonce, chunk, &mut ct).map_err(noise_err)?;
        c.nonce += 1;
        out.extend_from_slice(&(n as u16).to_be_bytes());
        out.extend_from_slice(&ct[..n]);
    }
    w.write_all(&out)
}

fn read_chunk(mut r: &TcpStream, c: &mut Cipher, buf: &mut [u8]) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 2];
    r.read_exact(&mut len)?;
    let ct = &mut buf[..u16::from_be_bytes(len) as usize];
    r.read_exact(ct)?;
    let mut plain = vec![0u8; ct.len()];
    let n = c.noise.read_message(c.nonce, ct, &mut plain).map_err(noise_err)?;
    c.nonce += 1;
    plain.truncate(n);
    Ok(plain)
}

fn recv_msg(r: &TcpStream, c: &mut Cipher) -> io::Result<(u8, Vec<u8>)> {
    let mut buf = vec![0u8; 65535];
    let first = read_chunk(r, c, &mut buf)?;
    if first.len() < 5 {
        return Err(invalid("short message"));
    }
    let total = u32::from_be_bytes(first[..4].try_into().unwrap()) as usize;
    if total > MAX_CONTENT + 64 {
        return Err(invalid("message too large"));
    }
    let mut plain = first[4..].to_vec();
    while plain.len() < total {
        plain.extend(read_chunk(r, c, &mut buf)?);
    }
    if plain.len() != total {
        return Err(invalid("bad message length"));
    }
    let kind = plain[0];
    plain.drain(..1);
    Ok((kind, plain))
}

fn event_id(p: &[u8]) -> Option<EventId> {
    Some(EventId { hlc: u64::from_be_bytes(p.get(..8)?.try_into().ok()?), origin: p.get(8..40)?.try_into().ok()? })
}

fn event_id_bytes(id: &EventId) -> Vec<u8> {
    let mut out = id.hlc.to_be_bytes().to_vec();
    out.extend_from_slice(&id.origin);
    out
}

/// Bind `addr` and accept peers on a background thread. Returns the port.
pub fn listen(addr: &str, hub: Arc<Hub>) -> io::Result<u16> {
    let listener = TcpListener::bind(addr)?;
    let local = listener.local_addr()?;
    log!("listening on {local}");
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let hub = hub.clone();
            thread::spawn(move || {
                let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
                match session(stream, &hub, false, None) {
                    // Closed early (abandoned dial attempts, port scans), dropped by us,
                    // not a member, or an incompatible version (the side that dialed,
                    // which knows the device's name, logs that): nothing worth logging.
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::UnexpectedEof
                                | io::ErrorKind::ConnectionAborted
                                | io::ErrorKind::PermissionDenied
                                | io::ErrorKind::Unsupported
                        ) => {}
                    Err(e) => log!("{peer}: {e}"),
                    Ok(()) => {}
                }
            });
        }
    });
    Ok(local.port())
}

/// Keep a connection to a member open, reconnecting with backoff. Stops once
/// it's no longer a member. A message on `wake` (new address, dropped
/// connection) cuts any wait short.
pub fn dial(hub: Arc<Hub>, id: DeviceId, wake: Receiver<()>) {
    let mut backoff = Duration::from_secs(1);
    let mut last_error = String::new();
    loop {
        let Some(addrs) = hub.addrs_for(&id) else { return };
        // Nothing to do while connected (e.g. it dialed us), or with no address
        // yet (discovery wakes us when it finds one).
        if hub.connected_to(&id) || addrs.is_empty() {
            nap(&wake, if addrs.is_empty() { MAX_BACKOFF } else { IDLE_CHECK });
            continue;
        }
        // If the peer's connection would be the one kept (see Hub::add_peer),
        // give it a moment to arrive; we still dial if it doesn't.
        if id < hub.id {
            thread::sleep(HEAD_START);
            if hub.connected_to(&id) {
                continue;
            }
        }
        let started = Instant::now();
        let err = match connect(addrs, &wake) {
            Ok(Some(stream)) => match session(stream, &hub, true, Some(id)) {
                // No longer a member (or we left): nothing to report.
                _ if hub.addrs_for(&id).is_none() => return,
                Ok(()) => "connection closed".to_string(),
                // We dropped it ourselves (and said why); just reconnect.
                Err(e) if e.kind() == io::ErrorKind::ConnectionAborted => {
                    backoff = Duration::from_secs(1);
                    continue;
                }
                Err(e) => e.to_string(),
            },
            // Woken mid-attempt: the peer's addresses changed, so retry with the new ones.
            Ok(None) => {
                backoff = Duration::from_secs(1);
                continue;
            }
            Err(e) => format!("can't connect: {e}"),
        };
        // Our connection lost to the one the peer dialed; that's fine.
        if hub.connected_to(&id) {
            last_error.clear();
            continue;
        }
        // Only log repeated failures once, so an offline device doesn't spam the log.
        if err != last_error {
            log!("{}: {err}; retrying", hub.name_of(&id));
            last_error = err;
        }
        if started.elapsed() > DEAD_AFTER {
            backoff = Duration::from_secs(1);
        }
        backoff = if nap(&wake, backoff) { Duration::from_secs(1) } else { (backoff * 2).min(MAX_BACKOFF) };
    }
}

/// Sleep for `d` or until woken. Returns true if woken (or the waker is gone).
fn nap(wake: &Receiver<()>, d: Duration) -> bool {
    match wake.recv_timeout(d) {
        Ok(()) => {
            while wake.try_recv().is_ok() {}
            true
        }
        Err(RecvTimeoutError::Timeout) => false,
        Err(RecvTimeoutError::Disconnected) => true,
    }
}

/// Connect to the first of `addrs` that answers. Returns `None` if woken
/// first: an address that vanished can take seconds to time out, and by
/// then mDNS has usually told us the peer's new one.
fn connect(addrs: Vec<SocketAddr>, wake: &Receiver<()>) -> io::Result<Option<TcpStream>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(connect_any(&addrs));
    });
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(result) => return result.map(Some),
            Err(RecvTimeoutError::Timeout) => {
                if matches!(wake.try_recv(), Ok(()) | Err(TryRecvError::Disconnected)) {
                    return Ok(None); // the attempt finishes in the background and is dropped
                }
            }
            Err(RecvTimeoutError::Disconnected) => return Err(io::Error::other("connect thread failed")),
        }
    }
}

fn connect_any(addrs: &[SocketAddr]) -> io::Result<TcpStream> {
    let mut err = io::Error::new(io::ErrorKind::NotFound, "no address known");
    for a in addrs {
        match TcpStream::connect_timeout(a, Duration::from_secs(3)) {
            Ok(s) => return Ok(s),
            Err(e) => err = e,
        }
    }
    Err(err)
}

/// Run one connection until it drops. `dialed` says which side opened it;
/// `expect` is the member we meant to reach, if we dialed.
fn session(stream: TcpStream, hub: &Arc<Hub>, dialed: bool, expect: Option<DeviceId>) -> io::Result<()> {
    run_session(stream, hub, dialed, expect).map_err(|e| match e.kind() {
        // A read timeout: nothing arrived for DEAD_AFTER, not even a ping.
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => {
            io::Error::new(io::ErrorKind::TimedOut, "peer stopped responding")
        }
        _ => e,
    })
}

/// Noise XX: -> e; <- e, ee, s, es; -> s, se. Returns the peer's device key.
fn handshake(mut s: &TcpStream, hub: &Hub, initiator: bool) -> io::Result<(StatelessTransportState, DeviceId)> {
    let mut prologue = MAGIC.to_vec();
    prologue.push(PROTOCOL);
    prologue.extend_from_slice(&hub.cluster_id);
    let builder = snow::Builder::new(NOISE.parse().unwrap())
        .local_private_key(&hub.private)
        .and_then(|b| b.prologue(&prologue))
        .map_err(noise_err)?;
    let mut hs = if initiator { builder.build_initiator() } else { builder.build_responder() }.map_err(noise_err)?;
    let mut msg = [0u8; 1024];
    let mut payload = [0u8; 1024];
    for step in 0..3 {
        if (step % 2 == 0) == initiator {
            let n = hs.write_message(&[], &mut msg).map_err(noise_err)?;
            s.write_all(&(n as u16).to_be_bytes())?;
            s.write_all(&msg[..n])?;
        } else {
            let mut len = [0u8; 2];
            s.read_exact(&mut len)?;
            let n = u16::from_be_bytes(len) as usize;
            if n > msg.len() {
                return Err(invalid("bad handshake"));
            }
            s.read_exact(&mut msg[..n])?;
            // Fails if the peer's cluster (the prologue) differs from ours.
            hs.read_message(&msg[..n], &mut payload)
                .map_err(|_| io::Error::new(io::ErrorKind::PermissionDenied, "not in this cluster"))?;
        }
    }
    let remote: DeviceId = hs.get_remote_static().and_then(|k| k.try_into().ok()).ok_or_else(|| invalid("no key"))?;
    Ok((hs.into_stateless_transport_mode().map_err(noise_err)?, remote))
}

/// Before the handshake, both sides say which clipsync they run.
fn exchange_versions(mut s: &TcpStream) -> io::Result<(u8, String)> {
    let mine = env!("CARGO_PKG_VERSION").as_bytes();
    let mut hello = MAGIC.to_vec();
    hello.extend_from_slice(&[PROTOCOL, mine.len() as u8]);
    hello.extend_from_slice(mine);
    s.write_all(&hello)?;
    let mut head = [0u8; 10];
    s.read_exact(&mut head)?;
    if &head[..8] != MAGIC {
        return Err(invalid("not clipsync"));
    }
    let mut version = vec![0u8; head[9] as usize];
    s.read_exact(&mut version)?;
    Ok((head[8], String::from_utf8_lossy(&version).into_owned()))
}

fn incompatible(theirs: &str) -> String {
    let mine = env!("CARGO_PKG_VERSION");
    let which = if crate::update::newer(mine, theirs) { "it" } else { "this device" };
    format!("runs clipsync {theirs}, which can't talk to {mine}; update {which} with `clipsync update`")
}

fn run_session(stream: TcpStream, hub: &Arc<Hub>, dialed: bool, expect: Option<DeviceId>) -> io::Result<()> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(DEAD_AFTER))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;

    let (protocol, version) = exchange_versions(&stream)?;
    if let Some(id) = expect {
        hub.set_version(&id, &version);
    }
    if protocol != PROTOCOL {
        return Err(io::Error::new(io::ErrorKind::Unsupported, incompatible(&version)));
    }
    let (noise, remote) = handshake(&stream, hub, dialed)?;
    let noise = Arc::new(noise);
    let mut send = Cipher { noise: noise.clone(), nonce: 0 };
    let mut recv = Cipher { noise, nonce: 0 };

    if remote == hub.id {
        return Err(invalid("that address is this device"));
    }
    if expect.is_some_and(|e| e != remote) {
        return Err(invalid("a different device answered at its old address"));
    }
    match hub.membership(&remote) {
        Membership::Member => {}
        // Tell it, so it stops trying, then hang up.
        Membership::Removed => {
            send_msg(&stream, &mut send, MEMBERS, hub.members_text().as_bytes())?;
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "removed device"));
        }
        // Possibly added by another member and we haven't heard yet; it retries.
        Membership::Unknown => return Err(io::Error::new(io::ErrorKind::PermissionDenied, "unknown device")),
    }

    let (tx, rx) = mpsc::channel();
    let writer = {
        let stream = stream.try_clone()?;
        let mut hello = hub.listen_port().to_be_bytes().to_vec();
        hello.extend(event_id_bytes(&hub.latest().unwrap_or(EventId { hlc: 0, origin: [0; 32] })));
        let members = hub.members_text();
        let addrs = hub.known_addrs();
        thread::spawn(move || {
            if write_loop(&stream, &mut send, &hello, &members, &addrs, rx).is_err() {
                // Unblock the reader so the session ends promptly.
                let _ = stream.shutdown(Shutdown::Both);
            }
        })
    };

    // If the hello fails, `tx` is dropped with the closure, which stops the writer.
    let result = expect_hello(&stream, &mut recv, hub).and_then(|hello| {
        let last_rx = Arc::new(AtomicU64::new(now_ms()));
        hub.set_version(&remote, &version);
        let added = hub.add_peer(remote, &hello, dialed, tx, stream.try_clone()?, last_rx.clone())?;
        let Some((conn, label, replaced)) = added else {
            return Ok(()); // duplicates a connection we prefer
        };
        if !replaced {
            log!("connected to {label}");
        }
        let r = read_loop(&stream, &mut recv, hub, conn, &last_rx);
        if !hub.remove_peer(conn) {
            // Replaced or dropped by us; whoever did that has logged it.
            return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "dropped"));
        }
        log!("disconnected from {label}");
        r
    });
    let _ = stream.shutdown(Shutdown::Both);
    let _ = writer.join();
    result
}

/// Wait for the peer's HELLO. A member list may come first: that's how a
/// removed device learns it was removed.
fn expect_hello(stream: &TcpStream, c: &mut Cipher, hub: &Arc<Hub>) -> io::Result<Hello> {
    loop {
        match recv_msg(stream, c)? {
            (HELLO, p) if p.len() >= 42 => {
                let latest = event_id(&p[2..]).filter(|id| id.hlc != 0);
                return Ok(Hello { listen_port: u16::from_be_bytes([p[0], p[1]]), latest });
            }
            (MEMBERS, p) => hub.merge_members(&p),
            _ => return Err(invalid("expected hello")),
        }
    }
}

fn write_loop(
    stream: &TcpStream,
    c: &mut Cipher,
    hello: &[u8],
    members: &str,
    addrs: &str,
    rx: Receiver<Out>,
) -> io::Result<()> {
    send_msg(stream, c, HELLO, hello)?;
    send_msg(stream, c, MEMBERS, members.as_bytes())?;
    send_msg(stream, c, ADDRS, addrs.as_bytes())?;
    loop {
        match rx.recv_timeout(PING_EVERY) {
            Ok(Out::Event(ev)) => {
                let mut p = event_id_bytes(&ev.id);
                let kind = match &ev.content {
                    Content::Text(text) => {
                        p.extend_from_slice(text.as_bytes());
                        EVENT
                    }
                    Content::Image(png) => {
                        p.extend_from_slice(png);
                        IMAGE
                    }
                };
                send_msg(stream, c, kind, &p)?;
            }
            Ok(Out::Members(text)) => send_msg(stream, c, MEMBERS, text.as_bytes())?,
            Ok(Out::MembersThenClose(text)) => {
                send_msg(stream, c, MEMBERS, text.as_bytes())?;
                let _ = stream.shutdown(Shutdown::Both);
                return Ok(());
            }
            Ok(Out::Want(id)) => send_msg(stream, c, WANT, &event_id_bytes(&id))?,
            Ok(Out::Nope(id)) => send_msg(stream, c, NOPE, &event_id_bytes(&id))?,
            Err(RecvTimeoutError::Timeout) => send_msg(stream, c, PING, &[])?,
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }
    }
}

fn read_loop(stream: &TcpStream, c: &mut Cipher, hub: &Arc<Hub>, conn: u64, last_rx: &AtomicU64) -> io::Result<()> {
    loop {
        let (kind, payload) = match recv_msg(stream, c) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        };
        last_rx.store(now_ms(), Ordering::Relaxed);
        match kind {
            EVENT => {
                let Some(id) = event_id(&payload) else { continue };
                match String::from_utf8(payload[40..].to_vec()) {
                    Ok(text) => hub.receive_event(conn, Event { id, content: Content::Text(text.into()) }),
                    Err(_) => log!("ignoring non-UTF-8 text from peer"),
                }
            }
            IMAGE => {
                let Some(id) = event_id(&payload) else { continue };
                let png = &payload[40..];
                if is_png(png) {
                    hub.receive_event(conn, Event { id, content: Content::Image(png.into()) });
                } else {
                    log!("ignoring an image from a peer that isn't a PNG");
                }
            }
            MEMBERS => hub.merge_members(&payload),
            ADDRS => hub.learn_addrs(&payload),
            WANT => {
                if let Some(id) = event_id(&payload) {
                    hub.want(conn, id);
                }
            }
            NOPE => {
                if let Some(id) = event_id(&payload) {
                    hub.nope(conn, id);
                }
            }
            _ => {} // pings; unknown message types are ignored for forward compatibility
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both ends of an established Noise session over a real socket pair.
    fn connected() -> ((TcpStream, Cipher), (TcpStream, Cipher)) {
        let params: snow::params::NoiseParams = NOISE.parse().unwrap();
        let key = || snow::Builder::new(NOISE.parse().unwrap()).generate_keypair().unwrap();
        let (ka, kb) = (key(), key());
        let mut a = snow::Builder::new(params.clone()).local_private_key(&ka.private).unwrap().build_initiator().unwrap();
        let mut b = snow::Builder::new(params).local_private_key(&kb.private).unwrap().build_responder().unwrap();
        let (mut msg, mut payload) = ([0u8; 1024], [0u8; 1024]);
        for step in 0..3 {
            let (from, to) = if step % 2 == 0 { (&mut a, &mut b) } else { (&mut b, &mut a) };
            let n = from.write_message(&[], &mut msg).unwrap();
            to.read_message(&msg[..n], &mut payload).unwrap();
        }
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let sa = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (sb, _) = l.accept().unwrap();
        let ca = Cipher { noise: Arc::new(a.into_stateless_transport_mode().unwrap()), nonce: 0 };
        let cb = Cipher { noise: Arc::new(b.into_stateless_transport_mode().unwrap()), nonce: 0 };
        ((sa, ca), (sb, cb))
    }

    #[test]
    fn messages_of_any_size_round_trip() {
        let ((sa, mut ca), (sb, mut cb)) = connected();
        // Around the 64 KiB chunk boundary, and well past it.
        let sizes = [0, 1, MAX_CHUNK - 5, MAX_CHUNK - 4, 200_000];
        let payloads: Vec<Vec<u8>> = sizes.iter().map(|&n| (0..n).map(|i| (i * 31) as u8).collect()).collect();
        let sent = payloads.clone();
        let writer = thread::spawn(move || {
            for p in &sent {
                send_msg(&sa, &mut ca, EVENT, p).unwrap();
            }
        });
        for p in payloads {
            assert_eq!(recv_msg(&sb, &mut cb).unwrap(), (EVENT, p));
        }
        writer.join().unwrap();
    }

    #[test]
    fn tampering_is_detected() {
        let ((sa, ca), (sb, mut cb)) = connected();
        let mut plain = 6u32.to_be_bytes().to_vec();
        plain.extend_from_slice(b"\x01hello");
        let mut ct = [0u8; 256];
        let n = ca.noise.write_message(0, &plain, &mut ct).unwrap();
        ct[n - 1] ^= 1; // flip a bit of the auth tag
        (&sa).write_all(&(n as u16).to_be_bytes()).unwrap();
        (&sa).write_all(&ct[..n]).unwrap();
        assert!(recv_msg(&sb, &mut cb).is_err());
    }

    #[test]
    fn event_ids_round_trip() {
        let id = EventId { hlc: 0x0123_4567_89ab_cdef, origin: [7; 32] };
        assert_eq!(event_id(&event_id_bytes(&id)), Some(id));
        assert_eq!(event_id(&[0; 39]), None);
    }
}
