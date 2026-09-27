#!/bin/sh
# Run a command or script inside a debuggable Local Desktop build's Linux rootfs over adb, with
# the same proot options the app uses. Works while the desktop is broken or not running.
#
# Usage: scripts/dev-shell.sh [-u USER] [-c COMMAND | SCRIPT]
#        (with neither -c nor SCRIPT, the script is read from stdin)
# Environment: LOCALDESKTOP_PACKAGE (default app.polarbear.dev), ANDROID_SERIAL
set -eu

package=${LOCALDESKTOP_PACKAGE:-app.polarbear.dev}
user=root
command=
while getopts u:c: opt; do
    case $opt in
        u) user=$OPTARG ;;
        c) command=$OPTARG ;;
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
# The guest's stdout goes through a pipe: proot's fstat() fails on adb's socket, which breaks cat.
adb shell run-as "$package" sh -c "'
set -o pipefail
mkdir -p $root/.l2s
PROOT_LOADER=$lib/libproot_loader.so PROOT_TMP_DIR=$data PROOT_L2S_DIR=$root/.l2s $lib/libproot.so \
    -r $root -w $home -L --link2symlink --sysvipc --kill-on-exit --root-id -H --uevent-stub \
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
