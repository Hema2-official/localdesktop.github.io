//! Android's battery (`Battery.java`), for UPower on the guest's system bus
//! (`guest::system_bus`).

use crate::android::utils::java::AppClass;
use crate::core::upower::AndroidBattery;
use jni::errors::Result as JniResult;
use jni::objects::{JLongArray, JObject, JObjectArray, JString, JValue};
use jni::JNIEnv;
use winit::platform::android::activity::AndroidApp;

/// How many numbers `Battery.read` returns.
const NUMBERS: usize = 13;

pub struct Battery(AppClass);

impl Battery {
    /// For the calling thread, which stays attached to the Java VM.
    pub fn new(android_app: &AndroidApp) -> Option<Self> {
        AppClass::new(android_app, "app.polarbear.Battery").map(Self)
    }

    /// Have changes of the charge or the plug reported as `guest::Event::Battery`, or no longer.
    pub fn watch(&self, on: bool) {
        self.0.call("watch the battery", |env, class, activity| {
            env.call_static_method(
                class,
                "watch",
                "(Landroid/content/Context;Z)V",
                &[activity.into(), JValue::Bool(on.into())],
            )?;
            Ok(())
        });
    }

    /// What Android says about the battery now. `None` before it has said anything.
    pub fn read(&self) -> Option<AndroidBattery> {
        self.0
            .call("read the battery", |env, class, _| {
                let numbers = env.call_static_method(class, "read", "()[J", &[])?.l()?;
                if numbers.is_null() {
                    return Ok(None);
                }
                let numbers = JLongArray::from(numbers);
                if env.get_array_length(&numbers)? as usize != NUMBERS {
                    return Ok(None);
                }
                let mut n = [0i64; NUMBERS];
                env.get_long_array_region(&numbers, 0, &mut n)?;
                let [technology, ..] = described(env, class)?;
                let known = |it: i64| (it != i64::MIN).then_some(it);
                Ok(Some(AndroidBattery {
                    present: n[0] != 0,
                    level: n[1],
                    scale: n[2],
                    status: n[3],
                    plugged: n[4],
                    voltage_mv: n[5],
                    temperature: n[6],
                    technology,
                    // Some phones say 0 for "don't know".
                    charge_uah: known(n[7]).filter(|it| *it > 0),
                    current_ua: known(n[8]),
                    time_to_full_ms: known(n[9]),
                    time_to_empty_ms: known(n[10]),
                    cycles: known(n[11]),
                    design_uah: known(n[12]),
                }))
            })
            .flatten()
    }

    /// The phone's maker and model.
    pub fn phone(&self) -> (String, String) {
        self.0
            .call("name the phone", |env, class, _| {
                let [_, maker, model] = described(env, class)?;
                Ok((maker, model))
            })
            .unwrap_or_default()
    }
}

/// `Battery.describe`: the battery's technology, the phone's maker and model.
fn described(env: &mut JNIEnv, class: &jni::objects::JClass) -> JniResult<[String; 3]> {
    let array = env
        .call_static_method(class, "describe", "()[Ljava/lang/String;", &[])?
        .l()?;
    let array = JObjectArray::from(array);
    let mut strings: [String; 3] = Default::default();
    for (index, string) in strings.iter_mut().enumerate() {
        let element: JObject = env.get_object_array_element(&array, index as i32)?;
        if !element.is_null() {
            *string = env.get_string(&JString::from(element))?.into();
        }
    }
    Ok(strings)
}
