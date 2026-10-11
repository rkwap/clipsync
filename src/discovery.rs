//! Finds the other members on the local network with mDNS (DNS-SD), so no
//! one has to type an IP address. Each device advertises `_clipsync._tcp`
//! with its device key and a tag derived from its cluster, and ignores every
//! device that isn't a member of its cluster.
//!
//! It searches again every 30 s. mDNS on its own backs off to one search an
//! hour, so a device that slept or lost Wi-Fi for a few minutes (long enough
//! for its records to expire here) could otherwise take that long to rediscover.

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use mdns_sd::{DaemonEvent, ServiceDaemon, ServiceEvent, ServiceInfo};

use crate::hub::Hub;
use crate::store::{hex, unhex32};

const SERVICE: &str = "_clipsync._tcp.local.";
const SEARCH_EVERY: Duration = Duration::from_secs(30);

/// The daemon and our registered service name, for `stop`.
static REGISTERED: OnceLock<(ServiceDaemon, String)> = OnceLock::new();

/// Advertise this device on `port`, and tell the hub where members are.
pub fn start(port: u16, hub: Arc<Hub>) -> io::Result<()> {
    let mdns = ServiceDaemon::new().map_err(io::Error::other)?;
    let id = hex(&hub.id);
    let tag = hub.tag();
    let props = [("id", id.as_str()), ("c", tag.as_str())];
    let info = ServiceInfo::new(
        SERVICE,
        &format!("{}-{}", crate::hostname(), &id[..6]),
        // Our own mDNS host name, so we never clash with the OS's.
        &format!("clipsync-{}.local.", &id[..16]),
        "",
        port,
        &props[..],
    )
    .map_err(io::Error::other)?
    .enable_addr_auto();
    let fullname = info.get_fullname().to_string();
    mdns.register(info).map_err(io::Error::other)?;
    let _ = REGISTERED.set((mdns.clone(), fullname));
    let mut events = mdns.browse(SERVICE).map_err(io::Error::other)?;
    log!("looking for the other devices on the local network (mDNS)");

    // React to our own addresses changing right away: connections from an
    // address we no longer have are dead, and a new address may mean a new
    // network where members are reachable again.
    let monitor = mdns.monitor().map_err(io::Error::other)?;
    {
        let hub = hub.clone();
        thread::spawn(move || {
            while let Ok(event) = monitor.recv() {
                match event {
                    DaemonEvent::IpDel(ip) => hub.drop_local(ip),
                    DaemonEvent::IpAdd(_) => {}
                    _ => continue,
                }
                hub.wake_dialers();
            }
        });
    }

    thread::spawn(move || {
        let mut next_search = Instant::now() + SEARCH_EVERY;
        loop {
            let event = match events.recv_deadline(next_search) {
                Ok(event) => event,
                Err(_) if Instant::now() >= next_search => {
                    // Browsing again restarts the query schedule; the old one
                    // stops once its receiver (the old `events`) is dropped.
                    match mdns.browse(SERVICE) {
                        Ok(new) => events = new,
                        Err(e) => log!("mDNS search failed: {e}"),
                    }
                    next_search = Instant::now() + SEARCH_EVERY;
                    continue;
                }
                Err(_) => break, // the mDNS daemon stopped
            };
            match event {
                ServiceEvent::ServiceResolved(svc) => {
                    if svc.get_property_val_str("c") != Some(tag.as_str()) {
                        continue; // some other cluster's device
                    }
                    let Some(peer) = svc.get_property_val_str("id").and_then(unhex32) else { continue };
                    if peer == hub.id {
                        continue;
                    }
                    let addrs: Vec<SocketAddr> =
                        svc.get_addresses_v4().into_iter().map(|ip| SocketAddr::new(IpAddr::V4(ip), svc.port)).collect();
                    if !addrs.is_empty() {
                        hub.discovered(peer, &svc.fullname, addrs);
                    }
                }
                ServiceEvent::ServiceRemoved(_, fullname) => hub.undiscovered(&fullname),
                _ => {}
            }
        }
    });
    Ok(())
}

/// Say goodbye on the network, so members forget our address right away
/// instead of waiting for our records to expire.
pub fn stop() {
    if let Some((mdns, fullname)) = REGISTERED.get()
        && let Ok(done) = mdns.unregister(fullname)
    {
        let _ = done.recv_timeout(Duration::from_secs(1));
    }
}
