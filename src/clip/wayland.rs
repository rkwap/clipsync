//! Wayland, via wl-clipboard. `wl-paste --watch` is event-driven (it uses
//! the compositor's data-control protocol), so idle cost is zero.

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use super::{Content, is_png};

/// Whether wl-paste is installed and the compositor supports data-control.
/// Without data-control (e.g. GNOME), `wl-paste --watch` exits right away.
pub fn supported() -> bool {
    let Ok(mut child) = Command::new("wl-paste")
        .args(["--watch", "true"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        if !matches!(child.try_wait(), Ok(None)) {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    true
}

/// The running `wl-paste --watch`, so `stop` can end it.
static WATCHER: AtomicI32 = AtomicI32::new(0);

pub fn stop() {
    let pid = WATCHER.swap(0, Ordering::Relaxed);
    if pid > 0 {
        // SAFETY: plain kill(2) on our own child.
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
}

pub fn set(content: &Content) -> io::Result<()> {
    let mut cmd = Command::new("wl-copy");
    let bytes: &[u8] = match content {
        Content::Text(t) => t.as_bytes(),
        Content::Image(png) => {
            cmd.args(["--type", "image/png"]);
            png
        }
    };
    let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::null()).spawn()?;
    // Dropping stdin after the write closes the pipe so wl-copy sees EOF.
    child.stdin.take().unwrap().write_all(bytes)?;
    let status = child.wait()?;
    if !status.success() {
        return Err(io::Error::other(format!("wl-copy exited with {status}")));
    }
    Ok(())
}

fn paste(args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new("wl-paste").args(args).stderr(Stdio::null()).output().ok()?;
    // Fails on an empty clipboard.
    out.status.success().then_some(out.stdout)
}

/// The clipboard's text if it offers any, otherwise its PNG image.
fn get() -> Option<Content> {
    let types = String::from_utf8(paste(&["--list-types"])?).ok()?;
    let is_text = |t: &str| t.starts_with("text/plain") || matches!(t, "UTF8_STRING" | "STRING" | "TEXT");
    if types.lines().any(is_text) {
        let text = String::from_utf8(paste(&["--no-newline", "--type", "text"])?).ok()?;
        return Some(Content::Text(text.into()));
    }
    if types.lines().any(|t| t == "image/png") {
        let png = paste(&["--no-newline", "--type", "image/png"]).filter(|b| is_png(b))?;
        return Some(Content::Image(png.into()));
    }
    None
}

pub fn watch(mut on_change: impl FnMut(Content, bool)) -> io::Result<()> {
    let mut quick_exits = 0;
    loop {
        let started = Instant::now();
        // wl-paste also reports the current contents once at startup; that isn't a change.
        let mut startup = get();
        on_change(startup.clone().unwrap_or(Content::Text("".into())), true);
        // wl-paste runs `echo` on every clipboard change, giving us one line per change.
        let mut cmd = Command::new("wl-paste");
        cmd.args(["--watch", "echo"]).stdout(Stdio::piped());
        // Have the kernel kill wl-paste if we die, even from SIGKILL; otherwise it
        // would outlive us. (The death signal fires when the spawning thread exits,
        // and this runs on the main thread.)
        // SAFETY: prctl is async-signal-safe, as pre_exec requires.
        unsafe {
            cmd.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| io::Error::new(e.kind(), format!("running wl-paste (is wl-clipboard installed?): {e}")))?;
        WATCHER.store(child.id() as i32, Ordering::Relaxed);
        for line in BufReader::new(child.stdout.take().unwrap()).lines() {
            line?;
            let content = get();
            if startup.take().is_some_and(|s| content.as_ref() == Some(&s)) {
                continue;
            }
            if let Some(content) = content {
                on_change(content, false);
            }
        }
        let status = child.wait()?;
        if started.elapsed() < Duration::from_secs(2) {
            quick_exits += 1;
            if quick_exits >= 5 {
                return Err(io::Error::other(format!(
                    "wl-paste --watch keeps exiting ({status}); does your compositor support data-control?"
                )));
            }
        } else {
            quick_exits = 0;
        }
        log!("wl-paste exited ({status}), restarting");
        thread::sleep(Duration::from_secs(1));
    }
}
