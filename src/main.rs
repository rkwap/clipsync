macro_rules! log {
    ($($arg:tt)*) => { eprintln!("[clipsync] {}", format_args!($($arg)*)) };
}

mod clip;
mod cluster;
mod discovery;
mod hub;
mod net;
mod pair;
mod service;
mod store;
mod update;

use std::ffi::CStr;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{self, Command};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};
use std::{env, fs, thread};

use hub::Hub;
use store::{DeviceId, hex};

const USAGE: &str = "\
clipsync - share copies between your devices on the same network

USAGE:
    clipsync [run] [OPTIONS]        run in this terminal (`clipsync start` for the background)
    clipsync invite                 add a device: shows a one-time code
    clipsync join CODE [--at ADDR]  join the cluster of the device that showed CODE
    clipsync devices                list the devices in this cluster
    clipsync remove DEVICE          remove a device (by name, or the start of its id)
    clipsync leave                  take this device out of its cluster
    clipsync start                  run clipsync in the background, now and at login
    clipsync stop                   stop it (and don't start it at login)
    clipsync status                 whether it's running, and the cluster at a glance
    clipsync update [--check]       install the latest version (or just check for one)

OPTIONS for run:
    --listen ADDR    accept other devices on ADDR [default: 0.0.0.0:7878]
    --no-discovery   don't use mDNS; only reach devices at their last-known addresses
    --backend NAME   Linux only: force the `wayland` or `x11` clipboard
    --no-update-check  don't check daily whether a new version is out

OTHER:
    -h, --help       show this help
    -V, --version    show the version

Nothing syncs until devices are paired: run `clipsync invite` on a device and
`clipsync join CODE` on the new one. The first invite creates the cluster.";

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// This device's name as others see it: $CLIPSYNC_NAME, or the short host
/// name, e.g. "laptop" (macOS reports "laptop.local").
fn hostname() -> String {
    if let Ok(name) = env::var("CLIPSYNC_NAME") {
        return name;
    }
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most buf.len() bytes into buf.
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
    let name = CStr::from_bytes_until_nul(&buf).ok().and_then(|c| c.to_str().ok()).filter(|_| ok);
    let short = name.and_then(|n| n.split('.').next()).unwrap_or_default();
    if short.is_empty() { "clipsync".into() } else { short.into() }
}

/// Write end of the self-pipe that the signal handler pokes.
static SIGNAL_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_signal(_: libc::c_int) {
    // SAFETY: write(2) is async-signal-safe; the fd stays open for the whole run.
    unsafe { libc::write(SIGNAL_FD.load(Ordering::Relaxed), [0u8].as_ptr().cast(), 1) };
}

/// Run `f` on a normal thread when we get Ctrl+C or SIGTERM, then exit.
fn on_shutdown(f: impl FnOnce() + Send + 'static) -> io::Result<()> {
    let (mut rx, tx) = UnixStream::pair()?;
    SIGNAL_FD.store(tx.as_raw_fd(), Ordering::Relaxed);
    std::mem::forget(tx); // keep the fd open
    let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    // SAFETY: the handler only calls write(2).
    unsafe {
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGTERM, handler);
    }
    thread::spawn(move || {
        let _ = io::Read::read(&mut rx, &mut [0u8]);
        f();
        process::exit(0);
    });
    Ok(())
}

/// This program's path. If the binary was replaced (updated or rebuilt)
/// while running, Linux reports the old one as "(deleted)"; this gives the new one.
pub fn current_exe() -> io::Result<PathBuf> {
    let p = env::current_exe()?;
    Ok(match p.to_string_lossy().strip_suffix(" (deleted)") {
        Some(s) => PathBuf::from(s),
        None => p,
    })
}

