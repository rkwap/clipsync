//! Pairing: adding a device to a cluster with a short one-time code.
//!
//! `clipsync invite` (on a member) shows a code like `37-482917` and waits.
//! `clipsync join 37-482917` (on the new device) finds it through mDNS by the
//! public first part, then both sides run SPAKE2 with the whole code. That
//! gives them a shared key only if their codes match, and reveals nothing an
//! eavesdropper could use to try codes offline: each guess costs a live
//! attempt, and the invite closes after three wrong ones. Over that key the
//! new device sends its public key and name, and receives the member list.

use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::thread;
use std::time::{Duration, Instant};

use chacha20poly1305::ChaCha20Poly1305;
use chacha20poly1305::aead::{Aead, KeyInit};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use spake2::{Ed25519Group, Identity as SpakeIdentity, Password, Spake2};

use crate::cluster::{self, Cluster};
use crate::store::{self, DeviceId, Identity, hex};

const SERVICE: &str = "_clipsync-pair._tcp.local.";
const MAGIC: &[u8; 8] = b"clippair";
const VALID_FOR: Duration = Duration::from_secs(10 * 60);
const MAX_FAILURES: u32 = 3;
const SEARCH_FOR: Duration = Duration::from_secs(20);
pub const DEFAULT_PORT: u16 = 7878;

/// Message directions, used as nonces: each key encrypts one message per direction.
const TO_INVITER: u8 = 1;
const TO_JOINER: u8 = 2;
const DONE: u8 = 3;

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

fn wrong_code() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "the code didn't match")
}

fn write_frame(mut s: &TcpStream, data: &[u8]) -> io::Result<()> {
    s.write_all(&(data.len() as u32).to_be_bytes())?;
    s.write_all(data)
}

fn read_frame(mut s: &TcpStream, max: usize) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    s.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > max {
        return Err(invalid("pairing message too large"));
    }
    let mut buf = vec![0u8; len];
    s.read_exact(&mut buf)?;
    Ok(buf)
}

/// Run SPAKE2 with the code; both sides end up with the same key only if
/// their codes match (which they find out when decryption fails or not).
fn pake(s: &TcpStream, code: &str) -> io::Result<ChaCha20Poly1305> {
    let (state, msg) = Spake2::<Ed25519Group>::start_symmetric(
        &Password::new(code.as_bytes()),
        &SpakeIdentity::new(b"clipsync pairing v1"),
    );
    (&*s).write_all(MAGIC)?;
    write_frame(s, &msg)?;
    let mut magic = [0u8; 8];
    (&*s).read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(invalid("not a clipsync invite"));
    }
    let theirs = read_frame(s, 1024)?;
    let key = state.finish(&theirs).map_err(|_| invalid("bad pairing message"))?;
    ChaCha20Poly1305::new_from_slice(&key).map_err(|_| invalid("bad pairing key"))
}

fn seal(key: &ChaCha20Poly1305, dir: u8, msg: &[u8]) -> Vec<u8> {
    let mut nonce = [0u8; 12];
    nonce[0] = dir;
    key.encrypt(&nonce.into(), msg).expect("encryption cannot fail")
}

fn open(key: &ChaCha20Poly1305, dir: u8, ct: &[u8]) -> io::Result<Vec<u8>> {
    let mut nonce = [0u8; 12];
    nonce[0] = dir;
    key.decrypt(&nonce.into(), ct).map_err(|_| wrong_code())
}

/// The address other devices would most likely reach us at (no packets sent).
fn local_ip() -> Option<IpAddr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    s.local_addr().ok().map(|a| a.ip())
}

fn daemon_port() -> u16 {
    store::State::load().listen_port.unwrap_or(DEFAULT_PORT)
}

