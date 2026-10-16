package app.polarbear;

import android.app.Activity;
import android.content.Context;
import android.os.Build;
import android.text.Editable;
import android.text.Selection;
import android.view.Gravity;
import android.view.View;
import android.view.ViewGroup;
import android.view.WindowInsets;
import android.view.WindowManager;
import android.view.inputmethod.BaseInputConnection;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.InputConnection;
import android.view.inputmethod.InputMethodManager;
import android.widget.FrameLayout;

/**
 * Android's keyboard for the programs on the app's own display (`localdesktop direct`), which
 * have no plasma-keyboard. Android's keyboard types into views, so a one-pixel view that draws
 * nothing takes the focus while it is up and passes on what is typed: text being composed and
 * finished text to the program's text field (text-input-v3, see text_input.rs), deletions and
 * cursor moves as key presses. Enter and the other keys the keyboard sends as key events reach
 * the program through the app's window, as a hardware keyboard's do.
 *
 * The keyboard covers the bottom of the window, which Android doesn't resize for a full-screen
 * app: the view tells the compositor how high it is, which makes the program's window shorter.
 *
 * The view keeps what was typed into the field since it got the keyboard, so that the keyboard
 * knows where a sentence starts and what the word before the cursor is. It knows nothing else of
 * the program's text: a composition can't be made of a word that is there already.
 */
final class SoftKeyboard {
    // Linux key codes.
    private static final int KEY_BACKSPACE = 14;
    private static final int KEY_LEFT = 105;
    private static final int KEY_RIGHT = 106;
    private static final int KEY_DELETE = 111;

    /** As much of what was typed as the keyboard gets to see. */
    private static final int KEPT = 1000;

    private static Field field;
    private static int inputType = EditorInfo.TYPE_CLASS_TEXT;
    private static int imeOptions = EditorInfo.IME_FLAG_NO_EXTRACT_UI;

    private SoftKeyboard() {}

    /**
     * Show the keyboard for a field of this kind (EditorInfo's input type and options). `fresh`:
     * another field, or the same one enabled again.
     */
    static void show(final Activity activity, final int type, final int options,
            final boolean fresh) {
        activity.runOnUiThread(new Runnable() {
            @Override
            public void run() {
                if (activity.isFinishing() || activity.isDestroyed()) {
                    return;
                }
                Field view = field(activity);
                InputMethodManager manager = (InputMethodManager)
                    activity.getSystemService(Context.INPUT_METHOD_SERVICE);
                // The compositor makes room for the keyboard (see Field.onApplyWindowInsets);
                // a window Android resized too would lose that much twice.
                activity.getWindow().setSoftInputMode(
                    WindowManager.LayoutParams.SOFT_INPUT_ADJUST_NOTHING);
                boolean changed = fresh || type != inputType || options != imeOptions;
                inputType = type;
                imeOptions = options;
                view.requestFocus();
                if (changed) {
                    // A connection of the new kind, with nothing typed yet.
                    manager.restartInput(view);
                }
                manager.showSoftInput(view, 0);
            }
        });
    }

    static void hide(final Activity activity) {
        activity.runOnUiThread(new Runnable() {
            @Override
            public void run() {
                Field view = field;
                if (view == null || view.getContext() != activity) {
                    return;
                }
                InputMethodManager manager = (InputMethodManager)
                    activity.getSystemService(Context.INPUT_METHOD_SERVICE);
                manager.hideSoftInputFromWindow(view.getWindowToken(), 0);
                view.clearFocus();
            }
        });
    }

    /** The activity's view, made the first time. Each activity of the app's has its own window. */
    private static Field field(Activity activity) {
        if (field != null && field.getContext() == activity && field.getParent() != null) {
            return field;
        }
        if (field != null && field.getParent() instanceof ViewGroup) {
            ((ViewGroup) field.getParent()).removeView(field);
        }
        Field view = new Field(activity);
        FrameLayout.LayoutParams params = new FrameLayout.LayoutParams(1, 1);
        params.gravity = Gravity.BOTTOM | Gravity.START;
        ((ViewGroup) activity.findViewById(android.R.id.content)).addView(view, params);
        field = view;
        return view;
    }

    private static final class Field extends View {
        private int keyboardHeight;

        Field(Context context) {
            super(context);
            setFocusable(true);
            setFocusableInTouchMode(true);
            setImportantForAccessibility(View.IMPORTANT_FOR_ACCESSIBILITY_NO);
        }

