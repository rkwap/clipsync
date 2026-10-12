//! The running daemon's shared state: connections to other members, copy
//! events, and the cluster membership as this device currently knows it.
//!
//! Copies are events, ordered by (hybrid logical clock, origin device). A
//! device applies an event only if it's newer than the newest one it has, so
//! when two devices copy at once, every device ends up with the same one.
//! When a device connects, it learns the newest event each peer has; if it
//! missed one while away, it asks a single peer for it.
//!
//! Lock order: `events` before `peers`. `cluster` and `contacts` are never
//! held while taking another lock.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::cluster::{self, Cluster, Membership};
use crate::store::{DeviceId, EventId, Identity, PeerState, State, hex, unhex32};
use crate::clip::{self, Content};
use crate::net::{self, MAX_CONTENT};
use crate::lock;

/// A connection that hasn't received anything (not even a ping) for this long
/// is presumed dead, and a new connection to the same peer replaces it.
const STALE_AFTER: Duration = Duration::from_secs(8);

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// A copy, as sent between devices.
pub struct Event {
    pub id: EventId,
    pub content: Content,
}

/// What a connection's writer sends.
pub enum Out {
    Event(Arc<Event>),
    Members(Arc<str>),
    /// Send the member list, then close: the peer was removed and should learn that.
    MembersThenClose(Arc<str>),
    Want(EventId),
    Nope(EventId),
}

/// What a peer said in its HELLO.
pub struct Hello {
    pub listen_port: u16,
    pub latest: Option<EventId>,
}

struct Peer {
    conn: u64,
    id: DeviceId,
    label: String,
    /// The peer's address, and ours, on this connection.
    addr: IpAddr,
    local: IpAddr,
    /// Whether this connection was dialed by the device with the lower id.
    preferred: bool,
    last_rx: Arc<AtomicU64>,
    /// The newest event we know this peer has.
    latest: Option<EventId>,
    tx: Sender<Out>,
    stream: TcpStream,
}

impl Peer {
    fn alive(&self) -> bool {
        now_ms().saturating_sub(self.last_rx.load(Ordering::Relaxed)) < STALE_AFTER.as_millis() as u64
    }
}

struct Latest {
    id: EventId,
    hash: [u8; 32],
    /// Kept in memory only, to hand to peers that missed it. Unknown after a
    /// restart until the clipboard watcher reports the same content again.
    content: Option<Content>,
}

struct Events {
    hlc: u64,
    latest: Option<Latest>,
    /// A missed copy we've asked one peer for.
    pending: Option<Pending>,
    /// The clipboard was empty at startup though we have a latest copy: on
    /// X11 the clipboard lives in the app that copied, so it was lost when
    /// clipsync (which held it) restarted. Fetch it again from a peer.
    restore: bool,
}

impl Events {
    /// Hybrid logical clock: wall-clock ms in the high bits, a counter in the
    /// low 16. Never goes backwards, and is always past every event seen.
    fn tick(&mut self) -> u64 {
        self.hlc = (self.hlc + 1).max(now_ms() << 16);
        self.hlc
    }
}

struct Pending {
    target: EventId,
    /// Who we're waiting on, if anyone.
    conn: Option<u64>,
    /// Peers already asked for this target.
    asked: Vec<u64>,
}

/// How to reach a member: where mDNS sees it, and where we last connected to it.
struct Contact {
    discovered: Vec<SocketAddr>,
    fullname: Option<String>,
    last_known: Option<SocketAddr>,
    last_seen: u64,
    version: Option<String>,
    wake: Sender<()>,
}

pub struct Hub {
    pub id: DeviceId,
    pub private: [u8; 32],
    pub cluster_id: [u8; 32],
    cluster: Mutex<Cluster>,
    peers: Mutex<Vec<Peer>>,
    next_conn: AtomicU64,
    events: Mutex<Events>,
    contacts: Mutex<HashMap<DeviceId, Contact>>,
    /// Saved peer state, used to seed contacts.
    saved: Mutex<BTreeMap<DeviceId, PeerState>>,
    listen_port: AtomicU16,
    update: Mutex<Option<String>>,
    /// Serializes state saves, so the last one written is the newest.
    saving: Mutex<()>,
}

