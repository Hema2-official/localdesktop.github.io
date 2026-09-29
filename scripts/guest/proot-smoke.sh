#!/bin/sh
# Smoke test for the proot quirks that break desktop and development tools inside Local Desktop.
# Run it inside the guest, as a normal user and as root, e.g. over SSH:
#   ssh -p 8022 user@phone 'sh -s' < scripts/guest/proot-smoke.sh
# Prints one PASS/FAIL/INFO line per check; the exit status is the number of failures.

failures=0
pass() { echo "PASS  $1"; }
fail() { echo "FAIL  $1${2:+: $2}"; failures=$((failures + 1)); }
info() { echo "INFO  $1${2:+: $2}"; }

work=$(mktemp -d "${TMPDIR:-/tmp}/proot-smoke.XXXXXX") || exit 1
cd "$work" || exit 1
echo "proot smoke test as $(id -un) (uid $(id -u)) in $work"

# Ownership records (.proot-meta-file.*) and hard-link targets (.proot.l2s.*) must not show up
# in directory listings: cp -r, pnpm, makepkg and editors copy or index them otherwise.
mkdir listing && touch listing/file && chmod 600 listing/file
extra=$(ls -A listing | grep -v '^file$' | tr '\n' ' ')
[ -z "$extra" ] && pass "directory listings show no proot records" \
    || fail "directory listings show proot records" "$extra"

# chmod/fchmod must change the real mode: install -m755 creates the file, then fchmods it.
printf '#!/bin/sh\necho ok\n' > script
install -m755 script installed 2>/dev/null
[ "$(./installed 2>/dev/null)" = ok ] && pass "install -m755 produces an executable" \
    || fail "install -m755 produces an executable" "$(ls -l installed 2>&1 | cut -c1-10)"
cp script chmodded && chmod +x chmodded
[ "$(./chmodded 2>/dev/null)" = ok ] && pass "chmod +x makes a file executable" \
    || fail "chmod +x makes a file executable"

# Hard links survive removal of the directory that held the original (pnpm's temp dirs).
mkdir a b && echo linked > a/f && ln a/f b/f 2>/dev/null && rm -rf a
[ "$(cat b/f 2>/dev/null)" = linked ] && pass "hard link survives removing the original's directory" \
    || fail "hard link survives removing the original's directory"

