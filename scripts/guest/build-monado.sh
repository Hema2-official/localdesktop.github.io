#!/bin/sh
# Build Monado with Local Desktop's driver inside the rootfs, as root: the OpenXR runtime that
# Linux XR apps use while the app is in immersive mode (patches/monado: the driver's sources and
# the patch that hooks them into Monado's build).
#
# Usage: build-monado.sh PATCHES_DIR   (a copy of the repo's patches/monado in the rootfs)
# Installs into /usr/local: monado-service, the OpenXR runtime library and its manifest
# (/usr/local/share/openxr/1/openxr_monado.json). Run monado-service in the desktop session while
# the app is in immersive mode, then the app with XR_RUNTIME_JSON set to the manifest.
set -eu
umask 022
export LANG=C.UTF-8
patches=$(realpath "${1:?usage: build-monado.sh PATCHES_DIR}")
commit=f037264d23e2472a444a157370647fcd601ed81b
src=/root/src/monado
build=/root/src/monado-build

pacman -S --needed --noconfirm --noprogressbar \
    base-devel cmake ninja git pkgconf python glslang vulkan-headers vulkan-icd-loader eigen \
    wayland wayland-protocols libdrm systemd-libs hidapi libusb libbsd

if [ ! -d "$src/.git" ]; then
    git init -q "$src"
    git -C "$src" remote add origin https://gitlab.freedesktop.org/monado/monado.git
fi
if ! git -C "$src" cat-file -e "$commit^{commit}" 2>/dev/null; then
    git -C "$src" fetch -q --depth 1 origin "$commit"
fi
git -C "$src" checkout -q -f "$commit"
git -C "$src" clean -q -fdx
cp -R "$patches/src" "$src/"
for patch in "$patches"/*.patch; do
    git -C "$src" apply "$patch"
done

# No systemd here: monado-service is started by hand, not through socket activation.
cmake -B "$build" -S "$src" -GNinja \
    -DCMAKE_BUILD_TYPE=RelWithDebInfo -DCMAKE_INSTALL_PREFIX=/usr/local \
    -DXRT_BUILD_DRIVER_LOCALDESKTOP=ON -DXRT_FEATURE_SERVICE=ON -DXRT_FEATURE_SERVICE_SYSTEMD=OFF \
    -DXRT_FEATURE_STEAMVR_PLUGIN=OFF -DXRT_BUILD_SAMPLES=OFF -DBUILD_TESTING=OFF \
    -DXRT_OPENXR_INSTALL_ABSOLUTE_RUNTIME_PATH=ON
# Four jobs: the desktop and the headset's own work need the rest of the memory.
time cmake --build "$build" -j 4
cmake --install "$build"
ls -l /usr/local/bin/monado-service /usr/local/share/openxr/1/openxr_monado.json
