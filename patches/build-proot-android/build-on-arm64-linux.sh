#!/bin/sh
# Build the proot that Local Desktop ships (the USERLAND variant, with ownership records) on an
# aarch64 Linux host such as WSL on Windows-on-ARM, using the host's clang and the NDK sysroot
# from scripts/setup-arm64-linux-toolchain.sh. The vendored sources are copied to a scratch
# directory, so the tree under build/ stays untouched.
#
# Usage: patches/build-proot-android/build-on-arm64-linux.sh [--install]
#   --install  copy libproot.so and libproot_loader.so into assets/libs/arm64-v8a
set -eu

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
DEV_DIR=${LOCALDESKTOP_DEV_DIR:-$HOME/.cache/localdesktop}
NDK_VERSION=${NDK_VERSION:-r28}
prebuilt="$DEV_DIR/android-ndk-$NDK_VERSION/toolchains/llvm/prebuilt/linux-x86_64"
if [ ! -d "$prebuilt/sysroot" ]; then
    echo "NDK sysroot not found; run scripts/setup-arm64-linux-toolchain.sh first" >&2
    exit 1
fi
resource_dir=$(ls -d "$prebuilt"/lib/clang/* | head -n 1)
work="$DEV_DIR/proot-build"
mkdir -p "$work/out"

cc="$work/aarch64-linux-android21-clang"
cat > "$cc" <<EOF
#!/bin/sh
exec clang --target=aarch64-linux-android21 --sysroot="$prebuilt/sysroot" \\
    -resource-dir="$resource_dir" -L"$resource_dir/lib/linux/aarch64" -fuse-ld=lld \\
    -rtlib=compiler-rt -unwindlib=libunwind -Wno-unused-command-line-argument "\$@"
EOF
chmod +x "$cc"
export CC="$cc" AR=llvm-ar RANLIB=llvm-ranlib STRIP=llvm-strip OBJCOPY=llvm-objcopy \
    OBJDUMP=llvm-objdump

# Static talloc, configured for cross-compiling like make-talloc-static.sh does.
if [ ! -f "$work/static/lib/libtalloc.a" ]; then
    echo "==> Building talloc"
    rm -rf "$work/talloc"
    cp -r "$here/build/talloc-2.4.3" "$work/talloc"
    cd "$work/talloc"
    rm -rf bin .lock-wscript
    cat > cross-answers.txt <<EOF
Checking uname sysname type: "Linux"
Checking uname machine type: "dontcare"
Checking uname release type: "dontcare"
Checking uname version type: "dontcare"
Checking simple C program: OK
rpath library support: OK
-Wl,--version-script support: FAIL
Checking getconf LFS_CFLAGS: OK
Checking for large file support without additional flags: OK
Checking for -D_FILE_OFFSET_BITS=64: OK
Checking for -D_LARGE_FILES: OK
Checking correct behavior of strtoll: OK
Checking for working strptime: OK
Checking for C99 vsnprintf: OK
Checking for HAVE_SHARED_MMAP: OK
Checking for HAVE_MREMAP: OK
Checking for HAVE_INCOHERENT_MMAP: OK
Checking for HAVE_SECURE_MKSTEMP: OK
Checking getconf large file support flags work: OK
Checking for HAVE_IFACE_IFCONF: FAIL
EOF
    PATH="$here/target-mock-bin:$PATH" ./configure build --prefix="$work/talloc-install" \
        --disable-rpath --disable-python --cross-compile --cross-answers=cross-answers.txt \
        > "$work/talloc.log" 2>&1 || { tail -30 "$work/talloc.log" >&2; exit 1; }
    mkdir -p "$work/static/include" "$work/static/lib"
    "$AR" rcs "$work/static/lib/libtalloc.a" bin/default/talloc*.o
    cp -f talloc.h "$work/static/include/"
fi

echo "==> Building proot (USERLAND)"
# The loader-info generator needs GNU awk (strtonum).
if ! command -v gawk > /dev/null 2>&1; then
    echo "Missing gawk. Install it first: sudo apt install gawk" >&2
    exit 1
fi
mkdir -p "$work/bin"
ln -sf "$(command -v gawk)" "$work/bin/awk"
PATH="$work/bin:$PATH"
rm -rf "$work/proot"
cp -r "$here/build/proot" "$work/proot"
cd "$work/proot/src"
make distclean > /dev/null 2>&1 || true
CFLAGS="-I$work/static/include -Werror=implicit-function-declaration -DUSERLAND" \
LDFLAGS="-L$work/static/lib" PROOT_UNBUNDLE_LOADER=. \
    make proot > "$work/proot.log" 2>&1 || { tail -40 "$work/proot.log" >&2; exit 1; }

cp proot "$work/out/libproot.so"
cp loader/loader "$work/out/libproot_loader.so"
"$STRIP" "$work/out/libproot.so" "$work/out/libproot_loader.so"
ls -l "$work/out"

if [ "${1:-}" = --install ]; then
    cp "$work/out/libproot.so" "$work/out/libproot_loader.so" "$repo/assets/libs/arm64-v8a/"
    echo "Installed into assets/libs/arm64-v8a"
fi
