#!/bin/sh
# Portable installer/updater for yak: downloads the prebuilt release binary
# from GitHub into ~/.local/bin (user-level, no root needed), verifies its
# sha256 and offers a PATH update. Re-running it updates in place: it checks
# the latest GitHub release, prints `updating old -> new` when the installed
# binary is behind, and leaves an equal version alone.
#
#   curl -fsSL https://jiaoyuan.org/yak/install.sh | sh
#
# Environment overrides:
#   YAK_VERSION=v0.1.0        pin a release tag (default: latest)
#   YAK_REPO=imjiaoyuan/yak   install from a fork
#   YAK_INSTALL_DIR=DIR       install directory (default: ~/.local/bin)
#   YAK_FORCE=1               reinstall even when the version is unchanged
#   YAK_INSTALL_ALLOW_SUDO=1  run under sudo despite the guard below
#   YAK_GH_PROXY=1            download via https://gh-proxy.com/ when GitHub is
#                             slow or unreachable, or set any prefix-style proxy
#                             URL as the value (0/false = direct, the default)
set -eu

REPO="${YAK_REPO:-imjiaoyuan/yak}"
INSTALL_DIR="${YAK_INSTALL_DIR:-$HOME/.local/bin}"

# Optional GitHub reverse proxy for hosts with unstable GitHub connectivity
# (e.g. mainland China). Prefix-style proxies take the full GitHub URL after
# them: YAK_GH_PROXY=1 picks the default https://gh-proxy.com/, any other
# non-empty value is used as the prefix itself (scheme added, trailing /
# normalized).
GH_PROXY="${YAK_GH_PROXY:-}"
case "$GH_PROXY" in
    ""|0|false) GH_PROXY="" ;;
    1|true|yes) GH_PROXY="https://gh-proxy.com/" ;;
    *://*) GH_PROXY="${GH_PROXY%/}/" ;;
    *) GH_PROXY="https://${GH_PROXY%/}/" ;;
esac
gh_url() { printf '%s%s\n' "$GH_PROXY" "$1"; }

say() { printf '%s\n' "$1"; }
die() { printf 'error: %s\n' "$1" >&2; exit 1; }

# Refuse to run under sudo from a regular user's shell. This installer puts
# everything under $HOME, which under sudo typically resolves to root's home:
# the binary lands in /root/.local/bin (or is left root-owned) and `yak` is
# then not found in the user's own shell. Plain root with no sudo (containers,
# CI, root-only systems) is unaffected.
if [ "$(id -u)" -eq 0 ] && [ -n "${SUDO_USER:-}" ] && [ "${SUDO_USER}" != "root" ] && [ -z "${YAK_INSTALL_ALLOW_SUDO:-}" ]; then
    die "do not run this installer with sudo.
yak installs into your home directory and does not need root access. Re-run it
without sudo:
    curl -fsSL https://jiaoyuan.org/yak/install.sh | sh
To install for the root user anyway, set YAK_INSTALL_ALLOW_SUDO=1"
fi

[ "$(uname -s)" = "Linux" ] && OS=linux
[ "$(uname -s)" = "Darwin" ] && OS=darwin
[ "${OS:-}" ] || die "unsupported OS $(uname -s) (this installer covers Linux and macOS; on Windows use install.ps1)"

case "$(uname -m)" in
    x86_64|amd64) ARCH=x86_64 ;;
    aarch64|arm64) ARCH=aarch64 ;;
    *) die "unsupported architecture $(uname -m)" ;;
esac

# linux uses the static musl builds: the same binary runs on any distribution
if [ "$OS" = "linux" ]; then
    TARGET="$ARCH-unknown-linux-musl"
else
    TARGET="$ARCH-apple-darwin"
fi

fetch() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "$1" -o "$2"
    elif command -v wget >/dev/null 2>&1; then
        wget -qO "$2" "$1"
    else
        die "need curl or wget to download"
    fi
}

