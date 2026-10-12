//! Images: copied as PNG, synced like text. Real clipsync processes, each
//! with a file as its clipboard (see common/mod.rs).

mod common;

use std::thread;
use std::time::Duration;

use common::*;

#[test]
fn images_sync_like_text() {
    let devices = cluster(&["a", "b", "c"]);
    let [a, b, c] = &devices[..] else { unreachable!() };
    a.copy_image(PNG);
    b.wait_for_image(PNG);
    c.wait_for_image(PNG);
    assert!(b.log().contains("received an image (1 KB), copied on a"), "{}", b.log());

    // Text after an image, and another image after text.
    b.copy("text again");
    a.wait_for_clipboard("text again");
    c.wait_for_clipboard("text again");
    let other = big_png(5000);
    c.copy_image(&other);
    a.wait_for_image(&other);
    b.wait_for_image(&other);
}

#[test]
fn large_images_arrive_intact() {
    let devices = cluster(&["a", "b"]);
    let big = big_png(20 << 20);
    devices[0].copy_image(&big);
    devices[1].wait_for_image(&big);
}

#[test]
fn images_over_the_limit_stay_on_their_device() {
    let devices = cluster(&["a", "b"]);
    devices[0].copy_image(&big_png(51 << 20));
    eventually("a to skip it", || devices[0].log().contains("not syncing an image (51.0 MB): the limit is 50 MB"));
    thread::sleep(Duration::from_secs(1));
    assert_eq!(devices[1].image(), None);
    // Smaller copies still go through afterwards.
    devices[0].copy("after the big one");
    devices[1].wait_for_clipboard("after the big one");
}

#[test]
fn a_device_that_was_away_gets_a_missed_image_from_one_peer() {
    let mut devices = cluster(&["a", "b", "c"]);
    devices[2].stop();
    devices[0].copy_image(PNG);
    devices[1].wait_for_image(PNG);
    devices[2].start();
    devices[2].wait_for_image(PNG);
    thread::sleep(Duration::from_secs(1));
    let served = devices[0].log_count("a copy it missed (an image") + devices[1].log_count("a copy it missed (an image");
    assert_eq!(served, 1, "exactly one device should send the missed image");
}