# The link count of a hard link, by name (stat, statx) and by descriptor (fstat): tar, rsync and
# backup tools decide from it whether to look for the file's other names.
if command -v python3 > /dev/null; then
    echo counted > counted && ln counted counted2 2>/dev/null
    counts=$(python3 -c '
import os
fd = os.open("counted2", os.O_RDONLY)
print(os.stat("counted2").st_nlink, os.fstat(fd).st_nlink, end="")
' 2>&1)
    statx_count=$(stat -c %h counted2 2>&1)
    [ "$counts $statx_count" = "2 2 2" ] && pass "stat, fstat and statx count both names of a hard link" \
        || fail "stat, fstat and statx count both names of a hard link" "stat/fstat/statx: $counts $statx_count"
fi

# rm -rf of a tree containing hard links succeeds in one pass.
mkdir -p tree/x tree/y && echo data > tree/x/f && ln tree/x/f tree/y/f 2>/dev/null
rm -rf tree 2>/dev/null
[ ! -e tree ] && pass "rm -rf removes a tree with hard links in one pass" \
    || fail "rm -rf removes a tree with hard links in one pass"

# mv takes the ownership record along. A record left behind is invisible but keeps its
# directory from ever being removed ("Directory not empty").
mkdir -p moved/from moved/to && touch moved/from/f && mv moved/from/f moved/to/f
rmdir moved/from 2>/dev/null
[ ! -e moved/from ] && pass "mv leaves no ownership record behind" \
    || fail "mv leaves no ownership record behind" "rmdir: directory not empty"

# fstat() on sockets and eventfds (libwayland-server, Python's signal.set_wakeup_fd, Zed).
if command -v python3 >/dev/null 2>&1; then
    result=$(python3 - <<'PY' 2>&1
import os, socket
sock = socket.socket(socket.AF_UNIX)  # keep it referenced, or its fd closes before fstat
fds = [("unix socket", sock.fileno())]
if hasattr(os, "eventfd"):
    fds.append(("eventfd", os.eventfd(0)))
for name, fd in fds:
    try:
        os.fstat(fd)
        print("ok", name)
    except OSError as e:
        print("bad", name, e.strerror)
PY
)
    bad=$(echo "$result" | grep '^bad' | cut -d' ' -f2- | tr '\n' ';')
    [ -z "$bad" ] && pass "fstat() works on sockets and eventfds" \
        || fail "fstat() works on sockets and eventfds" "$bad"
else
    info "fstat() on sockets" "python3 not installed, skipped"
fi

# A directory created with a trailing slash (git does mkdir(".git/hooks/")) must be removable,
# along with its parent.
mkdir slashed && mkdir slashed/sub/ && rmdir slashed/sub && rmdir slashed 2> /dev/null \
    && pass "directories made with a trailing slash can be removed" \
    || fail "directories made with a trailing slash can be removed" "$(rmdir slashed 2>&1)"

# proot remembers directories it has walked through; one replaced by a symlink to an absolute path
# must be followed inside the rootfs right away, not on Android's side.
mkdir -p swapped/sub && touch swapped/sub/f && ls swapped/sub/f > /dev/null
mv swapped swapped.old && ln -s /etc swapped
[ "$(cat swapped/hostname 2>&1)" = "$(cat /etc/hostname 2>&1)" ] && [ -e swapped/pacman.conf ] \
    && pass "a directory replaced by a symlink is followed at once" \
    || fail "a directory replaced by a symlink is followed at once" "$(ls swapped/ 2>&1 | head -3 | tr '\n' ' ')"

# proot also remembers directories' ownership records; a change must apply at once.
if [ "$(id -u)" != 0 ]; then
    mkdir -p locked && echo secret > locked/f && cat locked/f > /dev/null
    chmod 000 locked
    cat locked/f > /dev/null 2>&1 && denied=no || denied=yes
    chmod 755 locked
    cat locked/f > /dev/null 2>&1 && allowed=yes || allowed=no
    [ "$denied $allowed" = "yes yes" ] && pass "a directory's new permissions apply at once" \
        || fail "a directory's new permissions apply at once" "denied after chmod 000: $denied, allowed after chmod 755: $allowed"
fi

# SysV shared memory (proot --sysvipc): X11's MIT-SHM and GIMP's plug-ins use it.
if command -v ipcmk > /dev/null; then
    shm_id=$(ipcmk -M 65536 2>&1 | awk '/[Ii][Dd]:/ { print $NF }')
    [ -n "$shm_id" ] && ipcrm -m "$shm_id" 2> /dev/null \
        && pass "SysV shared memory segments can be created" \
        || fail "SysV shared memory segments can be created" "$(ipcmk -M 65536 2>&1)"
fi

# The emulated ids, through every call that reports them.
if command -v python3 > /dev/null; then
    ids=$(python3 -c 'import os; print(os.getuid(), os.geteuid(), *os.getresuid(), os.getgid(), os.getegid(), *os.getresgid())' 2>&1)
    want="$(id -u) $(id -u) $(id -u) $(id -u) $(id -u) $(id -g) $(id -g) $(id -g) $(id -g) $(id -g)"
    [ "$ids" = "$want" ] && pass "get*id and getres*id agree with id" \
        || fail "get*id and getres*id agree with id" "got $ids, want $want"
fi

# The ownership record must show through stat by descriptor (fstat), by name (fstatat) and through
# statx alike: ssh, git and sudo check owners with the first two, coreutils (ls -l) and Qt use statx.
if command -v python3 > /dev/null; then
    touch owned && chmod 640 owned
    [ "$(id -u)" = 0 ] && chown 1000:1000 owned
    stats=$(python3 -c '
import os
fd = os.open("owned", os.O_RDONLY)
for s in (os.fstat(fd), os.stat("owned")):
    print("%d:%d:%o" % (s.st_uid, s.st_gid, s.st_mode & 0o7777), end=" ")
' 2>&1)$(stat -c %u:%g:%a owned 2>&1)
    expected="$(id -u):$(id -g):640"
    [ "$(id -u)" = 0 ] && expected=1000:1000:640
    [ "$stats" = "$expected $expected $expected" ] \
        && pass "fstat(), fstatat() and statx() report the recorded owner and mode" \
        || fail "fstat(), fstatat() and statx() report the recorded owner and mode" "got $stats, want $expected"
else
    info "recorded owner through fstat()" "python3 not installed, skipped"
fi

# access() must agree with what actually happens on write (Xwayland trusted it for xkb). Probe a
# root-owned directory that has an ownership record, i.e. one that pacman created.
if [ "$(id -u)" != 0 ]; then
    probe_dir=
    for dir in /usr/lib/firefox /usr/share/xfce4 /usr/share/plasma /usr/share/labwc /etc/pacman.d; do
        if [ -d "$dir" ] && [ -e "$(dirname "$dir")/.proot-meta-file.$(basename "$dir").meta" ]; then
            probe_dir=$dir
            break
        fi
    done
    if [ -n "$probe_dir" ]; then
        says=no; [ -w "$probe_dir" ] && says=yes
        can=no; touch "$probe_dir/.proot-smoke" 2>/dev/null && can=yes && rm -f "$probe_dir/.proot-smoke"
        [ "$says" = "$can" ] && pass "access() matches real permissions ($probe_dir writable: $can)" \
            || fail "access() matches real permissions" "$probe_dir: access() says $says, writing: $can"
    else
        info "access() check" "no root-owned directory with an ownership record found"
    fi
    # Files that came with the rootfs tarball have no record, so any user may change them.
    if touch /usr/.proot-smoke 2>/dev/null; then
        rm -f /usr/.proot-smoke
        info "files without ownership records" "a normal user can write to /usr"
    fi
else
    info "access() check" "needs a normal user, skipped as root"
fi

# libudev monitors, which need a uevent netlink socket (KWin's GPU manager dereferences a NULL
# monitor otherwise).
if command -v python3 >/dev/null 2>&1; then
    monitor=$(python3 - <<'PY' 2>&1
import ctypes, ctypes.util
name = ctypes.util.find_library("udev")
if not name:
    print("skip libudev not installed")
    raise SystemExit
udev = ctypes.CDLL(name)
udev.udev_new.restype = ctypes.c_void_p
udev.udev_monitor_new_from_netlink.restype = ctypes.c_void_p
udev.udev_monitor_new_from_netlink.argtypes = [ctypes.c_void_p, ctypes.c_char_p]
udev.udev_monitor_enable_receiving.argtypes = [ctypes.c_void_p]
udev.udev_monitor_get_fd.argtypes = [ctypes.c_void_p]
monitor = udev.udev_monitor_new_from_netlink(udev.udev_new(), b"udev")
if not monitor:
    print("bad udev_monitor_new_from_netlink returned NULL")
elif udev.udev_monitor_enable_receiving(monitor) != 0:
    print("bad udev_monitor_enable_receiving failed")
elif udev.udev_monitor_get_fd(monitor) < 0:
    print("bad no monitor fd")
else:
    print("ok")
PY
)
    case "$monitor" in
        ok) pass "libudev monitor can be created and enabled" ;;
        skip*) info "libudev monitor" "${monitor#skip }" ;;
        *) fail "libudev monitor can be created and enabled" "${monitor#bad }" ;;
    esac
