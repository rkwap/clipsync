//! Running clipsync in the background: `clipsync start`, `stop` and `status`.
//!
//! On macOS that's a launchd agent; on Linux a systemd user service, or where
//! there's no systemd user session, a detached process (which doesn't start
//! again at login). `start` writes the agent or service definition, pointing
//! at this binary, so the installer uses it too.
//!
//! CLIPSYNC_SERVICE_MANAGER=systemd|launchd|none picks the mechanism instead
//! of detecting it (for tests).

use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::Duration;
use std::env;

use crate::{cluster, store, update};

const UNIT: &str = "clipsync.service";
const LABEL: &str = "com.clipsync";

#[derive(Clone, Copy, PartialEq)]
enum Manager {
    Systemd,
    Launchd,
    /// No service manager: a process in its own session.
    Detached,
}

fn manager() -> Manager {
    match env::var("CLIPSYNC_SERVICE_MANAGER").as_deref() {
        Ok("systemd") => Manager::Systemd,
        Ok("launchd") => Manager::Launchd,
        Ok("none") => Manager::Detached,
        _ if cfg!(target_os = "macos") => Manager::Launchd,
        _ if succeeds(systemctl(&["show-environment"])) => Manager::Systemd,
        _ => Manager::Detached,
    }
}

fn home() -> PathBuf {
    PathBuf::from(env::var_os("HOME").unwrap_or_default())
}

fn unit_path() -> PathBuf {
    let config = env::var_os("XDG_CONFIG_HOME").map_or_else(|| home().join(".config"), PathBuf::from);
    config.join("systemd/user").join(UNIT)
}

fn plist_path() -> PathBuf {
    home().join("Library/LaunchAgents").join(format!("{LABEL}.plist"))
}

fn launchd_log() -> PathBuf {
    home().join("Library/Logs/clipsync.log")
}

fn detached_log() -> PathBuf {
    store::path("clipsync.log")
}

fn run(program: &str, args: &[&str]) -> io::Result<Output> {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| io::Error::new(e.kind(), format!("running {program}: {e}")))
}

fn systemctl(args: &[&str]) -> io::Result<Output> {
    let mut all = vec!["--user"];
    all.extend_from_slice(args);
    run("systemctl", &all)
}

fn launchctl(args: &[&str]) -> io::Result<Output> {
    run("launchctl", args)
}

fn succeeds(out: io::Result<Output>) -> bool {
    out.is_ok_and(|o| o.status.success())
}

fn check(out: io::Result<Output>, what: &str) -> io::Result<()> {
    let out = out?;
    if out.status.success() {
        return Ok(());
    }
    let why = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Err(io::Error::other(format!("{what} failed: {why}")))
}

/// Poll `cond` for up to 10 s.
fn wait_for(cond: impl Fn() -> bool) -> bool {
    for _ in 0..50 {
        if cond() {
            return true;
        }
        thread::sleep(Duration::from_millis(200));
    }
    cond()
}

/// Write `contents` unless the file already holds exactly that; returns whether it changed.
fn write_if_changed(path: &Path, contents: &str) -> io::Result<bool> {
    if fs::read_to_string(path).is_ok_and(|c| c == contents) {
        return Ok(false);
    }
    fs::create_dir_all(path.parent().unwrap())?;
    fs::write(path, contents)?;
    Ok(true)
}

fn agent() -> (String, String) {
    // SAFETY: getuid can't fail.
    let domain = format!("gui/{}", unsafe { libc::getuid() });
    let agent = format!("{domain}/{LABEL}");
    (domain, agent)
}

/// Whether the running clipsync is the background one (rather than, say, one
/// started in a terminal).
fn in_background(m: Manager, pid: u32) -> bool {
    match m {
        Manager::Systemd => succeeds(systemctl(&["is-active", "--quiet", UNIT])),
        Manager::Launchd => launchctl(&["print", &agent().1])
            .is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains(&format!("pid = {pid}"))),
        // Detached processes have no controlling terminal (field 7 of /proc/<pid>/stat).
        Manager::Detached => fs::read_to_string(format!("/proc/{pid}/stat"))
            .is_ok_and(|s| s.rsplit(')').next().and_then(|rest| rest.split_whitespace().nth(4)) == Some("0")),
    }
}

