//! Runs real clipsync processes. Each `Device` has its own config directory,
//! port, and a plain file as its clipboard (CLIPSYNC_CLIPBOARD_FILE), so tests
//! never touch the real clipboard or each other. mDNS is off: devices pair with
//! `--at` and find each other through the addresses they share, so the tests
//! work where multicast doesn't.

#![allow(dead_code)] // each test file uses a different subset

use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

pub const BIN: &str = env!("CARGO_BIN_EXE_clipsync");

/// A real 4x3 PNG.
pub const PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
    0x52, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x03, 0x08, 0x02, 0x00, 0x00, 0x00, 0x3b,
    0x96, 0x39, 0x91, 0x00, 0x00, 0x00, 0x10, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x38,
    0xa1, 0xa1, 0x01, 0x47, 0x0c, 0x38, 0x39, 0x00, 0x07, 0xa6, 0x0d, 0x21, 0x94, 0xb0, 0x13,
    0xfc, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82
];

/// An image of about `size` bytes: a valid PNG header, padded (clipsync
/// passes images through without decoding them).
pub fn big_png(size: usize) -> Vec<u8> {
    let mut png = PNG.to_vec();
    png.extend((0..size).map(|i| (i * 7 % 251) as u8));
    png
}

/// How the device's clipboard is reached.
pub enum Clipboard {
    File,
    X11 { display: String, tk: PathBuf },
    Wayland { socket: String },
}

pub struct Device {
    pub name: String,
    pub dir: PathBuf,
    pub port: u16,
    pub clipboard: Clipboard,
    /// The clipsync binary to run (another version, for compatibility tests).
    pub bin: PathBuf,
    /// Use mDNS (off by default, so tests work without multicast).
    pub discovery: bool,
    daemon: Option<Child>,
}

static NEXT: AtomicU32 = AtomicU32::new(0);

pub fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// Poll `cond` until it holds, failing the test after 30 s.
pub fn eventually(what: &str, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(100));
    }
}

impl Device {
    pub fn new(name: &str) -> Device {
        Device::with_clipboard(name, Clipboard::File)
    }

