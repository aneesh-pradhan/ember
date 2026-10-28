#!/bin/sh
# Install ember, a terminal coding agent powered by NVIDIA Nemotron on Nebius Token Factory.
#
#   curl -fsSL https://raw.githubusercontent.com/aneesh-pradhan/ember/main/install.sh | sh
#
# Environment:
#   EMBER_VERSION      release tag to install (default: latest), e.g. v0.1.1
#   EMBER_INSTALL_DIR  where to put the binary (default: ~/.local/bin)
set -eu

REPO="aneesh-pradhan/ember"
VERSION="${EMBER_VERSION:-latest}"
INSTALL_DIR="${EMBER_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$*"; }
die() { printf 'ember install: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || die "'$1' is required but not installed"; }

need curl
need tar
need uname

case "$(uname -s)" in
    Linux) os="unknown-linux-musl" ;;
    Darwin) os="apple-darwin" ;;
    *) die "unsupported OS $(uname -s); build from source with: cargo install --git https://github.com/$REPO" ;;
esac
case "$(uname -m)" in
    x86_64 | amd64) arch="x86_64" ;;
    aarch64 | arm64) arch="aarch64" ;;
    *) die "unsupported CPU $(uname -m); build from source with: cargo install --git https://github.com/$REPO" ;;
esac
target="$arch-$os"

if [ "$VERSION" = "latest" ]; then
    # Ask the API (always current); fall back to the latest-release redirect, which can lag behind.
    VERSION=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" 2>/dev/null |
        sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)
    if [ -z "$VERSION" ]; then
        url=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest") ||
            die "could not reach GitHub"
        VERSION="${url##*/}"
    fi
    [ -n "$VERSION" ] && [ "$VERSION" != "latest" ] || die "could not determine the latest release"
fi

asset="ember-$VERSION-$target.tar.gz"
base="https://github.com/$REPO/releases/download/$VERSION"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

say "Downloading ember $VERSION for $target..."
curl -fsSL "$base/$asset" -o "$tmp/$asset" || die "download failed: $base/$asset"
curl -fsSL "$base/$asset.sha256" -o "$tmp/$asset.sha256" || die "checksum download failed"

expected=$(cut -d ' ' -f 1 <"$tmp/$asset.sha256")
if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$tmp/$asset" | cut -d ' ' -f 1)
elif command -v shasum >/dev/null 2>&1; then
    actual=$(shasum -a 256 "$tmp/$asset" | cut -d ' ' -f 1)
else
    die "need sha256sum or shasum to verify the download"
fi
[ "$expected" = "$actual" ] || die "checksum mismatch for $asset (expected $expected, got $actual)"

tar -xzf "$tmp/$asset" -C "$tmp"
mkdir -p "$INSTALL_DIR"
install -m 755 "$tmp/ember-$VERSION-$target/ember" "$INSTALL_DIR/ember" 2>/dev/null ||
    { cp "$tmp/ember-$VERSION-$target/ember" "$INSTALL_DIR/ember" && chmod 755 "$INSTALL_DIR/ember"; }

say "Installed $("$INSTALL_DIR/ember" --version) to $INSTALL_DIR/ember"

case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
        say ""
        say "$INSTALL_DIR is not on your PATH yet. Add it with:"
        case "${SHELL:-}" in
            */zsh) say "  echo 'export PATH=\"$INSTALL_DIR:\$PATH\"' >> ~/.zshrc && source ~/.zshrc" ;;
            */fish) say "  fish_add_path $INSTALL_DIR" ;;
            *) say "  echo 'export PATH=\"$INSTALL_DIR:\$PATH\"' >> ~/.bashrc && source ~/.bashrc" ;;
        esac
        ;;
esac

config="${XDG_CONFIG_HOME:-$HOME/.config}/ember/env"
if [ ! -s "$config" ]; then
    say ""
    say "Next, add your Nebius Token Factory API key (https://tokenfactory.nebius.com/project/api-keys):"
    say "  mkdir -p \"$(dirname "$config")\" && printf 'NEBIUS_API_KEY=%s\\n' 'your-key' > \"$config\" && chmod 600 \"$config\""
fi
say ""
say "Then cd into a project and run: ember"
