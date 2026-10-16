---
title: Configurations
---

## Settings page

Local Desktop's settings change the config file below for you: the desktop and its user, what Android and the desktop share, the CPU boost, the GPU drivers and their updates, and the SSH server. They open without starting the desktop, from:

- the app icon: touch and hold it, then **Settings**;
- Android's App info for Local Desktop: **Configure in Local Desktop** (on some phones a gear at the top);
- the desktop's menu: **Local Desktop Settings**, or `localdesktop open-settings` in a terminal.

The page says when a change takes effect: some when the desktop restarts (it has a button for that), the others the next time Local Desktop starts, since it has to install or set something up first (a new user, the other desktop, the SSH server). It also lists the problems the config file has. The commands that start the desktop, and everything else below, stay in the file.

When something is broken, its **Repair** section has two ways back:

- **Reinstall the packages**: the next time Local Desktop starts, every package from Arch Linux ARM installs again, over files that went missing or broke. Your files, and settings you changed in `/etc`, stay. Packages from elsewhere (the AUR) aren't touched, nor the GPU drivers Local Desktop installs. It takes a few minutes and downloads only what isn't in pacman's package cache.
- **Reset the settings**: everything back to its default, except the desktop and the user, and the commands in the file go. The old file is kept as `/etc/localdesktop/localdesktop.toml.bak`.

### Backups

The page's **Backup** section saves the whole Linux system into one file and brings it back:

- **Back up** asks Android's file picker where the file goes: Downloads, an SD card, a USB stick, Drive. A running desktop logs out first, so that its programs can save (you can cancel on the desktop, which cancels the backup), and starts again when you go back to it. pacman's package cache is left out unless you switch it on; pacman downloads those packages again when it needs them. On the test phone, a Plasma system of 4.3 GB took under two minutes and made a file of 1.4 GB.
- **Restore** asks for the file, says what's in it (when it was made, by which app, the desktop and the user), and lays the system out beside the current one, which goes on running meanwhile. The next time Local Desktop starts, the restored system takes its place, setup runs on it once more (and asks for a restart), and the previous system is deleted. Restoring needs room for both for a while.
- On a new phone, or after reinstalling the app, the first start's **Set up your desktop** form has **Restore a backup instead**: pick the file, and setup goes on with the restored system instead of installing a desktop.
- The file is a standard archive (`.tar.zst`): any `tar` that reads zstd lists or unpacks it with the owners, modes and hard links Linux programs saw (`tar --zstd -tvf FILE`). Backups made by any Local Desktop app (the official one, a bleeding-edge build) restore in any other, and on another phone.

To move to this app from an app without the Backup section (the official Local Desktop, say), make the archive inside the old system, from its terminal, with **All files access** granted so that `/android` is Android's storage ([Share files between Android and Linux](5-android-storage.md)):

```sh
sudo tar -I 'zstd -T0' -cpf /android/Download/my-system.tar.zst -C / \
    --exclude='./proc/*' --exclude='./sys/*' --exclude='./dev/*' --exclude='./tmp/*' \
    --exclude='./run/*' --exclude='./android/*' --exclude='./var/cache/pacman/pkg/*' .
```

Then restore `my-system.tar.zst` here. Archives that `tar` made (`.tar`, `.tar.gz`, `.tar.xz` or `.tar.zst`) restore if they hold a Linux system with `/etc` and `/usr`.

On Android 12 and later, the page also has a card about Android's [phantom process killer](1-getting-started.md#disable-phantom-process-killer). Android doesn't let apps see whether it's turned off, so the card says only whether Local Desktop turns it off itself (when you've granted it `WRITE_SECURE_SETTINGS` over adb), and links to the ways to turn it off. Its switch brings back the notice about it that "Don't show again" turned off.

## Config file

On launch, Local Desktop reads the config file located at:

```
/etc/localdesktop/localdesktop.toml
```

If part of the config file is invalid (a typo, a value of the wrong type, broken TOML), a notification lists the problems with their line numbers. A bad value only costs its own section, which falls back to the defaults; the rest of the config still applies. Local Desktop reads the file when it starts and when you restart the desktop from the notification.

Some important notes:
- Although TOML does support multi-line strings, Local Desktop requires each config to fit in a **single line**. You can use `\n` for multi-line config values if needed.
- We use **all lowercase** for config **keys**. For config **values**, the content is **case-sensitive**.

## Config schema