    pub fn with_clipboard(name: &str, clipboard: Clipboard) -> Device {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("clipsync-test-{}-{n}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Device { name: name.into(), dir, port: free_port(), clipboard, bin: BIN.into(), discovery: false, daemon: None }
    }

    /// A clipsync command run as this device.
    pub fn cmd(&self) -> Command {
        let mut c = Command::new(&self.bin);
        c.env("CLIPSYNC_DIR", self.dir.join("config")).env("CLIPSYNC_NAME", &self.name);
        c.env("CLIPSYNC_NO_UPDATE_CHECK", "1");
        c.env_remove("CLIPSYNC_CLIPBOARD_FILE").env_remove("WAYLAND_DISPLAY").env_remove("DISPLAY");
        match &self.clipboard {
            Clipboard::File => c.env("CLIPSYNC_CLIPBOARD_FILE", self.dir.join("clipboard")),
            Clipboard::X11 { display, .. } => c.env("DISPLAY", display),
            Clipboard::Wayland { socket } => c.env("WAYLAND_DISPLAY", socket),
        };
        c
    }

    /// Run a command; returns whether it succeeded and everything it printed.
    pub fn run(&self, args: &[&str]) -> (bool, String) {
        let out = self.cmd().args(args).output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
        (out.status.success(), text)
    }

    pub fn log(&self) -> String {
        fs::read_to_string(self.dir.join("log")).unwrap_or_default()
    }

    pub fn log_count(&self, pattern: &str) -> usize {
        self.log().matches(pattern).count()
    }

    /// Start the daemon and wait until it's up.
    pub fn start(&mut self) {
        let ready = |d: &Device| d.log_count("not in a cluster yet") + d.log_count("syncing with");
        let before = ready(self);
        let log = fs::OpenOptions::new().create(true).append(true).open(self.dir.join("log")).unwrap();
        let mut cmd = self.cmd();
        if self.discovery {
            // mDNS advertises the LAN addresses, so listen on them.
            cmd.args(["run", "--listen", &format!("0.0.0.0:{}", self.port)]);
        } else {
            cmd.args(["run", "--no-discovery", "--listen", &format!("127.0.0.1:{}", self.port)]);
        }
        if let Clipboard::X11 { .. } = self.clipboard {
            cmd.args(["--backend", "x11"]);
        }
        self.daemon = Some(cmd.stdout(Stdio::null()).stderr(log).spawn().unwrap());
        eventually(&format!("{} to start", self.name), || ready(self) > before);
    }

    pub fn stop(&mut self) {
        if let Some(mut child) = self.daemon.take() {
            // SAFETY: plain kill(2) on our own child.
            unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
            let _ = child.wait();
        }
    }

    pub fn pid(&self) -> i32 {
        self.daemon.as_ref().expect("daemon running").id() as i32
    }

    /// Copy `text` on this device, the way an app would.
    pub fn copy(&self, text: &str) {
        match &self.clipboard {
            Clipboard::File => {
                let tmp = self.dir.join("clipboard.new");
                fs::write(&tmp, text).unwrap();
                fs::rename(tmp, self.dir.join("clipboard")).unwrap();
            }
            Clipboard::X11 { display, tk } => {
                // Tk owns the selection while it runs, like any X11 app that copied.
                let file = self.dir.join("copy.txt");
                fs::write(&file, text).unwrap();
                let mut app = Command::new("python3")
                    .arg(tk)
                    .args(["copy", file.to_str().unwrap(), "5"])
                    .env("DISPLAY", display)
                    .spawn()
                    .unwrap();
                thread::spawn(move || app.wait()); // it exits on its own; reap it then
                thread::sleep(Duration::from_millis(700));
            }
            Clipboard::Wayland { socket } => {
                let mut child = Command::new("wl-copy").env("WAYLAND_DISPLAY", socket).stdin(Stdio::piped()).spawn().unwrap();
                std::io::Write::write_all(&mut child.stdin.take().unwrap(), text.as_bytes()).unwrap();
                child.wait().unwrap();
            }
        }
    }

    /// What pasting on this device would give.
    pub fn clipboard(&self) -> String {
        match &self.clipboard {
            Clipboard::File => fs::read_to_string(self.dir.join("clipboard")).unwrap_or_default(),
            Clipboard::X11 { display, tk } => {
                let out = Command::new("python3").arg(tk).arg("paste").env("DISPLAY", display).output().unwrap();
                String::from_utf8_lossy(&out.stdout).into_owned()
            }
            Clipboard::Wayland { socket } => {
                let out = Command::new("wl-paste").args(["-n", "-t", "text"]).env("WAYLAND_DISPLAY", socket).output().unwrap();
                String::from_utf8_lossy(&out.stdout).into_owned()
            }
        }
    }

    /// Copy an image (PNG bytes) on this device.
    pub fn copy_image(&self, png: &[u8]) {
        match &self.clipboard {
            Clipboard::File => {
                let tmp = self.dir.join("clipboard.new");
                fs::write(&tmp, png).unwrap();
                fs::rename(tmp, self.dir.join("clipboard")).unwrap();
            }
            Clipboard::Wayland { socket } => {
                let mut child = Command::new("wl-copy")
                    .args(["--type", "image/png"])
                    .env("WAYLAND_DISPLAY", socket)
                    .stdin(Stdio::piped())
                    .spawn()
                    .unwrap();
                std::io::Write::write_all(&mut child.stdin.take().unwrap(), png).unwrap();
                child.wait().unwrap();
            }
            Clipboard::X11 { .. } => panic!("no X11 app here can copy images"),
        }
    }

    /// The image pasting on this device would give, if it's an image.
    pub fn image(&self) -> Option<Vec<u8>> {
        let bytes = match &self.clipboard {
            Clipboard::File => fs::read(self.dir.join("clipboard")).ok()?,
            Clipboard::Wayland { socket } => {
                let out = Command::new("wl-paste")
                    .args(["-n", "-t", "image/png"])
                    .env("WAYLAND_DISPLAY", socket)
                    .output()
                    .ok()?;
                out.status.success().then_some(out.stdout)?
            }
            Clipboard::X11 { .. } => panic!("no X11 app here can paste images"),
        };
        bytes.starts_with(b"\x89PNG").then_some(bytes)
    }

    pub fn wait_for_image(&self, png: &[u8]) {
        eventually(&format!("{} to have the {}-byte image", self.name, png.len()), || {
            self.image().as_deref() == Some(png)
        });
    }

    /// Empty the clipboard, as restarting the app that held it does on X11.
    pub fn clear_clipboard(&self) {
        let _ = fs::remove_file(self.dir.join("clipboard"));
    }

    pub fn wait_for_clipboard(&self, text: &str) {
        let short: String = text.chars().take(40).collect();
        eventually(&format!("{} to have {short:?}", self.name), || self.clipboard() == text);
    }

    pub fn has_cluster(&self) -> bool {
        self.dir.join("config/cluster").exists()
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        self.stop();
        if thread::panicking() {
            eprintln!("--- log of {} ({}):\n{}", self.name, self.dir.display(), self.log());
        } else {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}

/// A running `clipsync invite`, with its code and port.
pub struct Invite {
    child: Child,
    out: BufReader<ChildStdout>,
    pub code: String,
    pub port: u16,
}

impl Invite {
    pub fn start(inviter: &Device) -> Invite {
        let mut child = inviter.cmd().arg("invite").stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let mut out = BufReader::new(child.stdout.take().unwrap());
        let (mut code, mut port) = (None, None);
        let mut line = String::new();
        while code.is_none() || port.is_none() {
            line.clear();
            assert!(out.read_line(&mut line).unwrap() > 0, "invite exited early");
            if let Some(c) = line.strip_prefix("Invite code:") {
                code = c.split_whitespace().next().map(String::from);
            }
            if let Some((_, at)) = line.split_once("--at ") {
                port = at.trim().rsplit(':').next().and_then(|p| p.parse().ok());
            }
        }
        Invite { child, out, code: code.unwrap(), port: port.unwrap() }
    }

    pub fn at(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    /// Wait for it to finish; returns whether it succeeded and what it printed.
    pub fn finish(mut self) -> (bool, String) {
        let mut rest = String::new();
        let _ = self.out.read_to_string(&mut rest);
        let mut err = String::new();
        let _ = self.child.stderr.take().unwrap().read_to_string(&mut err);
        (self.child.wait().unwrap().success(), rest + &err)
    }
}

/// Add `joiner` to `inviter`'s cluster (created if needed).
pub fn pair(inviter: &Device, joiner: &Device) {
    let invite = Invite::start(inviter);
    let (ok, out) = joiner.run(&["join", &invite.code, "--at", &invite.at()]);
    assert!(ok, "join failed: {out}");
    let (ok, out) = invite.finish();
    assert!(ok, "invite failed: {out}");
}

/// Wait until every device lists every other one as connected.
pub fn wait_connected(devices: &[&Device]) {
    for d in devices {
        eventually(&format!("{} to connect to the others", d.name), || {
            let (_, out) = d.run(&["devices"]);
            devices.iter().filter(|o| o.name != d.name).all(|o| {
                out.lines().any(|l| l.trim_start().starts_with(&format!("{} ", o.name)) && l.contains("connected ("))
            })
        });
    }
}

/// Start `names.len()` devices and pair them all through the first one.
pub fn cluster(names: &[&str]) -> Vec<Device> {
    let mut devices: Vec<Device> = names.iter().map(|n| Device::new(n)).collect();
    for d in &mut devices {
        d.start();
    }
    for d in &devices[1..] {
        pair(&devices[0], d);
    }
    wait_connected(&devices.iter().collect::<Vec<_>>());
    devices
}
