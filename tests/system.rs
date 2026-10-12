//! Tests that need parts of the system: an X server (Xvfb, with python3's
//! tkinter as the app that copies and pastes), a Wayland compositor (labwc,
//! run headless, with wl-clipboard), or working multicast for mDNS. They're
//! ignored by default; run them with `cargo test -- --ignored`. Each one
//! skips itself, with a note, if what it needs isn't installed.

mod common;

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::Duration;

use common::*;

fn have(cmd: &str) -> bool {
    Command::new("sh").args(["-c", &format!("command -v {cmd}")]).stdout(Stdio::null()).status().is_ok_and(|s| s.success())
}

fn skip(why: &str) {
    eprintln!("skipped: {why}");
}

/// A process killed when the test ends.
struct Guard(Child);

impl Drop for Guard {
    fn drop(&mut self) {
        // SIGTERM first, so servers clean up their sockets.
        // SAFETY: plain kill(2) on our own child.
        unsafe { libc::kill(self.0.id() as i32, libc::SIGTERM) };
        for _ in 0..20 {
            if self.0.try_wait().is_ok_and(|s| s.is_some()) {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A private X server; returns its display name.
fn xvfb() -> (Guard, String) {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    loop {
        let n = 300 + (std::process::id() % 200) * 4 + NEXT.fetch_add(1, Ordering::Relaxed);
        let socket = format!("/tmp/.X11-unix/X{n}");
        if Path::new(&socket).exists() || Path::new(&format!("/tmp/.X{n}-lock")).exists() {
            continue;
        }
        let child = Command::new("Xvfb")
            .args([&format!(":{n}"), "-nolisten", "tcp", "-screen", "0", "64x64x8"])
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        eventually("Xvfb to start", || Path::new(&socket).exists());
        return (Guard(child), format!(":{n}"));
    }
}

const TK: &str = r#"
# Minimal X11 app: `copy FILE SECS` owns the clipboard for SECS seconds; `paste` prints it.
import sys, tkinter
r = tkinter.Tk(); r.withdraw()
if sys.argv[1] == "copy":
    r.clipboard_clear(); r.clipboard_append(open(sys.argv[2]).read()); r.update()
    r.after(int(float(sys.argv[3]) * 1000), r.destroy); r.mainloop()
else:
    try: sys.stdout.write(r.clipboard_get(type="UTF8_STRING"))
    except tkinter.TclError: pass
"#;

#[test]
#[ignore = "needs Xvfb and python3 with tkinter"]
fn x11_backend() {
    let tk_ok = Command::new("python3").args(["-c", "import tkinter"]).status().is_ok_and(|s| s.success());
    if !have("Xvfb") || !tk_ok {
        return skip("needs Xvfb and python3 with tkinter");
    }
    let tk = std::env::temp_dir().join(format!("clipsync-test-tk-{}.py", std::process::id()));
    fs::write(&tk, TK).unwrap();
    let ((_xa, da), (_xb, db)) = (xvfb(), xvfb());
    let a_display = da.clone();
    let mut a = Device::with_clipboard("a", Clipboard::X11 { display: da, tk: tk.clone() });
    let mut b = Device::with_clipboard("b", Clipboard::X11 { display: db, tk: tk.clone() });
    a.start();
    b.start();
    pair(&a, &b);
    wait_connected(&[&a, &b]);

    a.copy("hello from X11 ✓ ünïcödé");
    b.wait_for_clipboard("hello from X11 ✓ ünïcödé");
    b.copy("multi\nline");
    a.wait_for_clipboard("multi\nline");
    // Large enough that X11 moves it in chunks (INCR), both into and out of clipsync.
    let big: String = (0..3_000).map(|i| format!("line {i:04} {}\n", "x".repeat(990))).collect();
    a.copy(&big);
    b.wait_for_clipboard(&big);
    // After the app that copied exits, b's clipsync still serves the text.
    thread::sleep(Duration::from_secs(6));
    assert_eq!(b.clipboard(), big);

    // Images. No X11 app here copies images, so one comes from a device with a
    // file clipboard. a's clipsync then serves it on a's display; to check what
    // it serves, a second clipsync on that display, in a separate cluster,
    // reads it and passes it on to g.
    let mut f = Device::new("f");
    f.start();
    pair(&a, &f);
    wait_connected(&[&a, &b, &f]);
    let mut reader = Device::with_clipboard("reader", Clipboard::X11 { display: a_display.clone(), tk: tk.clone() });
    let mut g = Device::new("g");
    reader.start();
    g.start();
    pair(&reader, &g);
    wait_connected(&[&reader, &g]);
    let image = big_png(3 << 20); // large enough to move in chunks
    f.copy_image(&image);
    g.wait_for_image(&image);
    let _ = fs::remove_file(tk);
}

/// A private headless Wayland compositor; returns its socket name.
fn labwc(config: &Path) -> (Guard, String) {
    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR"));
    // By inode, not name: a killed compositor can leave its socket file
    // behind, and the next one reuses the name.
    let sockets = || -> Vec<(String, u64)> {
        fs::read_dir(&runtime)
            .unwrap()
            .filter_map(|e| {
                let e = e.ok()?;
                let name = e.file_name().into_string().ok()?;
                (name.starts_with("wayland-") && !name.ends_with(".lock")).then(|| Some((name, e.metadata().ok()?.ino())))?
            })
            .collect()
    };
    let before = sockets();
    let guard = Guard(Command::new("labwc")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DISPLAY")
        .env("WLR_BACKENDS", "headless")
        .env("WLR_LIBINPUT_NO_DEVICES", "1")
        .env("XDG_CONFIG_HOME", config)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap());
    let new_socket = || sockets().into_iter().find(|s| !before.contains(s)).map(|(name, _)| name);
    eventually("labwc to start", || new_socket().is_some());
    (guard, new_socket().unwrap())
}

#[test]
#[ignore = "needs labwc and wl-clipboard"]
fn wayland_backend() {
    if !have("labwc") || !have("wl-copy") || std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        return skip("needs labwc, wl-clipboard and XDG_RUNTIME_DIR");
    }
    let config = std::env::temp_dir().join(format!("clipsync-test-labwc-{}", std::process::id()));
    fs::create_dir_all(&config).unwrap();
    // One after the other, so each new socket is unambiguous.
    let (_ca, sa) = labwc(&config);
    let (_cb, sb) = labwc(&config);
    let mut a = Device::with_clipboard("a", Clipboard::Wayland { socket: sa });
    let mut b = Device::with_clipboard("b", Clipboard::Wayland { socket: sb });
    a.start();
    b.start();
    pair(&a, &b);
    wait_connected(&[&a, &b]);
    assert!(a.log().contains("using Wayland clipboard"), "{}", a.log());

    a.copy("hello from Wayland ✓");
    b.wait_for_clipboard("hello from Wayland ✓");
    b.copy("multi\nline\n");
    a.wait_for_clipboard("multi\nline\n");
    a.copy_image(PNG);
    b.wait_for_image(PNG);
    let big = big_png(4 << 20);
    b.copy_image(&big);
    a.wait_for_image(&big);
    a.copy("text after images");
    b.wait_for_clipboard("text after images");
    // Stopping clipsync must not leave its wl-paste helper behind.
    let pid = a.pid();
    a.stop();
    thread::sleep(Duration::from_millis(300));
    let orphans = Command::new("pgrep").args(["-P", &pid.to_string()]).output().unwrap();
    assert!(orphans.stdout.is_empty(), "leftover children: {}", String::from_utf8_lossy(&orphans.stdout));
    let _ = fs::remove_dir_all(config);
}

#[test]
#[ignore = "needs multicast (mDNS) on the local network"]
fn devices_find_each_other_with_mdns() {
    let mut a = Device::new("a");
    let mut b = Device::new("b");
    a.discovery = true;
    b.discovery = true;
    a.start();
    b.start();
    pair(&a, &b);
    wait_connected(&[&a, &b]);
    // Forget every remembered address: only mDNS can bring them back together.
    a.stop();
    b.stop();
    for d in [&a, &b] {
        let _ = fs::remove_file(d.dir.join("config/state"));
    }
    a.start();
    b.start();
    wait_connected(&[&a, &b]);
    a.copy("found via mDNS");
    b.wait_for_clipboard("found via mDNS");
}

/// The release target for this machine, as the installer picks it.
fn release_target() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-musl"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        _ => None,
    }
}

#[test]
#[ignore = "downloads clipsync 0.1.0 from GitHub"]
fn works_with_0_1_0() {
    let Some(target) = release_target() else { return skip("no 0.1.0 build for this platform") };
    let dir = std::env::temp_dir().join(format!("clipsync-test-old-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let url = format!("https://github.com/rkwap/clipsync/releases/download/v0.1.0/clipsync-0.1.0-{target}.tar.gz");
    let fetched = Command::new("sh")
        .args(["-c", &format!("curl -fsSL '{url}' | tar -xz -C '{}'", dir.display())])
        .status()
        .is_ok_and(|s| s.success());
    if !fetched {
        return skip("couldn't download 0.1.0");
    }
    let mut new = Device::new("new");
    let mut old = Device::new("old");
    old.bin = dir.join(format!("clipsync-0.1.0-{target}/clipsync"));
    new.start();
    old.start();
    pair(&new, &old);
    wait_connected(&[&new, &old]);

    new.copy("text from the new version");
    old.wait_for_clipboard("text from the new version");
    old.copy("text from 0.1.0");
    new.wait_for_clipboard("text from 0.1.0");
    // 0.1.0 doesn't know images: it ignores them and keeps syncing text.
    new.copy_image(PNG);
    thread::sleep(Duration::from_secs(1));
    assert_eq!(old.clipboard(), "text from 0.1.0");
    new.copy("text after the image");
    old.wait_for_clipboard("text after the image");
    assert!(new.run(&["devices"]).1.contains("runs clipsync 0.1.0; update it"));
    let _ = fs::remove_dir_all(dir);
}
