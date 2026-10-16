# measure-startup-phone.sh — runs on the phone (scripts/measure.sh pushes it): restarts PACKAGE
# and times its desktop's startup from the app's process start: when KWin and plasmashell start,
# when the app's processes go quiet (under 8 CPU ticks per 0.5 s, 16 % of a core, for 3 s after
# plasmashell started), and the CPU they used until then.
# Usage: sh measure-startup-phone.sh PACKAGE USER
pkg=$1
user=$2
am force-stop "$pkg"
sleep 1
am start -n "$pkg/android.app.NativeActivity" > /dev/null
i=0
while :; do
    app=$(ps -A -o PID,USER,NAME | awk -v u="$user" -v p="$pkg" '$2 == u && $3 == p {print $1}')
    [ -n "$app" ] && break
    i=$((i + 1))
    [ $i -gt 100 ] && { echo "no app process"; exit 1; }
    sleep 0.1
done
app_start=$(awk '{print $22}' "/proc/$app/stat")
prev=0
quiet=0
kwin=
shell=
t=0
while [ $t -lt 240 ]; do
    sleep 0.5
    t=$((t + 1))
    total=0
    for p in $(ps -A -o PID,USER | awk -v u="$user" '$2 == u {print $1}'); do
        s=$(cat "/proc/$p/stat" 2>/dev/null) || continue
        name=${s#*\(}
        name=${name%%\)*}
        set -- ${s##*\) }
        total=$((total + ${12} + ${13} + ${14} + ${15}))
        case $name in
            kwin_wayland) [ -z "$kwin" ] && kwin=${20} ;;
            plasmashell) [ -z "$shell" ] && shell=${20} ;;
        esac
    done
    delta=$((total - prev))
    prev=$total
    if [ -n "$shell" ] && [ $delta -lt 8 ]; then quiet=$((quiet + 1)); else quiet=0; fi
    if [ $quiet -ge 6 ]; then
        now=$(awk '{print int($1 * 100)}' /proc/uptime)
        echo "startup: KWin $(( 10 * (kwin - app_start) )) ms, plasmashell $(( 10 * (shell - app_start) )) ms, quiet $(( 10 * (now - 300 - app_start) )) ms, $((total / 100)).$((total % 100 / 10)) s of CPU"
        exit 0
    fi
done
echo "startup: not quiet after 120 s (KWin started: ${kwin:-no}, plasmashell: ${shell:-no})"
