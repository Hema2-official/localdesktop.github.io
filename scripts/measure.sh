#!/bin/sh
# measure.sh — what Local Desktop costs on a phone: the time to the desktop, the CPU its
# processes use while the desktop is idle and while Plasma's launcher scrolls, and their memory.
# Runs where adb reaches the phone, which has to stay awake and unlocked (with the screen off
# the numbers are worthless; the output says the screen's state). Made for the Plasma preset:
# `startup` waits for plasmashell, `scroll` opens the launcher at -l X,Y, picks "All
# Applications" at -a X,Y and swipes its list at -w X,Y1,Y2 (defaults: a 1080x2340 portrait
# screen at scale 3). `memory` needs a debuggable build (run-as).
#
# CPU percentages are of one core and vary about 20 % between runs with the cores' clocks: compare
# builds in alternation, not against another day's numbers.
#
# Usage: scripts/measure.sh [-p PACKAGE] [-n RUNS] [-s SECONDS] [-l X,Y] [-a X,Y] [-w X,Y1,Y2]
#                           [startup] [idle] [scroll] [memory]      (all four without any)
set -eu

package=app.polarbear.dev
runs=2
seconds=30
launcher=105,2235
all_apps=246,907
swipe=850,1800,900
while getopts p:n:s:l:a:w: option; do
    case $option in
        p) package=$OPTARG ;;
        n) runs=$OPTARG ;;
        s) seconds=$OPTARG ;;
        l) launcher=$OPTARG ;;
        a) all_apps=$OPTARG ;;
        w) swipe=$OPTARG ;;
        *) sed -n '2,/^set -eu/p' "$0" | sed '$d; s/^# \{0,1\}//' >&2; exit 2 ;;
    esac
done
shift $((OPTIND - 1))
[ $# -gt 0 ] || set -- startup idle scroll memory

phone() { adb shell "$@" < /dev/null; }
uid=$(phone pm list packages -U "$package" | sed -n "s/^package:$package uid://p" | tr -d '\r')
[ -n "$uid" ] || { echo "$package isn't installed" >&2; exit 1; }
user="u0_a$((uid - 10000))"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

screen() { phone dumpsys power | grep -m1 -o 'mWakefulness=[A-Za-z]*' | cut -d= -f2; }

# The CPU ticks of the app's processes, and the phone's in all, now.
snapshot() {
    phone "head -1 /proc/stat; for p in \$(ps -A -o UID=,PID= | awk '\$1==$uid {print \$2}'); do cat /proc/\$p/stat 2>/dev/null; done"
}

# CPU use between two snapshots: the app's processes, most first, and the whole phone's.
cpu_report() {
    python3 - "$1" "$2" "$3" "$4" <<'PY'
import os, sys
label, seconds = sys.argv[1], float(sys.argv[2])
hz = os.sysconf("SC_CLK_TCK")
def load(path):
    procs, busy = {}, 0
    for line in open(path):
        line = line.strip()
        if line.startswith("cpu "):
            fields = [int(x) for x in line.split()[1:]]
            busy = sum(fields) - fields[3] - fields[4]
        elif line:
            pid, rest = line.split(" (", 1)
            name, fields = rest.rsplit(") ", 1)
            fields = fields.split()
            procs[pid] = (name, int(fields[11]) + int(fields[12]))
    return procs, busy
(a, busy_a), (b, busy_b) = load(sys.argv[3]), load(sys.argv[4])
rows = sorted(((ticks - a.get(pid, (name, 0))[1], name) for pid, (name, ticks) in b.items()), reverse=True)
total = sum(ticks for ticks, _ in rows)
print(f"{label}: {100 * total / hz / seconds:.1f} % of a core over {seconds:.0f} s"
      f" (the whole phone {100 * (busy_b - busy_a) / hz / seconds:.1f} %)")
for ticks, name in rows[:8]:
    if ticks:
        print(f"  {100 * ticks / hz / seconds:5.1f} %  {name}")
PY
}

for what in "$@"; do
    case $what in
    startup)
        phone mkdir -p /data/local/tmp/localdesktop-measure
        adb push "$(dirname "$0")/measure-startup-phone.sh" /data/local/tmp/localdesktop-measure/ > /dev/null
        # The first start after an install is slower: count from the second.
        for run in $(seq 0 "$runs"); do
            line=$(phone sh /data/local/tmp/localdesktop-measure/measure-startup-phone.sh "$package" "$user")
            [ "$run" -eq 0 ] || echo "$line"
        done
        phone rm -rf /data/local/tmp/localdesktop-measure
        sleep 10
        ;;
    idle)
        snapshot > "$work/a"
        sleep "$seconds"
        snapshot > "$work/b"
        cpu_report "idle (screen $(screen))" "$seconds" "$work/a" "$work/b"
        ;;
    scroll)
        x=${swipe%%,*}
        y1=${swipe#*,}
        y2=${y1#*,}
        y1=${y1%%,*}
        phone input tap "${launcher%,*}" "${launcher#*,}"
        sleep 2
        phone input tap "${all_apps%,*}" "${all_apps#*,}"
        sleep 2
        snapshot > "$work/a"
        end=$(($(date +%s) + seconds))
        while [ "$(date +%s)" -lt "$end" ]; do
            phone input swipe "$x" "$y1" "$x" "$y2" 300
            phone input swipe "$x" "$y2" "$x" "$y1" 300
        done
        snapshot > "$work/b"
        phone input keyevent KEYCODE_ESCAPE
        cpu_report "scrolling the launcher (screen $(screen))" "$seconds" "$work/a" "$work/b"
        ;;
    memory)
        phone run-as "$package" sh -c "'uid=\$(id -u); for p in /proc/[0-9]*; do [ \"\$(stat -c %u \$p 2>/dev/null)\" = \"\$uid\" ] || continue; pss=\$(sed -n \"s/^Pss: *\([0-9]*\) kB/\1/p\" \$p/smaps_rollup 2>/dev/null); echo \"\${pss:-0} \$(cat \$p/comm)\"; done'" |
        python3 -c '
import sys
rows = [(int(line.split(" ", 1)[0]), line.split(" ", 1)[1].strip()) for line in sys.stdin if line.strip()]
print(f"memory: {sum(kb for kb, _ in rows) / 1024:.0f} MiB of PSS")
for kb, name in sorted(rows, reverse=True)[:8]:
    print(f"  {kb / 1024:6.1f} MiB  {name}")
'
        ;;
    *)
        echo "measure.sh: no test called $what" >&2
        exit 2
        ;;
    esac
done
