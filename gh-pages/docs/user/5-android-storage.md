---
title: Share files between Android and Linux
---

## Setup

Before you can use this feature, you must grant "All files access" permission to Local Desktop:

1. Open your device's **Settings** app
2. Navigate to **Apps** or **Applications**
3. Find and select **Local Desktop**
4. Look for **Permissions** or **Special app access**
5. Enable **All files access** (or "Manage all files" on some devices)

:::tip
The exact location of this setting varies by Android version and device manufacturer. Search for "All files access" in your Settings app if you can't find it.
:::

![Storage Setup](/img/storage-setup.webp)

## Usage

You can access your Android storage from within Linux at:
- `/android`
- `~/Android`

![Storage Usage](/img/storage-usage.webp)

## Share and "Open with"

Android's apps can also hand things to the desktop themselves, without the permission above: Local Desktop is in Android's share sheet, and in the "Open with" list for files.

- Files go into the Downloads folder of the desktop's user (the `XDG_DOWNLOAD_DIR` of `xdg-user-dirs`, or `~/Downloads`), with a number added if the name is taken. A single file opens in the desktop's program for it; several open in the file manager, selected.
- A link opens in the desktop's browser, and other text goes into a text file named after its subject.
- Local Desktop comes to the front, and starts first if it wasn't running: what you shared waits until the desktop is up.
- Copying shows its progress when it takes a while, and can be cancelled there. Files are copied, so a big video takes its space twice.

## From the desktop to Android

The desktop's programs, and you in a terminal, can ask Android for things with the `localdesktop` command:

| Command | |
|---|---|
| `localdesktop open-url URL` | Opens a web or mail link (`http`, `https`, `mailto`) with Android's apps, such as its browser. |
| `localdesktop install FILE` | Installs an Android app: an `.apk`, or a bundle of split APKs (`.xapk`, `.apks`). Android asks first, and the first time also whether Local Desktop may install apps at all. A bundle's OBB data isn't copied. |
| `localdesktop open-settings` | Opens Android's settings for Local Desktop: its permissions, battery use and storage. The desktop's menu has it as "Local Desktop Settings". |
| `localdesktop open-terminal` | Opens Local Desktop's terminal, as its notification does. |
| `localdesktop restart-desktop` | Logs out of the desktop and starts it again, as the notification's "Restart desktop" does. |

- In the file manager, opening an `.apk`, `.xapk` or `.apks` installs it ("Install on Android"). As far as Android is concerned such apps come from an unknown source, so Google Play Protect may offer to scan them first.
- Android only lets Local Desktop open its apps and pages while Local Desktop is in front, so a command run over SSH while another app is in front does nothing.

