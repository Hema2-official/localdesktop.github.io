use crate::{
    android::{
        accessibility::{self, register_event_loop_proxy, AppUserEvent},
        app::build::PolarBearApp,
        guest,
        proot::launch,
        session,
        utils::{
            application_context::ApplicationContext,
            fullscreen_immersive::{enable_fullscreen_immersive_mode, keep_screen_on},
            ndk::run_in_jvm,
        },
    },
    core::config,
};
use sentry::integrations::log::{LogFilter, SentryLogger};
use std::sync::{Condvar, Mutex, Once};
use std::time::Duration;
use winit::{
    event_loop::{ControlFlow, EventLoop},
    platform::android::{activity::AndroidApp, EventLoopBuilderExtAndroid},
};

/// The app outlives its activities. Swiping Local Desktop out of the recents destroys the
/// activity, but the session service keeps the process running, and with it the desktop: the
/// next activity (opening the app again, tapping the notification) takes the app over. Each
/// activity runs `android_main` on a thread of its own.
static APP: Mutex<Slot> = Mutex::new(Slot::Unbuilt);
static APP_PARKED: Condvar = Condvar::new();

/// How long a new activity waits for the one before it to let go of the app.
const LET_GO_TIMEOUT: Duration = Duration::from_secs(10);

enum Slot {
    /// No activity has built the app yet.
    Unbuilt,
    /// An activity runs it.
    Running,
    /// Between activities.
    Parked(Parked),
}

/// The app runs on one activity's thread at a time, and only moves between them.
struct Parked(PolarBearApp);
unsafe impl Send for Parked {}

#[no_mangle]
fn android_main(android_app: AndroidApp) {
    static PROCESS: Once = Once::new();
    PROCESS.call_once(|| start_process(&android_app));

    session::register_natives(&android_app);

    run_in_jvm(enable_fullscreen_immersive_mode, android_app.clone());
    run_in_jvm(keep_screen_on, android_app.clone());

    // Before building an event loop: there is one at a time.
    let parked = take_app();

    let event_loop = EventLoop::<AppUserEvent>::with_user_event()
        .with_android_app(android_app.clone())
        .build()
        .expect("Failed to create event loop");
    register_event_loop_proxy(event_loop.create_proxy());

    // ControlFlow::Poll continuously runs the event loop, even if the OS hasn't
    // dispatched any events. This is ideal for games and similar applications.
    // event_loop.set_control_flow(ControlFlow::Poll);

    // ControlFlow::Wait pauses the event loop if no events are available to process.
    // This is ideal for non-game applications that only update in response to user
    // input, and uses significantly less power/CPU time than ControlFlow::Poll.
    event_loop.set_control_flow(ControlFlow::Wait);

    let mut app = match parked {
        Some(app) => {
            log::info!("A new activity takes the app over");
            guest::notify(guest::Event::Activity);
            app.attach(android_app)
        }
        // Phase 1: Setup
        None => PolarBearApp::build(android_app),
    };

    // Phase 2: Run, until the activity is gone (Android waits for this thread to return before
    // it lets the activity go) or another one takes over.
    event_loop.run_app(&mut app).expect("Failed to run app");

    app.detach();
    park(app);
}

/// What the process needs once, whichever activity comes first.
fn start_process(android_app: &AndroidApp) {
    std::env::set_var("RUST_BACKTRACE", "full");
    // The bundled libxkbcommon defaults to the official package's rootfs for its keymaps.
    std::env::set_var(
        "XKB_CONFIG_ROOT",
        format!("{}/usr/share/X11/xkb", config::ARCH_FS_ROOT),
    );
    let guard = sentry::init((
        config::SENTRY_DSN,
        sentry::ClientOptions {
            release: sentry::release_name!(),
            // Capture user IPs and potentially sensitive headers when using HTTP server integrations
            // see https://docs.sentry.io/platforms/rust/data-management/data-collected for more info
            send_default_pii: true,
            enable_logs: true,
            ..Default::default()
        },
    ));
    // For as long as the process lives. It ends in `process::exit`, which drops nothing anyway.
    std::mem::forget(guard);

    // Wrap the Android logger with Sentry's logger
    let logger = SentryLogger::with_dest(android_logger::AndroidLogger::default()).filter(|md| {
        // How to use log::*() macros in this project:
        // - log::error!() for critical errors that maintainers should be NOTIFIED about via email
        // - log::trace!() for very detailed debugging information that need NOT to be captured with telemetry
        // - log::info!() for everything else, maintainers can check this with Sentry's Logs
        match md.level() {
            // Capture error records as Sentry events
            // These are grouped into issues, representing high-severity errors to act upon
            log::Level::Error => LogFilter::Event,
            // Ignore trace level records, as they're too verbose
            log::Level::Trace => LogFilter::Ignore,
            // Capture everything else as a log
            _ => LogFilter::Log,
        }
    });

    #[cfg(debug_assertions)] // Enable verbose logging in debug builds
    let log_level = log::LevelFilter::Trace;
    #[cfg(not(debug_assertions))]
    let log_level = log::LevelFilter::Info;
    if log::set_boxed_logger(Box::new(logger)).is_ok() {
        log::set_max_level(log_level);
    } else {
        android_logger::init_once(android_logger::Config::default().with_max_level(log_level));
    }

    ApplicationContext::build(android_app);
}

/// The app, if an activity before this one built it. If that one still runs it (a new activity
/// can come before the last one is gone), ask it to let go, and wait.
fn take_app() -> Option<PolarBearApp> {
    let mut slot = APP.lock().unwrap();
    if let Slot::Running = *slot {
        log::info!("Asking the last activity to let go of the app");
        if let Some(proxy) = accessibility::event_loop_proxy() {
            let _ = proxy.send_event(AppUserEvent::LetGo);
        }
        let (waited, timeout) = APP_PARKED
            .wait_timeout_while(slot, LET_GO_TIMEOUT, |slot| matches!(slot, Slot::Running))
            .unwrap();
        slot = waited;
        if timeout.timed_out() {
            log::error!("The last activity didn't let go of the app");
            launch::quit();
        }
    }
    match std::mem::replace(&mut *slot, Slot::Running) {
        Slot::Parked(Parked(app)) => Some(app),
        _ => None,
    }
}

/// Keep the app for the next activity. The session service keeps the process running, and with
/// it the desktop.
fn park(app: PolarBearApp) {
    *APP.lock().unwrap() = Slot::Parked(Parked(app));
    APP_PARKED.notify_all();
    log::info!("The activity is gone; the app waits for the next one");
}
