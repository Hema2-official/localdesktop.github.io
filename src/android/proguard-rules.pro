# The Rust side reaches these classes and their methods by name through JNI (see
# src/android/session.rs and src/android/clipboard.rs), which R8 can't see: keep them all.
-keep class app.polarbear.** { *; }
