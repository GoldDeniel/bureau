#!/usr/bin/env bash
# Build the native Arch/CachyOS package (needs makepkg, cargo, rust).
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"

[ -x "$ROOT/target/release/bureau" ] || {
  echo "build the release binary first: cargo build --release" >&2
  exit 1
}

export PATH="$HOME/.cargo/bin:$PATH"
mkdir -p "$ROOT/dist"
cd "$HERE"
PKGDEST="$ROOT/dist" makepkg -f --nodeps --noconfirm
