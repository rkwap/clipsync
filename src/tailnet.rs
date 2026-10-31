//! Optional Tailscale support, set with `clipsync tailscale on|only|off`.
//!
//! To clipsync, Tailscale is just another network, one that reaches the
//! other devices from wherever they are. So all it needs to know is which of
//! this device's addresses are Tailscale's, to tell the other members.
//!
//! - off (the default): Tailscale isn't used.
//! - on: members tell each other their Tailscale addresses, and keep them
//!   apart from the addresses they last connected at, so a device that left
//!   the local network is still reachable.
//! - only: nothing but Tailscale. clipsync accepts connections only on this
//!   device's Tailscale addresses, dials only the others', and doesn't use
//!   mDNS, which would announce the device on every network it joins.
//!
//! An interface counts as Tailscale's if it has an address in Tailscale's
//! IPv6 range, or is named `tailscale*`. Its IPv4 range alone isn't enough:
//! that's also carrier-grade NAT space, which phone hotspots and some ISPs
//! hand out.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use std::{env, fmt, io, thread};

use crate::hub::Hub;
use crate::{net, store};

/// The setting's file in the config directory; absent means off.
pub const FILE: &str = "tailscale";

/// How often to look for changes to our Tailscale addresses: Tailscale
/// often comes up after clipsync, at login.
const CHECK_EVERY: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Mode {
    #[default]
    Off,
    On,
    Only,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "off" => Some(Mode::Off),
            "on" => Some(Mode::On),
            "only" => Some(Mode::Only),
            _ => None,
        }
    }

    /// The saved setting. A setting that can't be read is an error, not
    /// off: falling back to the local network would undo `only`.
    pub fn load() -> io::Result<Mode> {
        let unreadable = |e: io::Error| io::Error::new(e.kind(), format!("can't read the Tailscale setting: {e}"));
        let Some(s) = store::read(FILE).map_err(unreadable)? else { return Ok(Mode::Off) };
        Mode::parse(s.trim()).ok_or_else(|| {
            io::Error::other(format!(
                "unknown Tailscale setting {:?} in {}; set it again with `clipsync tailscale on|only|off`",
                s.trim(),
                store::path(FILE).display()
            ))
        })
    }

    pub fn save(self) -> io::Result<()> {
        match self {
            Mode::Off => store::delete(FILE),
            mode => store::write(FILE, &format!("{mode}\n")),
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            Mode::Off => "off",
            Mode::On => "on",
            Mode::Only => "only",
        })
    }
}

/// Whether `ip` is in Tailscale's ranges: 100.64.0.0/10 and fd7a:115c:a1e0::/48.
fn in_range(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.octets()[0] == 100 && v4.octets()[1] & 0xc0 == 64,
        IpAddr::V6(v6) => v6.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}

/// $CLIPSYNC_TAILNET_ADDRS, for tests: these addresses are this device's
/// Tailscale addresses, and the only ones that count as Tailscale's.
fn stand_ins() -> Option<&'static [IpAddr]> {
    static ADDRS: OnceLock<Option<Vec<IpAddr>>> = OnceLock::new();
    ADDRS
        .get_or_init(|| {
            let list = env::var("CLIPSYNC_TAILNET_ADDRS").ok()?;
            Some(list.split(',').filter_map(|s| s.trim().parse().ok()).collect())
        })
        .as_deref()
}

/// Whether `ip` is a Tailscale address, ours or another device's.
pub fn is_tailnet(ip: IpAddr) -> bool {
    let ip = ip.to_canonical();
    match stand_ins() {
        Some(addrs) => addrs.contains(&ip),
        None => in_range(ip),
    }
}

/// This device's Tailscale addresses, IPv4 first; none if Tailscale isn't up.
pub fn own_addrs() -> Vec<IpAddr> {
    if let Some(addrs) = stand_ins() {
        return addrs.to_vec();
    }
    let Ok(ifaces) = if_addrs::get_if_addrs() else { return Vec::new() };
    let is_tailscale = |name: &str| {
        name.starts_with("tailscale") || ifaces.iter().any(|i| i.name == name && i.ip().is_ipv6() && in_range(i.ip()))
    };
    let mut addrs: Vec<IpAddr> =
        ifaces.iter().filter(|i| in_range(i.ip()) && is_tailscale(&i.name)).map(|i| i.ip()).collect();
    addrs.sort_by_key(|ip| (ip.is_ipv6(), *ip));
    addrs.dedup();
    addrs
}