impl Hub {
    pub fn new(me: Identity, cluster: Cluster, state: State) -> Hub {
        let update = state.update.clone();
        let mut events = Events { hlc: state.hlc, latest: None, pending: None, restore: false };
        let mut saved = BTreeMap::new();
        if state.cluster == Some(cluster.id) {
            events.latest = state.latest.map(|(id, hash)| Latest { id, hash, content: None });
            saved = state.peers;
        } else {
            // New to this cluster: copies made before now aren't "missed".
            let id = EventId { hlc: events.tick(), origin: [0; 32] };
            events.latest = Some(Latest { id, hash: [0; 32], content: None });
        }
        Hub {
            id: me.public,
            private: me.private,
            cluster_id: cluster.id,
            cluster: Mutex::new(cluster),
            peers: Mutex::new(Vec::new()),
            next_conn: AtomicU64::new(0),
            events: Mutex::new(events),
            contacts: Mutex::new(HashMap::new()),
            saved: Mutex::new(saved),
            listen_port: AtomicU16::new(0),
            update: Mutex::new(update),
            saving: Mutex::new(()),
        }
    }

    pub fn set_listen_port(&self, port: u16) {
        self.listen_port.store(port, Ordering::Relaxed);
        self.save_state();
    }

    pub fn listen_port(&self) -> u16 {
        self.listen_port.load(Ordering::Relaxed)
    }

    // ---- membership ----

    pub fn membership(&self, id: &DeviceId) -> Membership {
        lock(&self.cluster).membership(id)
    }

    pub fn name_of(&self, id: &DeviceId) -> String {
        lock(&self.cluster).name_of(id)
    }

    pub fn tag(&self) -> String {
        lock(&self.cluster).tag()
    }

    pub fn members_text(&self) -> Arc<str> {
        lock(&self.cluster).to_text().into()
    }

    /// A peer sent its member list.
    pub fn merge_members(self: &Arc<Self>, text: &[u8]) {
        let Ok(incoming) = std::str::from_utf8(text).map_err(io::Error::other).and_then(Cluster::parse) else {
            return;
        };
        if lock(&self.cluster).merge(&incoming) {
            self.membership_changed();
        }
    }

    /// The cluster file changed (a command added or removed a device).
    pub fn reload(self: &Arc<Self>) {
        match cluster::load() {
            Ok(Some(file)) if file.id == self.cluster_id => {
                let changed = lock(&self.cluster).merge(&file);
                if changed {
                    self.membership_changed();
                }
            }
            // Gone, or replaced by another cluster: start over.
            Ok(_) => crate::restart(),
            Err(e) => log!("can't read the cluster file: {e}"),
        }
    }

    fn membership_changed(self: &Arc<Self>) {
        let mine = lock(&self.cluster).clone();
        // Save what we learned, merged with whatever commands changed meanwhile.
        let saved = cluster::update(|file| {
            if let Some(f) = file.as_mut().filter(|f| f.id == mine.id) {
                f.merge(&mine);
            }
            Ok(())
        });
        if let Err(e) = saved {
            log!("can't save the cluster file: {e}");
        }
        if !mine.is_member(&self.id) {
            self.leave_cluster();
        }
        let text: Arc<str> = mine.to_text().into();
        lock(&self.peers).retain(|p| {
            if mine.is_member(&p.id) {
                let _ = p.tx.send(Out::Members(text.clone()));
                return true;
            }
            log!("{} was removed from the cluster", p.label);
            let _ = p.tx.send(Out::MembersThenClose(text.clone()));
            false
        });
        self.sync_contacts();
    }

    /// We were removed (or left): tell connected members, then go idle.
    fn leave_cluster(&self) -> ! {
        log!("this device is no longer in the cluster; stopping");
        lock(&self.contacts).clear(); // stops the dialers
        let text = self.members_text();
        for p in lock(&self.peers).iter() {
            let _ = p.tx.send(Out::MembersThenClose(text.clone()));
        }
        thread::sleep(Duration::from_millis(500));
        let _ = cluster::update(|c| {
            *c = None;
            Ok(())
        });
        let _ = crate::store::delete("state");
        crate::restart()
    }

