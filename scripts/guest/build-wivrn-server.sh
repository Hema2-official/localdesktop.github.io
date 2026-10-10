#!/bin/sh
# Build WiVRn's server inside the Local Desktop rootfs, as root: Monado plus WiVRn's driver and
# encoders, with x264 as the only encoder (no VA-API or NVENC here, and Turnip has no Vulkan
# video). The server must be the client's version: WiVRn refuses others.
#
# Usage: build-wivrn-server.sh [VERSION]   (default v26.9)
# Installs into /usr/local: wivrn-server, wivrnctl, the OpenXR runtime library and its manifest.
set -eu
umask 022
export LANG=C.UTF-8
version=${1:-v26.9}
src=/root/src/wivrn-$version
build=/root/src/wivrn-$version-build

pacman -S --needed --noconfirm --noprogressbar \
    base-devel cmake ninja git pkgconf python glslang shaderc spirv-tools vulkan-headers \
    vulkan-icd-loader openssl boost x264 pipewire avahi eigen nlohmann-json cli11 glib2 \
    glib2-devel libnotify \
    librsvg libarchive libpng curl openxr wayland wayland-protocols libdrm systemd-libs hidapi \
    libusb libbsd

if [ ! -d "$src" ]; then
    git clone --depth 1 --branch "$version" https://github.com/WiVRn/WiVRn.git "$src"
fi
# The client checks the server's version, which comes from `git describe`; that fails in this
# shallow clone under proot, so the tag is given here (WiVRn takes it alone).
cmake -B "$build" -S "$src" -GNinja \
    -DGIT_TAG="$version" -DGIT_DESC= -DGIT_COMMIT= \
    -DCMAKE_BUILD_TYPE=RelWithDebInfo -DCMAKE_INSTALL_PREFIX=/usr/local \
    -DWIVRN_BUILD_CLIENT=OFF -DWIVRN_BUILD_SERVER=ON -DWIVRN_BUILD_DASHBOARD=OFF \
    -DWIVRN_USE_NVENC=OFF -DWIVRN_USE_VAAPI=OFF -DWIVRN_USE_VULKAN_ENCODE=OFF -DWIVRN_USE_X264=ON \
    -DWIVRN_USE_PIPEWIRE=ON -DWIVRN_FEATURE_STEAMVR_LIGHTHOUSE=OFF -DWIVRN_FEATURE_SOLARXR=OFF \
    -DWIVRN_OPENXR_MANIFEST_TYPE=absolute
# Four jobs: the desktop and the headset's own work need the rest of the memory.
time cmake --build "$build" -j 4
cmake --install "$build"
ls -l /usr/local/bin/wivrn-server /usr/local/share/openxr/1/openxr_wivrn.json
