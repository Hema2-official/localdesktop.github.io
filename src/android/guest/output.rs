//! The scale of a nested wlroots compositor's outputs (labwc, the Xfce preset's).
//!
//! Local Desktop's compositor gives such a compositor the Android window's size, which its
//! Wayland backend follows by itself, but no scale: its output would show everything at a third
//! of the size on a phone. (KWin takes its scale from the fractional-scale protocol instead.)
//! Over wlroots' output management the link gives each of the compositor's outputs the Android
//! window's UI scale whenever the compositor has described them, at the start and after any
//! change, and when the UI scale changes.

use super::{Globals, State};
use std::sync::atomic::{AtomicU32, Ordering};
use wayland_client::{event_created_child, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_wlr::output_management::v1::client::{
    zwlr_output_configuration_head_v1::ZwlrOutputConfigurationHeadV1,
    zwlr_output_configuration_v1::{self, ZwlrOutputConfigurationV1},
    zwlr_output_head_v1::{self, ZwlrOutputHeadV1},
    zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
    zwlr_output_mode_v1::{self, ZwlrOutputModeV1},
};

/// The Android window's UI scale; 0 until there is a window.
static SCALE: AtomicU32 = AtomicU32::new(0);

/// The Android window's UI scale, when it's first known or changes.
pub fn set_scale(scale: u32) {
    if SCALE.swap(scale, Ordering::AcqRel) != scale {
        super::notify(super::Event::Output);
    }
}

struct Head {
    head: ZwlrOutputHeadV1,
    enabled: bool,
    scale: f64,
}

/// The compositor's side: its outputs as it last described them.
pub struct Desktop {
    manager: ZwlrOutputManagerV1,
    heads: Vec<Head>,
    /// The serial of the compositor's latest description, until the link acted on it.
    described: Option<u32>,
    /// A configuration the compositor hasn't answered yet.
    applying: bool,
}

impl Desktop {
    /// `None` if the compositor doesn't let clients configure its outputs.
    pub fn bind(globals: &Globals, queue: &QueueHandle<State>) -> Option<Self> {
        Some(Self {
            manager: globals.bind(queue, 3)?,
            heads: Vec::new(),
            described: None,
            applying: false,
        })
    }

    /// Give the enabled outputs the UI scale, if one hasn't it, once the compositor has described
    /// them and answered the last change.
    pub fn apply(&mut self, queue: &QueueHandle<State>) {
        let scale = SCALE.load(Ordering::Acquire);
        if scale == 0 || self.applying {
            return;
        }
        let Some(serial) = self.described else {
            return;
        };
        let wanted = f64::from(scale);
        if !self
            .heads
            .iter()
            .any(|head| head.enabled && (head.scale - wanted).abs() > 0.001)
        {
            return;
        }
        // Every head, as the protocol wants: the enabled ones keep what isn't set here.
        let configuration = self.manager.create_configuration(serial, queue, ());
        for head in &self.heads {
            if head.enabled {
                configuration
                    .enable_head(&head.head, queue, ())
                    .set_scale(wanted);
            } else {
                configuration.disable_head(&head.head);
            }
        }
        configuration.apply();
        self.applying = true;
        self.described = None;
        log::info!("Output: scale {scale} for the desktop's outputs");
    }
}

impl Dispatch<ZwlrOutputManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrOutputManagerV1,
        event: zwlr_output_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(desktop) = state.output.as_mut() else {
            return;
        };
        match event {
            zwlr_output_manager_v1::Event::Head { head } => desktop.heads.push(Head {
                head,
                enabled: false,
                scale: 1.0,
            }),
            zwlr_output_manager_v1::Event::Done { serial } => desktop.described = Some(serial),
            _ => {}
        }
    }

    event_created_child!(State, ZwlrOutputManagerV1, [
        zwlr_output_manager_v1::EVT_HEAD_OPCODE => (ZwlrOutputHeadV1, ()),
    ]);
}

impl Dispatch<ZwlrOutputHeadV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ZwlrOutputHeadV1,
        event: zwlr_output_head_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(desktop) = state.output.as_mut() else {
            return;
        };
        let Some(index) = desktop.heads.iter().position(|it| &it.head == proxy) else {
            return;
        };
        match event {
            zwlr_output_head_v1::Event::Enabled { enabled } => {
                desktop.heads[index].enabled = enabled != 0
            }
            zwlr_output_head_v1::Event::Scale { scale } => desktop.heads[index].scale = scale,
            zwlr_output_head_v1::Event::Finished => {
                desktop.heads.remove(index);
                if proxy.version() >= 3 {
                    proxy.release();
                }
            }
            _ => {}
        }
    }

    event_created_child!(State, ZwlrOutputHeadV1, [
        zwlr_output_head_v1::EVT_MODE_OPCODE => (ZwlrOutputModeV1, ()),
    ]);
}

impl Dispatch<ZwlrOutputModeV1, ()> for State {
    fn event(
        _: &mut Self,
        proxy: &ZwlrOutputModeV1,
        event: zwlr_output_mode_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_output_mode_v1::Event::Finished = event {
            if proxy.version() >= 3 {
                proxy.release();
            }
        }
    }
}

impl Dispatch<ZwlrOutputConfigurationV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ZwlrOutputConfigurationV1,
        event: zwlr_output_configuration_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_output_configuration_v1::Event::Succeeded => {}
            // The compositor changed its outputs meanwhile, and describes them again.
            zwlr_output_configuration_v1::Event::Cancelled => {}
            zwlr_output_configuration_v1::Event::Failed => {
                log::warn!("Output: the desktop's compositor refused the scale")
            }
            _ => return,
        }
        proxy.destroy();
        if let Some(desktop) = state.output.as_mut() {
            desktop.applying = false;
        }
    }
}

impl Dispatch<ZwlrOutputConfigurationHeadV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrOutputConfigurationHeadV1,
        _: <ZwlrOutputConfigurationHeadV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
