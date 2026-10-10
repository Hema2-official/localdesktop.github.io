//! The headset's controllers, for Monado: their state and poses every display frame, read through
//! the oculus/touch_controller profile, and haptic pulses from Monado.

use super::protocol::{self, Controller, Haptic, PoseSample};
use super::Clock;
use anyhow::Result;
use openxr as xr;
use std::cell::Cell;

/// A button's action, its bit in `Controller::buttons`, and its input on the left and the right
/// controller.
const BUTTONS: [(&str, u32, [Option<&str>; 2]); 9] = [
    (
        "lower_click",
        protocol::BUTTON_LOWER_CLICK,
        [Some("x/click"), Some("a/click")],
    ),
    (
        "lower_touch",
        protocol::BUTTON_LOWER_TOUCH,
        [Some("x/touch"), Some("a/touch")],
    ),
    (
        "upper_click",
        protocol::BUTTON_UPPER_CLICK,
        [Some("y/click"), Some("b/click")],
    ),
    (
        "upper_touch",
        protocol::BUTTON_UPPER_TOUCH,
        [Some("y/touch"), Some("b/touch")],
    ),
    (
        "menu_click",
        protocol::BUTTON_MENU_CLICK,
        [Some("menu/click"), None],
    ),
    (
        "trigger_touch",
        protocol::BUTTON_TRIGGER_TOUCH,
        [Some("trigger/touch"), Some("trigger/touch")],
    ),
    (
        "thumbstick_click",
        protocol::BUTTON_THUMBSTICK_CLICK,
        [Some("thumbstick/click"), Some("thumbstick/click")],
    ),
    (
        "thumbstick_touch",
        protocol::BUTTON_THUMBSTICK_TOUCH,
        [Some("thumbstick/touch"), Some("thumbstick/touch")],
    ),
    (
        "thumbrest_touch",
        protocol::BUTTON_THUMBREST_TOUCH,
        [Some("thumbrest/touch"), Some("thumbrest/touch")],
    ),
];

const HANDS: [&str; 2] = ["left", "right"];

pub struct Controllers {
    set: xr::ActionSet,
    hands: [xr::Path; 2],
    grip: xr::Action<xr::Posef>,
    grip_spaces: Vec<xr::Space>,
    aim_spaces: Vec<xr::Space>,
    trigger: xr::Action<f32>,
    squeeze: xr::Action<f32>,
    thumbstick: xr::Action<xr::Vector2f>,
    buttons: Vec<(xr::Action<bool>, u32)>,
    haptic: xr::Action<xr::Haptic>,
    /// Whether each controller was in use at the last look, for the log.
    active: [Cell<bool>; 2],
}

