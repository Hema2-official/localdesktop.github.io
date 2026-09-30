package app.polarbear;

/**
 * Calls into the Rust side. NativeActivity loads the app's library without going through Java,
 * so the library registers these methods itself at startup (see src/android/session.rs).
 */
final class Native {
    private Native() {}

    /** "restart" restarts the desktop session, "quit" stops everything and exits. */
    static native void onAction(String action);

    /** Android's clipboard has something new (see Clipboard.watch). */
    static native void onClipboardChanged();
}
