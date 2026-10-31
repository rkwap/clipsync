//! Tailscale support, without Tailscale: each device is told
//! (CLIPSYNC_TAILNET_ADDRS) that 127.0.0.1 is its Tailscale address, and that
//! no other address is anyone's.

mod common;

use std::fs;
use std::thread;
use std::time::Duration;

use common::*;

fn on_tailnet(name: &str) -> Device {
    let mut d = Device::new(name);
    d.tailnet = Some("127.0.0.1".into());
    d
}

fn state(d: &Device) -> String {
    fs::read_to_string(d.dir.join("config/state")).unwrap_or_default()
}

/// Whether `d` saved `other`'s Tailscale address.
fn knows_tailnet_address(d: &Device, other: &Device) -> bool {
    let addr = format!("127.0.0.1:{}", other.port);
    state(d).lines().any(|l| l.starts_with("peer ") && l.split(' ').nth(6).is_some_and(|t| t.contains(&addr)))
}

/// Make `d` forget where it last reached each device, as if they had all
/// been on a network it has left.
fn forget_addresses(d: &Device) {
    let lines: Vec<String> = state(d)
        .lines()
        .map(|l| {
            let mut f: Vec<&str> = l.split(' ').collect();
            if f[0] == "peer" {
                f[2] = "-";
            }
            f.join(" ") + "\n"
        })
        .collect();
    fs::write(d.dir.join("config/state"), lines.concat()).unwrap();
}

#[test]
fn devices_reach_each_other_over_tailscale_when_other_addresses_are_gone() {
    let mut a = on_tailnet("a");
    let mut b = on_tailnet("b");
    let (ok, out) = a.run(&["tailscale"]);
    assert!(ok && out.starts_with("Tailscale: off"), "{out}");
    let (ok, out) = a.run(&["tailscale", "sometimes"]);
    assert!(!ok && out.contains("use on, only or off"), "{out}");
    let (ok, out) = a.run(&["tailscale", "on"]);
    assert!(ok && out.contains("This device's Tailscale address: 127.0.0.1"), "{out}");

    a.start();
    b.start();
    let invite = Invite::start(&a);
    let over_tailscale = format!("Or, over Tailscale:                 --at 127.0.0.1:{}", invite.port);
    let (ok, out) = b.run(&["join", &invite.code, "--at", &invite.at()]);
    assert!(ok, "join failed: {out}");
    let (ok, out) = invite.finish();
    assert!(ok && out.contains(&over_tailscale), "{out}");
    wait_connected(&[&a, &b]);

    // Turning it on applies right away.
    let (ok, out) = b.run(&["tailscale", "on"]);
    assert!(ok && out.contains("restarting"), "{out}");
    eventually("b to restart", || b.log().contains("the Tailscale setting changed; restarting"));
    eventually("a to save b's Tailscale address", || knows_tailnet_address(&a, &b));
    eventually("b to save a's Tailscale address", || knows_tailnet_address(&b, &a));

    // Without mDNS, and with the addresses they last connected at gone, only
    // their Tailscale addresses are left to find each other by.
    a.stop();
    b.stop();
    forget_addresses(&a);
    forget_addresses(&b);
    a.start();
    b.start();
    wait_connected(&[&a, &b]);
    a.copy("over Tailscale");
    b.wait_for_clipboard("over Tailscale");
    let (_, out) = a.run(&["devices"]);
    assert!(out.contains("connected (127.0.0.1 over Tailscale)"), "{out}");

    // Turned off, b tells the others to forget its Tailscale addresses.
    let (ok, out) = b.run(&["tailscale", "off"]);
    assert!(ok && out.starts_with("Tailscale: off"), "{out}");
    eventually("a to forget b's Tailscale address", || !knows_tailnet_address(&a, &b));
    wait_connected(&[&a, &b]);
}

#[test]
fn while_tailscale_is_down_the_others_keep_its_address() {
    let mut a = on_tailnet("a");
    let mut b = on_tailnet("b");
    for d in [&a, &b] {
        assert!(d.run(&["tailscale", "on"]).0);
    }
    a.start();
    b.start();
    pair(&a, &b);
    wait_connected(&[&a, &b]);
    eventually("b to save a's Tailscale address", || knows_tailnet_address(&b, &a));

    // a restarts while Tailscale isn't up there: it has no address to announce,
    // and b keeps the one a will be back at.
    a.stop();
    a.tailnet = Some(String::new());
    a.start();
    wait_connected(&[&a, &b]);
    thread::sleep(Duration::from_secs(1));
    assert!(a.log().contains("Tailscale isn't up on this device"));
    assert!(knows_tailnet_address(&b, &a), "{}", state(&b));

    // A setting that can't be understood stops clipsync, rather than
    // quietly going back to the local network.
    a.stop();
    fs::write(a.dir.join("config/tailscale"), "ONLY\n").unwrap();
    let (ok, out) = a.run(&["run", "--no-discovery", "--listen", &format!("127.0.0.1:{}", a.port)]);
    assert!(!ok && out.contains("unknown Tailscale setting \"ONLY\""), "{out}");
    let (ok, out) = a.run(&["tailscale"]);
    assert!(!ok && out.contains("unknown Tailscale setting"), "{out}");
    let (ok, out) = a.run(&["tailscale", "only"]);
    assert!(ok && out.starts_with("Tailscale: only"), "{out}");
}

#[test]
fn tailscale_only_uses_nothing_else() {
    let mut a = on_tailnet("a");
    let mut b = on_tailnet("b");
    for d in [&a, &b] {
        let (ok, out) = d.run(&["tailscale", "only"]);
        assert!(ok && out.starts_with("Tailscale: only"), "{out}");
    }
    // Asked to listen on every address, and to use mDNS: neither happens.
    a.discovery = true;
    a.start();
    b.start();

    let invite = Invite::start(&a);
    let (ok, out) = b.run(&["join", &invite.code]);
    assert!(!ok && out.contains("add --at"), "{out}");
    let (ok, out) = b.run(&["join", &invite.code, "--at", &format!("[::1]:{}", invite.port)]);
    assert!(!ok && out.contains("isn't a Tailscale address"), "{out}");
    let (ok, out) = b.run(&["join", &invite.code, "--at", &invite.at()]);
    assert!(ok, "join failed: {out}");
    let (ok, out) = invite.finish();
    assert!(ok, "invite failed: {out}");
    wait_connected(&[&a, &b]);
    a.copy("only over Tailscale");
    b.wait_for_clipboard("only over Tailscale");

    let log = a.log();
    assert!(log.contains(&format!("listening on 127.0.0.1:{}", a.port)), "{log}");
    assert!(!log.contains("listening on 0.0.0.0") && !log.contains("mDNS"), "{log}");
    // All of 127.0.0.0/8 is this machine on Linux, so this would connect if
    // clipsync listened on more than its Tailscale address.
    #[cfg(target_os = "linux")]
    assert!(std::net::TcpStream::connect(("127.0.0.2", a.port)).is_err());
}
