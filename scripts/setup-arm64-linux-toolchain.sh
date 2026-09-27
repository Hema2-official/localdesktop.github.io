#!/bin/sh
# One-time toolchain setup for building Local Desktop on an aarch64 Linux host, such as WSL on
# Windows-on-ARM, Asahi Linux or a Raspberry Pi. Then build with scripts/build-dev-apk.sh.
#
# Google's NDK only ships x86_64 host binaries, so the distribution's clang and lld do the
# compiling and linking; the NDK only provides the Android sysroot and clang's runtime libraries.
#
# Install these as root first (Debian/Ubuntu): sudo apt install clang lld llvm curl unzip
# Building proot (patches/build-proot-android/build-on-arm64-linux.sh) also needs gawk.
#
# Downloads go to LOCALDESKTOP_DEV_DIR (default ~/.cache/localdesktop). Rust goes wherever
# RUSTUP_HOME and CARGO_HOME point (default ~/.rustup and ~/.cargo).
set -eu

DEV_DIR=${LOCALDESKTOP_DEV_DIR:-$HOME/.cache/localdesktop}
NDK_VERSION=${NDK_VERSION:-r28}
ANDROID_PLATFORM=${ANDROID_PLATFORM:-platform-33-ext5_r01}

for tool in clang ld.lld llvm-ar curl unzip; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "Missing $tool. Install first: sudo apt install clang lld llvm curl unzip" >&2
        exit 1
    fi
done
mkdir -p "$DEV_DIR"

PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
if ! command -v rustup >/dev/null 2>&1; then
    echo "==> Installing Rust (rustup)"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
        sh -s -- -y --profile minimal --no-modify-path
fi
echo "==> Adding the aarch64-linux-android Rust target"
rustup target add aarch64-linux-android

ndk="android-ndk-$NDK_VERSION"
if [ ! -d "$DEV_DIR/$ndk/toolchains/llvm/prebuilt/linux-x86_64/sysroot" ]; then
    echo "==> Downloading Android NDK $NDK_VERSION (keeping the sysroot and clang runtime libraries)"
    zip="$DEV_DIR/$ndk-linux.zip"
    curl -fL -o "$zip" "https://dl.google.com/android/repository/$ndk-linux.zip"
    unzip -q -o "$zip" \
        "$ndk/toolchains/llvm/prebuilt/linux-x86_64/sysroot/*" \
        "$ndk/toolchains/llvm/prebuilt/linux-x86_64/lib/clang/*" \
        -d "$DEV_DIR"
    rm "$zip"
fi

# The packager compiles resources against the platform's android.jar. Its resource table
# parser predates the compact entries in API 34+ jars, so use API 33's (the APK can still
# target a newer API level).
jar_dir="$DEV_DIR/android-sdk/platforms/android-33"
if [ ! -f "$jar_dir/android.jar" ]; then
    echo "==> Downloading android.jar ($ANDROID_PLATFORM)"
    zip="$DEV_DIR/$ANDROID_PLATFORM.zip"
    curl -fL -o "$zip" "https://dl.google.com/android/repository/$ANDROID_PLATFORM.zip"
    mkdir -p "$jar_dir"
    unzip -q -o -j "$zip" '*/android.jar' -d "$jar_dir"
    rm "$zip"
fi

echo "Toolchain ready in $DEV_DIR. Build with: scripts/build-dev-apk.sh"