    // ---- contacts and dialers ----

    /// Make sure there's exactly one dialer per current member.
    pub fn sync_contacts(self: &Arc<Self>) {
        let members: Vec<DeviceId> =
            lock(&self.cluster).active().map(|(id, _)| *id).filter(|id| *id != self.id).collect();
        let saved = lock(&self.saved).clone();
        let mut contacts = lock(&self.contacts);
        // Dropping a contact drops its waker, which stops its dialer.
        contacts.retain(|id, _| members.contains(id));
        for id in members {
            if contacts.contains_key(&id) {
                continue;
            }
            let (wake, woken) = mpsc::channel();
            let s = saved.get(&id).cloned().unwrap_or_default();
            let contact = Contact {
                discovered: Vec::new(),
                fullname: None,
                last_known: s.addr,
                last_seen: s.last_seen,
                version: s.version,
                wake,
            };
            contacts.insert(id, contact);
            let hub = self.clone();
            thread::spawn(move || net::dial(hub, id, woken));
        }
    }

    /// Where to try reaching a member, or `None` if it's no longer one.
    pub fn addrs_for(&self, id: &DeviceId) -> Option<Vec<SocketAddr>> {
        let contacts = lock(&self.contacts);
        let c = contacts.get(id)?;
        let mut addrs = c.discovered.clone();
        addrs.extend(c.last_known.filter(|a| !addrs.contains(a)));
        Some(addrs)
    }

    /// mDNS found a member at these addresses.
    pub fn discovered(&self, id: DeviceId, fullname: &str, mut addrs: Vec<SocketAddr>) {
        // Loopback only reaches a device on this machine; try it last.
        addrs.sort_by_key(|a| (a.ip().is_loopback(), a.ip()));
        let mut contacts = lock(&self.contacts);
        let Some(c) = contacts.get_mut(&id) else { return };
        c.fullname = Some(fullname.to_string());
        if c.discovered == addrs {
            return;
        }
        // It moved (new DHCP lease, another access point): drop any connection to
        // an address it no longer has, and have its dialer try the new one now.
        let gone: Vec<IpAddr> =
            c.discovered.iter().map(|a| a.ip()).filter(|ip| !addrs.iter().any(|a| a.ip() == *ip)).collect();
        c.discovered = addrs;
        let wake = c.wake.clone();
        drop(contacts);
        self.drop_moved(&id, &gone);
        let _ = wake.send(());
    }

    pub fn undiscovered(&self, fullname: &str) {
        for c in lock(&self.contacts).values_mut() {
            if c.fullname.as_deref() == Some(fullname) {
                c.discovered.clear();
            }
        }
    }

    /// An address learned while pairing.
    pub fn hint(&self, id: &DeviceId, addr: SocketAddr) {
        if let Some(c) = lock(&self.contacts).get_mut(id) {
            c.last_known = Some(addr);
            let _ = c.wake.send(());
        }
    }

    /// Where we last reached each member, to share with peers.
    pub fn known_addrs(&self) -> String {
        let contacts = lock(&self.contacts);
        contacts.iter().filter_map(|(id, c)| Some(format!("{} {}\n", hex(id), c.last_known?))).collect()
    }

    /// A peer shared where it reaches other members. Only used for members we
    /// have no address for: that's what lets a third device find the others
    /// when mDNS is blocked.
    pub fn learn_addrs(&self, text: &[u8]) {
        let text = String::from_utf8_lossy(text);
        let mut contacts = lock(&self.contacts);
        for line in text.lines() {
            let Some((id, addr)) = line.split_once(' ') else { continue };
            let (Some(id), Ok(addr)) = (unhex32(id), addr.parse::<SocketAddr>()) else { continue };
            if let Some(c) = contacts.get_mut(&id).filter(|c| c.last_known.is_none() && c.discovered.is_empty()) {
                c.last_known = Some(addr);
                let _ = c.wake.send(());
            }
        }
    }