impl Controllers {
    /// Actions for the controllers' inputs, attached to `session`, which can't take others then.
    pub fn new<G: xr::Graphics>(instance: &xr::Instance, session: &xr::Session<G>) -> Result<Self> {
        let set = instance.create_action_set("localdesktop", "Local Desktop", 0)?;
        let hands = [
            instance.string_to_path("/user/hand/left")?,
            instance.string_to_path("/user/hand/right")?,
        ];
        let grip = set.create_action::<xr::Posef>("grip", "Grip", &hands)?;
        let aim = set.create_action::<xr::Posef>("aim", "Aim", &hands)?;
        let trigger = set.create_action::<f32>("trigger", "Trigger", &hands)?;
        let squeeze = set.create_action::<f32>("squeeze", "Squeeze", &hands)?;
        let thumbstick = set.create_action::<xr::Vector2f>("thumbstick", "Thumbstick", &hands)?;
        let haptic = set.create_action::<xr::Haptic>("haptic", "Haptic", &hands)?;
        let buttons = BUTTONS
            .iter()
            .map(|(name, bit, _)| Ok((set.create_action::<bool>(name, name, &hands)?, *bit)))
            .collect::<Result<Vec<_>>>()?;

        let path = |hand: &str, input: &str| {
            instance.string_to_path(&format!("/user/hand/{hand}/{input}"))
        };
        let mut bindings = Vec::new();
        for hand in HANDS {
            bindings.push(xr::Binding::new(&grip, path(hand, "input/grip/pose")?));
            bindings.push(xr::Binding::new(&aim, path(hand, "input/aim/pose")?));
            bindings.push(xr::Binding::new(
                &trigger,
                path(hand, "input/trigger/value")?,
            ));
            bindings.push(xr::Binding::new(
                &squeeze,
                path(hand, "input/squeeze/value")?,
            ));
            bindings.push(xr::Binding::new(
                &thumbstick,
                path(hand, "input/thumbstick")?,
            ));
            bindings.push(xr::Binding::new(&haptic, path(hand, "output/haptic")?));
        }
        for ((action, _), (_, _, inputs)) in buttons.iter().zip(BUTTONS.iter()) {
            for (hand, input) in HANDS.iter().zip(inputs) {
                if let Some(input) = input {
                    bindings.push(xr::Binding::new(
                        action,
                        path(hand, &format!("input/{input}"))?,
                    ));
                }
            }
        }
        instance.suggest_interaction_profile_bindings(
            instance.string_to_path("/interaction_profiles/oculus/touch_controller")?,
            &bindings,
        )?;
        session.attach_action_sets(&[&set])?;

        let spaces = |action: &xr::Action<xr::Posef>| {
            hands
                .iter()
                .map(|hand| action.create_space(session, *hand, xr::Posef::IDENTITY))
                .collect::<xr::Result<Vec<_>>>()
        };
        let grip_spaces = spaces(&grip)?;
        let aim_spaces = spaces(&aim)?;
        Ok(Self {
            set,
            hands,
            grip,
            grip_spaces,
            aim_spaces,
            trigger,
            squeeze,
            thumbstick,
            buttons,
            haptic,
            active: Default::default(),
        })
    }

    /// Both controllers now, with their poses in `space` predicted for `time`.
    pub fn read<G>(
        &self,
        session: &xr::Session<G>,
        space: &xr::Space,
        time: xr::Time,
        clock: &Clock,
    ) -> Result<[Controller; 2]> {
        session.sync_actions(&[xr::ActiveActionSet::new(&self.set)])?;
        let time_ns = clock.to_monotonic_ns(time);
        let mut controllers = [Controller::default(); 2];
        for (index, hand) in self.hands.iter().enumerate() {
            let active = self.grip.is_active(session, *hand)?;
            if active != self.active[index].replace(active) {
                let state = if active { "in use" } else { "put away" };
                log::info!("Immersive mode: {} controller {state}", HANDS[index]);
            }
            if !active {
                continue;
            }
            let sample = |spaces: &[xr::Space]| -> Result<PoseSample> {
                let (location, velocity) = spaces[index].relate(space, time)?;
                Ok(PoseSample::new(time_ns, location, velocity))
            };
            let mut buttons = 0;
            for (action, bit) in &self.buttons {
                if action.state(session, *hand)?.current_state {
                    buttons |= bit;
                }
            }
            controllers[index] = Controller {
                flags: protocol::CONTROLLER_ACTIVE,
                buttons,
                trigger: self.trigger.state(session, *hand)?.current_state,
                squeeze: self.squeeze.state(session, *hand)?.current_state,
                thumbstick: self.thumbstick.state(session, *hand)?.current_state,
                grip: sample(&self.grip_spaces)?,
                aim: sample(&self.aim_spaces)?,
            };
        }
        Ok(controllers)
    }

    /// Vibrate a controller as Monado asks, or stop it.
    pub fn vibrate<G>(&self, session: &xr::Session<G>, haptic: &Haptic) -> Result<()> {
        let Some(hand) = self.hands.get(haptic.hand) else {
            return Ok(());
        };
        if haptic.amplitude <= 0.0 {
            self.haptic.stop_feedback(session, *hand)?;
            return Ok(());
        }
        let duration = if haptic.duration_ns < 0 {
            xr::Duration::MIN_HAPTIC
        } else {
            xr::Duration::from_nanos(haptic.duration_ns)
        };
        let frequency = if haptic.frequency > 0.0 {
            haptic.frequency
        } else {
            xr::FREQUENCY_UNSPECIFIED
        };
        self.haptic.apply_feedback(
            session,
            *hand,
            &xr::HapticVibration::new()
                .amplitude(haptic.amplitude.min(1.0))
                .frequency(frequency)
                .duration(duration),
        )?;
        Ok(())
    }
}
