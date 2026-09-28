use super::bind::bind_socket;
use smithay::{
    backend::renderer::utils::on_commit_buffer_handler,
    delegate_compositor, delegate_data_device, delegate_fractional_scale, delegate_output,
    delegate_pointer_constraints,
    delegate_presentation, delegate_seat, delegate_shm, delegate_single_pixel_buffer,
    delegate_viewporter, delegate_xdg_shell,
    input::{self, keyboard::KeyboardHandle, touch::TouchHandle, Seat, SeatHandler, SeatState},
    output::Output,
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{protocol::wl_seat, Display},
    },
    utils::{Logical, Monotonic, Point, Serial, Size},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            with_states, with_surface_tree_downward, CompositorClientState, CompositorHandler,
            CompositorState, SurfaceAttributes, TraversalAction,
        },
        fractional_scale::{
            with_fractional_scale, FractionalScaleHandler, FractionalScaleManagerState,
        },
        output::OutputHandler,
        pointer_constraints::{PointerConstraintsHandler, PointerConstraintsState},
        presentation::{
            PresentationFeedbackCachedState, PresentationFeedbackCallback, PresentationState,
        },
        selection::{
            data_device::{
                ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
            },
            SelectionHandler,
        },
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
        },
        shm::{ShmHandler, ShmState},
        single_pixel_buffer::SinglePixelBufferState,
        viewporter::ViewporterState,
    },
};
use smithay::{
    input::pointer::PointerHandle,
    reexports::wayland_server::{
        backend::{ClientData, ClientId, DisconnectReason, GlobalId},
        protocol::{wl_buffer, wl_surface::WlSurface},
        Client, ListeningSocket, Resource,
    },
};
use crate::android::accessibility::{self, AppUserEvent};
use std::{
    error::Error,
    os::unix::io::{AsFd, AsRawFd, OwnedFd, RawFd},
    sync::{mpsc, Arc},
    thread,
    time::Instant,
};
use winit::event_loop::EventLoopProxy;

pub struct Compositor {
    pub state: State,
    pub display: Display<State>,
    pub listener: ListeningSocket,
    pub clients: Vec<Client>,
    pub start_time: Instant,
    pub seat: Seat<State>,
    pub keyboard: KeyboardHandle<State>,
    pub touch: TouchHandle<State>,
    pub pointer: PointerHandle<State>,
    pub output: Option<Output>,
    pub output_global: Option<GlobalId>,
    /// Frames presented so far, for presentation feedback.
    pub frame_sequence: u64,
    /// Tells the client waker that the clients were serviced since it last woke the loop.
    waker_ack: Option<mpsc::Sender<()>>,
    /// Whether the client waker is waiting for that.
    waker_waiting: bool,
}

pub struct State {
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    pub data_device_state: DataDeviceState,
    pub seat_state: SeatState<Self>,
    // Nested compositors such as KWin require these.
    pub viewporter_state: ViewporterState,
    pub single_pixel_buffer_state: SinglePixelBufferState,
    pub pointer_constraints_state: PointerConstraintsState,
    pub presentation_state: PresentationState,
    pub fractional_scale_state: FractionalScaleManagerState,
    /// The Android window's size in physical pixels.
    pub size: Size<i32, Logical>,
    /// The scale offered to clients that support fractional scaling: the Android UI scale.
    pub client_scale: f64,
    /// Surfaces whose clients asked for fractional scaling. They get logical sizes and are drawn
    /// scaled by `client_scale`; everyone else works in physical pixels.
    pub scaled_surfaces: Vec<WlSurface>,
    /// Something on screen changed since the last frame (a surface committed or went away).
    pub needs_redraw: bool,
}

impl State {
    /// The scale a surface's content maps to the screen with.
    pub fn surface_scale(&self, surface: &WlSurface) -> f64 {
        if self.scaled_surfaces.contains(surface) {
            self.client_scale
        } else {
            1.0
        }
    }

    /// Configure a toplevel to fill the window, in the toplevel's own size units.
    pub fn configure_toplevel(&self, toplevel: &ToplevelSurface) {
        let scale = self.surface_scale(toplevel.wl_surface());
        let size = Size::from((
            (self.size.w as f64 / scale).round() as i32,
            (self.size.h as f64 / scale).round() as i32,
        ));
        toplevel.with_pending_state(|state| {
            state.size = Some(size);
            state.states.set(xdg_toplevel::State::Activated);
        });
        toplevel.send_configure();
    }

    /// Reconfigure the toplevels that already got their first configure (the others get it on
    /// their initial commit), e.g. after the window size or scale changed.
    pub fn reconfigure_toplevels(&self) {
        for toplevel in self.xdg_shell_state.toplevel_surfaces() {
            if toplevel.is_initial_configure_sent() {
                self.configure_toplevel(toplevel);
            }
        }
    }

