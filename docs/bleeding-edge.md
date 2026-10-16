# Local Desktop (bleeding edge)

Builds of this fork's `bleeding-edge` branch: the work here that hasn't reached the official app
yet. It installs as its own app, **Local Desktop (bleeding edge)**, next to the official Local
Desktop, with its own Linux system, so trying it doesn't touch what you have.

## What's in it

- **KDE Plasma** as a desktop to choose on the first start, with an on-screen keyboard, next to
  Xfce. Plasma runs straight on the app's compositor.
- **A faster proot** (the layer that runs Linux programs on Android): programs start and read
  files with far fewer stops. Package installs and Node.js tools run about twice as fast as in
  the official app, the desktop starts three times as fast, and the file quirks that broke
  `git`, `pnpm`, `makepkg`, KWin and others are fixed.
- **Package downloads from the mirrors that answer fastest from where you are**: the first start
  times Arch Linux ARM's mirrors and puts the fastest first. The Plasma desktop's 820 MB of
  packages downloaded in 35 s on the test phone, against 2.5 to 14 minutes through the default
  mirror redirector.
- **One clipboard** for Android and the desktop (`[clipboard] sync`): text and images, both ways.
- **A normal user**, named on the first start (or later in `[user] username`), with `sudo`.
- **An SSH server** (`[ssh]`), a terminal and a "restart the desktop" button in the app's
  notification, so a broken desktop doesn't lock you out. Restart and Quit log the desktop out
  first, so programs can save or ask about unsaved work; cancelling on the desktop cancels them.
- **The desktop keeps running, sound included, when you switch apps or swipe the app out of the
  recent apps**: open the app or tap its notification to get back to it. Quit in the notification
  ends it.
- **The desktop's notifications on Android** while the app isn't in front, and **the screen
  turning off** after Android's timeout when nobody uses the desktop (a playing video keeps it on).
- **The phone's battery in the desktop's battery widgets** (`[battery] share`), with Plasma's
  switch to block sleep keeping the phone's screen on, and its power profiles switching the CPU
  boost (`[performance] cpu_boost`).
- **Sharing from Android's apps**: Local Desktop is in the share sheet and in "Open with"; files
  land in the desktop's Downloads folder and open there, links in the desktop's browser.
- **The other way round**: files go from the file manager to Android's apps through Android's
  share sheet ("Share with Android Apps…"); the `localdesktop` command opens links with Android's
  apps, Android's settings for the app and its terminal, and restarts the desktop; Android apps
  (`.apk`, `.xapk`, `.apks`) install from the file manager, with Android asking first.
- **A settings page** that needs no desktop: touch and hold the app icon, then Settings (or
  Android's App info, or the desktop's menu). It changes the config file line by line and says
  when each change takes effect. When something is broken, it reinstalls every package at the
  next start, or puts the settings back to their defaults (keeping the old file).
- **Backups of the Linux system** from the settings page, into a file you pick (Downloads, an SD
  card, Drive), and restores of them: a standard `.tar.zst` that any Local Desktop app restores,
  so moving between the official app and this one, or to another phone, keeps your system. A
  first start can restore one instead of installing a desktop ("Restore a backup instead").
- **The GPU** (Turnip on Qualcomm phones) for Vulkan programs, Plasma's own drawing, and
  OpenGL programs through Zink (`[graphics] zink = false` turns that off).
  `localdesktop direct <program>` runs a Wayland program (a game, a video player) over the
  desktop, fullscreen, on the app's own display: it hands the app its GPU buffers (dma-bufs)
  instead of having its frames copied through the desktop, for half the CPU or less.

The details are in the commits of the branch; the user docs in `gh-pages/docs/user/` describe
the settings (`4-configurations.md`), the non-root user (`2-creating-a-non-root-user.md`) and
sharing with Android (`5-android-storage.md`).

## Installing

1. Download the APK from the [latest release](https://github.com/Hema2-official/localdesktop.github.io/releases) (the file named
   `localdesktop-bleeding-edge-<date>-<commit>.apk`).
2. Open it on the phone. Android asks to allow installs from that source (the browser or the
   file manager) the first time.
3. Start **Local Desktop (bleeding edge)**, choose a user name (or leave the field empty for
   root) and a desktop. The first start downloads and
   installs Arch Linux and the desktop: 15–20 minutes on a good connection, with the phone
   awake and unlocked (it needs the network throughout). Allow the notification when asked: it
   keeps the setup going when the screen turns off.
4. When it says "Installation finished, please restart the app", close it from the recent apps
   and start it again.

Settings go in `/etc/localdesktop/localdesktop.toml` inside the Linux system (the terminal in
the notification gets you there), for example:

```toml
[user]
username = "teddy"      # made on the next start, with sudo

[ssh]
authorized_keys = "ssh-ed25519 AAAA... you@computer"

[clipboard]
sync = true
```

## What to expect

- It's a preview. Things are tested on one phone (a Galaxy S21 FE with Android 16); on yours,
  the desktop may not come up. The notification's terminal and `/var/log/localdesktop-session.log`
  show why; please report it with that log.
- Newer bleeding-edge builds install over older ones. The official app and this one can't be
  updated into each other, and uninstalling either deletes its Linux system: back it up first
  (the settings page's Backup section; for the official app, see "Backups" in
  `gh-pages/docs/user/4-configurations.md`).
- Android's phantom process killer and battery optimization affect it like the official app:
  see the app's own pages when it warns about them.

## Building it yourself

The [Bleeding edge APK](../.github/workflows/bleeding-edge.yml) workflow builds the APK on
every push to the `bleeding-edge` branch (download it from the run), and publishes a release
for every tag named `bleeding-edge-*`. It needs the signing key in the repository secrets
`ANDROID_KEYSTORE_BASE64`, `ANDROID_KEYSTORE_PASSWORD`, `ANDROID_KEY_ALIAS` and
`ANDROID_KEY_PASSWORD`; without a stable key, builds couldn't install over each other.

On an aarch64 Linux machine, `scripts/build-dev-apk.sh` builds the same thing as the
side-by-side dev app (`app.polarbear.dev`), see `scripts/setup-arm64-linux-toolchain.sh`.
