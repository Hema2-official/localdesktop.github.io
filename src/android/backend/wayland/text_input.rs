//! Android's keyboard for the programs on the app's own display (`localdesktop direct`), which
//! have no plasma-keyboard: zwp_text_input_v3. A program says when one of its text fields has
//! the keyboard focus (enable) and what kind of text it takes; the app then shows Android's
//! keyboard (`android::soft_keyboard`) and hands the program what is typed on it, text being
//! composed as preedit and finished text as a commit. The keys that keyboard sends (Enter,
//! Backspace) go through the seat's keyboard instead, like a hardware keyboard's.
//!
//! Only the user's touch or click brings the keyboard up, when a field takes the focus or moves
//! its text cursor right after it: SDL2 programs keep text input enabled from their start, and a
//! keyboard that came up by itself would cover a game (which says nothing of a cursor unless it
//! has a text box). While such a field keeps the focus, a tap brings the keyboard back.

use super::compositor::State;
use crate::core::soft_keyboard::editor_info;
use smithay::reexports::wayland_protocols::wp::text_input::zv3::server::{
    zwp_text_input_manager_v3::{self, ZwpTextInputManagerV3},
    zwp_text_input_v3::{self, ZwpTextInputV3},
};
use smithay::reexports::wayland_server::{
    backend::{ClientId, GlobalId},
    protocol::wl_surface::WlSurface,
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum,
};
use std::time::{Duration, Instant};

/// How soon after the user's touch or click a field taking the focus or moving its cursor counts
/// as the user's doing.
const INTENT: Duration = Duration::from_secs(1);

/// What Android's keyboard is to do, for the event loop to tell it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keyboard {
    /// Show it for a field of this kind (Android's input type and IME options). `fresh`: another
    /// field, or the same one enabled again, so whatever it held is forgotten.
    Show {
        input_type: i32,
        options: i32,
        fresh: bool,
    },
    Hide,
}

#[derive(Default)]
struct Pending {
    enable: Option<bool>,
    content: Option<(u32, u32)>,
    cursor: Option<(i32, i32, i32, i32)>,
}

struct Instance {
    object: ZwpTextInputV3,
    /// Commit requests so far, which `done` repeats.
    commits: u32,
    pending: Pending,
    enabled: bool,
    /// The field's hints and purpose.
    content: (u32, u32),
    /// Where its text cursor is, if the program says.
    cursor: Option<(i32, i32, i32, i32)>,
    /// Enabled right after the user's input: the keyboard is for it.
    wanted: bool,
}

impl Instance {
    fn show(&self, fresh: bool) -> Keyboard {
        let (input_type, options) = editor_info(self.content.0, self.content.1);
        Keyboard::Show {
            input_type,
            options,
            fresh,
        }
    }
}

pub struct TextInput {
    global: GlobalId,
    instances: Vec<Instance>,
    /// The surface with the keyboard focus.
    focus: Option<WlSurface>,
    /// The user's last touch or click.
    input: Option<Instant>,
    request: Option<Keyboard>,
}

impl TextInput {
    pub fn new(display: &DisplayHandle) -> Self {
        Self {
            global: display.create_global::<State, ZwpTextInputManagerV3, _>(1, ()),
            instances: Vec::new(),
            focus: None,
            input: None,
            request: None,
        }
    }

    pub fn global(&self) -> GlobalId {
        self.global.clone()
    }

    /// The surface with the keyboard focus, whose program the keyboard types for.
    pub fn focused(&self) -> Option<&WlSurface> {
        self.focus.as_ref()
    }

    /// What Android's keyboard is to do now, if anything changed.
    pub fn take_request(&mut self) -> Option<Keyboard> {
        self.request.take()
    }

    /// The one that takes what is typed: enabled, and its program has the focus.
    fn active(&self) -> Option<&Instance> {
        let focus = self.focus.as_ref()?;
        self.instances
            .iter()
            .find(|it| it.enabled && it.object.id().same_client_as(&focus.id()))
    }

    /// The keyboard focus moved: the programs' text inputs leave the old surface and enter the
    /// new one.
    pub fn focus(&mut self, surface: Option<&WlSurface>) {
        if self.focus.as_ref() == surface {
            return;
        }
        if let Some(old) = self.focus.take() {
            for instance in &mut self.instances {
                if instance.object.id().same_client_as(&old.id()) {
                    instance.object.leave(&old);
                    if std::mem::take(&mut instance.wanted) && instance.enabled {
                        self.request = Some(Keyboard::Hide);
                    }
                }
            }
        }
        if let Some(new) = surface {
            for instance in &self.instances {
                if instance.object.id().same_client_as(&new.id()) {
                    instance.object.enter(new);
                }
            }
        }
        self.focus = surface.cloned();
    }

    /// The user touched or clicked: a field taking the focus now is their doing, and one that has
    /// it gets the keyboard back if it was closed.
    pub fn user_input(&mut self) {
        self.input = Some(Instant::now());
        let request = self
            .active()
            .filter(|it| it.wanted)
            .map(|it| it.show(false));
        if request.is_some() {
            self.request = request;
        }
    }

