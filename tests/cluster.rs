//! End-to-end tests: real clipsync processes pairing, syncing, and managing
//! a cluster. Each device uses a file as its clipboard (see common/mod.rs).

mod common;

use std::thread;
use std::time::Duration;

use common::*;

#[test]
fn unpaired_devices_ignore_each_other_until_paired() {
    let mut a = Device::new("a");
    let mut b = Device::new("b");
    a.start();
    b.start();
    a.copy("copied before pairing");
    thread::sleep(Duration::from_secs(1));
    assert_eq!(b.clipboard(), "");

    pair(&a, &b);
    wait_connected(&[&a, &b]);
    // Pairing doesn't drag over what was copied before it.
    assert_eq!(b.clipboard(), "");

    a.copy("from a");
    b.wait_for_clipboard("from a");
    b.copy("from b");
    a.wait_for_clipboard("from b");
}

#[test]
fn any_member_can_invite_and_everyone_learns_about_new_devices() {
    let mut a = Device::new("a");
    let mut b = Device::new("b");
    let mut c = Device::new("c");
    for d in [&mut a, &mut b, &mut c] {
        d.start();
    }
    pair(&a, &b);
    pair(&b, &c); // b didn't create the cluster
    // a never paired with c, and there's no mDNS: a and c reach each other
    // through the addresses b shares.
    wait_connected(&[&a, &b, &c]);
    c.copy("from the newest device");
    a.wait_for_clipboard("from the newest device");
    b.wait_for_clipboard("from the newest device");
}

#[test]
fn wrong_codes_cancel_the_invite() {
    let mut a = Device::new("a");
    a.start();
    let stranger = Device::new("stranger");
    let invite = Invite::start(&a);
    let (plate, secret) = invite.code.split_once('-').unwrap();
    let wrong = |i: u32| format!("{plate}-{:06}", (secret.parse::<u32>().unwrap() + i) % 1_000_000);
    for i in 1..=3 {
        let (ok, out) = stranger.run(&["join", &wrong(i), "--at", &invite.at()]);
        assert!(!ok && out.contains("didn't match"), "wrong code {i}: {out}");
    }
    // Even the right code is too late now.
    let (ok, _) = stranger.run(&["join", &invite.code, "--at", &invite.at()]);
    assert!(!ok);
    let (ok, out) = invite.finish();
    assert!(!ok && out.contains("too many wrong codes"), "{out}");
    assert!(!stranger.has_cluster());
}

#[test]
fn removed_devices_are_cut_off_and_told() {
    let devices = cluster(&["a", "b", "c"]);
    let [a, b, c] = &devices[..] else { unreachable!() };
    let (ok, out) = b.run(&["remove", "c"]);
    assert!(ok, "{out}");

    eventually("c to learn it was removed", || !c.has_cluster());
    assert!(c.log().contains("no longer in the cluster"));
    a.copy("after the removal");
    b.wait_for_clipboard("after the removal");
    thread::sleep(Duration::from_secs(1));
    assert_ne!(c.clipboard(), "after the removal");
    let (_, out) = a.run(&["devices"]);
    assert!(out.starts_with("2 devices") && !out.contains("\n  c "), "{out}");
}

#[test]
fn leaving_tells_the_others() {
    let devices = cluster(&["a", "b"]);
    let [a, b] = &devices[..] else { unreachable!() };
    let (ok, out) = b.run(&["leave"]);
    assert!(ok && out.contains("Left the cluster."), "{out}");
    assert!(!b.has_cluster());
    eventually("a to drop b", || a.run(&["devices"]).1.starts_with("1 device in this cluster"));
}

#[test]
fn simultaneous_copies_converge() {
    let devices = cluster(&["a", "b", "c"]);
    let [a, b, c] = &devices[..] else { unreachable!() };
    for round in 0..3 {
        let (from_a, from_b) = (format!("round {round} from a"), format!("round {round} from b"));
        thread::scope(|s| {
            s.spawn(|| a.copy(&from_a));
            s.spawn(|| b.copy(&from_b));
        });
        eventually(&format!("round {round} to converge"), || {
            let (x, y, z) = (a.clipboard(), b.clipboard(), c.clipboard());
            x == y && y == z && (x == from_a || x == from_b)
        });
    }
}

#[test]
fn a_device_that_was_away_gets_the_latest_copy_from_one_peer() {
    let mut devices = cluster(&["a", "b", "c"]);
    devices[2].stop();
    let [a, b, _] = &devices[..] else { unreachable!() };
    a.copy("first, while c was away");
    b.wait_for_clipboard("first, while c was away");
    b.copy("latest, while c was away");
    a.wait_for_clipboard("latest, while c was away");

    devices[2].start();
    devices[2].wait_for_clipboard("latest, while c was away");
    thread::sleep(Duration::from_secs(1));
    let served = devices[0].log_count("a copy it missed") + devices[1].log_count("a copy it missed");
    assert_eq!(served, 1, "exactly one device should send the missed copy");
}

#[test]
fn restarts_dont_resend_and_a_lost_clipboard_is_restored() {
    let mut devices = cluster(&["a", "b"]);
    devices[0].copy("before the restart");
    devices[1].wait_for_clipboard("before the restart");

    // A plain restart: nothing is sent or applied again.
    let (sent, received) = (devices[1].log_count("bytes to"), devices[0].log_count("received"));
    devices[1].stop();
    devices[1].start();
    wait_connected(&[&devices[0], &devices[1]]);
    thread::sleep(Duration::from_secs(1));
    assert_eq!(devices[1].log_count("bytes to"), sent);
    assert_eq!(devices[0].log_count("received"), received);

    // On X11 a restart empties the clipboard clipsync was holding; it comes back.
    devices[1].stop();
    devices[1].clear_clipboard();
    devices[1].start();
    devices[1].wait_for_clipboard("before the restart");
    assert!(devices[1].log().contains("restored the last copy"));
}

#[test]
fn large_copies_arrive_intact() {
    let devices = cluster(&["a", "b"]);
    let big: String = (0..3_000_000).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
    devices[0].copy(&big);
    devices[1].wait_for_clipboard(&big);
}

#[test]
fn devices_lists_the_cluster() {
    let devices = cluster(&["a", "b"]);
    let (ok, out) = devices[0].run(&["devices"]);
    assert!(ok);
    assert!(out.starts_with("2 devices in this cluster"), "{out}");
    assert!(out.lines().any(|l| l.trim_start().starts_with("a ") && l.contains("this device")), "{out}");
    assert!(out.lines().any(|l| l.trim_start().starts_with("b ") && l.contains("connected")), "{out}");
}

#[test]
fn commands_explain_themselves_outside_a_cluster() {
    let d = Device::new("lonely");
    let (ok, out) = d.run(&["devices"]);
    assert!(ok && out.contains("isn't in a cluster"), "{out}");
    let (ok, out) = d.run(&["join", "not-a-code"]);
    assert!(!ok && out.contains("a code looks like"), "{out}");
    let (ok, out) = d.run(&["--version"]);
    assert!(ok && out.starts_with("clipsync "), "{out}");
}