/// Start over with a fresh process: after joining, leaving, being removed, or
/// an update.
pub fn restart() -> ! {
    discovery::stop();
    clip::stop();
    match current_exe() {
        Ok(exe) => log!("can't restart: {}", Command::new(exe).args(env::args_os().skip(1)).exec()),
        Err(e) => log!("can't restart: {e}"),
    }
    process::exit(1)
}

struct RunOpts {
    listen: String,
    discovery: bool,
    backend: Option<String>,
    update_check: bool,
}

enum Cmd {
    Run(RunOpts),
    Invite,
    Join { code: String, at: Option<String> },
    Devices,
    Remove(String),
    Leave,
    Update { check: bool },
    Start,
    Stop,
    Status,
}

fn parse_args() -> Result<Cmd, String> {
    let mut args: Vec<String> = env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        process::exit(0);
    }
    if args.iter().any(|a| a == "-V" || a == "--version") {
        println!("clipsync {}", update::VERSION);
        process::exit(0);
    }
    let cmd = if args.first().is_some_and(|a| !a.starts_with('-')) { args.remove(0) } else { "run".into() };
    let mut args = args.into_iter();
    let mut positional = Vec::new();
    let mut run = RunOpts {
        listen: format!("0.0.0.0:{}", pair::DEFAULT_PORT),
        discovery: true,
        backend: None,
        update_check: env::var_os("CLIPSYNC_NO_UPDATE_CHECK").is_none(),
    };
    let mut at = None;
    let mut check = false;
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        match (cmd.as_str(), arg.as_str()) {
            ("run", "--listen") => run.listen = value()?,
            ("run", "--no-discovery") => run.discovery = false,
            ("run", "--backend") => run.backend = Some(value()?),
            ("run", "--no-update-check") => run.update_check = false,
            ("update", "--check") => check = true,
            ("join", "--at") => at = Some(value()?),
            (_, a) if !a.starts_with('-') => positional.push(arg),
            _ => return Err(format!("unknown option for `{cmd}`: {arg}")),
        }
    }
    let one = |what: &str| -> Result<String, String> {
        match positional.as_slice() {
            [x] => Ok(x.clone()),
            _ => Err(format!("`clipsync {cmd}` needs {what}")),
        }
    };
    let none = || if positional.is_empty() { Ok(()) } else { Err(format!("unexpected argument: {}", positional[0])) };
    Ok(match cmd.as_str() {
        "run" => none().map(|()| Cmd::Run(run))?,
        "invite" => none().map(|()| Cmd::Invite)?,
        "join" => Cmd::Join { code: one("a code, like 37-482917")?, at },
        "devices" => none().map(|()| Cmd::Devices)?,
        "remove" => Cmd::Remove(one("a device name or id")?),
        "leave" => none().map(|()| Cmd::Leave)?,
        "update" => none().map(|()| Cmd::Update { check })?,
        "start" => none().map(|()| Cmd::Start)?,
        "stop" => none().map(|()| Cmd::Stop)?,
        "status" => none().map(|()| Cmd::Status)?,
        _ => return Err(format!("unknown command: {cmd}")),
    })
}

fn main() {
    let cmd = parse_args().unwrap_or_else(|e| {
        eprintln!("error: {e}\n\n{USAGE}");
        process::exit(2);
    });
    // Commands that only print should end quietly when their output is cut
    // short (`clipsync status | head -1`), like other Unix tools. Not the ones
    // that use the network, where a closed connection must stay an error.
    if !matches!(cmd, Cmd::Run(_) | Cmd::Invite | Cmd::Join { .. }) {
        // SAFETY: restoring the default disposition before any threads start.
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    }
    let result = match cmd {
        Cmd::Run(opts) => run(opts),
        Cmd::Invite => pair::invite(),
        Cmd::Join { code, at } => pair::join(&code, at.as_deref()),
        Cmd::Devices => devices(),
        Cmd::Remove(target) => remove(&target),
        Cmd::Leave => leave(),
        Cmd::Update { check } => update::update(check),
        Cmd::Start => service::start(),
        Cmd::Stop => service::stop(),
        Cmd::Status => service::status(),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        process::exit(1);
    }
}

