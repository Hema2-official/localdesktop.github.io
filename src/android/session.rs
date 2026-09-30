//! The session notification (`SessionService`) and the calls it makes back into Rust
//! (`app.polarbear.Native`). Java sources are in `src/android/java/app/polarbear/`.

use crate::android::guest;
use crate::android::proot::{launch, ssh};
use crate::android::terminal;
use crate::android::utils::application_context::get_application_context;
use crate::android::utils::ndk::run_in_jvm;
use jni::errors::Result as JniResult;
use jni::objects::{JClass, JObject, JString, JValue};
use jni::sys::_jobject;
use jni::{JNIEnv, NativeMethod};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};
use winit::platform::android::activity::AndroidApp;

/// For calls from threads that don't have the app at hand.
static APP: OnceLock<AndroidApp> = OnceLock::new();

/// Load one of the app's own classes. `FindClass` on a native thread only sees the system's.
pub(crate) fn app_class<'local>(
    env: &mut JNIEnv<'local>,
    activity: &JObject,
    name: &str,
) -> JniResult<JClass<'local>> {
    let loader = env
        .call_method(activity, "getClassLoader", "()Ljava/lang/ClassLoader;", &[])?
        .l()?;
    let name = env.new_string(name)?;
    let class = env
        .call_method(
            loader,
            "loadClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[(&name).into()],
        )?
        .l()?;
    Ok(JClass::from(class))
}

fn sdk_version(env: &mut JNIEnv) -> i32 {
    env.get_static_field("android/os/Build$VERSION", "SDK_INT", "I")
        .and_then(|it| it.i())
        .unwrap_or(0)
}

/// Run a JNI call against the activity, logging (and clearing) whatever it throws.
fn with_activity(android_app: &AndroidApp, what: &str, call: impl FnOnce(&mut JNIEnv, &JObject) -> JniResult<()>) {
    run_in_jvm(
        |env, app| {
            let activity = unsafe { JObject::from_raw(app.activity_as_ptr() as *mut _jobject) };
            if let Err(error) = call(env, &activity) {
                log::error!("Failed to {what}: {error}");
                if env.exception_check().unwrap_or(false) {
                    let _ = env.exception_describe();
                    let _ = env.exception_clear();
                }
            }
        },
        android_app.clone(),
    );
}

/// Make the methods of `Native` reach this library. NativeActivity loads it without
/// `System.loadLibrary`, so the JVM can't find it by symbol name.
pub fn register_natives(android_app: &AndroidApp) {
    let _ = APP.set(android_app.clone());
    with_activity(android_app, "register native methods", |env, activity| {
        let class = app_class(env, activity, "app.polarbear.Native")?;
        env.register_native_methods(
            &class,
            &[
                NativeMethod {
                    name: "onAction".into(),
                    sig: "(Ljava/lang/String;)V".into(),
                    fn_ptr: on_action as *mut std::ffi::c_void,
                },
                NativeMethod {
                    name: "onClipboardChanged".into(),
                    sig: "()V".into(),
                    fn_ptr: on_clipboard_changed as *mut std::ffi::c_void,
                },
            ],
        )
    });
}

extern "system" fn on_clipboard_changed(_env: JNIEnv, _class: JClass) {
    guest::notify(guest::Event::AndroidClipboard);
}

extern "system" fn on_action(mut env: JNIEnv, _class: JClass, action: JString) {
    let action: String = env
        .get_string(&action)
        .map(Into::into)
        .unwrap_or_default();
    match action.as_str() {
        // Off the service's main thread: restarting waits for the old session to end.
        "restart" => {
            thread::spawn(launch::restart);
        }
        "quit" => launch::quit(),
        _ => log::warn!("Unknown session action {action}"),
    }
}

/// Bring the notifications up to date, e.g. after the config was read again.
pub fn refresh() {
    if let Some(android_app) = APP.get() {
        start_service(android_app);
    }
}

/// The desktop session ended by itself: say so in the notification, and if it didn't even
/// start, open the terminal to look into it.
pub fn desktop_stopped(failed_start: bool) {
    let Some(android_app) = APP.get() else {
        return;
    };
    start_service(android_app);
    if failed_start {
        terminal::open(android_app, Some("desktop-failed"));
    }
}

/// Ask for the next thing the app needs from the user (showing the notification, running in the
/// background), one system dialog per start (`Permissions.java`).
pub fn ask_permissions(android_app: &AndroidApp) {
    with_activity(android_app, "ask for permissions", |env, activity| {
        let class = app_class(env, activity, "app.polarbear.Permissions")?;
        env.call_static_method(
            &class,
            "askNext",
            "(Landroid/app/Activity;)V",
            &[activity.into()],
        )?;
        Ok(())
    });
}

