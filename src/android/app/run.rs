use std::thread;

use super::build::{PolarBearApp, PolarBearBackend};
use crate::android::{
    accessibility::{self, AppUserEvent},
    backend::{
        pipewire_standalone_aaudio,
        wayland::{
            about_to_wait, bind, centralize, centralize_injected_keyboard, handle,
            write_guest_output_state, CentralizedEvent, State,
        },
        webview::ErrorVariant,
    },
    proot::launch::launch,
    session, terminal,
    utils::{
        ndk::{self, run_in_jvm},
        webview::show_webview_popup,
    },
};
use crate::core::config;
use smithay::output::{Mode, Output, PhysicalProperties, Scale, Subpixel};
use smithay::utils::Transform;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow};
use winit::window::WindowId;

fn configure_output(backend: &mut crate::android::backend::wayland::WaylandBackend) {
    let Some(winit) = backend.graphic_renderer.as_ref() else {
        return;
    };

    let window_size = winit.window_size();
    let size = (window_size.w, window_size.h);
    // Not `winit.scale_factor()`: that reads `AConfiguration`, which still reports the 160 dpi
    // default on the first launch and only becomes accurate after a configuration change.
    let guest_scale_factor = ndk::scale_factor(&backend.android_app);
    backend.guest_scale_factor = guest_scale_factor;
    backend.compositor.state.size = size.into();

    let output = backend
        .compositor
        .output
        .get_or_insert_with(|| {
            Output::new(
                "Local Desktop Wayland Compositor".into(),
                PhysicalProperties {
                    size: size.into(),
                    subpixel: Subpixel::HorizontalRgb,
                    make: "Local Desktop".into(),
                    model: config::VERSION.into(),
                },
            )
        })
        .clone();

    if backend.compositor.output_global.is_none() {
        let dh = backend.compositor.display.handle();
        backend.compositor.output_global = Some(output.create_global::<State>(&dh));
    }

    output.change_current_state(
        Some(Mode {
            size: size.into(),
            refresh: 60000,
        }),
        Some(Transform::Normal),
        Some(Scale::Integer(1)),
        Some((0, 0).into()),
    );

    let guest_scale = guest_scale_factor.round().max(1.0) as i32;
    write_guest_output_state(window_size.w, window_size.h, guest_scale);

    let state = &mut backend.compositor.state;
    state.set_client_scale(guest_scale as f64);
    state.reconfigure_toplevels();
}

impl ApplicationHandler<AppUserEvent> for PolarBearApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        match self.backend {
            PolarBearBackend::WebView(ref mut backend) => {
                accessibility::set_runtime_active(false);
                let url = match backend.error {
                    ErrorVariant::None => {
                        let port = backend.socket_port;
                        format!("file:///android_asset/setup-progress.html?port={}", port)
                    }
                    ErrorVariant::Unsupported => {
                        format!("file:///android_asset/unsupported.html")
                    }
                };
                let android_app = self.frontend.android_app.clone();
                if matches!(backend.error, ErrorVariant::None) {
                    // The setup notification shows the progress while the phone is doing other
                    // things, but only with the permission.
                    session::ask_permissions(&android_app);
                }
                thread::spawn(move || {
                    run_in_jvm(
                        move |env, app| {
                            show_webview_popup(env, app, &url);
                        },
                        android_app,
                    );
                });
            }
            PolarBearBackend::Wayland(ref mut backend) => {
                if backend.graphic_renderer.is_none() {
                    match bind(event_loop) {
                        Ok(winit) => backend.graphic_renderer = Some(winit),
                        Err(error) => {
                            log::error!("Failed to initialize Wayland renderer on resume: {error}");
                            accessibility::set_runtime_active(false);
                            event_loop.set_control_flow(ControlFlow::Wait);
                            return;
                        }
                    }
                } else {
                    log::info!("Ignoring redundant resume while renderer is already active");
                }

                configure_output(backend);
                accessibility::set_runtime_active(true);

                if let Some(winit) = backend.graphic_renderer.as_ref() {
                    winit.window().request_redraw();
                }
                handle(CentralizedEvent::Redraw, backend, event_loop);
                launch();
                let android_app = &self.frontend.android_app;
                session::ask_permissions(android_app);
                session::start_service(android_app);
                if ndk::take_open_request(android_app).as_deref() == Some("terminal") {
                    terminal::open(android_app, None);
                }
                // Start the standalone-client PipeWire/AAudio backend.
                pipewire_standalone_aaudio::spawn_after_ready(self.frontend.android_app.clone());
            }
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: AppUserEvent) {
        let PolarBearBackend::Wayland(backend) = &mut self.backend else {
            accessibility::drain_pending_events();
            return;
        };
        if let AppUserEvent::WaylandClientsReady = event {
            // Serviced in `about_to_wait`, right after this.
            backend.compositor.clients_ready();
            return;
        }

        for event in accessibility::drain_pending_events() {
            let event = centralize_injected_keyboard(
                event.scancode,
                event.state,
                event.event_time_ms,
                backend,
            );
            handle(event, backend, event_loop);
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        if let PolarBearBackend::Wayland(backend) = &mut self.backend {
            if backend.graphic_renderer.is_none() {
                if matches!(event, WindowEvent::CloseRequested) {
                    event_loop.exit();
                } else {
                    log::info!(
                        "Ignoring window event while renderer is suspended: {:?}",
                        event
                    );
                }
                return;
            }

            // Map raw events to our own events
            let event = centralize(event, backend);

            // Handle the centralized events
            handle(event, backend, event_loop);
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let PolarBearBackend::Wayland(backend) = &mut self.backend {
            about_to_wait(backend, event_loop);
        }
    }

    fn suspended(&mut self, event_loop: &ActiveEventLoop) {
        accessibility::set_runtime_active(false);
        event_loop.set_control_flow(ControlFlow::Wait);

        if let PolarBearBackend::Wayland(backend) = &mut self.backend {
            backend.graphic_renderer = None;
            backend.key_counter = 0;
            backend.reset_touch_state();
            backend.pointer_pressed = false;
            // Kill the standalone-client PipeWire/AAudio backend if it was started.
            pipewire_standalone_aaudio::shutdown();
        }
    }
}