pub fn list(addrs: &[IpAddr]) -> String {
    addrs.iter().map(|ip| ip.to_string()).collect::<Vec<_>>().join(", ")
}

/// Whether a listener bound to `bound` takes connections to `ip`. One on
/// `::` takes IPv4 too (on dual-stack systems, the default); one on
/// `0.0.0.0` only takes IPv4.
fn covers(bound: IpAddr, ip: IpAddr) -> bool {
    bound == ip || (bound.is_unspecified() && (bound.is_ipv6() || ip.is_ipv4()))
}

/// Keep the members told of our Tailscale addresses as they come and go,
/// and reconnect when they do. In `only` mode, also listen on each of them,
/// on `listen`'s port. Otherwise `listen` is where we already listen, and
/// only the Tailscale addresses it takes connections on are announced. The
/// first check happens before this returns.
pub fn watch(mode: Mode, listen: SocketAddr, hub: Arc<Hub>) {
    let mut port = listen.port();
    let mut known: Option<Vec<IpAddr>> = None;
    let mut listening: Vec<IpAddr> = Vec::new();
    let mut failed: Vec<IpAddr> = Vec::new();
    let mut check = move || {
        let found = own_addrs();
        let mut addrs = found.clone();
        if mode == Mode::Only {
            for ip in &addrs {
                if listening.contains(ip) {
                    continue;
                }
                match net::listen(&SocketAddr::new(*ip, port).to_string(), hub.clone()) {
                    Ok(local) => {
                        listening.push(*ip);
                        if port == 0 {
                            port = local.port(); // the rest on the same port
                            hub.set_listen_port(port);
                        }
                    }
                    // Retried at each check, but only reported once.
                    Err(e) if !failed.contains(ip) => {
                        log!("can't listen on {ip}: {e}");
                        failed.push(*ip);
                    }
                    Err(_) => {}
                }
            }
        } else {
            addrs.retain(|ip| covers(listen.ip(), *ip));
        }
        if known.as_ref() == Some(&addrs) {
            return;
        }
        match addrs.as_slice() {
            [] if !found.is_empty() => {
                log!("not listening on this device's Tailscale addresses ({}), so not using them", list(&found))
            }
            [] if mode == Mode::Only => log!("Tailscale isn't up on this device; nothing syncs until it is"),
            [] => log!("Tailscale isn't up on this device; using the local network until it is"),
            _ => log!("reachable over Tailscale at {}", list(&addrs)),
        }
        // Connections over an address that went away are dead (Tailscale
        // stopped), like when one of our addresses goes away (see discovery.rs).
        for ip in known.iter().flatten().filter(|ip| !addrs.contains(ip)) {
            hub.drop_local(*ip);
        }
        // While Tailscale is down, peers keep the addresses we'll be back at.
        if !addrs.is_empty() {
            let announced: Vec<SocketAddr> = addrs.iter().map(|ip| SocketAddr::new(*ip, port)).collect();
            hub.set_own_tailnet(&announced);
        }
        // Members may be reachable now; don't leave the dialers in their backoff.
        hub.wake_dialers();
        known = Some(addrs);
    };
    check();
    thread::spawn(move || {
        loop {
            thread::sleep(CHECK_EVERY);
            check();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tailscale_ranges() {
        for ip in ["100.64.0.1", "100.84.73.86", "100.127.255.254", "fd7a:115c:a1e0::b801:49ce"] {
            assert!(in_range(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["100.63.255.255", "100.128.0.1", "192.168.1.20", "10.0.0.1", "fd7a:115c:a1e1::1", "::1"] {
            assert!(!in_range(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn listeners_cover_what_they_bind() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        assert!(covers(ip("0.0.0.0"), ip("100.84.73.86")));
        assert!(!covers(ip("0.0.0.0"), ip("fd7a:115c:a1e0::1")));
        assert!(covers(ip("::"), ip("fd7a:115c:a1e0::1")) && covers(ip("::"), ip("100.84.73.86")));
        assert!(covers(ip("100.84.73.86"), ip("100.84.73.86")));
        assert!(!covers(ip("192.168.1.5"), ip("100.84.73.86")));
    }

    #[test]
    fn modes_round_trip() {
        for mode in [Mode::Off, Mode::On, Mode::Only] {
            assert_eq!(Mode::parse(&mode.to_string()), Some(mode));
        }
        assert_eq!(Mode::parse("yes"), None);
    }
}
