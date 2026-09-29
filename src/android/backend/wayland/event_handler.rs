use crate::android::{
    accessibility,
    backend::wayland::{
        compositor::{
            send_frames_surface_tree, take_presentation_feedback_surface_tree, State,
        },
        write_guest_output_state, CentralizedEvent, TouchMode, WaylandBackend,
    },
};
use smithay::backend::input::ButtonState;
use smithay::backend::renderer::element::surface::{
    render_elements_from_surface_tree, WaylandSurfaceRenderElement,
};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::utils::draw_render_elements;
use smithay::backend::renderer::{Color32F, Frame, Renderer};
use smithay::input::keyboard::FilterResult;
use smithay::input::pointer;
use smithay::reexports::wayland_server::protocol::wl_pointer::ButtonState as WlButtonState;
use smithay::utils::{Point, Rectangle, Transform, SERIAL_COUNTER};
use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback;
use smithay::wayland::presentation::Refresh;
use smithay::wayland::shell::xdg::ToplevelSurface;
use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, Event, InputEvent, KeyboardKeyEvent, PointerAxisEvent,
        PointerButtonEvent,
    },
    output::{Mode, Scale},
};
use std::time::{Duration, Instant};
use winit::event_loop::{ActiveEventLoop, ControlFlow};

/// Linux input event code for the left mouse button (`BTN_LEFT`).
const BTN_LEFT: u32 = 0x110;
/// Linux input event code for the right mouse button (`BTN_RIGHT`).
const BTN_RIGHT: u32 = 0x111;

/**
 * As we currently use Xwayland, there is only 1 surface
 */
fn get_surface(state: &State) -> Option<ToplevelSurface> {
    state
        .xdg_shell_state
        .toplevel_surfaces()
        .iter()
        .next()
        .cloned()
}

fn pointer_focus(
    state: &State,
) -> Option<(
    smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    Point<f64, smithay::utils::Logical>,
)> {
    get_surface(state).map(|surface| (surface.wl_surface().clone(), (0f64, 0f64).into()))
}

fn emit_pointer_motion(
    compositor: &mut crate::android::backend::wayland::Compositor,
    x: f64,
    y: f64,
    time: u32,
) {
    let pointer = compositor.pointer.clone();
    let state = &mut compositor.state;
    if let Some(focus) = pointer_focus(state) {
        // Touch and mouse positions are physical pixels; the surface may work in logical units.
        let scale = state.surface_scale(&focus.0);
        let serial = SERIAL_COUNTER.next_serial();
        pointer.motion(
            state,
            Some(focus),
            &pointer::MotionEvent {
                location: (x / scale, y / scale).into(),
                serial,
                time,
            },
        );
        pointer.frame(state);
    }
}

/// Press a button. Also moves keyboard focus to the surface under the pointer.
fn emit_pointer_press(
    compositor: &mut crate::android::backend::wayland::Compositor,
    button: u32,
    time: u32,
) {
    let pointer = compositor.pointer.clone();
    let state = &mut compositor.state;
    if let Some(surface) = get_surface(state) {
        compositor.keyboard.set_focus(
            state,
            Some(surface.wl_surface().clone()),
            SERIAL_COUNTER.next_serial().into(),
        );
    }

    let serial = SERIAL_COUNTER.next_serial();
    pointer.button(
        state,
        &pointer::ButtonEvent {
            button,
            state: ButtonState::Pressed,
            serial,
            time,
        },
    );
    pointer.frame(state);
}

/// Release a button.
fn emit_pointer_release(
    compositor: &mut crate::android::backend::wayland::Compositor,
    button: u32,
    time: u32,
) {
    let pointer = compositor.pointer.clone();
    let state = &mut compositor.state;
    let serial = SERIAL_COUNTER.next_serial();
    pointer.button(
        state,
        &pointer::ButtonEvent {
            button,
            state: ButtonState::Released,
            serial,
            time,
        },
    );
    pointer.frame(state);
}

/// A full tap: move to the location, then a press immediately followed by a release.
fn emit_pointer_click(
    compositor: &mut crate::android::backend::wayland::Compositor,
    button: u32,
    x: f64,
    y: f64,
    time: u32,
) {
    emit_pointer_motion(compositor, x, y, time);
    emit_pointer_press(compositor, button, time);
    emit_pointer_release(compositor, button, time);
}

