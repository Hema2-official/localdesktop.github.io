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
- **One clipboard** for Android and the desktop (`[clipboard] sync`).
- **A normal user**, named on the first start (or later in `[user] username`), with `sudo`.
- **An SSH server** (`[ssh]`), a terminal and a "restart the desktop" button in the app's
  notification, so a broken desktop doesn't lock you out. Restart and Quit log the desktop out
  first, so programs can save or ask about unsaved work; cancelling on the desktop cancels them.
- **The desktop keeps running when you swipe the app out of the recent apps**: open the app or
  tap its notification to get back to it. Quit in the notification ends it.
- **The desktop's notifications on Android** while the app isn't in front, and **the screen
  turning off** after Android's timeout when nobody uses the desktop (a playing video keeps it on).
- **The phone's battery in the desktop's battery widgets** (`[battery] share`), with Plasma's
  switch to block sleep keeping the phone's screen on.
- **Sharing from Android's apps**: Local Desktop is in the share sheet and in "Open with"; files
  land in the desktop's Downloads folder and open there, links in the desktop's browser.
- **The GPU** (Turnip on Qualcomm phones) for Vulkan programs, and Plasma's own drawing.

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
  updated into each other, and uninstalling either deletes its Linux system.
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
