#!/bin/sh
# Install or update clipsync: https://github.com/rkwap/clipsync
#
#   curl -fsSL https://github.com/rkwap/clipsync/releases/latest/download/install.sh | sh
#
# Options (when piping into sh, put them after `sh -s --`):
#   --version X.Y.Z   install that release instead of the latest
#   --no-service      don't set clipsync up to run in the background
#   --from-source     build with cargo instead of downloading a binary
#   --uninstall       stop clipsync and remove it
#
# Installs to ~/.local/bin; set PREFIX to change that (PREFIX=/usr/local
# installs to /usr/local/bin, which may need sudo). To update later, run
# `clipsync update`, or this again.

set -eu

REPO="rkwap/clipsync"
RELEASES="${CLIPSYNC_RELEASES:-https://github.com/$REPO/releases}"
PREFIX="${PREFIX:-$HOME/.local}"
BIN="$PREFIX/bin/clipsync"
OS="$(uname -s)"
VERSION=""
SERVICE=yes
FROM_SOURCE=no
ACTION=install

UNIT="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/clipsync.service"
PLIST="$HOME/Library/LaunchAgents/com.clipsync.plist"

say() { printf '%s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
    case "$1" in
        --version) [ $# -ge 2 ] || die "--version needs a version, like 0.2.0"; VERSION="${2#v}"; shift ;;
        --no-service) SERVICE=no ;;
        --from-source) FROM_SOURCE=yes ;;
        --uninstall) ACTION=uninstall ;;
        -h|--help) sed -n '2,17s/^# \{0,1\}//p' "$0" 2>/dev/null || say "See the comments at the top of install.sh."; exit 0 ;;
        *) die "unknown option: $1 (see --help)" ;;
    esac
    shift
done

case "$OS" in
    Linux|Darwin) ;;
    *) die "clipsync supports Linux and macOS, not $OS" ;;
esac

# ---- the background service ----
# `clipsync start` sets it up (a systemd user service on Linux, a launchd
# agent on macOS); this only cleans up when uninstalling.

have_systemd() {
    command -v systemctl >/dev/null 2>&1 && systemctl --user show-environment >/dev/null 2>&1
}

service_installed() {
    if [ "$OS" = Linux ]; then [ -f "$UNIT" ]; else [ -f "$PLIST" ]; fi
}

stop_service() {
    if [ -x "$BIN" ]; then "$BIN" stop >/dev/null 2>&1 || true; fi
    # Versions before 0.2.1 have no `clipsync stop`.
    if [ "$OS" = Linux ]; then
        if [ -f "$UNIT" ] && have_systemd; then
            systemctl --user disable --now clipsync.service >/dev/null 2>&1 || true
        fi
    else
        launchctl bootout "gui/$(id -u)/com.clipsync" 2>/dev/null || true
    fi
}

if [ "$ACTION" = uninstall ]; then
    stop_service
    rm -f "$UNIT" "$PLIST" "$BIN"
    if [ "$OS" = Linux ] && have_systemd; then systemctl --user daemon-reload; fi
    say "clipsync is uninstalled."
    say "This device's identity and cluster are still in ~/.config/clipsync. To forget them,"
    say "run \`clipsync leave\` before uninstalling, or delete that folder."
    exit 0
fi

# ---- get the binary ----

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# Which prebuilt binary fits this machine.
target() {
    arch="$(uname -m)"
    # A shell running under Rosetta reports x86_64 on Apple Silicon.
    if [ "$OS" = Darwin ] && [ "$(sysctl -n hw.optional.arm64 2>/dev/null || true)" = 1 ]; then arch=arm64; fi
    case "$arch" in
        x86_64|amd64) arch=x86_64 ;;
        arm64|aarch64) arch=aarch64 ;;
        *) return 1 ;;
    esac
    if [ "$OS" = Linux ]; then echo "$arch-unknown-linux-musl"; else echo "$arch-apple-darwin"; fi
}

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1
}

latest_version() {
    landed="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "$RELEASES/latest")" || return 1
    tag="${landed##*/}"
    case "${tag#v}" in [0-9]*) echo "${tag#v}" ;; *) return 1 ;; esac
}

# A checkout of this repository, when run as ./install.sh from one.
checkout() {
    case "$0" in */install.sh|install.sh) ;; *) return 1 ;; esac
    dir="$(cd "$(dirname "$0")" && pwd)"
    grep -q '^name = "clipsync"' "$dir/Cargo.toml" 2>/dev/null && echo "$dir"
}

command -v curl >/dev/null 2>&1 || die "curl is needed"
if [ "$FROM_SOURCE" = no ] && ! TARGET="$(target)"; then
    say "There's no prebuilt clipsync for $OS on $(uname -m); building it from source."
    FROM_SOURCE=yes
