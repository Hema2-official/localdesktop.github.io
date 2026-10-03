//! Whether the screen may turn off while the app is in front (`Screen.java`), for the thread that
//! hears from the desktop when nobody uses it (`guest::screen`).

use crate::android::utils::java::AppClass;
use jni::objects::JValue;
use winit::platform::android::activity::AndroidApp;

pub struct AndroidScreen(AppClass);

impl AndroidScreen {
    /// For the calling thread, which stays attached to the Java VM.
    pub fn new(android_app: &AndroidApp) -> Option<Self> {
        AppClass::new(android_app, "app.polarbear.Screen").map(Self)
    }

    /// Keep the screen on, or let Android's timeout turn it off.
    pub fn keep_on(&self, on: bool) {
        self.0
            .call("change whether the screen stays on", |env, class, activity| {
                env.call_static_method(
                    class,
                    "keepOn",
                    "(Landroid/app/Activity;Z)V",
                    &[activity.into(), JValue::Bool(on.into())],
                )?;
                Ok(())
            });
    }

    /// Android's screen timeout.
    pub fn timeout_ms(&self) -> Option<u32> {
        self.0
            .call("read the screen timeout", |env, class, activity| {
                env.call_static_method(
                    class,
                    "timeout",
                    "(Landroid/app/Activity;)I",
                    &[activity.into()],
                )?
                .i()
            })
            .map(|it| it.max(0) as u32)
    }
}
