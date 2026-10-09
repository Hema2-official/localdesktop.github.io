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

