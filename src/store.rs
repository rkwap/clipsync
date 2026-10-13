//! Files under ~/.config/clipsync (or $CLIPSYNC_DIR): this device's identity,
//! the cluster it belongs to, and the daemon's state. Clipboard contents are
//! never written to disk, only hashes of them.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::net::SocketAddr;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::{env, fmt, process};

/// A device is known by its X25519 public key.
pub type DeviceId = [u8; 32];

pub fn dir() -> PathBuf {
    if let Some(d) = env::var_os("CLIPSYNC_DIR") {
        return d.into();
    }
    PathBuf::from(env::var_os("HOME").unwrap_or_default()).join(".config/clipsync")
}

pub fn path(name: &str) -> PathBuf {
    dir().join(name)
}

/// Read a file, or `None` if it doesn't exist.
pub fn read(name: &str) -> io::Result<Option<String>> {
    match fs::read_to_string(path(name)) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Replace a file atomically (temp file + rename), readable only by us.
pub fn write(name: &str, contents: &str) -> io::Result<()> {
    // A temp name of its own, so concurrent writers can't trip over each other.
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = dir();
    fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!(".{name}.{}.{}.tmp", process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
    let mut f = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    f.write_all(contents.as_bytes())?;
    f.sync_all()?;
    fs::rename(tmp, dir.join(name))
}

pub fn delete(name: &str) -> io::Result<()> {
    match fs::remove_file(path(name)) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

fn lock_file(name: &str) -> io::Result<File> {
    fs::create_dir_all(dir())?;
    OpenOptions::new().create(true).truncate(false).write(true).mode(0o600).open(path(name))
}

/// Hold an exclusive lock on `name` until the returned file is dropped.
pub fn lock(name: &str) -> io::Result<File> {
    let f = lock_file(name)?;
    f.lock()?;
    Ok(f)
}

/// Like `lock`, but `None` instead of waiting if someone else holds it.
pub fn try_lock(name: &str) -> io::Result<Option<File>> {
    let f = lock_file(name)?;
    match f.try_lock() {
        Ok(()) => Ok(Some(f)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(e),
    }
}

/// The daemon holds this lock for as long as it runs, and writes its
/// process id into it.
pub const DAEMON_LOCK: &str = "daemon.lock";

pub fn daemon_running() -> bool {
    matches!(try_lock(DAEMON_LOCK), Ok(None))
}

/// The running daemon's process id.
pub fn daemon_pid() -> Option<u32> {
    if !daemon_running() {
        return None;
    }
    fs::read_to_string(path(DAEMON_LOCK)).ok()?.trim().parse().ok()
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

pub fn random<const N: usize>() -> io::Result<[u8; N]> {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).map_err(io::Error::other)?;
    Ok(b)
}

fn bad(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("corrupt {what} file in {}", dir().display()))
}

/// This device's long-term key pair.
pub struct Identity {
    pub private: [u8; 32],
    pub public: DeviceId,
}

pub fn load_identity() -> io::Result<Option<Identity>> {
    let Some(s) = read("identity")? else { return Ok(None) };
    let mut private = None;
    let mut public = None;
    for line in s.lines() {
        match line.split_once(' ') {
            Some(("private", v)) => private = unhex32(v),
            Some(("public", v)) => public = unhex32(v),
            _ => {}
        }
    }
    match (private, public) {
        (Some(private), Some(public)) => Ok(Some(Identity { private, public })),
        _ => Err(bad("identity")),
    }
}

/// Make a fresh identity, replacing any existing one.
pub fn new_identity() -> io::Result<Identity> {
    let kp = snow::Builder::new(crate::net::NOISE.parse().unwrap()).generate_keypair().map_err(io::Error::other)?;
    let id = Identity { private: kp.private.try_into().unwrap(), public: kp.public.try_into().unwrap() };
    write("identity", &format!("private {}\npublic {}\n", hex(&id.private), hex(&id.public)))?;
    Ok(id)
}

pub fn identity() -> io::Result<Identity> {
    match load_identity()? {
        Some(id) => Ok(id),
        None => new_identity(),
    }
}

/// A copy event: when it happened (hybrid logical clock) and on which device.
/// Ordering by (time, device) gives every device the same total order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct EventId {
    pub hlc: u64,
    pub origin: DeviceId,
}

impl fmt::Display for EventId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}@{}", self.hlc, &hex(&self.origin)[..8])
    }
}

/// What the daemon knows about another member.
#[derive(Clone, Default)]
pub struct PeerState {
    pub addr: Option<SocketAddr>,
    pub last_seen: u64,
    pub online: bool,
    /// The clipsync version it last said it runs.
    pub version: Option<String>,
}

/// The daemon's state, kept across restarts. Owned by the daemon; commands
/// only read it.
#[derive(Default)]
pub struct State {
    /// The cluster this state belongs to; anything else is stale.
    pub cluster: Option<[u8; 32]>,
    pub hlc: u64,
    /// The newest copy this device has, and the hash of its text.
    pub latest: Option<(EventId, [u8; 32])>,
    pub listen_port: Option<u16>,
    pub peers: BTreeMap<DeviceId, PeerState>,
    /// A newer clipsync release, if the daemon found one.
    pub update: Option<String>,
}

