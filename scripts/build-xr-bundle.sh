#!/bin/sh
# Build the VR support bundle the app installs on headsets (scripts/guest/build-xr.sh) on an arm64
# Linux machine: in a chroot of the Arch Linux the app installs, brought up to date. CI runs this
# for each release.
#
# Usage: scripts/build-xr-bundle.sh BUNDLE        (e.g. out/localdesktop-xr.tar.xz)
# As root, or as a user who may make user namespaces (then without pacman's download user).
# Environment: XR_BUNDLE_WORK, where the rootfs and the builds go (default target/xr-bundle; the
# builds and pacman's downloads are reused, the rootfs is made again each time).
set -eu
repo=$(cd "$(dirname "$0")/.." && pwd)
[ $# -eq 1 ] || { echo "usage: build-xr-bundle.sh BUNDLE" >&2; exit 2; }
mkdir -p "$(dirname "$1")"
out=$(cd "$(dirname "$1")" && pwd)
name=$(basename "$1")
work=${XR_BUNDLE_WORK:-$repo/target/xr-bundle}
mkdir -p "$work/builds" "$work/packages"
work=$(cd "$work" && pwd)

# The tarball the app downloads on first start.
archive_url=$(sed -n 's/^pub const ARCH_FS_ARCHIVE: &str = "\(.*\)";$/\1/p' "$repo/src/core/config.rs")
archive=$work/$(basename "$archive_url")
[ -f "$archive" ] || curl -fsSL -o "$archive.part" "$archive_url"
[ -f "$archive" ] || mv "$archive.part" "$archive"

root=$work/rootfs
if [ -d "$root" ]; then
    chmod -R u+rwX "$root"
    rm -rf "$root"
fi
mkdir -p "$root"
tar -xJf "$archive" -C "$root" --strip-components=1
cp -L /etc/resolv.conf "$root/etc/resolv.conf"
mkdir -p "$root/src" "$root/out" "$root/work"

if [ "$(id -u)" = 0 ]; then
    namespaces="--mount --pid --fork"
else
    # One mapped user: pacman can't switch to its own for downloads.
    namespaces="--user --map-root-user --mount --pid --fork"
    sed -i 's/^DownloadUser/#&/' "$root/etc/pacman.conf"
fi

# shellcheck disable=SC2086
unshare $namespaces sh -eu -s "$root" "$repo" "$out" "$work" "$name" "$(nproc)" <<'EOF'
root=$1
# A mount of its own, for pacman to find the free space in.
mount --bind "$root" "$root"
mount -t proc proc "$root/proc"
mount --rbind /sys "$root/sys"
mount --rbind /dev "$root/dev"
mount --bind "$2" "$root/src"
mount --bind "$3" "$root/out"
mount --bind "$4/builds" "$root/work"
mount --bind "$4/packages" "$root/var/cache/pacman/pkg"
exec chroot "$root" /usr/bin/env -i PATH=/usr/local/sbin:/usr/local/bin:/usr/bin HOME=/root \
    sh -euc '
        pacman -Sy --noconfirm --noprogressbar archlinuxarm-keyring
        pacman -Su --noconfirm --noprogressbar
        sh /src/scripts/guest/build-xr.sh -j "$2" -w /work /src "/out/$1"
    ' sh "$5" "$6"
EOF