    /// Change the scale offered to clients that support fractional scaling.
    pub fn set_client_scale(&mut self, scale: f64) {
        if self.client_scale == scale {
            return;
        }
        self.client_scale = scale;
        self.scaled_surfaces.retain(|surface| surface.is_alive());
        for surface in &self.scaled_surfaces {
            with_states(surface, |states| {
                with_fractional_scale(states, |fractional| fractional.set_preferred_scale(scale))
            });
        }
    }
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, _surface: ToplevelSurface) {
        // Configured on its initial commit, once the client has set up everything that affects
        // the configure (such as fractional scaling).
    }

    fn toplevel_destroyed(&mut self, _surface: ToplevelSurface) {
        self.needs_redraw = true;
    }

    fn new_popup(&mut self, _surface: PopupSurface, _positioner: PositionerState) {
        // Handle popup creation here
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {
        // Handle popup grab here
    }

    fn reposition_request(
        &mut self,
        _surface: PopupSurface,
        _positioner: PositionerState,
        _token: u32,
    ) {
        // Handle popup reposition here
    }
}

impl SelectionHandler for State {
    type SelectionUserData = ();
}

impl DataDeviceHandler for State {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for State {}
impl ServerDndGrabHandler for State {
    fn send(&mut self, _mime_type: String, _fd: OwnedFd, _seat: Seat<Self>) {}
}

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        // Also answers the frame callbacks and presentation feedback this commit asked for.
        self.needs_redraw = true;

        let toplevel = self
            .xdg_shell_state
            .toplevel_surfaces()
            .iter()
            .find(|toplevel| {
                toplevel.wl_surface() == surface && !toplevel.is_initial_configure_sent()
            })
            .cloned();
        if let Some(toplevel) = toplevel {
            self.configure_toplevel(&toplevel);
        }
    }
}

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl SeatHandler for State {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, _seat: &Seat<Self>, _focused: Option<&WlSurface>) {}
    fn cursor_image(&mut self, _seat: &Seat<Self>, _image: input::pointer::CursorImageStatus) {}
}

impl PointerConstraintsHandler for State {
    // Android input stays absolute, so pointer locks and confinement are never activated.
    fn new_constraint(&mut self, _surface: &WlSurface, _pointer: &PointerHandle<Self>) {}

    fn cursor_position_hint(
        &mut self,
        _surface: &WlSurface,
        _pointer: &PointerHandle<Self>,
        _location: Point<f64, Logical>,
    ) {
    }
}

impl FractionalScaleHandler for State {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        let scale = self.client_scale;
        with_states(&surface, |states| {
            with_fractional_scale(states, |fractional| fractional.set_preferred_scale(scale))
        });
        self.scaled_surfaces.retain(|surface| surface.is_alive());
        self.scaled_surfaces.push(surface.clone());

        // A toplevel that was already configured gets its logical size now.
        self.reconfigure_toplevels();
    }
}

/// Take the wp_presentation feedback requests of the surfaces' current state, to answer once the
/// frame showing that state is on screen.
pub fn take_presentation_feedback_surface_tree(
    surface: &WlSurface,
) -> Vec<PresentationFeedbackCallback> {
    let mut callbacks = Vec::new();
    with_surface_tree_downward(
        surface,
        (),
        |_, _, &()| TraversalAction::DoChildren(()),
        |_surf, states, &()| {
            callbacks.append(
                &mut states
                    .cached_state
                    .get::<PresentationFeedbackCachedState>()
                    .current()
                    .callbacks,
            );
        },
        |_, _, &()| true,
    );
    callbacks
}

pub fn send_frames_surface_tree(surface: &WlSurface, time: u32) {
    with_surface_tree_downward(
        surface,
        (),
        |_, _, &()| TraversalAction::DoChildren(()),
        |_surf, states, &()| {
            // the surface may not have any user_data if it is a subsurface and has not
            // yet been commited
            for callback in states
                .cached_state
                .get::<SurfaceAttributes>()
                .current()
                .frame_callbacks
                .drain(..)
            {
                callback.done(time);
            }
        },
        |_, _, &()| true,
    );
}

#[derive(Default)]
pub struct ClientState {
    compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}

    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

impl OutputHandler for State {}

// Macros used to delegate protocol handling to types in the app state.
delegate_xdg_shell!(State);
delegate_compositor!(State);
delegate_shm!(State);
delegate_seat!(State);
delegate_data_device!(State);
delegate_output!(State);
delegate_viewporter!(State);
delegate_single_pixel_buffer!(State);
delegate_pointer_constraints!(State);
delegate_presentation!(State);
delegate_fractional_scale!(State);

impl Compositor {
    pub fn build() -> Result<Compositor, Box<dyn Error>> {
        let mut display = Display::new()?;
        let dh = display.handle();

        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&dh, "Local Desktop");

        let listener = bind_socket()?;
        let clients = Vec::new();

