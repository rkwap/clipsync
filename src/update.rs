//! Updates. `clipsync update` downloads the latest release's installer and
//! runs it, the same way as the first install: it fetches the binary for this
//! platform, checks it against the release's checksums, and replaces this one.
//! A running clipsync notices its binary changed and restarts into the new one.
//!
//! The daemon also checks once a day whether a newer release exists, and says
//! so in its log and in `clipsync devices` (it never installs anything by
//! itself). Turn that off with `--no-update-check` or CLIPSYNC_NO_UPDATE_CHECK=1.
//!
//! Releases live at `<repository>/releases`; CLIPSYNC_RELEASES points elsewhere
//! (a mirror, or a test server).

use std::io;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use std::{env, fs, process};

use crate::hub::Hub;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn releases_url() -> String {
    env::var("CLIPSYNC_RELEASES").unwrap_or_else(|_| format!("{}/releases", env!("CARGO_PKG_REPOSITORY")))
}

/// Whether version `a` is newer than `b` ("1.10.0" > "1.9.2").
pub fn newer(a: &str, b: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.split(['-', '+']).next().unwrap_or("").split('.').map(|n| n.parse().unwrap_or(0)).collect()
    };
    parse(a) > parse(b)
}

fn curl(args: &[&str]) -> io::Result<String> {
    let out = Command::new("curl")
        .args(["-fsSL", "--max-time", "60"])
        .args(args)
        .output()
        .map_err(|e| io::Error::new(e.kind(), format!("running curl: {e}")))?;
    if !out.status.success() {
        let url = args.last().unwrap_or(&"");
        return Err(io::Error::other(format!("couldn't download {url}")));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The newest release: `releases/latest` redirects to `releases/tag/vX.Y.Z`.
pub fn latest_version() -> io::Result<String> {
    let url = format!("{}/latest", releases_url());
    let landed = curl(&["-o", "/dev/null", "-w", "%{url_effective}", &url])?;
    let tag = landed.trim().rsplit('/').next().unwrap_or("");
    let version = tag.strip_prefix('v').unwrap_or(tag);
    if version.is_empty() || !version.starts_with(|c: char| c.is_ascii_digit()) {
        return Err(io::Error::other(format!("no release found at {url}")));
    }
    Ok(version.to_string())
}

/// `clipsync update [--check]`.
pub fn update(check_only: bool) -> io::Result<()> {
    let latest = latest_version()?;
    if !newer(&latest, VERSION) {
        println!("clipsync {VERSION} is the latest version.");
        return Ok(());
    }
    if check_only {
        println!("clipsync {latest} is available (this is {VERSION}). Run `clipsync update` to install it.");
        return Ok(());
    }
    // Install where this binary lives: <prefix>/bin/clipsync.
    let exe = crate::current_exe()?;
    let prefix = exe
        .parent()
        .filter(|bin| bin.ends_with("bin"))
        .and_then(|bin| bin.parent())
        .ok_or_else(|| io::Error::other(format!("{} isn't in a bin directory; reinstall with the installer", exe.display())))?;

    println!("Updating clipsync {VERSION} to {latest}...");
    let script: PathBuf = env::temp_dir().join(format!("clipsync-install-{}.sh", process::id()));
    let script_url = format!("{}/download/v{latest}/install.sh", releases_url());
    curl(&["-o", script.to_str().unwrap(), &script_url])?;
    let status = Command::new("sh")
        .arg(&script)
        .args(["--version", &latest])
        .env("PREFIX", prefix)
        .env("CLIPSYNC_RELEASES", releases_url())
        .status();
    let _ = fs::remove_file(&script);
    match status? {
        s if s.success() => Ok(()),
        s => Err(io::Error::other(format!("the installer failed ({s})"))),
    }
}

/// In the daemon: check for a newer release now and then.
pub fn watch(hub: Arc<Hub>) {
    thread::spawn(move || {
        thread::sleep(Duration::from_secs(60));
        loop {
            if let Ok(latest) = latest_version()
                && newer(&latest, VERSION)
                && hub.set_update(&latest)
            {
                log!("clipsync {latest} is available (this is {VERSION}); run `clipsync update` to install it");
            }
            thread::sleep(Duration::from_secs(24 * 60 * 60));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions_numerically() {
        assert!(newer("0.2.0", "0.1.9"));
        assert!(newer("1.10.0", "1.9.3"));
        assert!(newer("1.0.0", "0.99.99"));
        assert!(!newer("0.1.0", "0.1.0"));
        assert!(!newer("0.1.0", "0.2.0"));
        assert!(!newer("1.0.0-beta.1", "1.0.0"));
    }
}
