//! `clipsync start`, `stop` and `status` with each way of running in the
//! background. systemd and launchd are stood in for by small scripts (real
//! ones would touch this machine's own services, and launchd is macOS-only)
//! that really start and stop the daemon; launchd's also lingers after an
//! unload, like the real one. The detached mode runs for real.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use common::*;

const SYSTEMCTL: &str = r#"#!/bin/sh
# Fake `systemctl --user` for clipsync.service. It runs ExecStart itself.
s="$MOCK_DIR"; shift  # --user
unit="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/clipsync.service"
alive() { [ -f "$s/pid" ] && kill -0 "$(cat "$s/pid")" 2>/dev/null; }
launch() {
    exe=$(sed -n 's/^ExecStart="\(.*\)"$/\1/p' "$unit")
    sh -c 'echo $$ > "$1/pid"; exec "$2" >> "$1/log" 2>&1' sh "$s" "$exe" > /dev/null 2>&1 < /dev/null &
    for i in 1 2 3 4 5 6 7 8 9 10; do [ -f "$s/pid" ] && break; sleep 0.1; done
}
halt() { if alive; then kill "$(cat "$s/pid")"; fi; rm -f "$s/pid"; }
case "$1" in
    show-environment|daemon-reload) ;;
    enable) touch "$s/enabled" ;;
    disable) rm -f "$s/enabled"; [ "$2" = --now ] && halt ;;
    start) alive || launch ;;
    restart) halt; launch ;;
    stop) halt ;;
    is-active) alive ;;
    is-enabled) [ -f "$s/enabled" ] ;;
    *) echo "fake systemctl: $*" >&2; exit 1 ;;
esac
"#;

const LAUNCHCTL: &str = r#"#!/bin/sh
# Fake launchctl for the com.clipsync agent. After bootout the agent lingers
# for a couple of calls, and bootstrap fails with error 5 meanwhile.
s="$MOCK_DIR"; plist="$HOME/Library/LaunchAgents/com.clipsync.plist"
state=$(cat "$s/state" 2>/dev/null || echo none)
alive() { [ -f "$s/pid" ] && kill -0 "$(cat "$s/pid")" 2>/dev/null; }
launch() {
    exe=$(sed -n 's|.*<array><string>\(.*\)</string></array>.*|\1|p' "$plist")
    sh -c 'echo $$ > "$1/pid"; exec "$2" >> "$1/log" 2>&1' sh "$s" "$exe" > /dev/null 2>&1 < /dev/null &
    for i in 1 2 3 4 5 6 7 8 9 10; do [ -f "$s/pid" ] && break; sleep 0.1; done
}
linger() { n=$(cat "$s/left"); if [ "$n" -le 1 ]; then echo none > "$s/state"; else echo $((n - 1)) > "$s/left"; fi; }
case "$1" in
    print)
        case $state in
            loaded) echo "state = $(alive && echo running || echo 'not running')"; alive && echo "pid = $(cat "$s/pid")"; exit 0 ;;
            unloading) linger; exit 0 ;;
            *) exit 113 ;;
        esac ;;
    print-disabled) [ -f "$s/disabled" ] && echo '	"com.clipsync" => disabled'; exit 0 ;;
    enable) rm -f "$s/disabled" ;;
    disable) touch "$s/disabled" ;;
    bootstrap)
        [ -f "$s/disabled" ] && { echo "Bootstrap failed: 119: Service is disabled" >&2; exit 119; }
        case $state in
            none) echo loaded > "$s/state"; launch ;;
            unloading) linger; echo "Bootstrap failed: 5: Input/output error" >&2; exit 5 ;;
            *) echo "Bootstrap failed: 5: Input/output error" >&2; exit 5 ;;
        esac ;;
    bootout)
        [ "$state" = loaded ] || exit 3
        alive && kill "$(cat "$s/pid")"; rm -f "$s/pid"
        echo unloading > "$s/state"; echo 2 > "$s/left" ;;
    kickstart) [ "$state" = loaded ] || exit 113; alive || launch ;;
    *) echo "fake launchctl: $*" >&2; exit 1 ;;
esac
"#;

/// A device whose background service is run by `manager` (systemd, launchd or none).
struct Setup {
    device: Device,
    manager: &'static str,
    mocks: PathBuf,
}

