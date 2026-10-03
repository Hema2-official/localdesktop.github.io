use crate::android::session::app_class;
use jni::objects::JObject;
use jni::sys::_jobject;
use jni::JNIEnv;
use winit::platform::android::activity::AndroidApp;

/// A function that can be passed into `run_in_jvm` to show one of the app's pages over the
/// activity while there's no desktop (setup, an unsupported phone): `SetupPage`, a full-screen
/// dialog that takes the keyboard. It runs this thread's Looper for the page, so it doesn't
/// return while the page shows.
pub fn show_webview_popup(env: &mut JNIEnv, android_app: &AndroidApp, url: &str) {
    let activity = unsafe { JObject::from_raw(android_app.activity_as_ptr() as *mut _jobject) };

    env.call_static_method("android/os/Looper", "prepare", "()V", &[])
        .expect("Failed to prepare Looper");

    if let Err(error) = show_page(env, &activity, url) {
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_describe();
            let _ = env.exception_clear();
        }
        panic!("Failed to show {url}: {error}");
    }

    env.call_static_method("android/os/Looper", "loop", "()V", &[])
        .expect("Failed to start Looper");
}

fn show_page(env: &mut JNIEnv, activity: &JObject, url: &str) -> jni::errors::Result<()> {
    let class = app_class(env, activity, "app.polarbear.SetupPage")?;
    let url = env.new_string(url)?;
    env.call_static_method(
        &class,
        "show",
        "(Landroid/app/Activity;Ljava/lang/String;)V",
        &[activity.into(), (&url).into()],
    )?;
    Ok(())
}
