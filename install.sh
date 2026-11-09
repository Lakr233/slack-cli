#!/bin/sh
# Install slack-cli for the current user.
#
#   curl -fsSL https://raw.githubusercontent.com/Lakr233/slack-cli/main/install.sh | sh
#   ./install.sh                # from a checkout: builds it with cargo
#   ./install.sh --uninstall
#
# Puts one binary at $BIN_DIR/slack-cli (default ~/.local/bin). It downloads
# the latest release for your Mac, or builds from source when cargo is
# available and no release fits. Nothing else is touched; no sudo needed.
set -eu

REPO="Lakr233/slack-cli"
BIN_DIR="${BIN_DIR:-$HOME/.local/bin}"
TARGET_BIN="$BIN_DIR/slack-cli"
VERSION="${SLACK_CLI_VERSION:-latest}"

say() { printf '%s\n' "$*"; }
die() { printf 'slack-cli install: %s\n' "$*" >&2; exit 1; }

if [ "${1:-}" = "--uninstall" ]; then
    rm -f "$TARGET_BIN"
    say "Removed $TARGET_BIN"
    exit 0
fi

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$BIN_DIR"

# Run from a checkout (not piped): build what is here.
HERE=""
case "$0" in
    *install.sh) HERE=$(cd "$(dirname "$0")" && pwd) ;;
esac

build_from() {
    command -v cargo >/dev/null 2>&1 || return 1
    say "Building with cargo (this takes a minute)"
    cargo build --release --locked --manifest-path "$1/Cargo.toml" --target-dir "$TMP/target" >&2 \
        || die "cargo build failed."
    cp "$TMP/target/release/slack-cli" "$TMP/slack-cli"
}

download_release() {
    [ "$(uname -s)" = "Darwin" ] || return 1
    case "$(uname -m)" in
        arm64 | aarch64) target="aarch64-apple-darwin" ;;
        x86_64) target="x86_64-apple-darwin" ;;
        *) return 1 ;;
    esac
    if [ "$VERSION" = "latest" ]; then
        base="https://github.com/$REPO/releases/latest/download"
    else
        base="https://github.com/$REPO/releases/download/$VERSION"
    fi
    asset="slack-cli-$target.tar.gz"
    say "Downloading $asset"
    curl -fsSL -o "$TMP/$asset" "$base/$asset" || return 1
    curl -fsSL -o "$TMP/$asset.sha256" "$base/$asset.sha256" || return 1
    expected=$(cut -d ' ' -f 1 < "$TMP/$asset.sha256")
    actual=$(shasum -a 256 "$TMP/$asset" | cut -d ' ' -f 1)
    [ "$expected" = "$actual" ] || die "checksum mismatch for $asset."
    tar -xzf "$TMP/$asset" -C "$TMP" || die "could not unpack $asset."
    [ -f "$TMP/slack-cli" ] || die "$asset has no slack-cli binary."
}

if [ -n "$HERE" ] && [ -f "$HERE/Cargo.toml" ] && command -v cargo >/dev/null 2>&1; then
    build_from "$HERE"
elif ! download_release; then
    say "No prebuilt release for this machine; trying to build from source."
    command -v cargo >/dev/null 2>&1 \
        || die "install Rust (https://rustup.rs) and run this again."
    command -v git >/dev/null 2>&1 || die "git not found."
    git clone --quiet --depth 1 "https://github.com/$REPO.git" "$TMP/src" || die "git clone failed."
    build_from "$TMP/src"
fi

install -m 0755 "$TMP/slack-cli" "$TARGET_BIN"
say "Installed $("$TARGET_BIN" --version) to $TARGET_BIN"
case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) say "Add $BIN_DIR to your PATH, for example:"
       say "  echo 'export PATH=\"$BIN_DIR:\$PATH\"' >> ~/.zshrc" ;;
esac
say "Try: slack-cli teams"
