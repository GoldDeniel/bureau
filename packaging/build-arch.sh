#!/usr/bin/env bash
# Build the native Arch/CachyOS package (needs makepkg, cargo, rust).
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"

[ -x "$ROOT/target/release/bureau" ] || {
  echo "build the release binary first: cargo build --release" >&2
  exit 1
}

command -v makepkg >/dev/null 2>&1 \
  && grep -qE '^ID(_LIKE)?=.*[[:space:]"]?arch[[:space:]"]?$' /etc/os-release 2>/dev/null || {
  echo "makepkg not found - the Arch package builds only on Arch/CachyOS." >&2
  echo "Fedora/Nobara: packaging/build-rpm.sh   Debian/Ubuntu: packaging/build-deb.sh" >&2
  exit 1
}

export PATH="$HOME/.cargo/bin:$PATH"
mkdir -p "$ROOT/dist"
cd "$HERE"
PKGDEST="$ROOT/dist" makepkg -f --nodeps --noconfirm