impl State {
    pub fn load() -> State {
        match read("state") {
            Ok(Some(s)) => State::parse(&s),
            _ => State::default(),
        }
    }

    pub fn save(&self) -> io::Result<()> {
        write("state", &self.to_text())
    }

    /// Unknown or malformed lines are skipped: it's only a cache.
    fn parse(s: &str) -> State {
        let mut st = State::default();
        for line in s.lines() {
            let f: Vec<&str> = line.split(' ').collect();
            match f.as_slice() {
                ["cluster", id] => st.cluster = unhex32(id),
                ["hlc", n] => st.hlc = n.parse().unwrap_or(0),
                ["latest", hlc, origin, hash] => {
                    if let (Ok(hlc), Some(origin), Some(hash)) = (hlc.parse(), unhex32(origin), unhex32(hash)) {
                        st.latest = Some((EventId { hlc, origin }, hash));
                    }
                }
                ["listen", port] => st.listen_port = port.parse().ok(),
                ["peer", id, addr, seen, online, rest @ ..] => {
                    if let Some(id) = unhex32(id) {
                        let p = PeerState {
                            addr: addr.parse().ok(),
                            last_seen: seen.parse().unwrap_or(0),
                            online: *online == "1",
                            version: rest.first().filter(|v| **v != "-").map(|v| v.to_string()),
                        };
                        st.peers.insert(id, p);
                    }
                }
                ["update", version] => st.update = Some(version.to_string()),
                _ => {}
            }
        }
        st
    }

    fn to_text(&self) -> String {
        let mut s = String::new();
        if let Some(c) = &self.cluster {
            s += &format!("cluster {}\n", hex(c));
        }
        s += &format!("hlc {}\n", self.hlc);
        if let Some((id, hash)) = &self.latest {
            s += &format!("latest {} {} {}\n", id.hlc, hex(&id.origin), hex(hash));
        }
        if let Some(p) = self.listen_port {
            s += &format!("listen {p}\n");
        }
        for (id, p) in &self.peers {
            let addr = p.addr.map_or("-".to_string(), |a| a.to_string());
            let version = p.version.as_deref().unwrap_or("-");
            s += &format!("peer {} {addr} {} {} {version}\n", hex(id), p.last_seen, p.online as u8);
        }
        if let Some(v) = &self.update {
            s += &format!("update {v}\n");
        }
        s
    }
}

/// Addresses learned while pairing, for the daemon to pick up (it owns the
/// state file, so commands leave hints instead of editing it).
pub fn add_hint(id: &DeviceId, addr: SocketAddr) -> io::Result<()> {
    let _lock = lock("hints.lock")?;
    let mut s = read("hints")?.unwrap_or_default();
    s += &format!("hint {} {addr}\n", hex(id));
    write("hints", &s)
}

pub fn take_hints() -> Vec<(DeviceId, SocketAddr)> {
    let Ok(_lock) = lock("hints.lock") else { return Vec::new() };
    let Ok(Some(s)) = read("hints") else { return Vec::new() };
    let _ = delete("hints");
    s.lines()
        .filter_map(|l| match l.split(' ').collect::<Vec<_>>().as_slice() {
            ["hint", id, addr] => Some((unhex32(id)?, addr.parse().ok()?)),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips() {
        let id: [u8; 32] = std::array::from_fn(|i| i as u8 * 7);
        assert_eq!(unhex32(&hex(&id)), Some(id));
        assert_eq!(unhex32("zz"), None);
        assert_eq!(unhex32(&"g".repeat(64)), None);
    }

    #[test]
    fn events_order_by_time_then_device() {
        let a = EventId { hlc: 5, origin: [1; 32] };
        let b = EventId { hlc: 5, origin: [2; 32] };
        let c = EventId { hlc: 6, origin: [0; 32] };
        assert!(a < b && b < c);
    }

    #[test]
    fn state_round_trips() {
        let mut st = State {
            cluster: Some([9; 32]),
            hlc: 123 << 16,
            latest: Some((EventId { hlc: 99, origin: [3; 32] }, [4; 32])),
            listen_port: Some(7878),
            peers: BTreeMap::new(),
            update: Some("0.2.0".into()),
        };
        let p = PeerState { addr: "10.0.0.2:7878".parse().ok(), last_seen: 42, online: true, version: Some("0.1.0".into()) };
        st.peers.insert([5; 32], p);
        st.peers.insert([6; 32], PeerState::default());
        let back = State::parse(&st.to_text());
        assert_eq!(back.to_text(), st.to_text());
        assert_eq!(back.latest, st.latest);
        assert_eq!(back.peers[&[5; 32]].addr, st.peers[&[5; 32]].addr);
        assert_eq!(back.peers[&[5; 32]].version.as_deref(), Some("0.1.0"));
        assert_eq!(back.update.as_deref(), Some("0.2.0"));
    }

    #[test]
    fn state_skips_garbage() {
        let st = State::parse("hlc nope\nlatest 1 2\nwhatever\nlisten 80\n");
        assert_eq!((st.hlc, st.latest, st.listen_port), (0, None, Some(80)));
    }
}
