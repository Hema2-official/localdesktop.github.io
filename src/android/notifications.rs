//! Android's notifications for the desktop's (`DesktopNotifications.java`), for the thread that
//! hears them on the session's bus (`guest::notifications`).

use crate::android::utils::java::AppClass;
use winit::platform::android::activity::AndroidApp;

pub struct AndroidNotifications(AppClass);

impl AndroidNotifications {
    /// For the calling thread, which stays attached to the Java VM.
    pub fn new(android_app: &AndroidApp) -> Option<Self> {
        AppClass::new(android_app, "app.polarbear.DesktopNotifications").map(Self)
    }

    /// Show one. `app` names the program, `title` and `text` are plain text.
    pub fn post(&self, app: &str, title: &str, text: &str) {
        self.0
            .call("show a desktop notification", |env, class, activity| {
                let app = env.new_string(app)?;
                let title = env.new_string(title)?;
                let text = env.new_string(text)?;
                env.call_static_method(
                    class,
                    "post",
                    "(Landroid/app/Activity;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V",
                    &[activity.into(), (&app).into(), (&title).into(), (&text).into()],
                )?;
                Ok(())
            });
    }

    /// Take them all off Android.
    pub fn cancel_all(&self) {
        self.0
            .call("take the desktop's notifications off", |env, class, activity| {
                env.call_static_method(
                    class,
                    "cancelAll",
                    "(Landroid/app/Activity;)V",
                    &[activity.into()],
                )?;
                Ok(())
            });
    }
}
