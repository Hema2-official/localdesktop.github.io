# Local Desktop (bleeding edge, VR)

Builds of this fork's `exp-xr` branch: the [bleeding-edge](bleeding-edge.md) app plus VR on
Meta Quest headsets, where Linux VR apps (OpenXR) run in the headset. It's the same app as the
bleeding-edge builds (**Local Desktop (bleeding edge)**, `app.polarbear.edge`): it installs over
them and keeps your Linux system, and a later bleeding-edge build installs over it again, without
VR. On a phone it behaves like the bleeding-edge build it's based on.

## What it does

- On a headset, setup also installs VR support into the Linux system: **Monado**, the OpenXR
  runtime Linux apps use, with a driver that hands their frames to the app, and **Turnip**, the
  Vulkan driver for the headset's GPU, patched for Horizon OS. Each release comes with a
  ready-made download of it (a few MB); where that doesn't work, the headset builds it itself,
  which takes about 15 minutes.
- **A Linux app that starts a VR session switches the headset to immersive mode**, and you're
  back at the desktop's panel when it ends. Apps render at the headset's resolution and 90 Hz, or
  at the refresh rate they ask for (72, 80 or 120 Hz on a Quest 3), with your head, the
  controllers and your hands tracked, and controllers vibrate when apps ask.
- **Passthrough**: apps that draw over your surroundings (OpenXR's alpha blend mode) appear in
  your room.
- The Meta button pauses an app as it does any VR app: Resume goes back to it. Quit asks the
  Linux app to end its VR session.

## Trying it

1. Download the APK (`localdesktop-exp-xr-<date>-<commit>.apk`) from the
   [releases](https://github.com/Hema2-official/localdesktop.github.io/releases).
2. Install it on the headset: it needs developer mode, then `adb install` from a computer (or
   SideQuest). It shows under Unknown Sources in the library.
3. Start it and set it up as the bleeding-edge build (15–20 minutes the first time, with the
   headset charging and awake). When it says "Installation finished", close it and start it
   again.
4. On the desktop, run a VR app from a terminal, for example `hello_xr -g Vulkan2` (it comes with
   VR support), or a Godot 4 project with `--rendering-driver vulkan`. OpenGL apps need the GPU's
   OpenGL through Vulkan: start them with `gpu`, such as `gpu hello_xr -g OpenGL`.

## What to expect

- It's an experiment, tested on one Meta Quest 3 (Horizon OS v81). Other Quests should work the
  same; other headsets aren't tried.
- 120 Hz asks more of the headset than 90 Hz: an app that can't keep up shows its frames late,
  as stutter, and the headset may lower the rate when it gets hot.
- VR drains the battery fast: keep the headset charging for long sessions.
- When an update changes VR support, the next start installs the new one (a few seconds, or the
  build on the headset).
- If an app doesn't switch the headset, look at Monado's log, `~/.cache/monado-service.log`,
  and the app's (`adb logcat | grep "Immersive mode"`).

## Building it yourself

The [Bleeding edge APK](../.github/workflows/bleeding-edge.yml) workflow publishes a release for
every tag named `exp-xr-*` as it does for `bleeding-edge-*`, with the VR support bundle
(`scripts/build-xr-bundle.sh`, built on an arm64 runner in the Arch Linux the app installs) that
the APK names. Without a bundle, headsets build VR support themselves with
`scripts/guest/build-xr.sh`, which the app carries. The Monado driver is in `patches/monado`, the
Turnip patch in `patches/mesa`.
