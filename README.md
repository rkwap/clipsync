# clipsync

Copy on one of your devices, paste on any of the others. clipsync shares what
you copy between Macs and Linux machines on the same network, from 2 devices
to 20.

- **Private by default.** Nothing syncs until you pair devices with a one-time
  code. Everyone else on the network, even hundreds of people on the same
  Wi-Fi, is ignored.
- **Survives network changes.** Devices are known by a cryptographic key, not
  an IP address, and find each other again on any network.
- **Text and images.** Each copy is sent to the other devices and lands on
  their clipboard as if it had been copied there, so clipboard-history apps
  record it too.
- **Light.** One ~1.5 MB binary, a few MB of memory, ~0% CPU when idle.
  Everything is end-to-end encrypted.

## Contents

- [Install](#install)
- [Pair your devices](#pair-your-devices)
- [Everyday use](#everyday-use)
- [Updating](#updating)
- [What gets synced](#what-gets-synced)
- [Troubleshooting](#troubleshooting)
- [Uninstall](#uninstall)
- [Development](#development)
- [How it works](#how-it-works)
- [License](#license)

## Install

On every device you want to share copies between, run:

```sh
curl -fsSL https://github.com/rkwap/clipsync/releases/latest/download/install.sh | sh
```

That downloads the latest release for your system, checks it against the
release's checksums, installs it as `~/.local/bin/clipsync`, and starts it in
the background (a systemd user service on Linux, a launchd agent on macOS). It
starts again whenever you log in. Then [pair your devices](#pair-your-devices).

Prebuilt binaries cover Linux (x86_64 and ARM64, any distribution) and macOS
(Apple Silicon and Intel). On anything else the installer builds from source,
which needs [Rust](https://rustup.rs).

**Linux, on Wayland:** also install `wl-clipboard`, e.g.
`sudo apt install wl-clipboard`, `sudo pacman -S wl-clipboard` or
`sudo dnf install wl-clipboard`. X11 needs nothing extra.

**macOS:** the first time clipsync runs, it may ask to **find devices on your
local network**. Allow it, or nothing can connect.

### Installer options

Pass options after `sh -s --`, e.g.
`curl -fsSL .../install.sh | sh -s -- --no-service`.

| Option | What it does |
|---|---|
| *(none)* | install the latest release and start the background service |
| `--version 0.2.0` | install that release instead |
| `--no-service` | install the binary only; start `clipsync` yourself |
| `--from-source` | build with `cargo` instead of downloading a binary |
| `--uninstall` | stop the service and remove clipsync |

To install somewhere other than `~/.local/bin`, set `PREFIX`, e.g.
`curl ... | PREFIX=/usr/local sh` (which may need `sudo`). If the install
folder isn't on your `PATH` (common on macOS), the installer says how to add it.

### Supported desktops

clipsync picks the clipboard backend by itself:

| Platform | Backend | Status |
|---|---|---|
| macOS | built in | untested on a real Mac (CI builds and tests the rest on macOS) |
| Linux, Wayland with data-control (KDE, sway, Hyprland, labwc, wayfire, river…) | Wayland, via `wl-clipboard` | tested |
| Linux, X11 with any window manager | X11, built in | tested |
| Linux, GNOME on Wayland | X11 through XWayland | untested |

GNOME doesn't let Wayland apps watch the clipboard, so clipsync uses XWayland
there, which relies on GNOME mirroring the clipboard to it. To force a backend,
run `clipsync run --backend wayland` (or `x11`).

### From source

With [Rust](https://rustup.rs) (and on macOS the Xcode command line tools,
`xcode-select --install`), from a checkout:

```sh
./install.sh --from-source     # build, install and start the service
cargo build --release          # or just build: target/release/clipsync
```

Or, without the background service: `cargo install --git https://github.com/rkwap/clipsync`.

## Pair your devices

clipsync does nothing until devices are paired into a **cluster**.

On a device that's already set up (the very first time, this creates the cluster):

```sh
clipsync invite
```

It shows a code:

```
Invite code:  37-482917   (valid for 10 minutes, for one device)

On the device you want to add, run:

    clipsync join 37-482917

If it can't find this device, add:  --at 192.168.1.20:40211
```

On the new device:

```sh
clipsync join 37-482917
```

Both start syncing within a few seconds. Repeat for each device; **any device
in the cluster can invite**. The two devices need to be on the same network
while pairing. After that, devices of a cluster find each other on any
network they share.

A code works for one device, expires after 10 minutes, and the invite is
cancelled after 3 wrong attempts, so it can't be guessed even on a busy network.

## Everyday use

From any device in the cluster:

```sh
clipsync devices           # list the devices, and which are connected
clipsync remove laptop     # remove a device, by name or the start of its id
clipsync leave             # take this device out of the cluster
clipsync --help            # all commands and options
```

```
3 devices in this cluster:

  workstation              9c6f555f  this device
  macbook                  ec455b7b  connected (192.168.1.20)
  old-laptop               a3874418  last seen 2 days ago (192.168.1.31)
```

A removed device is cut off right away by every device that knows about the
removal; devices that were offline learn about it the next time they connect.
A removed device can be invited again later.

### Running in the background

The installer starts clipsync in the background, and it starts again whenever
you log in. To manage that:

```sh
clipsync status     # is it running? does it start at login? how many devices are connected?
clipsync start      # start it in the background (again), now and at every login
clipsync stop       # stop it, and don't start it at login until `clipsync start`
```

If you quit it by accident, `clipsync start` brings it back. Running plain
`clipsync` in a terminal is different: that copy runs only until you press
Ctrl-C or close the terminal (`clipsync status` tells you which kind is
running).

In the background, clipsync is a systemd user service on Linux and a launchd
agent on macOS. On Linux without systemd, `clipsync start` runs it as a
detached process, which doesn't start again at login: add `clipsync start` to
your desktop's autostart. Its log:

| Linux (systemd) | macOS | Linux without systemd |
|---|---|---|
| `journalctl --user -u clipsync -f` | `tail -f ~/Library/Logs/clipsync.log` | `~/.config/clipsync/clipsync.log` |

## Updating

```sh
clipsync update            # install the latest release
clipsync update --check    # just check
```

Running the install command again does the same. Either way, clipsync
downloads the new release, verifies it, and replaces itself; a running
clipsync notices and restarts into the new version within seconds.

Once a day clipsync checks whether a new release is out (it never installs one
by itself) and mentions it in its log and in `clipsync devices`. To turn that
off, set `CLIPSYNC_NO_UPDATE_CHECK=1` in its environment, or run it with
`--no-update-check`.

**Update every device in a cluster.** Devices on different versions keep
syncing as long as the versions share a protocol. When a release changes the
protocol, devices that haven't updated can't connect, and both sides' logs and
`clipsync devices` say which device needs `clipsync update`:

```
  macbook                  ec455b7b  last seen 5 min ago (192.168.1.20), runs clipsync 0.3.0; update this device
```

## What gets synced

- **Every copy goes to every other device** in the cluster and becomes the
  newest item on its clipboard. That includes passwords copied from a password
  manager. A password manager that clears the clipboard after a while only
  clears it on the device you copied on.
- **Simultaneous copies:** if two devices copy at the same moment, all devices
  end up with the same one of the two.
- **Devices that were asleep or offline** get the most recent copy they missed
  when they're back (unless they copied something newer themselves). Older
  missed copies are skipped.
- **Restarts** don't re-send or re-apply anything. On X11, where the clipboard
  belongs to the app that copied, a clipboard emptied by restarting clipsync is
  restored from another device.
- **Copies made while clipsync isn't running** aren't sent later: at startup
  it can't tell a new copy from old clipboard contents.
- **Text and images**, up to 50 MB per copy. Images travel as PNG; on a Mac,
  they arrive as both PNG and TIFF so any app can paste them, and images that
  Mac apps copy as TIFF are converted. When a copy has both text and an image
  (spreadsheet cells, for example), the text is synced. Files aren't synced.
- Devices still on clipsync 0.1.0 keep syncing text with newer ones, but don't
  get images.
- Clipboard contents are never written to disk.

clipsync is meant to be left running. Measured recovery times:

| What happens | Recovery |
|---|---|
| Wi-Fi blips for a few seconds | nothing to do; the connection survives |
| A device gets a new IP (new DHCP lease, other access point) | ~2 s |
| The network drops for a while, then comes back | ~1 s after it's back |
| A laptop sleeps, or the network is down for minutes | within 30 s of waking |

## Troubleshooting

**Check the logs first** (see [Everyday use](#everyday-use)), and run
`clipsync devices` to see what each device can reach.

**`join` can't find the inviting device.** Both devices must be on the same
network. Use the `--at` address that `invite` printed:
`clipsync join 37-482917 --at 192.168.1.20:40211`.

**Devices are paired but don't connect.**

- Firewalls: clipsync uses UDP port 5353 (mDNS) to find devices, and TCP port
  7878 to connect (change it with `clipsync run --listen 0.0.0.0:PORT`).
  While `invite` runs, it also listens on a random TCP port.
- Some networks (guest Wi-Fi, hotels, many offices, phone hotspots with
  "client isolation") block traffic between devices. clipsync can't work there.
- Networks that only block mDNS are fine once devices have met: each device
  remembers where it last reached the others and shares those addresses.
- On macOS, check **System Settings → Privacy & Security → Local Network** and
  allow clipsync.

**clipsync isn't running in the background** (check with `clipsync status`):
run `clipsync start`. On macOS, updating from 0.2.0 or earlier could leave it
stopped (fixed in 0.2.1); those versions don't have `clipsync start` yet, so run
the install command again instead, which updates it and starts it.

**Linux: the service can't reach the clipboard** ("no usable clipboard" in
the log). The service needs your session's display. Most desktops pass it to
systemd automatically; if yours doesn't, add this to your session's startup:

```sh
systemctl --user import-environment DISPLAY WAYLAND_DISPLAY
```

**GNOME:** clipboard sync goes through XWayland, which is untested. If copies
from GNOME apps don't reach other devices, please report it.

**Starting over** on a device: `clipsync leave`, then pair it again. To reset
everything, stop clipsync and delete `~/.config/clipsync`.

## Uninstall

```sh
clipsync leave        # optional: tell the other devices this one is gone
curl -fsSL https://github.com/rkwap/clipsync/releases/latest/download/install.sh | sh -s -- --uninstall
rm -rf ~/.config/clipsync   # optional: forget this device's identity and cluster
```

## Development

```sh
cargo build
cargo test                      # unit tests, and end-to-end tests with real processes
cargo test -- --ignored         # also the system tests (see below)
```

The end-to-end tests in [`tests/cluster.rs`](tests/cluster.rs) and
[`tests/images.rs`](tests/images.rs) start real clipsync processes that pair,
sync text and images, remove and restart each other. They don't need a display
or a network: each test device uses a plain file as its clipboard (a file
starting with the PNG signature is an image), and mDNS is off. They run in
about 20 seconds. [`tests/service.rs`](tests/service.rs) checks `clipsync
start`, `stop` and `status` against stand-ins for systemd and launchd that
really start and stop clipsync.

The system tests in [`tests/system.rs`](tests/system.rs) exercise the real
backends, and skip themselves if what they need is missing:

| Test | Needs |
|---|---|
| `x11_backend` | `Xvfb`, and `python3` with `tkinter` |
| `wayland_backend` | `labwc` (run headless) and `wl-clipboard` |
| `devices_find_each_other_with_mdns` | multicast on the local network |
| `works_with_0_1_0` | internet access, to download the 0.1.0 release |

Environment variables, mainly for testing:

| Variable | Effect |
|---|---|
| `CLIPSYNC_DIR` | where config and state live (default `~/.config/clipsync`) |
| `CLIPSYNC_NAME` | this device's name (default: the host name) |
| `CLIPSYNC_CLIPBOARD_FILE` | use this file as the clipboard instead of the system one |
| `CLIPSYNC_RELEASES` | where to look for releases (default: this repository's GitHub releases) |
| `CLIPSYNC_NO_UPDATE_CHECK` | don't check for new releases |
| `CLIPSYNC_SERVICE_MANAGER` | `systemd`, `launchd` or `none`: how `clipsync start` runs it, instead of detecting that |

CI ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) runs clippy and
the tests on Linux and macOS for every push and pull request, plus the X11
system test on Linux.

### Releasing

1. Set the new version in `Cargo.toml` (and run `cargo build` to update
   `Cargo.lock`). If devices on the new version can't talk to older ones, also
   bump `PROTOCOL` in `src/net.rs`.
2. Commit and push, then release it under the tag `v` + that version, either
   with git:

   ```sh
   git tag v0.2.0
   git push origin main v0.2.0
   ```

   or by creating a release in GitHub's web interface with a new tag `v0.2.0`.
   The tag must match `Cargo.toml` exactly (`v0.2.0`, not `v0.2`).

[`.github/workflows/release.yml`](.github/workflows/release.yml) then builds
static Linux binaries (x86_64, ARM64) and macOS binaries (Apple Silicon,
Intel), and publishes them with their checksums and `install.sh` as a GitHub
release. `releases/latest/download/install.sh` always points at the newest
installer, and `clipsync update` finds the release through `releases/latest`.

## How it works

| File | Role |
|---|---|
| `src/main.rs` | commands, the daemon, picking up changes from commands and updates |
| `src/store.rs` | files on disk: identity, state |
| `src/cluster.rs` | the member list and how it merges |
| `src/pair.rs` | `invite` and `join` |
| `src/net.rs` | connections and the wire protocol |
| `src/hub.rs` | copy events, catch-up, membership changes, dialing |
| `src/discovery.rs` | mDNS |
| `src/update.rs` | `clipsync update` and the daily check for new releases |
| `src/clip/` | clipboard backends: Wayland, X11, macOS, file |

- **Identity and membership.** Each device has an X25519 key pair, and its
  public key is its id. The cluster is a list of member keys plus removed keys.
  Devices exchange the list on every connection and merge it (a union, and
  removals are permanent for a key), so changes spread without any device being
  in charge. Any member can add or remove devices.
- **Pairing.** The invite is advertised over mDNS by the code's public first
  part only. Both sides run SPAKE2 with the full code, which gives them a shared
  key only if the codes match, and leaks nothing an eavesdropper could use to
  test guesses offline. Over that key, the new device sends its public key and
  gets the member list.
- **Connections.** Noise XX (X25519, ChaCha20-Poly1305, BLAKE2s) with the
  device keys, with the cluster id mixed in, then a membership check. Every
  pair of devices keeps one connection: both dial, and if both connections
  succeed, both keep the one dialed by the lower device id. Pings every 5 s;
  a connection silent for 15 s is dead.
- **Copies.** A copy is an event: (hybrid logical clock, origin device,
  text or PNG image).
  The clock never runs backwards and always moves past anything the device has
  seen, so ordering follows cause and effect, and every device applies the same
  rule: newer than what I have, or ignore it. On connecting, devices say which
  copy they have; one that's behind asks a single other device for it.
- **Finding devices.** Devices advertise `_clipsync._tcp` with their key and a
  tag derived from the cluster id, and ignore other clusters. clipsync searches
  again every 30 s (mDNS on its own backs off to once an hour), reacts to its
  own and other devices' address changes immediately, and remembers and shares
  where it last reached each device.
- **Watching the clipboard.** On Linux, `wl-paste --watch` (Wayland) or the
  XFixes extension (X11) reports changes, so nothing runs while idle. macOS
  has no change notification, so clipsync checks the pasteboard's change
  counter, a cheap integer, every 250 ms.

## License

MIT; see [LICENSE](LICENSE).
