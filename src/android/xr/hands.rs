//! The hands, for Monado: their joints every display frame, through XR_EXT_hand_tracking.

use super::protocol::{self, Message};
use anyhow::Result;
use openxr as xr;
use std::cell::Cell;

pub struct Hands {
    trackers: [xr::HandTracker; 2],
    /// Whether each hand was tracked at the last look, for the log.
    tracked: [Cell<bool>; 2],
}

impl Hands {
    pub fn new<G: xr::Graphics>(session: &xr::Session<G>) -> Result<Self> {
        Ok(Self {
            trackers: [
                session.create_hand_tracker(xr::Hand::LEFT)?,
                session.create_hand_tracker(xr::Hand::RIGHT)?,
            ],
            tracked: Default::default(),
        })
    }

    /// The hands message: both hands' joints in `space` at `time` (`time_ns` in Linux's clock).
    pub fn message(&self, space: &xr::Space, time: xr::Time, time_ns: i64) -> Result<Vec<u8>> {
        let mut message = Message::default();
        message.u32(protocol::HANDS);
        message.u32(0);
        message.i64(time_ns);
        for (index, tracker) in self.trackers.iter().enumerate() {
            let joints = space.locate_hand_joints(tracker, time)?;
            if joints.is_some() != self.tracked[index].replace(joints.is_some()) {
                let hand = if index == 0 { "left" } else { "right" };
                let state = if joints.is_some() { "tracked" } else { "lost" };
                log::info!("Immersive mode: {hand} hand {state}");
            }
            match joints {
                Some(joints) => {
                    message.u32(protocol::HAND_ACTIVE);
                    message.u32(0);
                    for joint in joints.iter() {
                        message.pose(&joint.pose);
                        message.f32(joint.radius);
                        message.u32(protocol::relation_flags(
                            joint.location_flags,
                            xr::SpaceVelocityFlags::EMPTY,
                        ));
                    }
                }
                None => message.zeros(8 + protocol::HAND_JOINTS * protocol::JOINT_SIZE),
            }
        }
        debug_assert_eq!(message.0.len(), protocol::HANDS_SIZE);
        Ok(message.0)
    }
}
