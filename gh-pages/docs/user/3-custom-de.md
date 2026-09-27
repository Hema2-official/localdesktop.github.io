---
title: Using other Desktop Environments
---

## Desktop presets

Local Desktop can install and start one of two desktops for you. A fresh install asks which one, and saves the answer in the config:

```toml title="/etc/localdesktop/localdesktop.toml"
[desktop]
preset = "plasma" # or "xfce", the default
```

Switching presets installs the other desktop's packages the next time Local Desktop starts. Use `try_preset` to try one once.

The `plasma` preset runs KDE Plasma with KWin directly on Local Desktop's compositor, at your phone's display scale, with Plasma's on-screen keyboard. Its defaults suit a phone: no lock screen, splash screen, file indexing, wallet or animations. They live in `/etc/localdesktop/plasma`, so anything you change in System Settings still takes precedence.

## The `[command]` configs

:::warning
This is an advanced topic. Proceed with your own risk.
:::

A preset fills in 3 commands that set up your desktop environment. Any of them you set yourself takes precedence. For the default `xfce` preset they are:

```toml title="/etc/localdesktop/localdesktop.toml"
[command]
check="pacman -Q noto-fonts && pacman -Q xfce4-session && pacman -Q xfce4-panel && pacman -Q xfce4-settings && pacman -Q xfce4-terminal && pacman -Q thunar && pacman -Q xfdesktop && pacman -Q xfconf && pacman -Q labwc && pacman -Q wlr-randr && pacman -Q xorg-xwayland && pacman -Q xdg-desktop-portal && pacman -Q xdg-desktop-portal-gtk && pacman -Q onboard && pacman -Q firefox && pacman -Q evince && pacman -Q pipewire && pacman -Q pipewire-audio && pacman -Q pipewire-alsa"
install="stdbuf -oL pacman -Syu --needed --noconfirm --noprogressbar noto-fonts xfce4 labwc wlr-randr xorg-xwayland xdg-desktop-portal xdg-desktop-portal-gtk onboard firefox evince pipewire pipewire-audio pipewire-alsa"
launch="export PIPEWIRE_RUNTIME_DIR=/tmp PULSE_SERVER=unix:/tmp/pulse/native; WAYLAND_DISPLAY=/tmp/wayland-0 XDG_SESSION_TYPE=wayland XDG_CURRENT_DESKTOP=XFCE /usr/local/bin/startxfce4-localdesktop 2>&1"
```

You can change these 3 commands to install and launch your custom desktop environment. Please share your successful setups with us and we can put them here to help others.

:::success Tips
The `try_check`, `try_install`, `try_launch` configs are very handy to try different config values **without breaking anything**. Check out the [Configurations](/docs/user/configurations#special-try_-configs) documentation for more details about `try_*`.
:::

### check

The `check` command is used to verify if the required packages are installed and Local Desktop is ready to boot in Wayland mode. In case you are wondering, there are 2 modes in Local Desktop:
- Webview mode (the mode with the official website for documentation on top of a progress bar during installation)
- Wayland mode

If the command in `check` returns success, Local Desktop will boot in Wayland mode. Otherwise, it will enter Webview mode and proceed with the `install` command.

:::info Recipe
You can use `pacman -Q package` to check for a package and `pacman -Qg package-group` to check for a group. Use the `&&` operator to combine multiple checks.
:::

### install

When `check` fails, this command will be executed next. This is exactly the command that Local Desktop runs during the installation process. Some important notes:
- Always put `stdbuf -oL ` in front of the command. [Why?](/docs/developer/bug-cheat-sheet/pacman-progress)
- Always include the `--noconfirm` flag, otherwise, it will get stuck because it is waiting for a confirmation that never comes.
- For a clear output, include `--noprogressbar`.

:::info Recipe
Just keep all the syntax and put all the packages/groups between `pacman -Syu` and the first `--`. For example: `pacman -Syu package-1 package-group-2 package-3 --noconfirm`.
:::

### launch

When `check` returns success, this command will be executed next. This is exactly the command that Local Desktop runs to launch the desktop environment.

This is the most important command to set up your preferred desktop environment. It is also the most complicated command, as it requires a good understanding of display server components. Some important notes:
- When things go wrong, you must check the [logcat](/docs/developer/how-to-logcat) to view the logs.
- If you don't see any error logs, try appending `2>&1` to redirect stderr to stdout.
- The default session is **Xfce on Wayland**. The built-in compositor listens on `/tmp/wayland-0`; the guest runs `startxfce4 --wayland`, which starts labwc as a nested compositor and connects to that socket. Setup also installs `/usr/local/bin/startxfce4-localdesktop` as a thin wrapper around `startxfce4 --wayland`.

:::info Recipe
Put important environment variables at the beginning of the command, for example `XDG_RUNTIME_DIR=/tmp WAYLAND_DISPLAY=wayland-0 XDG_SESSION_TYPE=wayland XDG_CURRENT_DESKTOP=XFCE ...`, then start a Wayland session such as `/usr/local/bin/startxfce4-localdesktop` or `startplasma-wayland`.

For a legacy **X11 session via Xwayland**, start Xwayland first and point the desktop at `DISPLAY=:1`, for example: `Xwayland -hidpi :1 2>&1 & while [ ! -e /tmp/.X11-unix/X1 ]; do sleep 0.1; done; XDG_SESSION_TYPE=x11 DISPLAY=:1 dbus-launch startxfce4 2>&1`.
:::

## Config templates

### KDE Plasma

Use the `plasma` [preset](#desktop-presets). For an X11 session via Xwayland instead:

```toml title="/etc/localdesktop/localdesktop.toml"
[desktop]
preset = "plasma"

[command]
try_launch = "XDG_RUNTIME_DIR=/tmp Xwayland -hidpi :1 2>&1 & while [ ! -e /tmp/.X11-unix/X1 ]; do sleep 0.1; done; XDG_SESSION_TYPE=x11 DISPLAY=:1 dbus-launch startplasma-x11 2>&1"
```

![KDE Plasma on Local Desktop](/img/kde.webp)

Feedback:

- The time zone is not set; however, it is simple to set one with KDE's UI.
- "Could not enter folder tags:." error popups.
- The Wayland session offers notably better performance than the X11 session or PRoot Distro + Termux:X11, but some features (e.g., Spectacle screenshots) may not work. With KDE 7 dropping X11 support, improving Wayland compatibility and being less dependent on Xwayland will be a bigger priority.

### Others

```toml title="/etc/localdesktop/localdesktop.toml"
Feel free to contribute your configs by using the "Edit this page" link below
```