We might draw a table or have a mechanism to generate the config schema automatically here. But for now, please check the code for the schema: [localdesktop/src/core/config.rs#L28-L87](https://github.com/localdesktop/localdesktop.github.io/blob/main/src/core/config.rs#L28-L87).

## Special `try_*` configs

Some configs are so important that a misconfiguration can leave you stuck on a black screen. So we support a special `try_*` variant of each config. These configs have **higher priority**, but only get applied **once**.

For example, you just have to clone a config and prefix it with `try_`:

```toml
[user]
username="root"
try_username="teddy"
```

The next time Local Desktop starts, it will log in as `teddy` instead of `root`. But then the `try_` configs will be commented out like this:

```toml
[user]
username="root"
# try_username="teddy"
```

So if the config didn't work, and you got stuck on a black screen, you can just restart the desktop from Local Desktop's notification (or quit there and open the app again), and things will go back to normal. Swiping the app out of the recent apps doesn't restart it: the desktop keeps running for when you come back. Then you can uncomment the config and try with another value. If the config does work, you just have to remove the `try_` prefix to persist the config.

Some important notes:
- This rule applies to **all** configs.
- It is not required for the `try_` config to be inside the same group as the normal config. But it is strongly recommended to do so, and to put the `try_` variant right under its normal variant.
- If a normal config appears multiple times, the **first** entry is applied. If a `try_` config appears multiple times, the **last** entry is applied. This behavior is not guaranteed, and is subject to change. But in general, it is **invalid** to have duplicate config keys inside a TOML file.
- `try_x` and `x` are not duplicate keys. `try_x` always has higher priority than `x`.

## SSH

Local Desktop can run an SSH server, so you can reach the Linux system from another computer, even when the desktop itself doesn't start. Add your public key:

```toml title="/etc/localdesktop/localdesktop.toml"
[ssh]
authorized_keys = "ssh-ed25519 AAAA... you@computer"
```

The next start installs OpenSSH if needed, adds the key to the user's `~/.ssh/authorized_keys` and starts the server. Then connect with `ssh -p 8022 <username>@<phone's IP address>`.

- The server only runs when someone can log in: a key in `authorized_keys` (separate several keys with `\n`), a key already in the user's `~/.ssh/authorized_keys`, or `password_login = true`.
- `port` defaults to `8022`; Android apps can't use ports below 1024.
- `password_login` defaults to `false`. If you turn it on, set a password first with `passwd`.
- `enabled = false` turns the server off without removing your keys.

## GPU

On phones with a Qualcomm Adreno GPU, Local Desktop installs [Mesa for Android containers](https://github.com/lfdevs/mesa-for-android-container) by lfdevs in place of Arch's Mesa. Arch's build can only reach GPUs through `/dev/dri`, which Android doesn't give apps; this one talks to the Adreno through Android's own driver interface (KGSL). It's downloaded from the project's latest release and checked against its published checksum.

- **Vulkan** programs (games, Zed, `vkcube`) run on the GPU without anything else to do.
- **OpenGL** programs run on the GPU too, through Zink (OpenGL on top of Vulkan), for Wayland and X11 programs alike. Firefox draws on the CPU either way. With `zink = false` they draw on the CPU, unless started through `gpu`, for example `gpu mpv video.mkv`.
- **Plasma** draws its panels and menus with Vulkan when the driver works, and with Qt's software renderer otherwise.
- **Games** and other full-screen programs run fastest with `localdesktop direct`, for example `localdesktop direct vkcube`: the program shows over the desktop on Local Desktop's own display, which takes its GPU frames as they are, where the desktop's compositor would copy each one through the CPU. Wayland programs only; the desktop comes back when the program closes. The on-screen keyboard belongs to the desktop, so it isn't there for such programs.

`pacman -Syu` leaves these packages alone (they're in `IgnorePkg` in `/etc/pacman.conf`), so an update can't swap Arch's Mesa back in. If an update removes a library the drivers need, the next start installs the newest release again, or puts Arch's Mesa back if that doesn't help either.

Once a week, a minute after the desktop starts, Local Desktop looks for a newer release; the settings page's **Check now** looks right away. A newer release installs the next time Local Desktop starts, on its setup page, which then asks for one more start. If it doesn't work with the system's libraries, the release before it goes back on, and that one isn't offered again.

To use Arch's own Mesa instead, or to keep the drivers you have:

```toml title="/etc/localdesktop/localdesktop.toml"
[graphics]
adreno_drivers = false # Arch's Mesa
driver_updates = false # no looking for newer releases
zink = false           # OpenGL programs on the CPU
```

## Performance

Programs in Local Desktop run under proot, which steps in on many of their system calls. While it does, the program waits, so Android sees it as less busy than it is and gives it slower cores at a lower clock, where each of those system calls costs more. Local Desktop tells Android to count the Linux programs as busier than they look. It only makes a difference while programs are running, not when the desktop is idle.

proot itself is what the programs wait for, so while it steps in on system calls in quick succession (installing packages, extracting or listing many files), it also asks Android for the full clock, and lets go again after a quiet second. On a Galaxy S21 FE that takes a quarter to a half off extracting a package or listing thousands of files, an eighth off `npm ci` and a little off the desktop's start, for about the same energy in Android's battery statistics. Work that mostly computes, like a Vite dev server rendering a page, gains little, and the desktop's own drawing isn't affected, since it makes few system calls.

| `cpu_boost` | |
|---|---|
| `balanced` (default) | Starting programs takes about a quarter to a third less time than with `off`, and proot runs at full clock while it's busy. |
| `max` | About 40 % less than `off`. Everything runs at full clock, the desktop's drawing included, at the cost of more battery while programs run. |
| `off` | Leaves the choice of cores and clocks to Android, for proot too. |

```toml title="/etc/localdesktop/localdesktop.toml"
[performance]
cpu_boost = "max"
```

Plasma's battery widget switches it too: its power profiles stand for the boost, Power Save for `off`, Balanced for `balanced` and Performance for `max`, and so does `powerprofilesctl`. A profile picked there is saved here and applies to the running programs at once; proot's own boost follows at the next start.

Programs also turn paths into their full, canonical form with `realpath()`, some of them a lot: Node.js tools such as Vite do it for every file they load. glibc's version checks each part of the path in turn, and proot steps in for every one of them. Local Desktop loads a `realpath()` into every program (through `/etc/ld.so.preload`) that asks proot for the whole answer at once. That makes it three to four times faster, and Vite's first page loads about 15 % sooner; in return, starting a program takes about a quarter of a millisecond longer. To use glibc's own:

```toml title="/etc/localdesktop/localdesktop.toml"
[performance]
fast_realpath = false
```

Both take effect the next time Local Desktop starts.

## Clipboard

Android and the desktop share one clipboard: what you copy in an Android app can be pasted in the desktop's programs, and what you copy there in Android's apps.

- Android only shows its clipboard to the app in front, so what you copied on Android reaches the desktop once Local Desktop's window is in front again. Android 12 and later may then say "Local Desktop pasted from your clipboard": that is the desktop taking your copy, when a program there reads it (Plasma's clipboard history does so right away).
- What you copy on the desktop reaches Android when you switch to another app. Android takes up to about 500 KB of text.
- Images go across too, up to 64 MB: a picture copied in an Android app (Chrome's "Copy image", for one) pastes in the desktop's programs, and an image copied on the desktop (a screenshot in Spectacle, "Copy Image" in a browser, a selection in GIMP) pastes in the Android apps that take pictures, such as messaging and note apps. When the desktop's selection has text as well, as a spreadsheet's cells do, Android gets the text.
- The desktop's compositor has to let clipboard managers in (`ext-data-control-v1` or `wlr-data-control`), which KWin (the Plasma preset), labwc (the Xfce preset) and sway do.

To keep the two clipboards apart:

```toml title="/etc/localdesktop/localdesktop.toml"
[clipboard]
sync = false
```

## Screen

Local Desktop keeps the phone's screen on while it's in front, so that a video or a long build isn't cut short by Android's screen timeout. Once nobody has used the desktop for as long as that timeout, and nothing in it asks to stay awake (a playing video does, as on a PC), it lets Android turn the screen off as usual.

- The desktop's compositor tells when it's idle (`ext-idle-notify-v1`), as KWin (the Plasma preset) does; with one that doesn't, the screen stays on.
- Blocking sleep in Plasma's battery widget keeps the screen on as well (see [Battery](#battery)).
- While the phone is charging, Android's "Stay awake" developer option keeps the screen on anyway.

To keep the screen on for as long as Local Desktop is in front:

```toml title="/etc/localdesktop/localdesktop.toml"
[screen]
sleep_when_idle = false
```

## Desktop notifications

While Local Desktop isn't in front (another app is, or the screen is off), the desktop's notifications show up on Android too: a build that finished while the phone was in your pocket, say. They have a channel of their own, "Desktop notifications", which Android's settings can mute. Once you're back in Local Desktop they come off Android, since the desktop keeps them in its own history. To keep them on the desktop only:

```toml title="/etc/localdesktop/localdesktop.toml"
[notifications]
forward = false
```

## Battery

The desktop's battery widgets show the phone's battery: Plasma's "Power & Battery" in the system tray, and other programs that ask UPower for it, as Xfce's power manager and browsers do. Local Desktop answers for UPower on the desktop's system bus (`/run/dbus/system_bus_socket`) with the charge, whether the phone is charging, the time until full, the temperature and the battery's health; on Android 12 and later also Android's estimate of the time until empty.

- The battery's health is an estimate: what the battery holds when full, going by Android's charge counter, against its capacity when new in the phone's power profile. It comes closest after a good charge, and may be a few percent off the phone's own figure.
- In Plasma, Local Desktop also stands in for PowerDevil, Plasma's power manager, which the Plasma preset leaves out since Android manages the phone's power. The widget's "Manually Block Sleep and Screen Locking" switch keeps the phone's screen on, and so does any program that blocks sleep or the screen saver (video players do).
- The widget's power profiles set the CPU boost (see [Performance](#performance)).
- The system bus has UPower and power profiles alone, so programs that need other system services (NetworkManager, logind, polkit) still find none. If the rootfs runs a system bus of its own, Local Desktop leaves the place to it.

To keep the battery to Android:

```toml title="/etc/localdesktop/localdesktop.toml"
[battery]
share = false
```