fi
# The network interfaces, which glibc lists through route netlink (getifaddrs(), if_nameindex())
# and SIOCGIF* ioctls on a Unix socket (if_nametoindex()): Node's os.networkInterfaces(), which
# Vite's dev server calls, Go's net.Interfaces() and Python's socket.if_nameindex().
if command -v python3 > /dev/null; then
    interfaces=$(python3 -c '
import socket
print(len(socket.if_nameindex()), socket.if_nametoindex("lo"), end="")
' 2>&1)
    case "$interfaces" in
        [1-9]*" 1") pass "network interfaces can be listed (${interfaces% 1} of them)" ;;
        *) fail "network interfaces can be listed" "$(echo "$interfaces" | tail -1)" ;;
    esac
fi
[ -r /dev/kgsl-3d0 ] && info "GPU (/dev/kgsl-3d0)" "accessible" || info "GPU (/dev/kgsl-3d0)" "not accessible"
[ -r /dev/dri/renderD128 ] && info "DRM render node" "accessible" || info "DRM render node" "not accessible"
if [ -n "${LD_PRELOAD:-}" ]; then info "LD_PRELOAD" "$LD_PRELOAD"; fi

cd / && rm -rf "$work" 2>/dev/null
rm -rf "$work" 2>/dev/null  # under proot, removing hard-link leftovers can take a second pass
echo "$failures failure(s)"
exit "$failures"