fn modified(name: &str) -> Option<(SystemTime, u64)> {
    let m = fs::metadata(store::path(name)).ok()?;
    Some((m.modified().ok()?, m.len()))
}

fn run(opts: RunOpts) -> io::Result<()> {
    let Some(mut running) = store::try_lock(store::DAEMON_LOCK)? else {
        return Err(io::Error::other("clipsync is already running on this device"));
    };
    running.set_len(0)?;
    write!(running, "{}", process::id())?;
    let me = store::identity()?;
    let cluster = match cluster::load()? {
        Some(c) if c.is_member(&me.public) => c,
        other => {
            if other.is_some() {
                // Removed while we weren't running.
                cluster::update(|c| {
                    *c = None;
                    Ok(())
                })?;
            }
            // So pairing can tell the other device which port we'll be on.
            let mut state = store::State::load();
            state.listen_port = opts.listen.parse::<std::net::SocketAddr>().ok().map(|a| a.port());
            state.save()?;
            log!("not in a cluster yet; nothing to sync.");
            log!("run `clipsync invite` here, or `clipsync join CODE` with a code from another device");
            let exe = exe_stamp();
            loop {
                thread::sleep(Duration::from_secs(2));
                if cluster::load().ok().flatten().is_some() || exe_stamp() != exe {
                    restart(); // paired, or updated
                }
            }
        }
    };

    // Before networking starts, so the clipboard is ready when a device sends text.
    clip::init(opts.backend.as_deref())?;
    let others = cluster.active().count() - 1;
    let hub = Arc::new(Hub::new(me, cluster, store::State::load()));
    let port =
        net::listen(&opts.listen, hub.clone()).map_err(|e| io::Error::new(e.kind(), format!("listening on {}: {e}", opts.listen)))?;
    hub.set_listen_port(port);
    hub.sync_contacts();
    for (id, addr) in store::take_hints() {
        hub.hint(&id, addr);
    }
    log!("syncing with {others} other device(s) in this cluster");
    if opts.discovery
        && let Err(e) = discovery::start(port, hub.clone())
    {
        log!("mDNS discovery unavailable ({e}); using last-known addresses only");
    }
    if let Err(e) = on_shutdown(discovery::stop) {
        log!("can't handle signals ({e}); other devices will notice we left only on timeout");
    }
    watch_files(hub.clone());
    if opts.update_check {
        update::watch(hub.clone());
    }

    // Watch the clipboard on the main thread; this only returns on fatal errors.
    clip::watch(|text, initial| hub.local_change(text, initial))
        .map_err(|e| io::Error::new(e.kind(), format!("watching the clipboard: {e}")))
}

fn exe_stamp() -> Option<(u64, SystemTime)> {
    let m = fs::metadata(current_exe().ok()?).ok()?;
    Some((m.ino(), m.modified().ok()?))
}

/// Pick up changes made by `invite`, `join`, `remove` and `leave`, and updates.
fn watch_files(hub: Arc<Hub>) {
    thread::spawn(move || {
        let identity = modified("identity");
        let mut cluster = modified("cluster");
        let exe = exe_stamp();
        loop {
            thread::sleep(Duration::from_secs(2));
            if modified("identity") != identity {
                restart(); // joined another cluster
            }
            if exe.is_some() && exe_stamp().is_some_and(|now| Some(now) != exe) {
                log!("clipsync was updated; restarting");
                restart();
            }
            let now = modified("cluster");
            if now != cluster {
                cluster = now;
                hub.reload();
            }
            if store::path("hints").exists() {
                for (id, addr) in store::take_hints() {
                    hub.hint(&id, addr);
                }
            }
        }
    });
}

