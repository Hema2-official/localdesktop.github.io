#!/bin/sh
# Run a command or script inside a debuggable Local Desktop build's Linux rootfs over adb, with
# the same proot options the app uses. Works while the desktop is broken or not running.
#
# Usage: scripts/dev-shell.sh [-u USER] [-p DIR] [-o OPTIONS] [-e VAR=VALUE]... [-c COMMAND | SCRIPT]
#        (with neither -c nor SCRIPT, the script is read from stdin)
#   -p DIR      use the libproot.so and libproot_loader.so in the local DIR instead of the app's
#               (copied to files/.proot-test), e.g. a build from build-on-arm64-linux.sh
#   -o OPTIONS  proot options replacing the app's (everything but -r, -w and the binds)
#   -e VAR=VAL  environment for proot itself, e.g. PROOT_PROFILE=/data/data/<package>/files/prof
#   -w PREFIX   run proot through a command, e.g. "taskset 10"
# Environment: LOCALDESKTOP_PACKAGE (default app.polarbear.dev), ANDROID_SERIAL
set -eu

package=${LOCALDESKTOP_PACKAGE:-app.polarbear.dev}
user=root
command=
proot_dir=
options="-L --link2symlink --sysvipc --kill-on-exit --root-id -H --uevent-stub --netlink-route"
proot_env=
wrapper=
while getopts u:c:p:o:e:w: opt; do
    case $opt in
        u) user=$OPTARG ;;
        c) command=$OPTARG ;;
        p) proot_dir=$OPTARG ;;
        o) options=$OPTARG ;;
        e) proot_env="$proot_env $OPTARG" ;;
        w) wrapper="$OPTARG " ;;
        *) exit 2 ;;
    esac
done
shift $((OPTIND - 1))

payload=$(mktemp)
trap 'rm -f "$payload"' EXIT
if [ -n "$command" ]; then
    printf '%s\n' "$command" > "$payload"
elif [ $# -gt 0 ]; then
    cat "$1" > "$payload"
else
    cat > "$payload"
fi

apk=$(adb shell pm path "$package" < /dev/null | sed -n 's/^package://p' | tr -d '\r')
if [ -z "$apk" ]; then
    echo "$package is not installed" >&2
    exit 1
fi
lib="${apk%/*}/lib/arm64"
data="/data/data/$package/files"
root="$data/arch"

if [ "$user" = root ]; then
    home=/root
    shell="sh /tmp/.dev-shell.sh"
else
    home="/home/$user"
    shell="runuser -u $user -- sh /tmp/.dev-shell.sh"
fi

adb shell run-as "$package" sh -c "'cat > $root/tmp/.dev-shell.sh'" < "$payload"
proot=$lib/libproot.so
loader=$lib/libproot_loader.so
if [ -n "$proot_dir" ]; then
    proot=$data/.proot-test/libproot.so
    loader=$data/.proot-test/libproot_loader.so
    adb shell run-as "$package" mkdir -p "$data/.proot-test" < /dev/null
    for file in libproot.so libproot_loader.so; do
        adb shell run-as "$package" sh -c "'cat > $data/.proot-test/$file && chmod 700 $data/.proot-test/$file'" \
            < "$proot_dir/$file"
    done
fi
# The guest's stdout goes through a pipe: proot's fstat() fails on adb's socket, which breaks cat.
adb shell run-as "$package" sh -c "'
set -o pipefail
mkdir -p $root/.l2s
PROOT_LOADER=$loader PROOT_TMP_DIR=$data PROOT_L2S_DIR=$root/.l2s$proot_env $wrapper$proot \
    -r $root -w $home $options \
    --bind=/dev --bind=/proc --bind=/sys --bind=$root/tmp:/dev/shm \
    --bind=/dev/urandom:/dev/random --bind=/proc/self/fd:/dev/fd \
    --bind=/proc/self/fd/0:/dev/stdin --bind=/proc/self/fd/1:/dev/stdout \
    --bind=/proc/self/fd/2:/dev/stderr \
    --bind=$root/proc/.loadavg:/proc/loadavg --bind=$root/proc/.stat:/proc/stat \
    --bind=$root/proc/.uptime:/proc/uptime --bind=$root/proc/.version:/proc/version \
    --bind=$root/proc/.vmstat:/proc/vmstat \
    --bind=$root/proc/.sysctl_entry_cap_last_cap:/proc/sys/kernel/cap_last_cap \
    --bind=$root/proc/.sysctl_inotify_max_user_watches:/proc/sys/fs/inotify/max_user_watches \
    --bind=$root/sys/.empty:/sys/fs/selinux \
    /usr/bin/env -i HOME=$home LANG=C.UTF-8 TERM=xterm-256color TMPDIR=/tmp \
    USER=$user LOGNAME=$user \
    PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
    $shell < /dev/null | cat
'" < /dev/null
