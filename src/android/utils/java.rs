//! One of the app's Java classes with static methods that take the activity first, for a native
//! thread that calls it now and then (the guest link).

use crate::android::session::{self, app_class};
use jni::errors::Result as JniResult;
use jni::objects::{GlobalRef, JClass, JObject};
use jni::sys::{JNIInvokeInterface_, _jobject};
use jni::{JNIEnv, JavaVM};
use winit::platform::android::activity::AndroidApp;

pub struct AppClass {
    vm: JavaVM,
    class: GlobalRef,
}

impl AppClass {
    /// Load `name` (`app.polarbear.…`) for the calling thread, which stays attached to the VM.
    pub fn new(android_app: &AndroidApp, name: &str) -> Option<Self> {
        let vm = unsafe {
            JavaVM::from_raw(android_app.vm_as_ptr() as *mut *const JNIInvokeInterface_)
        }
        .ok()?;
        let class = {
            let mut env = vm.attach_current_thread_permanently().ok()?;
            let activity =
                unsafe { JObject::from_raw(android_app.activity_as_ptr() as *mut _jobject) };
            let found = env.with_local_frame(8, |env| -> JniResult<_> {
                let class = app_class(env, &activity, name)?;
                env.new_global_ref(class)
            });
            match found {
                Ok(found) => found,
                Err(error) => {
                    log::error!("Failed to load {name}: {error}");
                    clear_exception(&mut env);
                    return None;
                }
            }
        };
        Some(Self { vm, class })
    }

    /// Call the class with the newest activity (the app outlives its activities); `what` the
    /// call does, for the log if it fails.
    pub fn call<T>(
        &self,
        what: &str,
        call: impl FnOnce(&mut JNIEnv, &JClass, &JObject) -> JniResult<T>,
    ) -> Option<T> {
        let android_app = session::current_app()?;
        let mut env = self.vm.attach_current_thread_permanently().ok()?;
        let class: &JClass = self.class.as_obj().into();
        let activity = unsafe { JObject::from_raw(android_app.activity_as_ptr() as *mut _jobject) };
        let result = env.with_local_frame(16, |env| call(env, class, &activity));
        match result {
            Ok(result) => Some(result),
            Err(error) => {
                log::error!("Failed to {what}: {error}");
                clear_exception(&mut env);
                None
            }
        }
    }
}

fn clear_exception(env: &mut JNIEnv) {
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_describe();
        let _ = env.exception_clear();
    }
}