fn ago(ms: u64) -> String {
    let s = hub::now_ms().saturating_sub(ms) / 1000;
    match s {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", s / 60),
        3600..86400 => format!("{} h ago", s / 3600),
        _ => format!("{} days ago", s / 86400),
    }
}

fn devices() -> io::Result<()> {
    let me = store::load_identity()?.map(|i| i.public);
    let Some(c) = cluster::load()? else {
        println!("This device isn't in a cluster.");
        println!("Run `clipsync invite` to start one, or `clipsync join CODE` to join one.");
        return Ok(());
    };
    let state = store::State::load();
    let running = store::daemon_running();
    let n = c.active().count();
    println!("{n} {} in this cluster:\n", if n == 1 { "device" } else { "devices" });
    for (id, m) in c.active() {
        let p = state.peers.get(id).cloned().unwrap_or_default();
        let at = p.addr.map(|a| format!(" ({})", a.ip())).unwrap_or_default();
        let mut status = if Some(*id) == me {
            "this device".to_string()
        } else if running && p.online {
            format!("connected{at}")
        } else if p.last_seen > 0 {
            format!("last seen {}{at}", ago(p.last_seen))
        } else {
            "not seen yet".to_string()
        };
        // Versions only matter when they differ.
        if let Some(v) = p.version.filter(|v| v != update::VERSION) {
            let fix = if update::newer(update::VERSION, &v) { "; update it" } else { "; update this device" };
            status += &format!(", runs clipsync {v}{fix}");
        }
        println!("  {:<24} {}  {status}", m.name, &hex(id)[..8]);
    }
    if !running {
        println!("\nclipsync isn't running on this device.");
    }
    if let Some(v) = state.update.filter(|v| update::newer(v, update::VERSION)) {
        println!("\nclipsync {v} is available (this is {}): run `clipsync update`.", update::VERSION);
    }
    Ok(())
}

fn remove(target: &str) -> io::Result<()> {
    let me = store::identity()?.public;
    let name = cluster::update(|c| {
        let c = c.as_mut().ok_or_else(|| io::Error::other("this device isn't in a cluster"))?;
        let t = target.to_lowercase();
        let matches: Vec<(DeviceId, String)> = c
            .active()
            .filter(|(id, m)| m.name.to_lowercase() == t || (t.len() >= 4 && hex(*id).starts_with(&t)))
            .map(|(id, m)| (*id, m.name.clone()))
            .collect();
        match matches.as_slice() {
            [] => Err(io::Error::other(format!("no device called {target:?}; see `clipsync devices`"))),
            [(id, _)] if *id == me => Err(io::Error::other("that's this device; use `clipsync leave`")),
            [(id, name)] => {
                c.remove(*id);
                Ok(name.clone())
            }
            _ => Err(io::Error::other(format!(
                "{} devices match {target:?}; use the start of the id shown by `clipsync devices`",
                matches.len()
            ))),
        }
    })?;
    println!("Removed {name}.");
    if store::daemon_running() {
        println!("The other devices will drop it within seconds.");
    } else {
        println!("The other devices will drop it once clipsync runs here again.");
    }
    Ok(())
}

fn leave() -> io::Result<()> {
    let me = store::identity()?.public;
    if cluster::load()?.is_none() {
        println!("This device isn't in a cluster.");
        return Ok(());
    }
    if store::daemon_running() {
        // Mark ourselves removed; the daemon tells the others, then forgets the cluster.
        cluster::update(|c| {
            if let Some(c) = c {
                c.remove(me);
            }
            Ok(())
        })?;
        for _ in 0..50 {
            thread::sleep(Duration::from_millis(100));
            if cluster::load()?.is_none() {
                println!("Left the cluster.");
                return Ok(());
            }
        }
    }
    cluster::update(|c| {
        *c = None;
        Ok(())
    })?;
    store::delete("state")?;
    println!("Left the cluster. The other devices still list this one until it's removed there");
    println!("with `clipsync remove {}`.", hostname());
    Ok(())
}