    /// The clipsync version a member said it runs.
    pub fn set_version(&self, id: &DeviceId, version: &str) {
        let changed = match lock(&self.contacts).get_mut(id) {
            Some(c) if c.version.as_deref() != Some(version) => {
                c.version = Some(version.to_string());
                true
            }
            _ => false,
        };
        if changed {
            self.save_state();
        }
    }

    /// The daemon found a newer release. Returns whether that's news.
    pub fn set_update(&self, version: &str) -> bool {
        let mut update = lock(&self.update);
        if update.as_deref() == Some(version) {
            return false;
        }
        *update = Some(version.to_string());
        drop(update);
        self.save_state();
        true
    }

    pub fn wake_dialers(&self) {
        for c in lock(&self.contacts).values() {
            let _ = c.wake.send(());
        }
    }

    // ---- connections ----

    /// Register an authenticated connection. Returns its handle and whether
    /// it replaced an existing connection to the same peer, or `None` if it
    /// should be closed because it duplicates a connection we prefer.
    ///
    /// Two devices may dial each other at the same time. Both then keep the
    /// connection dialed by the device with the lower id, so they agree on
    /// which one survives without having to talk about it. A connection that
    /// has gone quiet is replaced regardless, since it's most likely dead.
    pub fn add_peer(
        &self,
        id: DeviceId,
        hello: &Hello,
        dialed: bool,
        tx: Sender<Out>,
        stream: TcpStream,
        last_rx: Arc<AtomicU64>,
    ) -> io::Result<Option<(u64, String, bool)>> {
        let preferred = dialed == (self.id < id);
        let (addr, local) = (stream.peer_addr()?.ip(), stream.local_addr()?.ip());
        let label = format!("{} ({addr})", self.name_of(&id));
        let conn = {
            let mut peers = lock(&self.peers);
            let mut replaced = false;
            if let Some(i) = peers.iter().position(|p| p.id == id) {
                if peers[i].alive() && (peers[i].preferred || !preferred) {
                    return Ok(None);
                }
                let _ = peers.swap_remove(i).stream.shutdown(Shutdown::Both);
                replaced = true;
            }
            let conn = self.next_conn.fetch_add(1, Ordering::Relaxed);
            let peer = Peer { conn, id, label: label.clone(), addr, local, preferred, last_rx, latest: hello.latest, tx, stream };
            peers.push(peer);
            (conn, replaced)
        };
        if let Some(c) = lock(&self.contacts).get_mut(&id) {
            c.last_known = Some(SocketAddr::new(addr, hello.listen_port));
            c.last_seen = now_ms();
        }
        self.save_state();
        self.maybe_catch_up();
        Ok(Some((conn.0, label, conn.1)))
    }

    /// Returns false if the connection had already been replaced or dropped.
    pub fn remove_peer(&self, conn: u64) -> bool {
        let removed = {
            let mut peers = lock(&self.peers);
            let i = peers.iter().position(|p| p.conn == conn);
            i.map(|i| peers.swap_remove(i))
        };
        let Some(p) = removed else { return false };
        if let Some(c) = lock(&self.contacts).get_mut(&p.id) {
            c.last_seen = now_ms();
        }
        self.save_state();
        // If we were waiting on this peer for a missed copy, ask someone else.
        if let Some(pending) = lock(&self.events).pending.as_mut().filter(|pe| pe.conn == Some(conn)) {
            pending.conn = None;
        }
        self.maybe_catch_up();
        true
    }

    pub fn connected_to(&self, id: &DeviceId) -> bool {
        lock(&self.peers).iter().any(|p| p.id == *id)
    }

    /// The peer stopped advertising some addresses: close our connection to it
    /// if it uses one of them, instead of waiting for it to time out.
    fn drop_moved(&self, id: &DeviceId, gone: &[IpAddr]) {
        let mut peers = lock(&self.peers);
        if let Some(i) = peers.iter().position(|p| p.id == *id && gone.contains(&p.addr)) {
            let p = peers.swap_remove(i);
            let _ = p.stream.shutdown(Shutdown::Both);
            log!("{} changed address; reconnecting", p.label);
        }
    }

