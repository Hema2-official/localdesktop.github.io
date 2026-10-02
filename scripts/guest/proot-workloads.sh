#!/bin/bash
# Time four workloads that make many system calls, to compare proot builds and settings. Run it in
# the app's own processes (over SSH, or the in-app terminal): dev-shell.sh runs outside the app's
# cgroup, where Android schedules differently. Compare settings interleaved (A B A B), and skip
# the first rounds after the app starts, while the desktop is still starting too.
#
# Usage: proot-workloads.sh [ROUNDS [WORKLOAD...]]   (workloads: ls exec py tar; default all)
# Prints, per run: seconds of wall time, CPU seconds of the workload, and CPU seconds of the proot
# that traces it (from /proc; "?" when that proot isn't visible).
set -u
rounds=${1:-3}
shift || true
works=("$@")
[ ${#works[@]} -eq 0 ] && works=(ls exec py tar)

icons=/usr/share/icons/breeze
[ -d "$icons" ] || icons=/usr/share/icons
python_lib=$(ls -d /usr/lib/python3.* 2>/dev/null | head -1)
# A package with a few thousand files from pacman's cache, for an extraction.
package=$(ls /var/cache/pacman/pkg/perl-[0-9]*.pkg.tar.* /var/cache/pacman/pkg/python-[0-9]*.pkg.tar.* 2>/dev/null | grep -v '\.sig$' | head -1)
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT

run_ls() { ls -lR "$icons" > /dev/null; }
run_exec() { for i in $(seq 300); do /usr/bin/true; done; }
run_py() {
    python3 - "$python_lib" <<'EOF'
import os, sys
for root, dirs, files in os.walk(sys.argv[1]):
    for name in files:
        path = os.path.join(root, name)
        try:
            os.stat(path)
            os.path.realpath(path)
            with open(path, "rb") as f:
                f.read(4096)
        except OSError:
            pass
EOF
}
run_tar() { mkdir "$scratch/x" && bsdtar -xf "$package" -C "$scratch/x" && rm -rf "$scratch/x"; }

tracer=$(awk '/^TracerPid/{print $2}' /proc/$$/status)
proot_ticks() { awk '{print $14 + $15}' "/proc/$tracer/stat" 2>/dev/null || echo 0; }

printf '%-5s %7s %9s %9s\n' work wall_s cpu_s proot_s
for round in $(seq "$rounds"); do
    for work in "${works[@]}"; do
        case $work in
            py) [ -n "$python_lib" ] || { echo "py: no Python"; continue; };;
            tar) [ -n "$package" ] || { echo "tar: no perl or python package in pacman's cache"; continue; };;
            ls|exec) ;;
            *) echo "unknown workload $work"; exit 2;;
        esac
        before=$(proot_ticks)
        start=$(date +%s%N)
        cpu=$( { TIMEFORMAT='%U %S'; time "run_$work" > /dev/null; } 2>&1 | tail -1 )
        end=$(date +%s%N)
        after=$(proot_ticks)
        printf '%-5s %7.2f %9.2f %9s\n' "$work" "$(( (end - start) / 1000000 ))e-3" \
            "$(echo "$cpu" | awk '{print $1 + $2}')" \
            "$([ "$tracer" -gt 0 ] && echo "$(( after - before ))e-2" | awk '{printf "%.2f", $1}' || echo '?')"
    done
done