/// Start the foreground service, with what its notification shows.
pub fn start_service(android_app: &AndroidApp) {
    let local_config = get_application_context().local_config;
    let terminal_url = terminal::url()
        .map_err(|error| log::error!("Failed to start the terminal server: {error}"))
        .ok();
    let login = ssh::login(&local_config);

    with_activity(android_app, "start the session service", |env, activity| {
        let class = app_class(env, activity, "app.polarbear.SessionService")?;
        let intent = env.new_object(
            "android/content/Intent",
            "(Landroid/content/Context;Ljava/lang/Class;)V",
            &[activity.into(), (&class).into()],
        )?;
        let put_string = |env: &mut JNIEnv, key: &str, value: &str| -> JniResult<()> {
            let key = env.new_string(key)?;
            let value = env.new_string(value)?;
            env.call_method(
                &intent,
                "putExtra",
                "(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;",
                &[(&key).into(), (&value).into()],
            )?;
            Ok(())
        };
        if let Some(url) = &terminal_url {
            put_string(env, "terminal_url", url)?;
        }
        if !local_config.problems.is_empty() {
            put_string(env, "config_problems", &local_config.problems.join("\n"))?;
        }
        if launch::stopped() {
            let key = env.new_string("desktop_stopped")?;
            env.call_method(
                &intent,
                "putExtra",
                "(Ljava/lang/String;Z)Landroid/content/Intent;",
                &[(&key).into(), JValue::Bool(1)],
            )?;
            put_string(env, "session_log", launch::SESSION_LOG)?;
        }
        if let Some((user, port)) = &login {
            put_string(env, "ssh_user", user)?;
            let key = env.new_string("ssh_port")?;
            env.call_method(
                &intent,
                "putExtra",
                "(Ljava/lang/String;I)Landroid/content/Intent;",
                &[(&key).into(), JValue::Int(*port as i32)],
            )?;
        }
        let start = if sdk_version(env) >= 26 {
            "startForegroundService"
        } else {
            "startService"
        };
        env.call_method(
            activity,
            start,
            "(Landroid/content/Intent;)Landroid/content/ComponentName;",
            &[(&intent).into()],
        )?;
        Ok(())
    });
}

/// Start the service for the first setup, which keeps it going with the screen off.
pub fn start_setup_service(android_app: &AndroidApp) {
    with_activity(android_app, "start the setup service", |env, activity| {
        let class = app_class(env, activity, "app.polarbear.SessionService")?;
        let intent = env.new_object(
            "android/content/Intent",
            "(Landroid/content/Context;Ljava/lang/Class;)V",
            &[activity.into(), (&class).into()],
        )?;
        let key = env.new_string("setup")?;
        env.call_method(
            &intent,
            "putExtra",
            "(Ljava/lang/String;Z)Landroid/content/Intent;",
            &[(&key).into(), JValue::Bool(1)],
        )?;
        let start = if sdk_version(env) >= 26 {
            "startForegroundService"
        } else {
            "startService"
        };
        env.call_method(
            activity,
            start,
            "(Landroid/content/Intent;)Landroid/content/ComponentName;",
            &[(&intent).into()],
        )?;
        Ok(())
    });
}

/// Setup progress for the notification. Called for every progress message; passes on changes of
/// `progress` and otherwise at most one message every two seconds.
pub fn setup_progress(progress: u16, message: &str, failed: bool) {
    static LAST: Mutex<Option<(Instant, u16)>> = Mutex::new(None);
    let finished = progress >= 100 && !failed;
    {
        let mut last = LAST.lock().unwrap();
        if let Some((at, last_progress)) = *last {
            if !finished
                && !failed
                && last_progress == progress
                && at.elapsed() < Duration::from_secs(2)
            {
                return;
            }
        }
        *last = Some((Instant::now(), progress));
    }
    let Some(android_app) = APP.get() else {
        return;
    };
    with_activity(android_app, "show setup progress", |env, activity| {
        let class = app_class(env, activity, "app.polarbear.SessionService")?;
        let message = env.new_string(message)?;
        env.call_static_method(
            &class,
            "showSetup",
            "(Landroid/content/Context;ILjava/lang/String;ZZ)V",
            &[
                activity.into(),
                JValue::Int(progress as i32),
                (&message).into(),
                JValue::Bool(finished as u8),
                JValue::Bool(failed as u8),
            ],
        )?;
        Ok(())
    });
}

/// Show one of the app's pages (e.g. the terminal) in `WebPageActivity`.
pub fn open_page(android_app: &AndroidApp, url: &str) {
    with_activity(android_app, "open a page", |env, activity| {
        let class = app_class(env, activity, "app.polarbear.WebPageActivity")?;
        let intent = env.new_object(
            "android/content/Intent",
            "(Landroid/content/Context;Ljava/lang/Class;)V",
            &[activity.into(), (&class).into()],
        )?;
        let key = env.new_string("url")?;
        let value = env.new_string(url)?;
        env.call_method(
            &intent,
            "putExtra",
            "(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;",
            &[(&key).into(), (&value).into()],
        )?;
        env.call_method(
            activity,
            "startActivity",
            "(Landroid/content/Intent;)V",
            &[(&intent).into()],
        )?;
        Ok(())
    });
}