        let start_time = Instant::now();

        // Key repeat rate and delay are in milliseconds: https://wayland-book.com/seat/keyboard.html
        let keyboard = seat
            .add_keyboard(Default::default(), 1000, 200)
            .expect("Failed to add keyboard");
        let touch = seat.add_touch();
        let pointer = seat.add_pointer();

        let state = State {
            compositor_state: CompositorState::new::<State>(&dh),
            xdg_shell_state: XdgShellState::new::<State>(&dh),
            shm_state: ShmState::new::<State>(&dh, vec![]),
            data_device_state: DataDeviceState::new::<State>(&dh),
            seat_state,
            viewporter_state: ViewporterState::new::<State>(&dh),
            single_pixel_buffer_state: SinglePixelBufferState::new::<State>(&dh),
            pointer_constraints_state: PointerConstraintsState::new::<State>(&dh),
            presentation_state: PresentationState::new::<State>(
                &dh,
                smithay::utils::Clock::<Monotonic>::new().id() as u32,
            ),
            fractional_scale_state: FractionalScaleManagerState::new::<State>(&dh),
            size: (1920, 1080).into(),
            client_scale: 1.0,
            scaled_surfaces: Vec::new(),
            needs_redraw: false,
        };

        let waker_ack = accessibility::event_loop_proxy().and_then(|proxy| {
            spawn_client_waker(
                display.backend().poll_fd().as_raw_fd(),
                listener.as_fd().as_raw_fd(),
                proxy,
            )
        });

        Ok(Compositor {
            state,
            listener,
            clients,
            start_time,
            display,
            seat,
            keyboard,
            touch,
            pointer,
            output: None,
            output_global: None,
            frame_sequence: 0,
            waker_ack,
            waker_waiting: false,
        })
    }

    /// Accept new clients, handle what they sent and flush our replies and events. Call it
    /// whenever the event loop is about to sleep, so nothing waits on us.
    pub fn service_clients(&mut self) -> Result<(), String> {
        loop {
            match self.listener.accept() {
                Ok(Some(stream)) => match self
                    .display
                    .handle()
                    .insert_client(stream, Arc::new(ClientState::default()))
                {
                    Ok(client) => self.clients.push(client),
                    Err(error) => log::error!("Failed to insert Wayland client: {error}"),
                },
                Ok(None) => break,
                Err(error) => {
                    log::error!("Failed to accept Wayland client: {error}");
                    break;
                }
            }
        }

        self.display
            .dispatch_clients(&mut self.state)
            .map_err(|error| format!("Failed to dispatch clients: {error}"))?;

        // Give the desktop keyboard focus as soon as it's there, not only after the first tap.
        if self.keyboard.current_focus().is_none() {
            if let Some(toplevel) = self.state.xdg_shell_state.toplevel_surfaces().first() {
                let surface = toplevel.wl_surface().clone();
                self.keyboard.set_focus(
                    &mut self.state,
                    Some(surface),
                    smithay::utils::SERIAL_COUNTER.next_serial(),
                );
            }
        }

        self.display
            .flush_clients()
            .map_err(|error| format!("Failed to flush clients: {error}"))?;

        if self.waker_waiting {
            self.waker_waiting = false;
            if let Some(ack) = &self.waker_ack {
                let _ = ack.send(());
            }
        }
        Ok(())
    }

    /// The client waker woke the event loop; `service_clients` lets it poll again.
    pub fn clients_ready(&mut self) {
        self.waker_waiting = true;
    }

    /// Whether the client waker runs. Without it the event loop has to poll for clients.
    pub fn has_client_waker(&self) -> bool {
        self.waker_ack.is_some()
    }
}

/// Wakes the event loop when a Wayland client sent something or a new one is connecting, so the
/// loop can sleep in between instead of polling. It then waits until the loop has serviced the
/// clients before polling again, or a still-readable fd would flood the loop with wake-ups.
fn spawn_client_waker(
    display_fd: RawFd,
    listener_fd: RawFd,
    proxy: EventLoopProxy<AppUserEvent>,
) -> Option<mpsc::Sender<()>> {
    let (ack, acked) = mpsc::channel();
    let spawned = thread::Builder::new()
        .name("wayland-waker".into())
        .spawn(move || {
            let mut fds = [
                libc::pollfd { fd: display_fd, events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: listener_fd, events: libc::POLLIN, revents: 0 },
            ];
            loop {
                let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
                if ready < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    log::error!("Wayland client waker stopped: {error}");
                    return;
                }
                if proxy.send_event(AppUserEvent::WaylandClientsReady).is_err()
                    || acked.recv().is_err()
                {
                    return;
                }
            }
        });
    match spawned {
        Ok(_) => Some(ack),
        Err(error) => {
            log::error!("Failed to start the Wayland client waker: {error}");
            None
        }
    }
}