impl Setup {
    fn new(manager: &'static str) -> Setup {
        let device = Device::new(manager);
        let mocks = device.dir.join("mocks");
        fs::create_dir_all(mocks.join("bin")).unwrap();
        for (name, script) in [("systemctl", SYSTEMCTL), ("launchctl", LAUNCHCTL)] {
            let path = mocks.join("bin").join(name);
            fs::write(&path, script).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        Setup { device, manager, mocks }
    }

    fn run(&self, args: &[&str]) -> (bool, String) {
        let path = format!("{}:{}", self.mocks.join("bin").display(), std::env::var("PATH").unwrap());
        let out = self
            .device
            .cmd()
            .args(args)
            .env("CLIPSYNC_SERVICE_MANAGER", self.manager)
            .env("MOCK_DIR", &self.mocks)
            .env("HOME", self.device.dir.join("home"))
            .env_remove("XDG_CONFIG_HOME")
            .env("PATH", path)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
        (out.status.success(), text)
    }

    fn running(&self) -> Option<i32> {
        let lock = self.device.dir.join("config/daemon.lock");
        let pid: i32 = fs::read_to_string(lock).ok()?.trim().parse().ok()?;
        // SAFETY: signal 0 only checks the process exists.
        (unsafe { libc::kill(pid, 0) } == 0).then_some(pid)
    }

    fn expect(&self, args: &[&str], wanted: &str) -> String {
        let (ok, out) = self.run(args);
        assert!(ok && out.contains(wanted), "`clipsync {}` ({}): wanted {wanted:?}, got:\n{out}", args.join(" "), self.manager);
        out
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        let _ = self.run(&["stop"]);
        if let Some(pid) = self.running() {
            // SAFETY: plain kill(2).
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

fn start_stop_status(manager: &'static str) {
    let s = Setup::new(manager);
    s.expect(&["status"], "isn't running");
    s.expect(&["start"], "is running in the background");
    let pid = s.running().expect("daemon running after start");
    s.expect(&["status"], &format!("running in the background (process {pid})"));
    s.expect(&["start"], "already running in the background");
    assert_eq!(s.running(), Some(pid), "start again leaves it alone");

    // Quit by accident: `clipsync start` brings it back.
    // SAFETY: plain kill(2).
    unsafe { libc::kill(pid, libc::SIGTERM) };
    eventually("the daemon to quit", || s.running().is_none());
    s.expect(&["status"], "isn't running");
    s.expect(&["start"], "is running in the background");
    assert!(s.running().is_some());

    s.expect(&["stop"], "Stopped clipsync");
    assert!(s.running().is_none());
    s.expect(&["stop"], "wasn't running");
    s.expect(&["start"], "is running in the background");
    assert!(s.running().is_some());
}

#[test]
fn systemd() {
    start_stop_status("systemd");
    let s = Setup::new("systemd");
    s.expect(&["start"], "starts whenever you log in");
    s.expect(&["status"], "It starts when you log in.");
    let unit = fs::read_to_string(s.device.dir.join("home/.config/systemd/user/clipsync.service")).unwrap();
    assert!(unit.contains(&format!("ExecStart=\"{BIN}\"")), "{unit}");
    s.expect(&["stop"], "won't start when you log in");
    s.expect(&["status"], "doesn't start when you log in");
}

#[test]
fn launchd() {
    start_stop_status("launchd");
    let s = Setup::new("launchd");
    s.expect(&["start"], "starts whenever you log in");
    s.expect(&["status"], "It starts when you log in.");
    let plist = s.device.dir.join("home/Library/LaunchAgents/com.clipsync.plist");
    // A changed definition is reloaded, waiting out launchd's slow unload.
    let old = fs::read_to_string(&plist).unwrap();
    fs::write(&plist, old.replace("<integer>10</integer>", "<integer>11</integer>")).unwrap();
    s.expect(&["start"], "running in the background");
    assert_eq!(fs::read_to_string(&plist).unwrap(), old);
    assert!(s.running().is_some());
    s.expect(&["stop"], "won't start when you log in");
    s.expect(&["status"], "doesn't start when you log in");
}

#[test]
fn detached() {
    start_stop_status("none");
    let s = Setup::new("none");
    s.expect(&["start"], "won't start again when you log in");
    s.expect(&["status"], "Nothing starts it when you log in");
}

#[test]
fn a_clipsync_in_a_terminal_is_left_alone() {
    for manager in ["systemd", "launchd"] {
        let mut s = Setup::new(manager);
        s.device.start(); // like running `clipsync` in a terminal
        let pid = s.running().unwrap();
        s.expect(&["start"], "not in the background");
        assert_eq!(s.running(), Some(pid));
        s.expect(&["status"], "not in the background");
        s.expect(&["stop"], "Stopped clipsync");
        s.expect(&["status"], "isn't running");
        s.device.stop(); // reap it
    }
}

