#!/bin/sh
# Build a side-by-side development APK on an aarch64 Linux host (run
# scripts/setup-arm64-linux-toolchain.sh once first).
#
# The APK installs as its own app, "Local Desktop Dev" (app.polarbear.dev) by default, with its
# own Linux rootfs and no crash reporting, so the official app and its rootfs stay untouched.
#
# Usage: scripts/build-dev-apk.sh [--debug]
# Environment: LOCALDESKTOP_DEV_DIR, LOCALDESKTOP_PACKAGE, LOCALDESKTOP_LABEL, CARGO_TARGET_DIR,
# NDK_VERSION
set -eu

cd "$(dirname "$0")/.."
PACKAGE=${LOCALDESKTOP_PACKAGE:-app.polarbear.dev}
LABEL=${LOCALDESKTOP_LABEL:-Local Desktop Dev}
NDK_VERSION=${NDK_VERSION:-r28}
DEV_DIR=${LOCALDESKTOP_DEV_DIR:-$HOME/.cache/localdesktop}
# Keep build output on a native Linux filesystem even when the sources live on a Windows drive.
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$DEV_DIR/target}
PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"

prebuilt="$DEV_DIR/android-ndk-$NDK_VERSION/toolchains/llvm/prebuilt/linux-x86_64"
if [ ! -d "$prebuilt/sysroot" ]; then
    echo "NDK sysroot not found; run scripts/setup-arm64-linux-toolchain.sh first" >&2
    exit 1
fi
resource_dir=$(ls -d "$prebuilt"/lib/clang/* | head -n 1)

# The host's clang, pointed at the NDK's sysroot and Android runtime libraries.
mkdir -p "$CARGO_TARGET_DIR"
cc="$CARGO_TARGET_DIR/aarch64-linux-android-clang"
cat > "$cc" <<EOF
#!/bin/sh
exec clang --target=aarch64-linux-android21 --sysroot="$prebuilt/sysroot" \\
    -resource-dir="$resource_dir" -L"$resource_dir/lib/linux/aarch64" -fuse-ld=lld "\$@"
EOF
chmod +x "$cc"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$cc"
export CC_aarch64_linux_android="$cc"
export AR_aarch64_linux_android=llvm-ar
# 16 KB ELF alignment, as in the release build.
export CARGO_TARGET_AARCH64_LINUX_ANDROID_RUSTFLAGS="-C link-arg=-Wl,-z,max-page-size=16384"

# Same manifest under another package name and label, targeting the release builds' API level.
# Debuggable, so `adb shell run-as` can reach the app's files and rootfs (scripts/dev-shell.sh).
manifest="$CARGO_TARGET_DIR/manifest.dev.yaml"
sed -e "s/^\(    package:\) app\.polarbear\$/\1 $PACKAGE/" \
    -e "s/^\(      label:\) \"Local Desktop\"\$/\1 \"$LABEL\"\n      debuggable: true/" \
    -e 's/target_sdk_version: 33/target_sdk_version: 35/' \
    manifest.yaml > "$manifest"
if ! grep -q "^    package: $PACKAGE\$" "$manifest"; then
    echo "Could not set the package name in $manifest" >&2
    exit 1
fi

# The packager runs on this host, so build it quickly rather than with the size-optimized,
# LTO release profile the app itself uses.
CARGO_PROFILE_RELEASE_LTO=false CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 \
CARGO_PROFILE_RELEASE_OPT_LEVEL=2 \
    cargo build --release --bin build_apk

out="$CARGO_TARGET_DIR/$PACKAGE.apk"
CARGO_BUILD_TARGET=aarch64-linux-android LOCALDESKTOP_PACKAGE="$PACKAGE" \
    "$CARGO_TARGET_DIR/release/build_apk" --manifest "$manifest" --out "$out" \
    --android-jar "$DEV_DIR/android-sdk/platforms/android-33/android.jar" "$@"
echo "Built $out"