/// Arm the long press once the finger has stayed put for `ViewConfiguration`'s timeout.
///
/// No button is sent here: moving afterwards starts a drag with the left button held, lifting
/// instead fires a right click. Checked whenever the event loop is about to sleep, which wakes
/// up at `long_press_deadline` for it.
fn poll_long_press(backend: &mut WaylandBackend) {
    if backend.touch_mode != TouchMode::Undecided || backend.touch_points.len() != 1 {
        return;
    }
    let (Some(down_time), Some(down_position)) =
        (backend.touch_down_time, backend.touch_down_position)
    else {
        return;
    };
    let now = backend.clock.now().as_millis() as u64;
    if now.saturating_sub(down_time) < backend.long_press_timeout_ms {
        return;
    }

    backend.touch_mode = TouchMode::LongPress;
    // Anchor the pointer where the finger landed, so a drag selects from there.
    emit_pointer_motion(
        &mut backend.compositor,
        down_position.x,
        down_position.y,
        now as u32,
    );
}

/// When a finger that is down and hasn't moved becomes a long press.
fn long_press_deadline(backend: &WaylandBackend) -> Option<Instant> {
    if backend.touch_mode != TouchMode::Undecided || backend.touch_points.len() != 1 {
        return None;
    }
    let down_time = backend.touch_down_time?;
    let now = backend.clock.now().as_millis() as u64;
    let remaining = (down_time + backend.long_press_timeout_ms).saturating_sub(now);
    Some(Instant::now() + Duration::from_millis(remaining))
}

/// Runs whenever the event loop has handled what woke it and is about to sleep: answer the
/// clients, draw a frame only if something on screen changed, and sleep until the next event
/// (a client, input, the long-press timeout). Nothing is drawn while the desktop is idle.
pub fn about_to_wait(backend: &mut WaylandBackend, event_loop: &ActiveEventLoop) {
    poll_long_press(backend);

    if let Err(error) = backend.compositor.service_clients() {
        log::error!("{error}");
    }

    if backend.compositor.state.needs_redraw {
        if let Some(winit) = backend.graphic_renderer.as_ref() {
            winit.window().request_redraw();
        }
    }

    let control_flow = if !backend.compositor.has_client_waker() {
        // Nothing would wake the loop for clients: look every frame.
        ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(16))
    } else if let Some(deadline) = long_press_deadline(backend) {
        ControlFlow::WaitUntil(deadline)
    } else {
        ControlFlow::Wait
    };
    event_loop.set_control_flow(control_flow);
}

