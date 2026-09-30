//! Android's clipboard (`Clipboard.java`), for the thread that shares it with the desktop
//! (`guest::clipboard`). What Android allows when is described there.

use crate::android::session::app_class;
use crate::core::clipboard::{AndroidClip, Kinds};
use jni::errors::Result as JniResult;
use jni::objects::{GlobalRef, JClass, JObject, JObjectArray, JString, JValue};
use jni::sys::{JNIInvokeInterface_, _jobject};
use jni::{JNIEnv, JavaVM};
use winit::platform::android::activity::AndroidApp;

/// What a clip holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Content {
    pub text: String,
    pub html: Option<String>,
}

pub struct AndroidClipboard {
    vm: JavaVM,
    activity: GlobalRef,
    class: GlobalRef,
}

impl AndroidClipboard {
    /// For the calling thread, which stays attached to the Java VM.
    pub fn new(android_app: &AndroidApp) -> Option<Self> {
        let vm = unsafe {
            JavaVM::from_raw(android_app.vm_as_ptr() as *mut *const JNIInvokeInterface_)
        }
        .ok()?;
        let (activity, class) = {
            let mut env = vm.attach_current_thread_permanently().ok()?;
            let activity =
                unsafe { JObject::from_raw(android_app.activity_as_ptr() as *mut _jobject) };
            let found = env.with_local_frame(8, |env| -> JniResult<_> {
                let class = app_class(env, &activity, "app.polarbear.Clipboard")?;
                Ok((env.new_global_ref(&activity)?, env.new_global_ref(class)?))
            });
            match found {
                Ok(found) => found,
                Err(error) => {
                    log::error!("Failed to reach Android's clipboard: {error}");
                    clear_exception(&mut env);
                    return None;
                }
            }
        };
        Some(Self {
            vm,
            activity,
            class,
        })
    }

    /// Call one of `Clipboard`'s methods, which all take the context first.
    fn call<T>(
        &self,
        what: &str,
        call: impl FnOnce(&mut JNIEnv, &JClass, &JObject) -> JniResult<T>,
    ) -> Option<T> {
        let mut env = self.vm.attach_current_thread_permanently().ok()?;
        let class: &JClass = self.class.as_obj().into();
        let result = env.with_local_frame(16, |env| call(env, class, self.activity.as_obj()));
        match result {
            Ok(result) => Some(result),
            Err(error) => {
                log::error!("Failed to {what} Android's clipboard: {error}");
                clear_exception(&mut env);
                None
            }
        }
    }

    /// What is on the clipboard, as far as Android tells without it being read. `None` without
    /// a clip, or while Android keeps it from the app (its window doesn't have focus).
    pub fn describe(&self) -> Option<AndroidClip> {
        self.call("look at", |env, class, context| {
            let fields = env
                .call_static_method(
                    class,
                    "describe",
                    "(Landroid/content/Context;)[Ljava/lang/String;",
                    &[context.into()],
                )?
                .l()?;
            let Some(fields) = strings(env, fields)? else {
                return Ok(None);
            };
            let [stamp, own, text, html] = fields.as_slice() else {
                return Ok(None);
            };
            let has = |field: &Option<String>| field.as_deref().is_some_and(|it| !it.is_empty());
            Ok(Some(AndroidClip {
                stamp: stamp.as_deref().and_then(|it| it.parse().ok()).unwrap_or(0),
                own: has(own),
                kinds: Kinds {
                    text: has(text),
                    html: has(html),
                },
            }))
        })
        .flatten()
    }

    /// Read the clip. Android 12 and later tell the user when it is another app's.
    pub fn read(&self) -> Option<Content> {
        self.call("read", |env, class, context| {
            let fields = env
                .call_static_method(
                    class,
                    "read",
                    "(Landroid/content/Context;)[Ljava/lang/String;",
                    &[context.into()],
                )?
                .l()?;
            let Some(fields) = strings(env, fields)? else {
                return Ok(None);
            };
            let mut fields = fields.into_iter();
            Ok(Some(Content {
                text: fields.next().flatten().unwrap_or_default(),
                html: fields.next().flatten(),
            }))
        })
        .flatten()
    }

    /// Make a clip of `content`. Whether Android took it.
    pub fn write(&self, content: &Content) -> bool {
        self.call("write to", |env, class, context| {
            let text = env.new_string(&content.text)?;
            let html = match &content.html {
                Some(html) => JObject::from(env.new_string(html)?),
                None => JObject::null(),
            };
            env.call_static_method(
                class,
                "write",
                "(Landroid/content/Context;Ljava/lang/String;Ljava/lang/String;)Z",
                &[context.into(), (&text).into(), (&html).into()],
            )?
            .z()
        })
        .unwrap_or(false)
    }

    /// Have changes of the clipboard reported as `guest::Event::AndroidClipboard`, or no longer.
    pub fn watch(&self, on: bool) {
        self.call("watch", |env, class, context| {
            env.call_static_method(
                class,
                "watch",
                "(Landroid/content/Context;Z)V",
                &[JValue::Object(context), JValue::Bool(on as u8)],
            )?;
            Ok(())
        });
    }
}

fn clear_exception(env: &mut JNIEnv) {
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_clear();
    }
}

/// The strings of a Java `String[]`, `None` for `null` (the array or a string in it).
fn strings(env: &mut JNIEnv, array: JObject) -> JniResult<Option<Vec<Option<String>>>> {
    if array.is_null() {
        return Ok(None);
    }
    let array = JObjectArray::from(array);
    let length = env.get_array_length(&array)?;
    let mut strings = Vec::with_capacity(length as usize);
    for index in 0..length {
        let string = env.get_object_array_element(&array, index)?;
        strings.push(if string.is_null() {
            None
        } else {
            Some(env.get_string(&JString::from(string))?.into())
        });
    }
    Ok(Some(strings))
}
