//! The settings page's side in the app (`assets/settings.html`, `SettingsBridge.java`): the page
//! hands `handle` a JSON request through a JavaScript interface and gets JSON back.
//!
//! The page edits the config file, so it needs no desktop: Java loads the library for it, as for
//! the keyboard service, and opening the settings from the launcher or from Android's App info
//! starts nothing else. While the app runs in the same process, the page also restarts the
//! desktop, opens the terminal and tells what waits for a restart.

use crate::android::gpu_drivers;
use crate::android::proot::{launch, ssh};
use crate::android::terminal;
use crate::android::utils::application_context::try_application_context;
use crate::core::config::{self, ARCH_FS_ROOT, CONFIG_FILE, VERSION};
use crate::core::settings;
use jni::objects::{JClass, JString};
use jni::sys::jstring;
use jni::JNIEnv;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;

fn config_path() -> PathBuf {
    Path::new(ARCH_FS_ROOT).join(CONFIG_FILE.trim_start_matches('/'))
}

/// The answer to one of the page's requests.
pub fn handle(request: &str) -> String {
    let request: Value = serde_json::from_str(request).unwrap_or_default();
    let reply = match request["op"].as_str() {
        Some("get") => state(),
        Some("set") => set(&request),
        Some("restart") => restart(),
        Some("quit") => quit(),
        Some("terminal") => terminal(),
        Some("gpu-check") => {
            gpu_drivers::check_in_background();
            state()
        }
        _ => json!({ "error": "Not a request the app knows" }),
    };
    reply.to_string()
}

/// What the page shows.
fn state() -> Value {
    let content = fs::read_to_string(config_path()).unwrap_or_default();
    let file = config::parse_text(&content);
    let context = try_application_context();
    let pending: Vec<Value> = context
        .as_ref()
        .map(|it| settings::pending(&it.local_config, &file))
        .unwrap_or_default()
        .into_iter()
        .map(|(setting, applies)| json!({ "setting": setting, "applies": applies.name() }))
        .collect();
    let login = context
        .as_ref()
        .and_then(|it| ssh::login(&it.local_config))
        .map(|(user, port)| json!({ "user": user, "port": port }));
    json!({
        // Setup made the Linux system, where the config file is.
        "ready": Path::new(ARCH_FS_ROOT).join("etc").is_dir(),
        // The app runs here, with or without a desktop.
        "running": context.is_some(),
        "values": settings::values(&file),
        "problems": config::problems(&content),
        "pending": pending,
        "ssh": login,
        "adreno": Path::new("/dev/kgsl-3d0").exists(),
        "gpu": gpu_drivers::status(),
        "version": VERSION,
    })
}

fn set(request: &Value) -> Value {
    let Some((section, key)) = request["setting"]
        .as_str()
        .and_then(|it| it.split_once('.'))
    else {
        return json!({ "error": "No setting named" });
    };
    if !Path::new(ARCH_FS_ROOT).join("etc").is_dir() {
        return json!({ "error": "Local Desktop isn't set up yet" });
    }
    let path = config_path();
    if let Some(directory) = path.parent() {
        let _ = fs::create_dir_all(directory);
    }
    match config::update_file(&path, |content| {
        settings::set(content, section, key, &request["value"])
    }) {
        Ok(()) => {
            log::info!("Settings: changed {section}.{key}");
            state()
        }
        Err(error) => json!({ "error": error }),
    }
}

fn restart() -> Value {
    if try_application_context().is_none() {
        return json!({ "error": "Local Desktop isn't running" });
    }
    log::info!("Settings: restarts the desktop");
    // Restarting waits for the desktop to log out.
    thread::spawn(launch::restart);
    json!({ "ok": true })
}

fn quit() -> Value {
    if try_application_context().is_none() {
        return json!({ "error": "Local Desktop isn't running" });
    }
    log::info!("Settings: quits");
    thread::spawn(launch::log_out_and_quit);
    json!({ "ok": true })
}

fn terminal() -> Value {
    if try_application_context().is_none() {
        return json!({ "error": "Local Desktop isn't running" });
    }
    match terminal::url() {
        Ok(url) => json!({ "url": url }),
        Err(error) => json!({ "error": format!("No terminal: {error}") }),
    }
}

#[no_mangle]
pub extern "system" fn Java_app_polarbear_SettingsBridge_nativeCall<'local>(
    mut env: JNIEnv<'local>,
    _class: JClass<'local>,
    request: JString<'local>,
) -> jstring {
    let request: String = env.get_string(&request).map(Into::into).unwrap_or_default();
    let reply = handle(&request);
    env.new_string(reply)
        .map(|it| it.into_raw())
        .unwrap_or(std::ptr::null_mut())
}