    /// One of our own addresses went away (Wi-Fi dropped, new DHCP lease):
    /// connections using it are dead, so close them now.
    pub fn drop_local(&self, ip: IpAddr) {
        lock(&self.peers).retain(|p| {
            if p.local != ip {
                return true;
            }
            let _ = p.stream.shutdown(Shutdown::Both);
            log!("{}: our address {ip} went away; reconnecting", p.label);
            false
        });
    }

    fn send_to(&self, conn: u64, msg: Out) {
        if let Some(p) = lock(&self.peers).iter().find(|p| p.conn == conn) {
            let _ = p.tx.send(msg);
        }
    }

    // ---- copy events ----

    /// The newest copy we have, as advertised to peers in HELLO.
    pub fn latest(&self) -> Option<EventId> {
        // A placeholder (from joining) isn't a real copy; nobody can ask for it.
        lock(&self.events).latest.as_ref().map(|l| l.id).filter(|id| id.origin != [0; 32])
    }

    /// The local clipboard changed. `initial` is its contents at startup
    /// (empty if there's nothing usable), which isn't a new copy.
    pub fn local_change(&self, content: Content, initial: bool) {
        if content.len() > MAX_CONTENT {
            log!("not syncing {}: the limit is {} MB", content.describe(), MAX_CONTENT >> 20);
            return;
        }
        let hash = content.hash();
        let event = {
            let mut ev = lock(&self.events);
            let same = ev.latest.as_ref().is_some_and(|l| l.hash == hash);
            if initial {
                if same && let Some(l) = ev.latest.as_mut() {
                    // Our newest copy: keep it to serve peers that missed it.
                    l.content = Some(content);
                } else if content.is_empty() && ev.latest.as_ref().is_some_and(|l| l.id.origin != [0; 32]) {
                    ev.restore = true;
                    drop(ev);
                    self.maybe_catch_up();
                }
                return;
            }
            if content.is_empty() || same {
                return;
            }
            ev.restore = false;
            let id = EventId { hlc: ev.tick(), origin: self.id };
            ev.latest = Some(Latest { id, hash, content: Some(content.clone()) });
            ev.pending = None;
            Arc::new(Event { id, content })
        };
        self.save_state();
        let peers = lock(&self.peers);
        for p in peers.iter() {
            let _ = p.tx.send(Out::Event(event.clone()));
        }
        if !peers.is_empty() {
            log!("sent {} to {} device(s)", event.content.describe(), peers.len());
        }
    }

    /// A peer sent a copy: put it on our clipboard if it's newer than ours.
    /// Older ones are concurrent copies that lost, or ones we already have.
    pub fn receive_event(&self, conn: u64, event: Event) {
        let hash = event.content.hash();
        let applied = {
            let mut ev = lock(&self.events);
            ev.hlc = ev.hlc.max(event.id.hlc);
            let restoring = ev.restore && ev.latest.as_ref().is_some_and(|l| event.id == l.id);
            if !restoring && ev.latest.as_ref().is_some_and(|l| event.id <= l.id) {
                None
            } else {
                // Hold the lock while setting, so the watcher sees the new hash
                // when it notices our own write, and doesn't send it back.
                let same = !restoring && ev.latest.as_ref().is_some_and(|l| l.hash == hash);
                ev.restore = false;
                if !same && let Err(e) = clip::set(&event.content) {
                    log!("failed to set clipboard: {e}");
                    return;
                }
                ev.latest = Some(Latest { id: event.id, hash, content: Some(event.content.clone()) });
                if ev.pending.as_ref().is_some_and(|p| p.target <= event.id) {
                    ev.pending = None;
                }
                Some((!same, restoring))
            }
        };
        if let Some(p) = lock(&self.peers).iter_mut().find(|p| p.conn == conn) {
            p.latest = p.latest.max(Some(event.id));
        }
        if let Some((changed, restored)) = applied {
            self.save_state();
            if restored {
                log!("restored the last copy ({}), lost while clipsync was stopped", event.content.describe());
            } else if changed {
                log!("received {}, copied on {}", event.content.describe(), self.name_of(&event.id.origin));
            }
        }
        self.maybe_catch_up();
    }

