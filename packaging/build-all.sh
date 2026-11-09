#!/usr/bin/env bash
# Build all three packages into dist/
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"

export PATH="$HOME/.cargo/bin:$PATH"
cd "$ROOT"
cargo build --release --locked

"$HERE/build-arch.sh"
"$HERE/build-deb.sh"
"$HERE/build-rpm.sh"

echo
echo "packages:"
ls -lh "$ROOT/dist"/*.pkg.tar.zst "$ROOT/dist"/*.deb "$ROOT/dist"/*.rpm
