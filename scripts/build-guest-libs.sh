#!/usr/bin/env bash
# build-guest-libs.sh — build the libraries the app installs into the Linux rootfs
# (assets/guest/), which run on Arch Linux ARM's glibc, not on Android.
#
# Usage (on aarch64 Linux with gcc and glibc, e.g. the WSL build environment):
#   ./scripts/build-guest-libs.sh
#
# - librealpath.so (src/guest/realpath.c): realpath(3) in one question to proot,
#   preloaded through /etc/ld.so.preload by setup.
# - libegl-zink.so (src/guest/egl-zink.c): Zink's own presentation for OpenGL programs on
#   Wayland, preloaded by the `gpu` helper.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CC="${CC:-gcc}"
OUT="$ROOT/assets/guest"

if [[ "$(uname -m)" != aarch64 && "$CC" == gcc ]]; then
  echo "Build on aarch64 Linux, or set CC to an aarch64-linux-gnu compiler" >&2
  exit 2
fi

mkdir -p "$OUT"
build() {
  local name="$1" source="$2"
  # Nothing but libc.so.6: every program loads it, and each more library costs every program
  # start a few proot stops (dlsym and dladdr have been in libc since glibc 2.34).
  "$CC" -O2 -fPIC -shared -fvisibility=hidden -Wall -Wextra -Werror \
    -Wl,-soname,"$name" -Wl,-z,now -Wl,-z,relro -Wl,--as-needed \
    -o "$OUT/$name" "$ROOT/src/guest/$source"
  strip --strip-unneeded "$OUT/$name"

  if readelf -d "$OUT/$name" | grep NEEDED | grep -v 'libc\.so\.6' >&2; then
    echo "$name needs more than libc.so.6" >&2
    exit 1
  fi
  # Newer than 2.34 would keep it from loading on an older rootfs.
  if objdump -T "$OUT/$name" | grep -E 'GLIBC_2\.(3[5-9]|[4-9][0-9])' >&2; then
    echo "$name needs a newer glibc than 2.34" >&2
    exit 1
  fi
  echo "Built $OUT/$name"
}

build librealpath.so realpath.c
build libegl-zink.so egl-zink.c
