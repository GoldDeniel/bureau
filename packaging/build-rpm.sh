#!/usr/bin/env bash
# Build an RPM from the prebuilt binary.
# Uses rpmbuild from PATH, or bootstraps rpm-tools from the Arch repos into
# packaging/.tools/ when running on Arch (no root needed).
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
DIST="$ROOT/dist"
VERSION=$(grep -m1 '^version' "$ROOT/Cargo.toml" | cut -d'"' -f2)

[ -x "$ROOT/target/release/bureau" ] || {
  echo "build the release binary first: cargo build --release" >&2
  exit 1
}

if command -v rpmbuild >/dev/null 2>&1; then
  RPMBUILD=$(command -v rpmbuild)
else
  TOOLS="$HERE/.tools"
  RPMROOT="$TOOLS/rpmroot"
  if [ ! -x "$RPMROOT/usr/bin/rpmbuild" ]; then
    command -v pacman >/dev/null 2>&1 || {
      echo "rpmbuild not found. Install rpm-build (Fedora: dnf install rpm-build)." >&2
      exit 1
    }
    echo "bootstrapping rpm-tools into $RPMROOT …"
    mkdir -p "$TOOLS/cache" "$RPMROOT"
    mapfile -t URLS < <(pacman -Sp rpm-tools)
    for url in "${URLS[@]}"; do
      file="$TOOLS/cache/$(basename "$url")"
      [ -f "$file" ] || curl -fsSL "$url" -o "$file"
      bsdtar -xf "$file" -C "$RPMROOT"
    done
  fi
  export LD_LIBRARY_PATH="$RPMROOT/usr/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
  export RPM_CONFIGDIR="$RPMROOT/usr/lib/rpm"
  RPMBUILD="$RPMROOT/usr/bin/rpmbuild"
fi

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