/// `clipsync start`.
pub fn start() -> io::Result<()> {
    let exe = crate::current_exe()?;
    let m = manager();
    if let Some(pid) = store::daemon_pid()
        && !in_background(m, pid)
    {
        println!("clipsync is already running (process {pid}), but not in the background: it stops when its terminal closes.");
        println!("Quit that one (Ctrl-C in its terminal), then run `clipsync start` again.");
        return Ok(());
    }
    let was_running = store::daemon_running();
    match m {
        Manager::Systemd => start_systemd(&exe)?,
        Manager::Launchd => start_launchd(&exe)?,
        Manager::Detached if !was_running => start_detached(&exe)?,
        Manager::Detached => {}
    }
    if !wait_for(store::daemon_running) {
        let log = match m {
            Manager::Systemd => "journalctl --user -u clipsync".to_string(),
            Manager::Launchd => launchd_log().display().to_string(),
            Manager::Detached => detached_log().display().to_string(),
        };
        return Err(io::Error::other(format!("clipsync didn't start; see {log}")));
    }
    if was_running {
        println!("clipsync is already running in the background.");
    } else {
        println!("clipsync is running in the background.");
    }
    match m {
        Manager::Detached => {
            println!("It won't start again when you log in (there's no systemd here): add `clipsync start` to your");
            println!("desktop's autostart for that.");
        }
        _ => println!("It also starts whenever you log in. `clipsync stop` stops it."),
    }
    Ok(())
}

fn start_systemd(exe: &Path) -> io::Result<()> {
    // systemd unescapes C-style quoting, and expands % specifiers.
    let quoted = exe.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%");
    let unit = format!(
        "# clipsync's systemd user service, written by `clipsync start`.
#
# Wanted by default.target rather than graphical-session.target, which many
# desktops (XFCE, labwc, sway without extra setup) never start. If the session
# hasn't passed its display to systemd yet, clipsync exits and is retried.

[Unit]
Description=clipsync: share copies between your devices
After=graphical-session.target

[Service]
ExecStart=\"{quoted}\"
Restart=on-failure
RestartSec=10

[Install]
WantedBy=default.target
"
    );
    let changed = write_if_changed(&unit_path(), &unit)?;
    if changed {
        check(systemctl(&["daemon-reload"]), "systemctl daemon-reload")?;
    }
    check(systemctl(&["enable", UNIT]), "enabling the clipsync service")?;
    let verb = if changed && succeeds(systemctl(&["is-active", "--quiet", UNIT])) { "restart" } else { "start" };
    check(systemctl(&[verb, UNIT]), "starting the clipsync service")
}

fn start_launchd(exe: &Path) -> io::Result<()> {
    let escape = |p: &Path| p.to_string_lossy().replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let log = launchd_log();
    fs::create_dir_all(log.parent().unwrap())?;
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!-- clipsync's launchd agent, written by `clipsync start`. -->
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array><string>{exe}</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
        exe = escape(exe),
        log = escape(&log),
    );
    let path = plist_path();
    let changed = write_if_changed(&path, &plist)?;
    let (domain, agent) = agent();
    let loaded = || succeeds(launchctl(&["print", &agent]));
    // Undo a `clipsync stop`, which keeps it from starting at login.
    let _ = launchctl(&["enable", &agent]);
    if changed && loaded() {
        // Reload the new definition. bootout returns before launchd has
        // finished removing the agent; wait for that before loading it again.
        let _ = launchctl(&["bootout", &agent]);
        wait_for(|| !loaded());
    }
    if loaded() {
        // Loaded but not running, e.g. after it was quit: (re)start it.
        if !store::daemon_running() {
            check(launchctl(&["kickstart", &agent]), "launchctl kickstart")?;
        }
        return Ok(());
    }
    // Loading right after an unload can still fail while launchd is busy
    // ("Bootstrap failed: 5: Input/output error"); retry briefly.
    let path = path.to_string_lossy().into_owned();
    for _ in 0..5 {
        if succeeds(launchctl(&["bootstrap", &domain, &path])) {
            return Ok(());
        }
        thread::sleep(Duration::from_secs(1));
    }
    check(launchctl(&["bootstrap", &domain, &path]), "launchctl bootstrap")
}

