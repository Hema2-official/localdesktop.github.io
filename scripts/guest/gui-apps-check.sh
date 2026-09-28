#!/bin/sh
# Start GUI apps in the running Plasma session and time how long until their window shows up
# (KWin's window search over D-Bus), then close them. Run it inside the guest as the session's
# user, e.g. over SSH: ssh -p 8022 root@phone 'sh -s' < scripts/guest/gui-apps-check.sh
# Usage: gui-apps-check.sh [APP...] (default: every known app that is installed)

shell_pid=$(pgrep -x plasmashell | head -1)
if [ -z "$shell_pid" ]; then
    echo "no Plasma session running"
    exit 1
fi
# The session's environment (display, D-Bus, runtime dir).
eval "$(tr '\0' '\n' < "/proc/$shell_pid/environ" | grep -E '^(DISPLAY|XAUTHORITY|WAYLAND_DISPLAY|XDG_RUNTIME_DIR|DBUS_SESSION_BUS_ADDRESS|XDG_CURRENT_DESKTOP|QT_QPA_PLATFORMTHEME|XDG_SESSION_TYPE|XDG_DATA_DIRS|XDG_CONFIG_DIRS|LD_PRELOAD|MESA_VK_WSI_DEBUG|MOZ_SHM_NO_SEALS)=' | sed "s/'/'\\\\''/g; s/=\(.*\)/='\1'/; s/^/export /")"

sandbox=
[ "$(id -u)" = 0 ] && sandbox=--no-sandbox

# app NAME WINDOW_MATCH COMMAND...
app() {
    name=$1
    match=$2
    shift 2
    command -v "$1" > /dev/null 2>&1 || { echo "SKIP  $name (not installed)"; return; }
    if [ -n "$only" ] && ! echo " $only " | grep -q " $name "; then
        return
    fi
    start=$(date +%s%N)
    setsid "$@" > "/tmp/gui-check-$name.log" 2>&1 < /dev/null &
    pid=$!
    found=
    for _ in $(seq 1 120); do
        if qdbus6 --literal org.kde.KWin /WindowsRunner org.kde.krunner1.Match "$match" 2> /dev/null | grep -qi "$match"; then
            found=yes
            break
        fi
        kill -0 "$pid" 2> /dev/null || [ -n "$(pgrep -f -- "$1" | head -1)" ] || break
        sleep 0.5
    done
    elapsed=$(( ($(date +%s%N) - start) / 1000000 ))
    if [ -n "$found" ]; then
        echo "PASS  $name: window after $elapsed ms"
    else
        echo "FAIL  $name: no window after $elapsed ms: $(tail -2 "/tmp/gui-check-$name.log" | tr '\n' ' ')"
        failures=$((failures + 1))
    fi
    sleep 2
    pkill -f -- "$1" 2> /dev/null
    kill "$pid" 2> /dev/null
    sleep 1
}

only="$*"
failures=0
work=$(mktemp -d /tmp/gui-check.XXXXXX)
printf 'import javax.swing.*;\npublic class Hello { public static void main(String[] a) { SwingUtilities.invokeLater(() -> { JFrame f = new JFrame("Java Swing check"); f.setSize(300, 200); f.setVisible(true); }); } }\n' > "$work/Hello.java"

app konsole Konsole konsole
app dolphin Dolphin dolphin
app firefox Firefox firefox --new-instance about:blank
app chromium Chromium chromium $sandbox --user-data-dir="$work/chromium" about:blank
app libreoffice LibreOffice libreoffice --writer --norestore
app gimp GIMP gimp --no-splash
app java "Java Swing check" java "$work/Hello.java"

rm -rf "$work"
echo "$failures failure(s)"
exit "$failures"
