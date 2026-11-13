#!/usr/bin/env bash
# Build the packages this host can actually produce into dist/.
#
# Each format is gated on the tooling it needs, because a missing builder must
# not fail the run (and must never drag another distro's package manager onto
# this system):
#   .deb  needs ar + tar + xz
#   .rpm  needs rpmbuild (Fedora/RHEL/Nobara/openSUSE)
#   Arch  needs makepkg *and* an Arch-family /etc/os-release
# Building only your own distro's format is the normal case.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"

export PATH="$HOME/.cargo/bin:$PATH"
cd "$ROOT"
cargo build --release --locked

is_arch_family() {
  # shellcheck disable=SC1091
  [ -r /etc/os-release ] || return 1
  . /etc/os-release
  case " ${ID:-} ${ID_LIKE:-} " in
    *" arch "*|*" archlinux "*|*" cachyos "*|*" manjaro "*) return 0 ;;
  esac
  return 1
}

skipped=()

if is_arch_family && command -v makepkg >/dev/null 2>&1; then
  "$HERE/build-arch.sh"
else
  if is_arch_family; then
    skipped+=("arch: makepkg not installed")
  else
    skipped+=("arch: this is not an Arch-family system")
  fi
fi

if command -v ar >/dev/null 2>&1 && command -v tar >/dev/null 2>&1 \
   && command -v xz >/dev/null 2>&1; then
  "$HERE/build-deb.sh"
else
  skipped+=("deb: needs ar + tar + xz")
fi

if command -v rpmbuild >/dev/null 2>&1; then
  "$HERE/build-rpm.sh"
elif command -v pacman >/dev/null 2>&1; then
  # Arch host without rpmbuild: build-rpm.sh can bootstrap rpm-tools locally.
  "$HERE/build-rpm.sh"
else
  skipped+=("rpm: rpmbuild not found (Fedora/RHEL: sudo dnf install rpm-build)")
fi

echo
if [ ${#skipped[@]} -gt 0 ]; then
  echo "skipped:"
  for s in "${skipped[@]}"; do echo "  - $s"; done
  echo
fi

shopt -s nullglob
pkgs=("$ROOT"/dist/*.pkg.tar.zst "$ROOT"/dist/*.deb "$ROOT"/dist/*.rpm)
if [ ${#pkgs[@]} -eq 0 ]; then
  echo "no packages were built — see the reasons above" >&2
  exit 1
fi
echo "packages:"
ls -lh "${pkgs[@]}"