fn start_detached(exe: &Path) -> io::Result<()> {
    let log = OpenOptions::new().create(true).append(true).open(detached_log())?;
    let mut cmd = Command::new(exe);
    cmd.arg("run").stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
    // SAFETY: setsid is async-signal-safe. Its own session: closing the
    // terminal this was started from doesn't stop it.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    // Not waited for: it outlives this command, and init adopts it.
    #[allow(clippy::zombie_processes)]
    cmd.spawn()?;
    Ok(())
}

/// `clipsync stop`.
pub fn stop() -> io::Result<()> {
    let m = manager();
    let was_running = store::daemon_running();
    match m {
        Manager::Systemd if unit_path().exists() => {
            let _ = systemctl(&["disable", "--now", UNIT]);
        }
        Manager::Launchd => {
            let agent = agent().1;
            // Persists: it won't load at login until `clipsync start`.
            let _ = launchctl(&["disable", &agent]);
            if succeeds(launchctl(&["print", &agent])) {
                let _ = launchctl(&["bootout", &agent]);
                wait_for(|| !succeeds(launchctl(&["print", &agent])));
            }
        }
        _ => {}
    }
    // Anything still running (started by hand, or detached): ask it to quit.
    if let Some(pid) = store::daemon_pid() {
        // SAFETY: plain kill(2).
        unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        if !wait_for(|| !store::daemon_running()) {
            return Err(io::Error::other(format!("clipsync (process {pid}) is still running")));
        }
    }
    if !was_running {
        println!("clipsync wasn't running.");
    } else if m == Manager::Detached {
        println!("Stopped clipsync.");
    } else {
        println!("Stopped clipsync. It won't start when you log in until you run `clipsync start`.");
    }
    Ok(())
}

/// `clipsync status`.
pub fn status() -> io::Result<()> {
    let m = manager();
    let version = update::VERSION;
    match store::daemon_pid() {
        Some(pid) if in_background(m, pid) => println!("clipsync {version} is running in the background (process {pid})."),
        Some(pid) => {
            println!("clipsync is running (process {pid}), but not in the background: it stops when its terminal closes.");
            println!("To keep it running, quit it (Ctrl-C) and run `clipsync start`.");
        }
        None => println!("clipsync {version} isn't running. Start it with `clipsync start`."),
    }
    let at_login = match m {
        Manager::Systemd => Some(succeeds(systemctl(&["is-enabled", "--quiet", UNIT]))),
        Manager::Launchd => {
            let disabled = launchctl(&["print-disabled", &agent().0]).is_ok_and(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .any(|l| l.contains(&format!("\"{LABEL}\"")) && (l.contains("disabled") || l.contains("true")))
            });
            Some(plist_path().exists() && !disabled)
        }
        Manager::Detached => None,
    };
    match at_login {
        Some(true) => println!("It starts when you log in."),
        Some(false) => println!("It doesn't start when you log in; `clipsync start` sets that up."),
        None => println!("Nothing starts it when you log in (there's no systemd here)."),
    }
    match cluster::load()? {
        Some(c) => {
            let n = c.active().count();
            let state = store::State::load();
            let online = if store::daemon_running() { state.peers.values().filter(|p| p.online).count() } else { 0 };
            println!("In a cluster of {n} devices, {online} of the others connected. See `clipsync devices`.");
        }
        None => println!("Not paired with any device yet: run `clipsync invite`, or `clipsync join CODE`."),
    }
    Ok(())
}
