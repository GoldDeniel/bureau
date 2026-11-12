#!/usr/bin/env bash
# Build an RPM from the prebuilt binary.
# Needs rpmbuild on PATH (Fedora/Nobara: sudo dnf install rpm-build).
# Arch hosts cannot build this format without installing rpm tooling - that is
# expected; build-rpm.sh is the Fedora/openSUSE path, not the Arch one.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
DIST="$ROOT/dist"
VERSION=$(grep -m1 '^version' "$ROOT/Cargo.toml" | cut -d'"' -f2)

[ -x "$ROOT/target/release/bureau" ] || {
  echo "build the release binary first: cargo build --release" >&2
  exit 1
}

if ! command -v rpmbuild >/dev/null 2>&1; then
  cat >&2 <<'EOF'
rpmbuild not found. Install your distro's rpm build tools first:
  Fedora / RHEL / Nobara : sudo dnf install rpm-build
  openSUSE               : sudo zypper install rpm-build
  Debian / Ubuntu        : sudo apt install rpm
This script used to bootstrap rpm-tools out of the Arch repositories with pacman.
That only worked on Arch hosts and is gone: an RPM does not need pacman.
EOF
  exit 1
fi
RPMBUILD=$(command -v rpmbuild)

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
STAGE="$WORK/stage"
install -Dm755 "$ROOT/target/release/bureau"     "$STAGE/bureau"
install -Dm644 "$HERE/bureau@.service"           "$STAGE/bureau@.service"
install -Dm644 "$ROOT/README.md"                 "$STAGE/README.md"
install -Dm644 "$ROOT/docs/AGENT-INTEGRATION.md" "$STAGE/AGENT-INTEGRATION.md"
install -Dm644 "$HERE/INSTALL.md"                "$STAGE/INSTALL.md"
install -Dm644 "$ROOT/LICENSE"                   "$STAGE/LICENSE"

TOP="$WORK/rpmbuild"
mkdir -p "$TOP"/{BUILD,BUILDROOT,RPMS,SOURCES,SPECS,SRPMS}
mkdir -p "$DIST"

"$RPMBUILD" -bb \
  --define "_topdir $TOP" \
  --define "stage $STAGE" \
  --define "_binary_payload w2T.xzdio" \
  "$HERE/bureau.spec"

cp "$TOP"/RPMS/*/bureau-"$VERSION"-1*.rpm "$DIST"/
echo "$DIST/bureau-${VERSION}-1.x86_64.rpm"
