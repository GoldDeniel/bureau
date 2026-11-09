#!/usr/bin/env bash
# Build a Debian/Ubuntu .deb without dpkg-deb (works on any distro with ar+tar+xz).
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
VERSION=$(grep -m1 '^version' "$ROOT/Cargo.toml" | cut -d'"' -f2)
ARCH=amd64
DIST="$ROOT/dist"

[ -x "$ROOT/target/release/bureau" ] || {
  echo "build the release binary first: cargo build --release" >&2
  exit 1
}

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
STAGE="$WORK/stage"

install -Dm755 "$ROOT/target/release/bureau"            "$STAGE/usr/bin/bureau"
install -Dm644 "$HERE/bureau@.service"                  "$STAGE/usr/lib/systemd/system/bureau@.service"
install -Dm644 "$ROOT/README.md"                        "$STAGE/usr/share/doc/bureau/README.md"
install -Dm644 "$ROOT/docs/AGENT-INTEGRATION.md"        "$STAGE/usr/share/doc/bureau/AGENT-INTEGRATION.md"
install -Dm644 "$HERE/INSTALL.md"                       "$STAGE/usr/share/doc/bureau/INSTALL.md"
install -Dm644 "$ROOT/LICENSE"                          "$STAGE/usr/share/licenses/bureau/LICENSE"
install -Dm644 "$ROOT/docs/board.png"                   "$STAGE/usr/share/doc/bureau/board.png" 2>/dev/null || true

mkdir -p "$STAGE/DEBIAN"
SIZE_KB=$(du -sk "$STAGE" | cut -f1)
sed -e "s/@VERSION@/$VERSION/" -e "s/@SIZE@/$SIZE_KB/" "$HERE/debian/control.in" > "$STAGE/DEBIAN/control"
install -m755 "$HERE/debian/postinst" "$STAGE/DEBIAN/postinst"
install -m755 "$HERE/debian/postrm"   "$STAGE/DEBIAN/postrm"

(
  cd "$STAGE"
  find usr -type f -print0 | sort -z | xargs -0 md5sum > DEBIAN/md5sums
  tar --owner=0 --group=0 --numeric-owner -cJf "$WORK/data.tar.xz" usr
)
(
  cd "$STAGE/DEBIAN"
  tar --owner=0 --group=0 --numeric-owner -cJf "$WORK/control.tar.xz" .
)
printf '2.0\n' > "$WORK/debian-binary"

mkdir -p "$DIST"
OUT="$DIST/bureau_${VERSION}_${ARCH}.deb"
(
  cd "$WORK"
  rm -f "$OUT"
  ar rcs "$OUT" debian-binary control.tar.xz data.tar.xz
)

echo "$OUT"