        @Override
        public WindowInsets onApplyWindowInsets(WindowInsets insets) {
            if (Build.VERSION.SDK_INT >= 30) {
                int height = insets.getInsets(WindowInsets.Type.ime()).bottom;
                if (height != keyboardHeight) {
                    keyboardHeight = height;
                    Native.onKeyboardHeight(height);
                }
            }
            return super.onApplyWindowInsets(insets);
        }

        @Override
        public boolean onCheckIsTextEditor() {
            return true;
        }

        @Override
        public InputConnection onCreateInputConnection(EditorInfo info) {
            info.inputType = inputType;
            info.imeOptions = imeOptions;
            info.initialSelStart = 0;
            info.initialSelEnd = 0;
            return new Connection(this);
        }
    }

    private static final class Connection extends BaseInputConnection {
        Connection(View view) {
            super(view, true);
        }

        private static int cursor(Editable text) {
            return Math.max(Selection.getSelectionEnd(text), 0);
        }

        /** Forget the oldest of what was typed, past what the keyboard looks at. */
        private void trim() {
            Editable text = getEditable();
            int extra = text.length() - KEPT;
            if (extra > 0 && getComposingSpanStart(text) < 0) {
                text.delete(0, extra);
            }
        }

        /** Type text into the field, a line at a time: a line break is the Enter key. */
        private static void type(CharSequence text) {
            String[] lines = text.toString().split("\n", -1);
            for (int i = 0; i < lines.length; i++) {
                if (i > 0) {
                    Native.onKeyboardKey(28, 1);
                }
                if (!lines[i].isEmpty()) {
                    Native.onKeyboardText(lines[i], false);
                }
            }
        }

        @Override
        public boolean commitText(CharSequence text, int newCursorPosition) {
            super.commitText(text, newCursorPosition);
            type(text);
            trim();
            return true;
        }

        @Override
        public boolean setComposingText(CharSequence text, int newCursorPosition) {
            super.setComposingText(text, newCursorPosition);
            Native.onKeyboardText(text.toString(), true);
            return true;
        }

        @Override
        public boolean finishComposingText() {
            Editable text = getEditable();
            int start = getComposingSpanStart(text);
            int end = getComposingSpanEnd(text);
            CharSequence composed = start >= 0 && end > start
                ? text.subSequence(start, end).toString()
                : null;
            super.finishComposingText();
            if (composed != null) {
                type(composed);
            }
            trim();
            return true;
        }

        @Override
        public boolean setComposingRegion(int start, int end) {
            // The program has that text already, not as a composition.
            return false;
        }

        @Override
        public boolean deleteSurroundingText(int before, int after) {
            Editable text = getEditable();
            int at = cursor(text);
            // In characters, which is what a key press deletes; what the view doesn't hold of
            // the program's text counts one each.
            int behind = Character.codePointCount(text, Math.max(at - before, 0), at)
                + Math.max(before - at, 0);
            int ahead = Character.codePointCount(text, at, Math.min(at + after, text.length()))
                + Math.max(at + after - text.length(), 0);
            super.deleteSurroundingText(before, after);
            Native.onKeyboardKey(KEY_BACKSPACE, behind);
            Native.onKeyboardKey(KEY_DELETE, ahead);
            return true;
        }

        @Override
        public boolean deleteSurroundingTextInCodePoints(int before, int after) {
            super.deleteSurroundingTextInCodePoints(before, after);
            Native.onKeyboardKey(KEY_BACKSPACE, before);
            Native.onKeyboardKey(KEY_DELETE, after);
            return true;
        }

        @Override
        public boolean setSelection(int start, int end) {
            // The keyboard moves the cursor (its space bar, say): move the program's too.
            Editable text = getEditable();
            int from = cursor(text);
            int to = Math.max(0, Math.min(end, text.length()));
            super.setSelection(start, end);
            if (to < from) {
                Native.onKeyboardKey(KEY_LEFT, Character.codePointCount(text, to, from));
            } else if (to > from) {
                Native.onKeyboardKey(KEY_RIGHT, Character.codePointCount(text, from, to));
            }
            return true;
        }

        @Override
        public boolean performEditorAction(int action) {
            Native.onKeyboardKey(28, 1);
            return true;
        }
    }
}