/// `clipsync invite`: let one more device into the cluster, creating the
/// cluster first if this device isn't in one.
pub fn invite() -> io::Result<()> {
    let me = store::identity()?;
    let name = crate::hostname();
    let created = cluster::update(|c| match c {
        Some(c) if !c.is_member(&me.public) => {
            Err(io::Error::other("this device was removed from its cluster; run `clipsync leave` to reset it"))
        }
        Some(_) => Ok(false),
        None => {
            *c = Some(Cluster::new(me.public, &name)?);
            Ok(true)
        }
    })?;
    if created {
        println!("Created a new cluster with this device ({name}) in it.\n");
    }

    let listener = TcpListener::bind("0.0.0.0:0")?;
    let port = listener.local_addr()?.port();
    let [a, b, c, d] = store::random::<4>()?;
    let nameplate = 10 + a % 90;
    let code = format!("{nameplate}-{:06}", u32::from_be_bytes([0, b, c, d]) % 1_000_000);

    // Advertise the invite. Without mDNS, `join --at` still works.
    let advertised = advertise(&name, nameplate, port);
    if let Err(e) = &advertised {
        println!("Can't advertise the invite on the network ({e}); the new device will need --at.\n");
    }

    println!("Invite code:  {code}   (valid for 10 minutes, for one device)\n");
    println!("On the device you want to add, run:\n\n    clipsync join {code}\n");
    let ip = local_ip().map_or("<this device's IP>".to_string(), |ip| ip.to_string());
    println!("If it can't find this device, add:  --at {ip}:{port}\n");
    if !store::daemon_running() {
        println!("Note: clipsync isn't running on this device; start it so the new device can sync.\n");
    }
    println!("Waiting...");

    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + VALID_FOR;
    let mut failures = 0;
    let result = loop {
        if Instant::now() > deadline {
            break Err(io::Error::new(io::ErrorKind::TimedOut, "the invite expired; run `clipsync invite` again"));
        }
        let (stream, from) = match listener.accept() {
            Ok(conn) => conn,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(200));
                continue;
            }
            Err(e) => break Err(e),
        };
        stream.set_nonblocking(false)?;
        match welcome(&stream, &code, &me) {
            Ok(joiner) => {
                println!("{joiner} joined the cluster.");
                break Ok(());
            }
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                failures += 1;
                println!("A device at {} tried a wrong code ({failures} of {MAX_FAILURES}).", from.ip());
                if failures >= MAX_FAILURES {
                    break Err(io::Error::other("too many wrong codes, so the invite is cancelled"));
                }
            }
            Err(e) => println!("Pairing with {} failed: {e}", from.ip()),
        }
    };
    if let Ok((mdns, fullname)) = advertised {
        if let Ok(done) = mdns.unregister(&fullname) {
            let _ = done.recv_timeout(Duration::from_secs(1));
        }
        let _ = mdns.shutdown();
    }
    result
}

fn advertise(name: &str, nameplate: u8, port: u16) -> io::Result<(ServiceDaemon, String)> {
    let mdns = ServiceDaemon::new().map_err(io::Error::other)?;
    let tag = hex(&store::random::<4>()?);
    let nameplate = nameplate.to_string();
    let info = ServiceInfo::new(
        SERVICE,
        &format!("{name}-invite-{tag}"),
        &format!("clipsync-pair-{tag}.local."),
        "",
        port,
        &[("n", nameplate.as_str())][..],
    )
    .map_err(io::Error::other)?
    .enable_addr_auto();
    let fullname = info.get_fullname().to_string();
    mdns.register(info).map_err(io::Error::other)?;
    Ok((mdns, fullname))
}

/// The inviting side of one pairing attempt. Returns the new device's name.
fn welcome(stream: &TcpStream, code: &str, me: &Identity) -> io::Result<String> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let key = pake(stream, code)?;
    let request = open(&key, TO_INVITER, &read_frame(stream, 4096)?)?;
    if request.len() < 34 {
        return Err(invalid("bad pairing request"));
    }
    let joiner: DeviceId = request[..32].try_into().unwrap();
    let joiner_port = u16::from_be_bytes([request[32], request[33]]);
    let name = String::from_utf8_lossy(&request[34..]).into_owned();

    let Some(mut list) = cluster::load()? else { return Err(io::Error::other("cluster file disappeared")) };
    list.add(joiner, &name);
    let mut reply = me.public.to_vec();
    reply.extend_from_slice(&daemon_port().to_be_bytes());
    reply.extend_from_slice(list.to_text().as_bytes());
    write_frame(stream, &seal(&key, TO_JOINER, &reply))?;
    // Only add it once it confirms it saved the cluster.
    open(&key, DONE, &read_frame(stream, 64)?)?;
    cluster::update(|c| {
        c.as_mut().ok_or_else(|| io::Error::other("cluster file disappeared"))?.add(joiner, &name);
        Ok(())
    })?;
    store::add_hint(&joiner, SocketAddr::new(stream.peer_addr()?.ip(), joiner_port))?;
    Ok(name)
}