    /// A peer that missed a copy asks for it.
    pub fn want(&self, conn: u64, target: EventId) {
        let reply = match &lock(&self.events).latest {
            Some(l) if l.id >= target && l.content.is_some() => {
                Out::Event(Arc::new(Event { id: l.id, content: l.content.clone().unwrap() }))
            }
            _ => Out::Nope(target),
        };
        if let Out::Event(e) = &reply
            && let Some(p) = lock(&self.peers).iter().find(|p| p.conn == conn)
        {
            log!("sent {} a copy it missed ({})", p.label, e.content.describe());
        }
        self.send_to(conn, reply);
    }

    /// The peer we asked doesn't have the content (e.g. it restarted since).
    pub fn nope(&self, conn: u64, target: EventId) {
        if let Some(p) = lock(&self.events).pending.as_mut().filter(|p| p.target == target && p.conn == Some(conn)) {
            p.conn = None;
        }
        self.maybe_catch_up();
    }

    /// If a connected peer has a newer copy than we do, ask one peer for it.
    /// Asking just one avoids every peer sending the same text at once.
    fn maybe_catch_up(&self) {
        let mut ev = lock(&self.events);
        let mine = ev.latest.as_ref().map(|l| l.id);
        let restore = ev.restore;
        let peers = lock(&self.peers);
        let wanted = |l: &EventId| Some(*l) > mine || (restore && Some(*l) == mine);
        let Some(target) = peers.iter().filter_map(|p| p.latest).filter(wanted).max() else {
            ev.pending = None;
            return;
        };
        let mut asked = Vec::new();
        if let Some(p) = &ev.pending {
            if p.target >= target && p.conn.is_some() {
                return; // already waiting on someone
            }
            if p.target == target {
                asked = p.asked.clone();
            }
        }
        let Some(peer) = peers.iter().find(|p| p.latest >= Some(target) && !asked.contains(&p.conn)) else {
            return; // everyone who has it was asked already
        };
        asked.push(peer.conn);
        let _ = peer.tx.send(Out::Want(target));
        ev.pending = Some(Pending { target, conn: Some(peer.conn), asked });
    }

    // ---- persistence ----

    fn save_state(&self) {
        let _saving = lock(&self.saving);
        let (hlc, latest) = {
            let ev = lock(&self.events);
            (ev.hlc, ev.latest.as_ref().map(|l| (l.id, l.hash)))
        };
        let online: Vec<DeviceId> = lock(&self.peers).iter().map(|p| p.id).collect();
        let mut peers = BTreeMap::new();
        for (id, c) in lock(&self.contacts).iter() {
            let p = PeerState {
                addr: c.last_known,
                last_seen: c.last_seen,
                online: online.contains(id),
                version: c.version.clone(),
            };
            peers.insert(*id, p);
        }
        let state = State {
            cluster: Some(self.cluster_id),
            hlc,
            latest,
            listen_port: Some(self.listen_port()).filter(|p| *p != 0),
            peers,
            update: lock(&self.update).clone(),
        };
        if let Err(e) = state.save() {
            log!("can't save state: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_only_moves_forward() {
        let mut ev = Events { hlc: 0, latest: None, pending: None, restore: false };
        let a = ev.tick();
        let b = ev.tick();
        assert!(b > a, "two ticks in the same millisecond still differ");
        assert!((a >> 16) + 1000 >= now_ms(), "tracks wall-clock time");
        // Having seen an event from a device whose clock runs ahead, our next
        // copy still orders after it.
        let ahead = (now_ms() + 60_000) << 16;
        ev.hlc = ev.hlc.max(ahead);
        assert!(ev.tick() > ahead);
    }
}
