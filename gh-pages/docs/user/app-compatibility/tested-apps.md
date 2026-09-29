---
title: Tested apps
sidebar_position: 1
---

Linux programs run under proot on Local Desktop, which translates their file paths and emulates the root user. Most programs don't notice. These were checked on a Samsung Galaxy S21 FE (Snapdragon 888), as a normal user and as root, with the Plasma desktop.

## Development

| Tool | Works | Checked |
|---|---|---|
| git | ✅ | init, commit, clone over HTTPS, removing a repository |
| Python (`venv`, `pip`) | ✅ | creating a venv and installing a package from PyPI |
| Node.js, npm, pnpm, yarn | ✅ | installing packages; pnpm works with its default settings; a Vite dev server (`pnpm run dev`), reachable from the network |
| Go | ✅ | building and running a module |
| Rust (cargo) | ✅ | building and running a crate |
| C (gcc, make, cmake) | ✅ | building KDE's KSvg library from source, with its tests |
| makepkg | ✅ | building a package as a normal user (it refuses to run as root, as on any Arch system) |
| Java (OpenJDK) | ✅ | a Swing window |

`scripts/guest/dev-tools-check.sh` in the repository runs these checks.

## Desktop apps

| App | Works | Window after |
|---|---|---|
| Konsole, Dolphin | ✅ | 1–2 s |
| [Firefox](./firefox.md) | ✅ | 2.5 s |
| Chromium | ✅ | 2 s; runs with `--no-sandbox` |
| LibreOffice | ✅ | 4.5 s |
| [GIMP](./gimp.md) | ✅ | 22 s |

`scripts/guest/gui-apps-check.sh` starts these in the running desktop and times them. [Visual Studio Code](./visual-studio-code.md) works too.

## What doesn't work

- **Flatpak and Snap** need Linux user namespaces, which Android doesn't give apps.
- **Docker and Podman** need namespaces and cgroups too.
- **AppImages** can't mount themselves (no FUSE); `--appimage-extract-and-run` unpacks them instead.
- **Sticky `/tmp`**: proot doesn't enforce the sticky bit, so users can delete each other's files in `/tmp`.
- **`ip link` and `ip addr`** report "Permission denied": Android doesn't give apps the list of network interfaces. Local Desktop answers for it when programs ask the usual way, so Node.js, Python and Go see the interfaces and their addresses (but not hardware addresses), but `ip` asks differently. `ifconfig` from `net-tools` shows the IPv4 addresses.
