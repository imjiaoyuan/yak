#!/bin/sh
# Portable installer/updater for llm: downloads the prebuilt release binary
# from GitHub into ~/.local/bin (user-level, no root needed), verifies its
# sha256 and offers a PATH update. Re-running it updates in place: it checks
# the latest GitHub release, prints `updating old -> new` when the installed
# binary is behind, and leaves an equal version alone.
#
#   curl -fsSL https://jiaoyuan.org/llm/install.sh | sh
#
# Environment overrides:
#   LLM_VERSION=v0.1.0        pin a release tag (default: latest)
#   LLM_REPO=imjiaoyuan/llm   install from a fork
#   LLM_INSTALL_DIR=DIR       install directory (default: ~/.local/bin)
#   LLM_FORCE=1               reinstall even when the version is unchanged
#   LLM_INSTALL_ALLOW_SUDO=1  run under sudo despite the guard below
set -eu

REPO="${LLM_REPO:-imjiaoyuan/llm}"
INSTALL_DIR="${LLM_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$1"; }
die() { printf 'error: %s\n' "$1" >&2; exit 1; }

# Refuse to run under sudo from a regular user's shell. This installer puts
# everything under $HOME, which under sudo typically resolves to root's home:
# the binary lands in /root/.local/bin (or is left root-owned) and `llm` is
# then not found in the user's own shell. Plain root with no sudo (containers,
# CI, root-only systems) is unaffected.
if [ "$(id -u)" -eq 0 ] && [ -n "${SUDO_USER:-}" ] && [ "${SUDO_USER}" != "root" ] && [ -z "${LLM_INSTALL_ALLOW_SUDO:-}" ]; then
    die "do not run this installer with sudo.
llm installs into your home directory and does not need root access. Re-run it
without sudo:
    curl -fsSL https://jiaoyuan.org/llm/install.sh | sh
To install for the root user anyway, set LLM_INSTALL_ALLOW_SUDO=1"
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

# Resolve the latest tag from the releases/latest redirect instead of the GitHub
# API, which is rate-limited to 60 req/hour unauthenticated.
latest_version() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSI --max-time 30 "https://github.com/$REPO/releases/latest" |
            sed -n 's/^[Ll]ocation:.*\/tag\///p' | tr -d '\r' | head -n 1
    elif command -v wget >/dev/null 2>&1; then
        wget --spider -S --max-redirect=0 "https://github.com/$REPO/releases/latest" 2>&1 |
            sed -n 's/^[[:space:]]*Location:.*\/tag\///p' | tr -d '\r' | head -n 1
    fi
}

# `llm --version` prints `llm, version X.Y.Z`; extract just the version for a
# clean `updating old -> new` line and an exact comparison.
ver_of() { "$1" --version 2>/dev/null | sed -n 's/^llm, version \(.*\)$/\1/p' | head -n 1; }

if [ -n "${LLM_VERSION:-}" ]; then
    VERSION="$LLM_VERSION"
else
    VERSION=$(latest_version)
    [ -n "$VERSION" ] || die "could not resolve the latest release (set LLM_VERSION=vX.Y.Z to pin)"
fi
VERSION_NUM="${VERSION#v}"

ARCHIVE="llm-$TARGET.tar.gz"
CHECKSUM="llm-$TARGET.sha256"
BASE="https://github.com/$REPO/releases/download/$VERSION"

# Already at the requested release? Skip the download entirely.
if [ -x "$INSTALL_DIR/llm" ]; then
    INSTALLED=$(ver_of "$INSTALL_DIR/llm" || printf 'unknown')
    if [ "$INSTALLED" = "$VERSION_NUM" ] && [ "${LLM_FORCE:-0}" != "1" ]; then
        say "==> already $VERSION_NUM at $INSTALL_DIR/llm (up to date; LLM_FORCE=1 reinstalls)"
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
chmod +x "$TMP/llm"
NEWVER=$(ver_of "$TMP/llm")
[ -n "$NEWVER" ] || die "downloaded binary did not report a version"

# update semantics: same version stays put unless LLM_FORCE=1
if [ -x "$INSTALL_DIR/llm" ]; then
    OLDVER=$(ver_of "$INSTALL_DIR/llm" || printf 'unknown')
    if [ "$OLDVER" = "$NEWVER" ] && [ "${LLM_FORCE:-0}" != "1" ]; then
        say "==> already $NEWVER at $INSTALL_DIR/llm (up to date; LLM_FORCE=1 reinstalls)"
        exit 0
    fi
    if [ "$OLDVER" != "unknown" ] && [ "$OLDVER" != "$NEWVER" ]; then
        say "==> updating $OLDVER -> $NEWVER"
    fi
fi

say "==> installing to $INSTALL_DIR"
mkdir -p "$INSTALL_DIR"
mv -f "$TMP/llm" "$INSTALL_DIR/llm"

case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
        # pick the startup file the current shell reads, defaulting to ~/.profile
        case "${SHELL:-}" in
            */bash) RC="$HOME/.bashrc" ;;
            */zsh)  RC="$HOME/.zshrc" ;;
            *)      RC="$HOME/.profile" ;;
        esac
        if ! grep -qs 'added by llm installer' "$RC" 2>/dev/null; then
            printf '\n# added by llm installer\nexport PATH="%s:$PATH"\n' "$INSTALL_DIR" >> "$RC"
            say "==> added $INSTALL_DIR to PATH in $RC (restart your shell or: export PATH=\"$INSTALL_DIR:\$PATH\")"
        fi
        ;;
esac

"$INSTALL_DIR/llm" --version