fi
if [ -z "$VERSION" ] && ! { [ "$FROM_SOURCE" = yes ] && checkout >/dev/null; }; then
    VERSION="$(latest_version)" || die "couldn't find the latest release at $RELEASES"
fi

if [ "$FROM_SOURCE" = no ]; then
    say "Downloading clipsync $VERSION for $TARGET..."
    archive="clipsync-$VERSION-$TARGET.tar.gz"
    curl -fsSL --retry 2 -o "$TMP/$archive" "$RELEASES/download/v$VERSION/$archive" \
        || die "couldn't download $archive (is $VERSION a release?)"
    curl -fsSL --retry 2 -o "$TMP/SHA256SUMS" "$RELEASES/download/v$VERSION/SHA256SUMS" \
        || die "couldn't download the release's checksums"
    expected="$(grep "  $archive\$" "$TMP/SHA256SUMS" | cut -d' ' -f1)"
    [ -n "$expected" ] || die "$archive isn't listed in the release's checksums"
    [ "$(sha256 "$TMP/$archive")" = "$expected" ] || die "$archive doesn't match its checksum; not installing it"
    tar -xzf "$TMP/$archive" -C "$TMP"
    NEW="$TMP/clipsync-$VERSION-$TARGET/clipsync"
else
    command -v cargo >/dev/null 2>&1 || die "building from source needs Rust; install it from https://rustup.rs"
    if [ "$OS" = Darwin ] && ! xcode-select -p >/dev/null 2>&1; then
        die "building from source needs the Xcode command line tools: xcode-select --install"
    fi
    if src="$(checkout)"; then
        say "Building clipsync from $src..."
    else
        say "Downloading clipsync $VERSION's source..."
        curl -fsSL -o "$TMP/src.tar.gz" "https://github.com/$REPO/archive/refs/tags/v$VERSION.tar.gz" \
            || die "couldn't download the source of $VERSION"
        tar -xzf "$TMP/src.tar.gz" -C "$TMP"
        src="$TMP/clipsync-$VERSION"
        say "Building it (this takes a minute)..."
    fi
    cargo build --release --locked --manifest-path "$src/Cargo.toml"
    NEW="$src/target/release/clipsync"
fi

# ---- install ----

PREVIOUS=""
if [ -x "$BIN" ]; then PREVIOUS="$("$BIN" --version 2>/dev/null | cut -d' ' -f2 || true)"; fi
mkdir -p "$PREFIX/bin"
cp "$NEW" "$BIN.new"
chmod 755 "$BIN.new"
mv -f "$BIN.new" "$BIN" # a running clipsync notices and restarts into the new one
INSTALLED="$("$BIN" --version | cut -d' ' -f2)"

if [ -n "$PREVIOUS" ]; then
    # An update: keep the existing setup.
    if [ "$SERVICE" = yes ] && service_installed; then "$BIN" start >/dev/null; fi
    if [ "$PREVIOUS" = "$INSTALLED" ]; then
        say "clipsync $INSTALLED was already installed; reinstalled it."
    else
        say "Updated clipsync from $PREVIOUS to $INSTALLED."
    fi
    say "Update every device in your cluster, so they all run the same version."
    exit 0
fi

say "Installed clipsync $INSTALLED to $BIN."
case ":$PATH:" in
    *":$PREFIX/bin:"*) ;;
    *) say "Note: $PREFIX/bin isn't on your PATH. Add it to run \`clipsync\` by name, e.g.:"
       say "    echo 'export PATH=\"$PREFIX/bin:\$PATH\"' >> ~/.$(basename "${SHELL:-sh}")rc" ;;
esac
if [ "$OS" = Linux ] && [ -n "${WAYLAND_DISPLAY:-}" ] && ! command -v wl-paste >/dev/null 2>&1; then
    say "Note: on Wayland, install wl-clipboard too (e.g. sudo apt install wl-clipboard);"
    say "without it clipsync falls back to X11, which doesn't work on every compositor."
fi

if [ "$SERVICE" = yes ]; then
    "$BIN" start
    if [ "$OS" = Linux ] && have_systemd && ! systemctl --user show-environment | grep -qE '^(WAYLAND_)?DISPLAY='; then
        say "Note: your session hasn't given systemd its display, so clipsync can't reach"
        say "the clipboard. Add this to your session's startup:"
        say "    systemctl --user import-environment DISPLAY WAYLAND_DISPLAY"
    fi
    if [ "$OS" = Darwin ]; then
        say "macOS may ask to let clipsync find devices on your local network: allow it."
    fi
    say "Run \`clipsync status\` any time to check on it."
fi

say ""
say "Next, pair your devices. On one that's set up (the first time, this creates"
say "your cluster), run \`clipsync invite\`; it shows a code. On the new device,"
say "run \`clipsync join <code>\`."
