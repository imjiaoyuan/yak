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
#   LLM_VERSION=v0.1.0     pin a release tag (default: latest)
#   LLM_REPO=imjiaoyuan/llm  install from a fork
#   LLM_INSTALL_DIR=DIR    install directory (default: ~/.local/bin)
#   LLM_FORCE=1            reinstall even when the version is unchanged
set -eu

REPO="${LLM_REPO:-imjiaoyuan/llm}"
INSTALL_DIR="${LLM_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$1"; }
die() { printf 'error: %s\n' "$1" >&2; exit 1; }

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

if [ -n "${LLM_VERSION:-}" ]; then
    VERSION="$LLM_VERSION"
else
    VERSION=$(fetch "https://api.github.com/repos/$REPO/releases/latest" - \
        | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)
    [ -n "$VERSION" ] || die "could not resolve the latest release (set LLM_VERSION=vX.Y.Z to pin)"
fi

ARCHIVE="llm-$TARGET.tar.gz"
CHECKSUM="llm-$TARGET.sha256"
BASE="https://github.com/$REPO/releases/download/$VERSION"

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
NEWVER=$("$TMP/llm" --version)

# update semantics: same version stays put unless LLM_FORCE=1
if [ -x "$INSTALL_DIR/llm" ]; then
    OLDVER=$("$INSTALL_DIR/llm" --version 2>/dev/null || printf 'unknown')
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
        RC="$HOME/.profile"
        [ -n "${ZSH_VERSION:-}" ] && RC="$HOME/.zshrc"
        if [ -f "$RC" ] || [ "${RC}" = "$HOME/.zshrc" ]; then
            if ! grep -qs 'added by llm installer' "$RC"; then
                printf '\n# added by llm installer\nexport PATH="%s:$PATH"\n' "$INSTALL_DIR" >> "$RC"
                say "==> added $INSTALL_DIR to PATH in $RC (restart your shell or: export PATH=\"$INSTALL_DIR:\$PATH\")"
            fi
        else
            say "==> add $INSTALL_DIR to your PATH to use llm"
        fi
        ;;
esac

"$INSTALL_DIR/llm" --version
