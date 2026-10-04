use winit::platform::android::activity::AndroidApp;

use crate::android::{
    backend::{wayland::WaylandBackend, webview::WebviewBackend},
    proot::setup::setup,
};

pub struct PolarBearApp {
    pub frontend: PolarBearFrontend,
    pub backend: PolarBearBackend,
}

pub struct PolarBearFrontend {
    pub android_app: AndroidApp,
}

pub enum PolarBearBackend {
    /// Use a webview to report setup progress to the user
    /// The setup progress should only be done once, when the user first installed the app
    WebView(WebviewBackend),

    /// Use a wayland compositor to render Linux GUI applications back to the Android Native Activity
    Wayland(WaylandBackend),
}

impl PolarBearApp {
    pub fn build(android_app: AndroidApp) -> Self {
        Self {
            backend: setup(android_app.clone()),
            frontend: PolarBearFrontend { android_app },
        }
    }

    /// Hand the app to a new activity, after the last one let go of it (`detach`). Its event
    /// loop's proxy has to be registered already.
    pub fn attach(mut self, android_app: AndroidApp) -> Self {
        match &mut self.backend {
            PolarBearBackend::WebView(backend) if backend.finished() => {
                // What a restart after the setup does.
                log::info!("Setup finished while no activity showed it: starting the desktop");
                return Self::build(android_app);
            }
            PolarBearBackend::WebView(_) => {}
            PolarBearBackend::Wayland(backend) => {
                backend.android_app = android_app.clone();
                backend.compositor.restart_client_waker();
            }
        }
        self.frontend.android_app = android_app;
        self
    }
}