# Resolve the latest tag. Direct connections use the releases/latest redirect
# instead of the GitHub API, which is rate-limited to 60 req/hour
# unauthenticated; behind a reverse proxy the redirect is not forwarded (the
# proxy answers 200 with the page itself), so the proxied path asks the API,
# which prefix-style proxies pass through without the rate limit.
latest_version() {
    if [ -n "$GH_PROXY" ]; then
        if command -v curl >/dev/null 2>&1; then
            curl -fsSL --max-time 30 "$(gh_url "https://api.github.com/repos/$REPO/releases/latest")"
        elif command -v wget >/dev/null 2>&1; then
            wget -qO- --timeout=30 "$(gh_url "https://api.github.com/repos/$REPO/releases/latest")"
        fi | sed -n 's/.*"tag_name":[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1
    elif command -v curl >/dev/null 2>&1; then
        curl -fsSI --max-time 30 "https://github.com/$REPO/releases/latest" |
            sed -n 's/^[Ll]ocation:.*\/tag\///p' | tr -d '\r' | head -n 1
    elif command -v wget >/dev/null 2>&1; then
        wget --spider -S --max-redirect=0 "https://github.com/$REPO/releases/latest" 2>&1 |
            sed -n 's/^[[:space:]]*Location:.*\/tag\///p' | tr -d '\r' | head -n 1
    fi
}

# `yak --version` prints `yak, version X.Y.Z`; extract just the version for a
# clean `updating old -> new` line and an exact comparison.
ver_of() { "$1" --version 2>/dev/null | sed -n 's/^yak, version \(.*\)$/\1/p' | head -n 1; }

if [ -n "${YAK_VERSION:-}" ]; then
    VERSION="$YAK_VERSION"
else
    VERSION=$(latest_version)
    [ -n "$VERSION" ] || die "could not resolve the latest release (set YAK_VERSION=vX.Y.Z to pin)"
fi
VERSION_NUM="${VERSION#v}"

ARCHIVE="yak-$TARGET.tar.gz"
CHECKSUM="yak-$TARGET.sha256"
BASE=$(gh_url "https://github.com/$REPO/releases/download/$VERSION")

# Already at the requested release? Skip the download entirely.
if [ -x "$INSTALL_DIR/yak" ]; then
    INSTALLED=$(ver_of "$INSTALL_DIR/yak" || printf 'unknown')
    if [ "$INSTALLED" = "$VERSION_NUM" ] && [ "${YAK_FORCE:-0}" != "1" ]; then
        say "==> already $VERSION_NUM at $INSTALL_DIR/yak (up to date; YAK_FORCE=1 reinstalls)"
        exit 0
    fi
fi

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

say "==> $REPO $VERSION ($TARGET)"
say "==> downloading $BASE/$ARCHIVE"
fetch "$BASE/$ARCHIVE" "$TMP/$ARCHIVE"
fetch "$BASE/$CHECKSUM" "$TMP/$CHECKSUM" || die "checksum file missing for $TARGET"

say "==> verifying sha256"
if command -v sha256sum >/dev/null 2>&1; then
    CHECK=sha256sum
else
    CHECK="shasum -a 256"
fi
(cd "$TMP" && $CHECK -c "$CHECKSUM" >/dev/null) || die "checksum mismatch — try again"

tar -xzf "$TMP/$ARCHIVE" -C "$TMP"
chmod +x "$TMP/yak"
NEWVER=$(ver_of "$TMP/yak")
[ -n "$NEWVER" ] || die "downloaded binary did not report a version"

# update semantics: same version stays put unless YAK_FORCE=1
if [ -x "$INSTALL_DIR/yak" ]; then
    OLDVER=$(ver_of "$INSTALL_DIR/yak" || printf 'unknown')
    if [ "$OLDVER" = "$NEWVER" ] && [ "${YAK_FORCE:-0}" != "1" ]; then
        say "==> already $NEWVER at $INSTALL_DIR/yak (up to date; YAK_FORCE=1 reinstalls)"
        exit 0
    fi
    if [ "$OLDVER" != "unknown" ] && [ "$OLDVER" != "$NEWVER" ]; then
        say "==> updating $OLDVER -> $NEWVER"
    fi
fi

say "==> installing to $INSTALL_DIR"
mkdir -p "$INSTALL_DIR"
mv -f "$TMP/yak" "$INSTALL_DIR/yak"

case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
        # pick the startup file the current shell reads, defaulting to ~/.profile
        case "${SHELL:-}" in
            */bash) RC="$HOME/.bashrc" ;;
            */zsh)  RC="$HOME/.zshrc" ;;
            *)      RC="$HOME/.profile" ;;
        esac
        if ! grep -qs 'added by yak installer' "$RC" 2>/dev/null; then
            printf '\n# added by yak installer\nexport PATH="%s:$PATH"\n' "$INSTALL_DIR" >> "$RC"
            say "==> added $INSTALL_DIR to PATH in $RC (restart your shell or: export PATH=\"$INSTALL_DIR:\$PATH\")"
        fi
        ;;
esac

"$INSTALL_DIR/yak" --version