    /// Text from Android's keyboard: being composed (`preedit`), or finished.
    pub fn type_text(&mut self, text: &str, preedit: bool) {
        let Some(instance) = self.active() else {
            log::debug!("Keyboard: no text field to type into");
            return;
        };
        if preedit {
            // The cursor after it, in bytes.
            let end = text.len() as i32;
            instance
                .object
                .preedit_string(Some(text.to_owned()), end, end);
        } else {
            // Replaces the preedit, which `done` clears.
            instance.object.commit_string(Some(text.to_owned()));
        }
        instance.object.done(instance.commits);
    }

    fn commit(&mut self, object: &ZwpTextInputV3) {
        let recent = self.input.is_some_and(|at| at.elapsed() < INTENT);
        let Some(instance) = self.instances.iter_mut().find(|it| it.object == *object) else {
            return;
        };
        instance.commits = instance.commits.wrapping_add(1);
        let pending = std::mem::take(&mut instance.pending);
        let moved = pending.cursor.is_some() && pending.cursor != instance.cursor;
        if pending.cursor.is_some() {
            instance.cursor = pending.cursor;
        }
        match pending.enable {
            Some(true) => {
                // Enabling starts the field afresh.
                instance.enabled = true;
                instance.content = pending.content.unwrap_or_default();
                instance.cursor = pending.cursor;
                instance.wanted = recent || instance.wanted;
                if instance.wanted {
                    log::info!(
                        "Keyboard: Android's keyboard for a text field (hints {:#x}, purpose {})",
                        instance.content.0,
                        instance.content.1
                    );
                    self.request = Some(instance.show(true));
                }
            }
            Some(false) => {
                instance.enabled = false;
                if std::mem::take(&mut instance.wanted) {
                    self.request = Some(Keyboard::Hide);
                }
            }
            None if !instance.enabled => {}
            None => {
                let changed = pending.content.is_some_and(|it| it != instance.content);
                if let Some(content) = pending.content {
                    instance.content = content;
                }
                if moved && recent && !instance.wanted {
                    // A tap in the field, which had the focus already.
                    instance.wanted = true;
                    log::info!("Keyboard: Android's keyboard for a text field that was tapped");
                    self.request = Some(instance.show(true));
                } else if changed && instance.wanted {
                    self.request = Some(instance.show(true));
                }
            }
        }
    }

    fn pending(&mut self, object: &ZwpTextInputV3) -> Option<&mut Pending> {
        self.instances
            .iter_mut()
            .find(|it| it.object == *object)
            .map(|it| &mut it.pending)
    }

    fn remove(&mut self, object: &ZwpTextInputV3) {
        if let Some(index) = self.instances.iter().position(|it| it.object == *object) {
            let instance = self.instances.swap_remove(index);
            if instance.enabled && instance.wanted {
                self.request = Some(Keyboard::Hide);
            }
        }
    }
}

impl GlobalDispatch<ZwpTextInputManagerV3, ()> for State {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ZwpTextInputManagerV3>,
        _: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwpTextInputManagerV3, ()> for State {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ZwpTextInputManagerV3,
        request: zwp_text_input_manager_v3::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let zwp_text_input_manager_v3::Request::GetTextInput { id, .. } = request {
            let object = data_init.init(id, ());
            let text_input = &mut state.text_input;
            if let Some(focus) = &text_input.focus {
                if object.id().same_client_as(&focus.id()) {
                    object.enter(focus);
                }
            }
            text_input.instances.push(Instance {
                object,
                commits: 0,
                pending: Pending::default(),
                enabled: false,
                content: (0, 0),
                cursor: None,
                wanted: false,
            });
        }
    }
}

impl Dispatch<ZwpTextInputV3, ()> for State {
    fn request(
        state: &mut Self,
        _: &Client,
        object: &ZwpTextInputV3,
        request: zwp_text_input_v3::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        let text_input = &mut state.text_input;
        match request {
            zwp_text_input_v3::Request::Enable => {
                if let Some(pending) = text_input.pending(object) {
                    pending.enable = Some(true);
                }
            }
            zwp_text_input_v3::Request::Disable => {
                if let Some(pending) = text_input.pending(object) {
                    pending.enable = Some(false);
                }
            }
            zwp_text_input_v3::Request::SetContentType { hint, purpose } => {
                let hint = match hint {
                    WEnum::Value(hint) => hint.bits(),
                    WEnum::Unknown(bits) => bits,
                };
                let purpose = match purpose {
                    WEnum::Value(purpose) => purpose as u32,
                    WEnum::Unknown(value) => value,
                };
                if let Some(pending) = text_input.pending(object) {
                    pending.content = Some((hint, purpose));
                }
            }
            zwp_text_input_v3::Request::SetCursorRectangle {
                x,
                y,
                width,
                height,
            } => {
                if let Some(pending) = text_input.pending(object) {
                    pending.cursor = Some((x, y, width, height));
                }
            }
            zwp_text_input_v3::Request::Commit => text_input.commit(object),
            zwp_text_input_v3::Request::Destroy => text_input.remove(object),
            // The keyboard keeps no text of the program's.
            _ => {}
        }
    }

    fn destroyed(state: &mut Self, _: ClientId, object: &ZwpTextInputV3, _: &()) {
        state.text_input.remove(object);
    }
}
