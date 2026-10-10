#!/bin/sh
# Build Mesa's Turnip (Vulkan for Adreno) for KGSL inside the Local Desktop rootfs, as root, and
# install it into /usr/local: for devices that need patches/mesa/ (Meta Horizon OS refuses the
# KGSL ioctls released builds allocate with). Only the Vulkan driver: no OpenGL, so no LLVM.
#
# Usage: build-turnip.sh [MESA_SOURCE_DIR]   (default /root/src/mesa, patches already applied)
# Arch's own Turnip only knows msm, and its manifest names the library without a path, so with
# ours loaded it listed the GPU a second time: its manifest goes, and NoExtract keeps it away.
# Undo: rm -r /usr/local/lib/libvulkan_freedreno.so /usr/local/share/vulkan/icd.d, drop the
# NoExtract line, pacman -S vulkan-freedreno.
set -eu
umask 022
export LANG=C.UTF-8
src=${1:-/root/src/mesa}

pacman -S --needed --noconfirm --noprogressbar \
    base-devel meson ninja python-mako python-packaging python-yaml glslang bison flex \
    libdrm wayland wayland-protocols libx11 libxcb libxshmfence libxrandr xcb-util-keysyms \
    libdisplay-info spirv-tools systemd-libs expat zlib zstd

cd "$src"
[ -d build ] || meson setup build --prefix=/usr/local --buildtype=release \
    -Dvulkan-drivers=freedreno -Dfreedreno-kmds=kgsl -Dgallium-drivers= \
    -Dplatforms=wayland,x11 -Dllvm=disabled -Dglx=disabled -Degl=disabled -Dgles1=disabled \
    -Dgles2=disabled -Dopengl=false -Dbuild-tests=false -Dvideo-codecs= -Dvulkan-layers= \
    -Dtools=
# Four jobs: the desktop and the headset's own work need the rest.
time ninja -C build -j 4
ninja -C build install
grep -q '^NoExtract.*freedreno_icd.json' /etc/pacman.conf ||
    sed -i 's|^\[options\]$|[options]\nNoExtract = usr/share/vulkan/icd.d/freedreno_icd.json|' /etc/pacman.conf
rm -f /usr/share/vulkan/icd.d/freedreno_icd.json
ls -l /usr/local/lib/libvulkan_freedreno.so /usr/local/share/vulkan/icd.d/
XDG_RUNTIME_DIR=/tmp vulkaninfo --summary 2>&1 | grep -E '^GPU[0-9]|deviceName'
