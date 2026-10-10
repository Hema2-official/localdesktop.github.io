#!/bin/sh
# Build the VR support Local Desktop installs on headsets, inside its Arch Linux rootfs, as root:
# - Turnip, Mesa's Vulkan driver for Adreno GPUs, with patches/mesa (Meta Horizon OS refuses the
#   KGSL ioctls released builds allocate with). Only the Vulkan driver: no OpenGL, so no LLVM.
# - Monado, the OpenXR runtime of Linux apps, with Local Desktop's driver (patches/monado): it
#   renders into the buffers of the app's immersive mode.
# The result is a bundle the app installs into /usr/local. CI builds one for each release; the app
# runs this itself where it has none for its version.
#
# Usage: build-xr.sh [-j JOBS] [-w WORK_DIR] [--remove-build-deps] RECIPE_DIR BUNDLE
#   RECIPE_DIR  holds this script as scripts/guest/build-xr.sh, and patches/mesa and
#               patches/monado: the repository, or the copy the app writes
#   BUNDLE      the tarball to write (.tar or .tar.xz): `manifest`, then `root/` with the files to
#               install, relative to /
#   -j          parallel jobs (default 4: on a headset, the desktop needs the rest of the memory)
#   -w          sources and builds (default /var/cache/localdesktop-xr/work), kept for the next run
#   --remove-build-deps  uninstall the packages installed for the build afterwards
#
# The manifest's lines:
#   recipe <hash>          SHA-256 of `sha256sum` over the recipe's files, sorted by path, as the
#                          app computes it from its own copy
#   packages <name>...     the packages providing the libraries the files link
set -eu
umask 022
export LANG=C.UTF-8

mesa_commit=07d7e87b43678cb42f2f0a23b13a725e3172bd04
monado_commit=f037264d23e2472a444a157370647fcd601ed81b

usage="usage: build-xr.sh [-j JOBS] [-w WORK_DIR] [--remove-build-deps] RECIPE_DIR BUNDLE"
jobs=4
work=/var/cache/localdesktop-xr/work
remove_build_deps=
while [ $# -gt 0 ]; do
    case $1 in
    -j) jobs=$2; shift 2 ;;
    -w) work=$2; shift 2 ;;
    --remove-build-deps) remove_build_deps=1; shift ;;
    -*) echo "$usage" >&2; exit 2 ;;
    *) break ;;
    esac