pub fn handle(event: CentralizedEvent, backend: &mut WaylandBackend, event_loop: &ActiveEventLoop) {
    match event {
        CentralizedEvent::CloseRequested => {
            event_loop.exit();
        }
        CentralizedEvent::Redraw => {
            // The next frame comes when something changes again (see `about_to_wait`).
            if let Err(error) = redraw(backend) {
                log::error!("Redraw failed; dropping renderer until next resume: {error}");
                backend.graphic_renderer = None;
                accessibility::set_runtime_active(false);
                event_loop.set_control_flow(ControlFlow::Wait);
            }
        }
        CentralizedEvent::Input(event) => match event {
            InputEvent::Keyboard { event } => {
                let compositor = &mut backend.compositor;
                let state = &mut compositor.state;
                let serial = SERIAL_COUNTER.next_serial();
                let time = compositor.start_time.elapsed().as_millis() as u32;
                compositor.keyboard.input::<(), _>(
                    state,
                    event.key_code(),
                    event.state(),
                    serial,
                    time,
                    |_, _, _| {
                        //
                        FilterResult::Forward
                    },
                );
            }
            InputEvent::TouchDown { event } => {
                // Just move the cursor. Which button (if any) this gesture sends is only known
                // once the finger moves, lifts, or sits still long enough to be a long press.
                emit_pointer_motion(
                    &mut backend.compositor,
                    event.x(),
                    event.y(),
                    event.time_msec(),
                );
            }
            InputEvent::TouchMotion { event } => {
                let time = event.time_msec();

                // The centralizer only emits motion in Drag mode, and flips into it on the
                // first move after a long press — that transition is where the grab starts.
                if !backend.pointer_pressed {
                    emit_pointer_press(&mut backend.compositor, BTN_LEFT, time);
                    backend.pointer_pressed = true;
                }

                emit_pointer_motion(&mut backend.compositor, event.x(), event.y(), time);
            }
            InputEvent::TouchUp { event } => {
                let time = event.time_msec();

                if backend.pointer_pressed {
                    // End of a drag.
                    emit_pointer_motion(&mut backend.compositor, event.x, event.y, time);
                    emit_pointer_release(&mut backend.compositor, BTN_LEFT, time);
                    backend.pointer_pressed = false;
                } else {
                    match event.mode {
                        // A tap: left click where the finger lifted.
                        TouchMode::Undecided => emit_pointer_click(
                            &mut backend.compositor,
                            BTN_LEFT,
                            event.x,
                            event.y,
                            time,
                        ),
                        // Held still, then lifted without moving: a context menu, as on Android.
                        TouchMode::LongPress => emit_pointer_click(
                            &mut backend.compositor,
                            BTN_RIGHT,
                            event.x,
                            event.y,
                            time,
                        ),
                        // A scroll consumed the gesture; nothing to click.
                        TouchMode::Scroll | TouchMode::Drag => {}
                    }
                }
            }
            InputEvent::TouchCancel { event } => {
                if backend.pointer_pressed {
                    emit_pointer_release(&mut backend.compositor, BTN_LEFT, event.time() as u32);
                    backend.pointer_pressed = false;
                }
            }
            InputEvent::PointerMotionAbsolute { event, .. } => {
                let compositor = &mut backend.compositor;
                let pointer = compositor.pointer.clone();
                let serial = SERIAL_COUNTER.next_serial();

                if let Some(surface) = get_surface(&compositor.state) {
                    let scale = compositor.state.surface_scale(surface.wl_surface());
                    pointer.motion(
                        &mut compositor.state,
                        Some((surface.wl_surface().clone(), (0f64, 0f64).into())),
                        &pointer::MotionEvent {
                            location: (event.x() / scale, event.y() / scale).into(),
                            serial,
                            time: event.time_msec(),
                        },
                    );
                }
                pointer.frame(&mut compositor.state);
            }
            InputEvent::PointerButton { event, .. } => {
                let serial = SERIAL_COUNTER.next_serial();
                let button = event.button_code();

                let state = WlButtonState::from(event.state());

                let compositor = &mut backend.compositor;
                let pointer = compositor.pointer.clone();

                if let Some(surface) = get_surface(&compositor.state) {
                    compositor.keyboard.set_focus(
                        &mut compositor.state,
                        Some(surface.wl_surface().clone()),
                        0.into(),
                    );
                }
                pointer.button(
                    &mut compositor.state,
                    &pointer::ButtonEvent {
                        button,
                        state: state.try_into().unwrap(),
                        serial,
                        time: event.time_msec(),
                    },
                );
                pointer.frame(&mut compositor.state);
            }
            InputEvent::PointerAxis { event } => {
                // A second finger can turn an in-progress drag into a scroll; drop the button
                // the drag was holding rather than scrolling with it down.
                if backend.pointer_pressed {
                    emit_pointer_release(&mut backend.compositor, BTN_LEFT, event.time_msec());
                    backend.pointer_pressed = false;
                }
                // Scroll distances are in the desktop's logical pixels: KWin gets them at its own
                // scale, and a nested labwc passes them unchanged to clients at that scale.
                let scale = backend.guest_scale_factor.round().max(1.0);
                let horizontal_amount = event.amount(Axis::Horizontal).map_or_else(
                    || event.amount_v120(Axis::Horizontal).unwrap_or(0.0) / 120.,
                    |amount| amount / scale,
                );
                let vertical_amount = event.amount(Axis::Vertical).map_or_else(
                    || event.amount_v120(Axis::Vertical).unwrap_or(0.0) / 120.,
                    |amount| amount / scale,
                );
                let horizontal_amount_discrete = event.amount_v120(Axis::Horizontal);
                let vertical_amount_discrete = event.amount_v120(Axis::Vertical);

                {
                    let mut frame =
                        pointer::AxisFrame::new(event.time_msec()).source(event.source());
                    if horizontal_amount != 0.0 {
                        frame = frame.relative_direction(
                            Axis::Horizontal,
                            event.relative_direction(Axis::Horizontal),
                        );
                        frame = frame.value(Axis::Horizontal, horizontal_amount);
                        if let Some(discrete) = horizontal_amount_discrete {
                            frame = frame.v120(Axis::Horizontal, discrete as i32);
                        }
                    }
                    if vertical_amount != 0.0 {
                        frame = frame.relative_direction(
                            Axis::Vertical,
                            event.relative_direction(Axis::Vertical),
                        );
                        frame = frame.value(Axis::Vertical, vertical_amount);
                        if let Some(discrete) = vertical_amount_discrete {
                            frame = frame.v120(Axis::Vertical, discrete as i32);
                        }
                    }
                    if event.amount(Axis::Horizontal) == Some(0.0) {
                        frame = frame.stop(Axis::Horizontal);
                    }
                    if event.amount(Axis::Vertical) == Some(0.0) {
                        frame = frame.stop(Axis::Vertical);
                    }
                    let compositor = &mut backend.compositor;
                    let pointer = compositor.pointer.clone();
                    pointer.axis(&mut compositor.state, frame);
                    pointer.frame(&mut compositor.state);
                }
            }
            _ => {}
        },
        CentralizedEvent::Resized {
            size,
            guest_scale_factor,
        } => {
            backend.compositor.state.size = (size.w, size.h).into();
            backend.compositor.state.needs_redraw = true;

            if let Some(output) = &backend.compositor.output {
                output.change_current_state(
                    Some(Mode {
                        size: size.into(),
                        refresh: 60000,
                    }),
                    Some(Transform::Normal),
                    Some(Scale::Integer(1)),
                    Some((0, 0).into()),
                );
            }

            let guest_scale = guest_scale_factor.round().max(1.0) as i32;
            write_guest_output_state(size.w, size.h, guest_scale);

            let state = &mut backend.compositor.state;
            state.set_client_scale(guest_scale as f64);
            state.reconfigure_toplevels();
        }
        _ => (),
    }
}

