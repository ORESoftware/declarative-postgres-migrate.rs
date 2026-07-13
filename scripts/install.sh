#!/usr/bin/env bash
# dpm installer:
#   curl -fsSL https://raw.githubusercontent.com/declarative-migrations/declarative-postgres-migrate.rs/main/scripts/install.sh | bash
# Downloads the latest GitHub release binary for this OS/arch; falls back to
# `cargo install --git` when no prebuilt binary matches.
set -euo pipefail

REPO="declarative-migrations/declarative-postgres-migrate.rs"
BIN="dpm"
INSTALL_DIR="${DPM_INSTALL_DIR:-}"
if [ -z "$INSTALL_DIR" ]; then
  if [ -w /usr/local/bin ]; then INSTALL_DIR=/usr/local/bin; else INSTALL_DIR="$HOME/.local/bin"; fi
fi
mkdir -p "$INSTALL_DIR"

os=$(uname -s | tr '[:upper:]' '[:lower:]')
arch=$(uname -m)
case "$arch" in x86_64|amd64) arch=x86_64 ;; aarch64|arm64) arch=aarch64 ;; esac
case "$os" in darwin) target="${arch}-apple-darwin" ;; linux) target="${arch}-unknown-linux-gnu" ;; *) target="" ;; esac

tag=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -1)
asset="dpm-${tag}-${target}.tar.gz"
url="https://github.com/$REPO/releases/download/$tag/$asset"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

if [ -n "$target" ] && curl -fsSL -o "$tmp/$asset" "$url"; then
  echo "downloading $asset"
  tar -xzf "$tmp/$asset" -C "$tmp"
  # Verify checksum when published alongside the asset
  if curl -fsSL -o "$tmp/$asset.sha256" "$url.sha256" 2>/dev/null; then
    (cd "$tmp" && shasum -a 256 -c "$asset.sha256")
  fi
  install -m 0755 "$tmp/$BIN" "$INSTALL_DIR/$BIN"
else
  echo "no prebuilt binary for ${target:-$os/$arch}; building from source (requires cargo)"
  command -v cargo >/dev/null || { echo "cargo not found — install Rust from https://rustup.rs"; exit 1; }
  cargo install --git "https://github.com/$REPO" --root "$tmp/cargo" declarative-postgres-migrate
  install -m 0755 "$tmp/cargo/bin/$BIN" "$INSTALL_DIR/$BIN"
fi

echo "installed: $INSTALL_DIR/$BIN"
"$INSTALL_DIR/$BIN" version || true
case ":$PATH:" in *":$INSTALL_DIR:"*) ;; *) echo "NOTE: add $INSTALL_DIR to your PATH";; esac