/// `clipsync join CODE`: join the cluster of the device that showed the code.
pub fn join(code: &str, at: Option<&str>) -> io::Result<()> {
    if cluster::load()?.is_some() {
        return Err(io::Error::other("this device is already in a cluster; run `clipsync leave` first"));
    }
    let (nameplate, secret) = code.trim().split_once('-').ok_or_else(|| invalid("a code looks like 37-482917"))?;
    if nameplate.parse::<u8>().is_err() || secret.len() != 6 || secret.parse::<u32>().is_err() {
        return Err(invalid("a code looks like 37-482917"));
    }
    let code = code.trim();

    // Try an inviting device at each of its addresses until one answers; then
    // never again, so a wrong code costs that invite one attempt, not one per
    // address (mDNS may also report the same invite more than once).
    let mut answered: Vec<String> = Vec::new();
    let mut try_inviter = |invite: &str, addrs: Vec<SocketAddr>| -> Option<io::Result<Cluster>> {
        if answered.iter().any(|a| a == invite) {
            return None;
        }
        for addr in addrs {
            if let Ok(stream) = TcpStream::connect_timeout(&addr, Duration::from_secs(3)) {
                answered.push(invite.to_string());
                return Some(request(&stream, code));
            }
        }
        None
    };
    let mut wrong = false;
    let mut outcome = |result: Option<io::Result<Cluster>>| match result {
        Some(Ok(c)) => Some(Ok(c)),
        Some(Err(e)) if e.kind() == io::ErrorKind::PermissionDenied => {
            wrong = true;
            None
        }
        Some(Err(e)) => {
            println!("Pairing failed: {e}");
            None
        }
        None => None,
    };

    let joined = if let Some(at) = at {
        outcome(try_inviter(at, at.to_socket_addrs()?.collect()))
    } else {
        println!("Looking for the device that showed this code...");
        let mdns = ServiceDaemon::new().map_err(io::Error::other)?;
        let events = mdns.browse(SERVICE).map_err(io::Error::other)?;
        let deadline = Instant::now() + SEARCH_FOR;
        let mut joined = None;
        while joined.is_none()
            && let Ok(event) = events.recv_deadline(deadline)
        {
            if let ServiceEvent::ServiceResolved(svc) = event
                && svc.get_property_val_str("n") == Some(nameplate)
            {
                let addrs = svc.get_addresses_v4().into_iter().map(|ip| SocketAddr::new(IpAddr::V4(ip), svc.port));
                joined = outcome(try_inviter(&svc.fullname, addrs.collect()));
            }
        }
        let _ = mdns.shutdown();
        joined
    };

    match joined {
        Some(Ok(c)) => {
            let me = store::load_identity()?.expect("saved while joining").public;
            let others: Vec<String> = c.active().filter(|(id, _)| **id != me).map(|(_, m)| m.name.clone()).collect();
            println!("Joined. The other devices in this cluster: {}.", others.join(", "));
            if store::daemon_running() {
                println!("clipsync is running and will start syncing in a few seconds.");
            } else {
                println!("Start clipsync to begin syncing.");
            }
            Ok(())
        }
        Some(Err(e)) => Err(e),
        None if wrong => Err(io::Error::other(
            "the code didn't match; check it and try again (an invite closes after 3 wrong attempts)",
        )),
        None if at.is_some() => Err(io::Error::other("couldn't pair with that address")),
        None => Err(io::Error::other(
            "couldn't find the inviting device; check that both are on the same network, or use --at",
        )),
    }
}

/// The joining side of one pairing attempt.
fn request(stream: &TcpStream, code: &str) -> io::Result<Cluster> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let key = pake(stream, code)?;
    // A fresh key: if this device was once removed from this cluster, its old
    // key is barred for good.
    let me = store::new_identity()?;
    let mut req = me.public.to_vec();
    req.extend_from_slice(&daemon_port().to_be_bytes());
    req.extend_from_slice(crate::hostname().as_bytes());
    write_frame(stream, &seal(&key, TO_INVITER, &req))?;
    // With a wrong code the inviter can't decrypt that, and hangs up.
    let reply = match read_frame(stream, 1 << 20) {
        Ok(r) => open(&key, TO_JOINER, &r)?,
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Err(wrong_code()),
        Err(e) => return Err(e),
    };
    if reply.len() < 34 {
        return Err(invalid("bad pairing reply"));
    }
    let inviter: DeviceId = reply[..32].try_into().unwrap();
    let inviter_port = u16::from_be_bytes([reply[32], reply[33]]);
    let list = Cluster::parse(std::str::from_utf8(&reply[34..]).map_err(|_| invalid("bad member list"))?)?;
    if !list.is_member(&me.public) || !list.is_member(&inviter) {
        return Err(invalid("bad member list"));
    }
    cluster::update(|c| {
        if c.is_some() {
            return Err(io::Error::other("this device joined a cluster meanwhile"));
        }
        *c = Some(list.clone());
        Ok(())
    })?;
    store::add_hint(&inviter, SocketAddr::new(stream.peer_addr()?.ip(), inviter_port))?;
    write_frame(stream, &seal(&key, DONE, b"ok"))?;
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run SPAKE2 between two sockets with the given codes; return both keys.
    fn exchange(code_a: &str, code_b: &'static str) -> (ChaCha20Poly1305, ChaCha20Poly1305) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let a = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (b, _) = l.accept().unwrap();
        let other = thread::spawn(move || pake(&b, code_b).unwrap());
        (pake(&a, code_a).unwrap(), other.join().unwrap())
    }

    #[test]
    fn matching_codes_share_a_key() {
        let (a, b) = exchange("37-482917", "37-482917");
        assert_eq!(open(&b, TO_INVITER, &seal(&a, TO_INVITER, b"hi")).unwrap(), b"hi");
    }

    #[test]
    fn a_wrong_code_fails_as_wrong_code() {
        let (a, b) = exchange("37-482917", "37-482918");
        let err = open(&b, TO_INVITER, &seal(&a, TO_INVITER, b"hi")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn directions_are_separate() {
        let (a, b) = exchange("10-000000", "10-000000");
        assert!(open(&b, TO_JOINER, &seal(&a, TO_INVITER, b"hi")).is_err());
    }
}