fn redraw(backend: &mut WaylandBackend) -> Result<(), String> {
    let Some(winit) = backend.graphic_renderer.as_mut() else {
        return Ok(());
    };

    let size = winit.window_size();
    let origin = winit.content_area().loc;
    let damage = Rectangle::from_size(size);
    let mut presentation_feedback = Vec::new();
    backend.compositor.state.needs_redraw = false;
    {
        let (renderer, mut framebuffer) = winit
            .bind()
            .map_err(|error| format!("Failed to bind EGL surface: {error}"))?;

        let compositor = &mut backend.compositor;

        // Each toplevel with the scale its content maps to the screen with; the first one is on top.
        let toplevels = compositor
            .state
            .xdg_shell_state
            .toplevel_surfaces()
            .iter()
            .map(|surface| {
                let scale = compositor.state.surface_scale(surface.wl_surface());
                let elements = render_elements_from_surface_tree(
                    renderer,
                    surface.wl_surface(),
                    origin,
                    scale,
                    1.0,
                    Kind::Unspecified,
                );
                (scale, elements)
            })
            .collect::<Vec<(f64, Vec<WaylandSurfaceRenderElement<GlesRenderer>>)>>();

        let mut frame = renderer
            .render(&mut framebuffer, size, Transform::Flipped180)
            .map_err(|error| format!("Failed to render frame: {error:?}"))?;
        frame
            .clear(Color32F::new(0.1, 0.0, 0.0, 1.0), &[damage])
            .map_err(|error| format!("Failed to clear frame: {error:?}"))?;
        for (scale, elements) in toplevels.iter().rev() {
            draw_render_elements(&mut frame, *scale, elements, &[damage])
                .map_err(|error| format!("Failed to draw render elements: {error:?}"))?;
        }
        // We rely on the nested compositor to do the sync for us.
        let _ = frame
            .finish()
            .map_err(|error| format!("Failed to finish frame: {error:?}"))?;

        for surface in compositor.state.xdg_shell_state.toplevel_surfaces() {
            send_frames_surface_tree(
                surface.wl_surface(),
                compositor.start_time.elapsed().as_millis() as u32,
            );
            presentation_feedback.extend(take_presentation_feedback_surface_tree(
                surface.wl_surface(),
            ));
        }

        // Hand out the frame callbacks before swapping, which may block.
        compositor.service_clients()?;
    }

    // It is important that all events on the display have been dispatched and flushed to clients
    // before swapping buffers because this operation may block.
    winit
        .submit(Some(&[damage]))
        .map_err(|error| format!("Failed to submit frame: {error}"))?;

    // The frame is on screen: answer the wp_presentation feedback for the state it showed, which
    // KWin paces itself by.
    let compositor = &mut backend.compositor;
    compositor.frame_sequence += 1;
    if let Some(output) = &compositor.output {
        // Matches the 60 Hz mode the output advertises.
        let refresh = Refresh::fixed(Duration::from_micros(16_667));
        let time = backend.clock.now();
        for callback in presentation_feedback {
            callback.presented(
                output,
                time,
                refresh,
                compositor.frame_sequence,
                wp_presentation_feedback::Kind::Vsync,
            );
        }
    }

    Ok(())
}
