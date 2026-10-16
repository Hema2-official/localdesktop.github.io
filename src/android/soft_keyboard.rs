//! Android's keyboard for the programs on the app's own display (`SoftKeyboard.java`; the
//! compositor's side is `backend::wayland::text_input`). Java calls in from Android's UI thread;
//! what it typed waits here for the event loop, which it wakes.

use crate::android::accessibility::{self, AppUserEvent};
use crate::android::backend::wayland::Keyboard;
use crate::android::session::{app_class, with_activity};
use jni::objects::{JClass, JString, JValue};
use jni::sys::{jboolean, jint};
use jni::{JNIEnv, NativeMethod};
use std::collections::VecDeque;
use std::sync::Mutex;
use winit::platform::android::activity::AndroidApp;

/// What the keyboard did.
#[derive(Debug)]
pub enum Typed {
    /// Text being composed, or finished.
    Text { text: String, composing: bool },
    /// A key pressed `count` times, by its Linux key code.
    Key { code: u32, count: u32 },
    /// It covers this much of the window's bottom now, in pixels.
    Height(i32),
}

static TYPED: Mutex<VecDeque<Typed>> = Mutex::new(VecDeque::new());

/// What the keyboard did since the last time.
pub fn drain() -> Vec<Typed> {
    TYPED.lock().unwrap().drain(..).collect()
}

fn typed(typed: Typed) {
    TYPED.lock().unwrap().push_back(typed);
    if let Some(proxy) = accessibility::event_loop_proxy() {
        let _ = proxy.send_event(AppUserEvent::Keyboard);
    }
}

extern "system" fn on_text(mut env: JNIEnv, _class: JClass, text: JString, composing: jboolean) {
    let Ok(text) = env.get_string(&text) else {
        return;
    };
    typed(Typed::Text {
        text: text.into(),
        composing: composing != 0,
    });
}

extern "system" fn on_key(_env: JNIEnv, _class: JClass, code: jint, count: jint) {
    if code > 0 && count > 0 {
        typed(Typed::Key {
            code: code as u32,
            count: count as u32,
        });
    }
}

extern "system" fn on_height(_env: JNIEnv, _class: JClass, height: jint) {
    typed(Typed::Height(height.max(0)));
}

/// The methods of `Native` that `SoftKeyboard.java` calls.
pub fn natives() -> [NativeMethod; 3] {
    [
        NativeMethod {
            name: "onKeyboardText".into(),
            sig: "(Ljava/lang/String;Z)V".into(),
            fn_ptr: on_text as *mut std::ffi::c_void,
        },
        NativeMethod {
            name: "onKeyboardKey".into(),
            sig: "(II)V".into(),
            fn_ptr: on_key as *mut std::ffi::c_void,
        },
        NativeMethod {
            name: "onKeyboardHeight".into(),
            sig: "(I)V".into(),
            fn_ptr: on_height as *mut std::ffi::c_void,
        },
    ]
}

/// Show or hide Android's keyboard, as the compositor asks.
pub fn apply(android_app: &AndroidApp, keyboard: Keyboard) {
    match keyboard {
        Keyboard::Show {
            input_type,
            options,
            fresh,
        } => with_activity(android_app, "show Android's keyboard", |env, activity| {
            let class = app_class(env, activity, "app.polarbear.SoftKeyboard")?;
            env.call_static_method(
                &class,
                "show",
                "(Landroid/app/Activity;IIZ)V",
                &[
                    activity.into(),
                    JValue::Int(input_type),
                    JValue::Int(options),
                    JValue::Bool(fresh as u8),
                ],
            )?;
            Ok(())
        }),
        Keyboard::Hide => {
            log::info!("Keyboard: hides Android's keyboard");
            with_activity(android_app, "hide Android's keyboard", |env, activity| {
                let class = app_class(env, activity, "app.polarbear.SoftKeyboard")?;
                env.call_static_method(
                    &class,
                    "hide",
                    "(Landroid/app/Activity;)V",
                    &[activity.into()],
                )?;
                Ok(())
            })
        }
    }
}
