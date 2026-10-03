pub mod core {
    pub mod clipboard;
    pub mod config;
    pub mod dbus;
    pub mod hard_links;
}

#[cfg(target_os = "android")]
pub mod android {
    pub mod accessibility;
    pub mod clipboard;
    pub mod guest;
    pub mod notifications;
    pub mod screen;

    pub mod main;
    pub mod session;
    pub mod terminal;
    pub mod app {
        pub mod build;
        pub mod run;
    }
    pub mod backend {
        pub mod pipewire_standalone_aaudio;
        pub mod wayland;
        pub mod webview;
    }
    pub mod proot {
        pub mod launch;
        pub mod process;
        pub mod setup;
        pub mod ssh;
    }
    pub mod utils {
        pub mod application_context;
        pub mod fullscreen_immersive;
        pub mod java;
        pub mod ndk;
        pub mod webview;
    }
}
