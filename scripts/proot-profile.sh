#!/bin/sh
# Profile proot in a debuggable build (see dev-shell.sh) and print where its time went: stops
# per syscall, and its hottest code paths.
#
# Usage: scripts/proot-profile.sh [-p DIR] [-H HZ] [dev-shell.sh options] -c COMMAND
#        scripts/proot-profile.sh [-H HZ] -S SECONDS
#   -c COMMAND  run COMMAND through dev-shell.sh with the proot build in DIR (default: the output
#               dir of build-on-arm64-linux.sh)
#   -S SECONDS  profile the app's own proot processes instead: restart the app, let it run for
#               SECONDS, then report (symbolized with the build dir's proot.unstripped, so install
#               that build first)
#   -H HZ       stack samples per second of proot CPU time (default 1000, 0 for none)
#   -P          also log every path the programs use, and summarize by program and directory
#   -k DIR      keep the raw profile files in DIR
# Environment: LOCALDESKTOP_PACKAGE (default app.polarbear.dev), LOCALDESKTOP_DEV_DIR
set -eu

here=$(cd "$(dirname "$0")" && pwd)
package=${LOCALDESKTOP_PACKAGE:-app.polarbear.dev}
proot_dir=${LOCALDESKTOP_DEV_DIR:-$HOME/.cache/localdesktop}/proot-build/out
hz=1000
passthrough=
command=
session=
paths=
keep=
while getopts p:H:S:Pk:u:c:o:e:w: opt; do
    case $opt in
        p) proot_dir=$OPTARG ;;
        H) hz=$OPTARG ;;
        S) session=$OPTARG ;;
        c) command=$OPTARG ;;
        P) paths=paths ;;
        k) keep=$OPTARG ;;
        *) passthrough="$passthrough -$opt '$OPTARG'" ;;
    esac
done

data=/data/data/$package/files
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT
as_app() { adb shell run-as "$package" "$@" < /dev/null; }

as_app sh -c "'rm -f $data/.proot-profile.*'"
if [ -n "$session" ]; then
    printf '%s %s\n' "$hz" "$paths" | adb shell run-as "$package" sh -c "'cat > $data/.proot-profile-enable'"
    adb shell am force-stop "$package" < /dev/null
    adb shell am start -n "$package/android.app.NativeActivity" < /dev/null > /dev/null
    sleep "$session"
    as_app rm -f "$data/.proot-profile-enable"
    # The tables and samples are written every 5 s.
    sleep 6
else
    [ -n "$paths" ] && passthrough="$passthrough -e PROOT_PROFILE_PATHS=1"
    eval "sh '$here/dev-shell.sh' -p '$proot_dir' -e PROOT_PROFILE=$data/.proot-profile \
        -e PROOT_PROFILE_HZ=$hz $passthrough -c \"\$command\"" | tail -20
fi

for file in $(as_app ls -a "$data" | tr -d '\r' | grep '^\.proot-profile\.'); do
    as_app cat "$data/$file" > "$out/$file"
done
if [ -n "$keep" ]; then
    mkdir -p "$keep"
    cp "$out"/.proot-profile.* "$keep/"
fi
mkdir -p "$out/libs"
adb pull /apex/com.android.runtime/lib64/bionic/libc.so "$out/libs/" > /dev/null 2>&1 || true

# The busiest proot first.
for table in $(grep -l '^wall' "$out"/.proot-profile.* | xargs -r grep -H '^wall' |
        sed 's/:wall.*stops, \([0-9.]*\) s in proot.*/ \1/' | sort -k2 -rn | cut -d' ' -f1); do
    echo "== $(basename "$table")"
    head -25 "$table"
    if [ -s "$table.samples" ]; then
        python3 "$here/../patches/build-proot-android/profile-report.py" "$table.samples" \
            "$proot_dir/proot.unstripped" "$out/libs" 20
    fi
    if [ -s "$table.paths" ]; then
        echo; echo "== paths by program and syscall"
        cut -f1,2 "$table.paths" | sort | uniq -c | sort -rn | head -20
        echo; echo "== paths by syscall and directory"
        awk -F'\t' '{ d = $3; sub("/[^/]*$", "", d); print $2 "\t" d }' "$table.paths" |
            sort | uniq -c | sort -rn | head -30
    fi
done
