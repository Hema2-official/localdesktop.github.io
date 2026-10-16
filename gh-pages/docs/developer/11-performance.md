---
title: Measuring performance
---

`scripts/measure.sh` measures what Local Desktop costs on a phone, from a computer where `adb` reaches it:

```bash
scripts/measure.sh -p app.polarbear.dev            # all four tests
scripts/measure.sh -p app.polarbear.dev -s 60 idle # one test, over 60 s
```

| Test | What it does |
|---|---|
| `startup` | Restarts the app (`-n` times, after one start that isn't counted: the first after an install is slower) and times, from the app's process start, when KWin and plasmashell start and when the app's processes go quiet, with the CPU they used until then. |
| `idle` | The CPU the app's processes use while nothing happens on the desktop. |
| `scroll` | Opens Plasma's launcher, picks "All Applications" and swipes its list up and down for the whole interval. The positions are for a 1080x2340 portrait screen at scale 3; `-l`, `-a` and `-w` move them. |
| `memory` | The proportional memory (PSS) of the app's processes. Needs a debuggable build (`run-as`). |

CPU is in percent of one core, from the processes' `/proc/<pid>/stat`, read as adb's shell user (which sees every process); the whole phone's line includes adb's own `input` commands.

Numbers move by about 20 % between runs with the cores' clocks, and a lot more when the phone throttles (compare `scaling_max_freq` with `cpuinfo_max_freq` under `/sys/devices/system/cpu/cpu*/cpufreq/`). Compare builds by alternating them, not against another day's numbers. With the screen off the numbers mean nothing: the output says whether the phone was awake.

For proot's share, `scripts/proot-profile.sh` counts the system calls proot stops programs for and where it spends its time.

## Baseline

Galaxy S21 FE (Snapdragon 888), Android 16, the Plasma preset with the GPU drivers, a debuggable build of `m5-proot-performance` (1138963), portrait:

| | |
|---|---|
| Startup | KWin at 3.9 s, plasmashell at 5.3–5.4 s, quiet at 19–21 s, 16 s of CPU |
| Idle | 0.9 % of a core: WirePlumber 0.5 %, plasmashell 0.2 % |
| Scrolling the launcher | 100 % of a core: KWin 39 %, plasmashell 30 %, the app 25 %, proot 7 % |
| Memory | 665 MiB: plasmashell 302, KWin 85, the app 66, plasma-keyboard 60, Xwayland 25 |

Most of the scrolling cost is moving pixels through the CPU: KWin composites the desktop on the CPU, and the app uploads the parts of KWin's frames that changed into a texture. Both go once KWin composites on the GPU, which needs a patched KWin (see [The GPU](10-gpu.md#the-desktop-on-the-gpu)).

## proot's options

Each option the app gives proot, left out in turn: `scripts/guest/syscall-bench.c` and `scripts/guest/proot-workloads.sh` through `scripts/dev-shell.sh -o OPTIONS`, with proot and the program pinned to one A78 core, three rounds alternating the settings (same phone, 2026-10-16). Times are medians; the workloads list Breeze's icons with `ls -lR`, run `/usr/bin/true` 300 times, and walk Python's library with `stat`, `realpath` and a read of each file.

| Option | Cost | Why it's there |
|---|---|---|
| seccomp (on unless `PROOT_NO_SECCOMP` is set) | It saves: a system call proot doesn't handle costs 0.3 µs with it and 22 µs without; `stat` 26 instead of 36 µs, `open`+`close` 39 instead of 79, `fork`+`wait` 283 instead of 450; the workloads take 0.6 instead of 1.3 s (exec), 2.7 instead of 3.2 (ls), 1.7 instead of 2.3 (Python). | The desktop's programs show `Seccomp: 2` in `/proc/<pid>/status`. |
| `--link2symlink` | Nothing measurable. | Hard links, which Android refuses apps. |
| `--sysvipc` | Nothing measurable. | System V IPC: without it `shmget` fails with `ENOSYS`, and X11 programs lose MIT-SHM. |
| `--root-id` with `-H` | `getuid` 13 µs instead of 0.3 (proot answers it), `readlink` 135 instead of 50 µs, listing a directory 321 instead of 246 µs; `ls -lR` 2.7 instead of 2.3 s. | Ownership and permissions (`root`, `sudo`, package managers). Without `-H` the ownership records show in listings, which doubled the Python walk. |

`--uevent-stub` and `--netlink-route` only answer the netlink sockets Android refuses, so they cost nothing elsewhere. proot's `-L` (fix the size `lstat` reports for symbolic links) is left out: it handles only the `lstat` system calls, which arm64 doesn't have.
