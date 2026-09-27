This directory stores JVM bytecode artifacts that the on-device `cargo run` APK builder can embed without requiring a Java or Kotlin toolchain in Termux.

`classes.dex` contains every class in [`../java/app/polarbear/`](../java/app/polarbear/): the keyboard accessibility service, the foreground service with the session notification, the activity that shows the app's own pages (the terminal), and the bridge to the Rust side.

To regenerate it on a machine with a JDK, the Android platform `android.jar` and r8's `d8` ([Google's Maven repository](https://dl.google.com/android/maven2/com/android/tools/r8/)):

```bash
ANDROID_JAR=path/to/android-33/android.jar R8_JAR=path/to/r8.jar scripts/build-dex.sh
```