done
[ $# -eq 2 ] || { echo "$usage" >&2; exit 2; }
recipe=$(realpath "$1")
mkdir -p "$work" "$(dirname "$2")"
bundle=$(realpath "$2")
stage=$work/stage

recipe_hash=$(cd "$recipe" && { echo scripts/guest/build-xr.sh; find patches/mesa patches/monado -type f; } |
    LC_ALL=C sort | xargs sha256sum | sha256sum | cut -d' ' -f1)
echo "Building VR support, recipe $recipe_hash"

# New packages are marked as dependencies, so that what nothing needs afterwards can go.
build_deps="base-devel git cmake meson ninja pkgconf python python-mako python-packaging python-yaml
    glslang bison flex vulkan-headers vulkan-icd-loader eigen libglvnd libx11 libxcb
    libxshmfence libxrandr xcb-util-keysyms libdrm wayland wayland-protocols libdisplay-info
    systemd-libs expat zlib zstd libbsd"
pacman -Qq | LC_ALL=C sort > "$work/packages-before"
# shellcheck disable=SC2086
pacman -S --needed --asdeps --noconfirm --noprogressbar $build_deps ||
    pacman -Sy --needed --asdeps --noconfirm --noprogressbar $build_deps
pacman -Qq | LC_ALL=C sort | LC_ALL=C comm -13 "$work/packages-before" - >> "$work/packages-added"

# fetch DIR URL COMMIT: a clean checkout of COMMIT, fetching only that commit.
fetch() {
    if [ ! -d "$1/.git" ]; then
        rm -rf "$1"
        git init -q "$1"
        git -C "$1" remote add origin "$2"
    fi
    git -C "$1" cat-file -e "$3^{commit}" 2>/dev/null || git -C "$1" fetch -q --depth 1 origin "$3"
    git -C "$1" checkout -q -f "$3"
    git -C "$1" clean -q -fdx
}

rm -rf "$stage"
mkdir -p "$stage/root"

fetch "$work/mesa" https://gitlab.freedesktop.org/mesa/mesa.git "$mesa_commit"
for patch in "$recipe"/patches/mesa/*.patch; do
    git -C "$work/mesa" apply "$patch"
done
[ -d "$work/mesa-build" ] && reconfigure=--reconfigure || reconfigure=
meson setup $reconfigure "$work/mesa-build" "$work/mesa" --prefix=/usr/local --buildtype=release \
    -Dvulkan-drivers=freedreno -Dfreedreno-kmds=kgsl -Dgallium-drivers= -Dplatforms=wayland,x11 \
    -Dllvm=disabled -Dglx=disabled -Degl=disabled -Dgles1=disabled -Dgles2=disabled -Dopengl=false \
    -Dbuild-tests=false -Dvideo-codecs= -Dvulkan-layers= -Dtools= -Dspirv-tools=disabled
ninja -C "$work/mesa-build" -j "$jobs"
meson install -C "$work/mesa-build" --destdir "$stage/root" --strip --no-rebuild

# No systemd here: the session starts monado-service. No peek window: making one starts SDL's
# video, and SDL keeps the screen from sleeping. No drivers but ours, and none of the libraries
# for hardware (USB, HID, Bluetooth, cameras) or window systems: the headset is the app's, and the
# files link only what every desktop has. libbsd for the pid file that tells a running service from
# the socket a killed one left (/tmp outlives the app).
fetch "$work/monado" https://gitlab.freedesktop.org/monado/monado.git "$monado_commit"
cp -R "$recipe/patches/monado/src" "$work/monado/"
for patch in "$recipe"/patches/monado/*.patch; do
    git -C "$work/monado" apply "$patch"
done
cmake -B "$work/monado-build" -S "$work/monado" -GNinja \
    -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/usr/local \
    -DXRT_BUILD_DRIVER_LOCALDESKTOP=ON -DXRT_FEATURE_SERVICE=ON -DXRT_FEATURE_SERVICE_SYSTEMD=OFF \
    -DXRT_INSTALL_SYSTEMD_UNIT_FILES=OFF -DXRT_FEATURE_WINDOW_PEEK=OFF \
    -DXRT_FEATURE_STEAMVR_PLUGIN=OFF -DXRT_BUILD_SAMPLES=OFF -DBUILD_TESTING=OFF \
    -DXRT_OPENXR_INSTALL_ABSOLUTE_RUNTIME_PATH=ON \
    -DXRT_HAVE_SDL2=OFF -DXRT_HAVE_GST=OFF -DXRT_HAVE_JPEG=OFF -DXRT_HAVE_OPENCV=OFF \
    -DXRT_HAVE_HIDAPI=OFF -DXRT_HAVE_LIBUSB=OFF -DXRT_HAVE_LIBUVC=OFF -DXRT_HAVE_BLUETOOTH=OFF \
    -DXRT_HAVE_DBUS=OFF -DXRT_HAVE_REALSENSE=OFF -DXRT_HAVE_ONNXRUNTIME=OFF \
    -DXRT_HAVE_PERCETTO=OFF -DXRT_HAVE_SYSTEMD=OFF -DXRT_HAVE_LIBUDEV=OFF -DXRT_HAVE_LIBBSD=ON \
    -DXRT_HAVE_XCB=OFF -DXRT_HAVE_XRANDR=OFF -DXRT_HAVE_WAYLAND=OFF -DXRT_HAVE_WAYLAND_DIRECT=OFF
# The drivers that need none of those are on by default: off with them too (some don't even build
# without the rest).
others=$(sed -n 's/^\(XRT_BUILD_DRIVER_[A-Z0-9_]*\):BOOL=ON$/-D\1=OFF/p' \
    "$work/monado-build/CMakeCache.txt" | grep -v -e -DXRT_BUILD_DRIVER_LOCALDESKTOP= || true)
# shellcheck disable=SC2086
[ -z "$others" ] || cmake -B "$work/monado-build" -S "$work/monado" $others
cmake --build "$work/monado-build" -j "$jobs"
DESTDIR=$stage/root cmake --install "$work/monado-build" --strip

# What the files link, from Arch's packages.
packages=$(find "$stage/root" -type f \( -perm -u+x -o -name '*.so*' \) -exec ldd {} + 2>/dev/null |
    sed -n 's|^.* => \(/[^ ]*\) (0x[0-9a-f]*)$|\1|p' | grep -v '^/usr/local/' | LC_ALL=C sort -u |
    xargs -r pacman -Qoq | LC_ALL=C sort -u | tr '\n' ' ')
printf 'recipe %s\npackages %s\n' "$recipe_hash" "${packages% }" > "$stage/manifest"
cat "$stage/manifest"

case $bundle in
*.tar.xz) compress=--xz ;;
*) compress= ;;
esac
XZ_OPT=-T0 tar -c $compress -f "$bundle.part" --sort=name --owner=0 --group=0 --numeric-owner \
    -C "$stage" manifest root
mv "$bundle.part" "$bundle"
rm -rf "$stage"
ls -l "$bundle"

# Uninstall what only the build needed: the packages it installed that nothing needs now (some
# may suggest them, as pacman does base-devel), round by round, as removing some leaves their own
# dependencies unneeded.
if [ -n "$remove_build_deps" ]; then
    LC_ALL=C sort -u -o "$work/packages-added" "$work/packages-added"
    printf '%s\n' $packages | LC_ALL=C sort -u > "$work/packages-kept"
    while :; do
        unneeded=$(pacman -Qdttq | LC_ALL=C sort | LC_ALL=C comm -12 - "$work/packages-added" |
            LC_ALL=C comm -23 - "$work/packages-kept")
        [ -n "$unneeded" ] || break
        # shellcheck disable=SC2086
        pacman -Rn --noconfirm --noprogressbar $unneeded || break
    done
    rm -f "$work/packages-added"
fi
