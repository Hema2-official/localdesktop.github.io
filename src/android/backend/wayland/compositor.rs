use super::{bind::bind_socket, dmabuf};
use smithay::{
    backend::{
        allocator::dmabuf::Dmabuf,
        renderer::{
            gles::GlesRenderer,
            utils::{on_commit_buffer_handler, with_renderer_surface_state},
        },
    },
    delegate_compositor, delegate_data_device, delegate_dmabuf, delegate_ext_data_control,
    delegate_fractional_scale, delegate_output, delegate_pointer_constraints,
    delegate_presentation, delegate_seat, delegate_shm, delegate_single_pixel_buffer,
    delegate_viewporter, delegate_xdg_shell,
    desktop::{find_popup_root_surface, get_popup_toplevel_coords, PopupKind, PopupManager},
    input::{self, keyboard::KeyboardHandle, touch::TouchHandle, Seat, SeatHandler, SeatState},
    output::Output,
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{protocol::wl_seat, Display, DisplayHandle},
    },
    utils::{Logical, Monotonic, Point, Rectangle, Serial, Size, SERIAL_COUNTER},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            with_states, with_surface_tree_downward, CompositorClientState, CompositorHandler,
            CompositorState, SurfaceAttributes, TraversalAction,
        },
        dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
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
                set_data_device_focus, ClientDndGrabHandler, DataDeviceHandler, DataDeviceState,
                ServerDndGrabHandler,
            },
            ext_data_control::{DataControlHandler, DataControlState},
            SelectionHandler,
        },
        shell::xdg::{
            PopupSurface, PositionerState, SurfaceCachedState, ToplevelSurface, XdgShellHandler,
            XdgShellState,
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
    /// Clipboard managers' access to the selection: only the guest link's, which keeps the
    /// selection here and the desktop's the same.
    pub data_control_state: DataControlState,
    pub seat_state: SeatState<Self>,
    pub display_handle: DisplayHandle,
    // Nested compositors such as KWin require these.
    pub viewporter_state: ViewporterState,
    pub single_pixel_buffer_state: SinglePixelBufferState,
    pub pointer_constraints_state: PointerConstraintsState,
    pub presentation_state: PresentationState,
    pub fractional_scale_state: FractionalScaleManagerState,
    pub dmabuf_state: DmabufState,
    /// The dma-buf global, once the renderer can draw clients' dma-bufs.
    pub dmabuf_global: Option<DmabufGlobal>,
    /// The Android window's size in physical pixels.
    pub size: Size<i32, Logical>,
    /// The scale offered to clients that support fractional scaling: the Android UI scale.
    pub client_scale: f64,
    /// Surfaces whose clients asked for fractional scaling. They get logical sizes and are drawn
    /// scaled by `client_scale`; everyone else works in physical pixels.
    pub scaled_surfaces: Vec<WlSurface>,
    /// Something on screen changed since the last frame (a surface committed or went away).
    pub needs_redraw: bool,
    pub popups: PopupManager,
    /// The popup that asked for keyboard input (xdg_popup.grab), while it's open.
    pub popup_grab: Option<WlSurface>,
    /// The one output, the Android window, once there is one.
    pub output: Option<Output>,
}

/// Marks a surface that was told it's on the output.
struct EnteredOutput;

/// Marks a toplevel configured again when it first showed something.
struct ConfiguredWhenShown;

impl State {
    /// The scale a surface's content maps to the screen with.
    pub fn surface_scale(&self, surface: &WlSurface) -> f64 {
        if self.scaled_surfaces.contains(surface) {
            self.client_scale
        } else {
            1.0
        }
    }

    /// The toplevels from the bottom up, the newest on top. A program started straight on this
    /// display (a game, say) comes up over the desktop's own compositor and gives it back when it
    /// closes. Only toplevels that have drawn something count, so one that hasn't yet doesn't take
    /// input away from what's on screen.
    pub fn stacked_toplevels(&self) -> Vec<ToplevelSurface> {
        self.xdg_shell_state
            .toplevel_surfaces()
            .iter()
            .filter(|toplevel| has_buffer(toplevel.wl_surface()))
            .cloned()
            .collect()
    }

    /// The toplevel on top, which input goes to.
    pub fn top_toplevel(&self) -> Option<ToplevelSurface> {
        self.stacked_toplevels().pop()
    }

    /// The surface at `location` (in the top toplevel's units) and where its origin is: one of
    /// the top toplevel's popups, the topmost first, or else the toplevel itself.
    pub fn surface_under(
        &self,
        location: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        let toplevel = self.top_toplevel()?;
        let root = toplevel.wl_surface();
        let popups: Vec<_> = popups_of(root).collect();
        for (popup, origin, area) in popups.into_iter().rev() {
            if area.to_f64().contains(location) {
                return Some((popup.wl_surface().clone(), origin.to_f64()));
            }
        }
        Some((root.clone(), (0.0, 0.0).into()))
    }

    /// Whether `location` (in the top toplevel's units) is over one of its popups.
    pub fn over_popup(&self, location: Point<f64, Logical>) -> bool {
        let top = self.top_toplevel();
        self.surface_under(location)
            .is_some_and(|(surface, _)| top.is_some_and(|top| top.wl_surface() != &surface))
    }

    /// The surface keyboard input goes to: the top toplevel, or a popup of it that asked for
    /// keyboard input.
    pub fn keyboard_target(&self) -> Option<WlSurface> {
        let toplevel = self.top_toplevel()?;
        if let Some(popup) = self.popup_grab.as_ref().filter(|it| it.is_alive()) {
            if popups_of(toplevel.wl_surface()).any(|(kind, ..)| kind.wl_surface() == popup) {
                return Some(popup.clone());
            }
        }
        Some(toplevel.wl_surface().clone())
    }

    /// Close the top toplevel's popups, as a press away from a menu does.
    pub fn dismiss_popups(&mut self) {
        let Some(toplevel) = self.top_toplevel() else {
            return;
        };
        let root = toplevel.wl_surface();
        let popups: Vec<_> = PopupManager::popups_for_surface(root).collect();
        if popups.is_empty() {
            return;
        }
        for (popup, _) in popups {
            let _ = PopupManager::dismiss_popup(root, &popup);
        }
        self.popup_grab = None;
        self.needs_redraw = true;
    }

    /// The surfaces whose frame callbacks are due when a frame goes out: the top toplevel with
    /// its popups, and toplevels that haven't drawn yet. A compositor under a program started
    /// straight on this display (the desktop's KWin under a game) waits instead of drawing.
    pub fn surfaces_shown(&self) -> Vec<WlSurface> {
        let top = self.top_toplevel();
        let mut surfaces: Vec<WlSurface> = self
            .xdg_shell_state
            .toplevel_surfaces()
            .iter()
            .map(|toplevel| toplevel.wl_surface())
            .filter(|surface| !has_buffer(surface))
            .cloned()
            .collect();
        if let Some(top) = top {
            surfaces
                .extend(popups_of(top.wl_surface()).map(|(popup, ..)| popup.wl_surface().clone()));
            surfaces.push(top.wl_surface().clone());
        }
        surfaces
    }

    /// Tell a surface it's on the output (wl_surface.enter), once: clients such as mpv, GTK and
    /// SDL take their scale from the output their surfaces are on.
    fn enter_output(&self, surface: &WlSurface) {
        let Some(output) = &self.output else {
            return;
        };
        if with_states(surface, |states| {
            states
                .data_map
                .insert_if_missing_threadsafe(|| EnteredOutput)
        }) {
            output.enter(surface);
        }
    }

    /// Place a popup where its positioner says, moved into the window as far as it allows.
    fn place_popup(&self, popup: &PopupSurface) {
        let kind = PopupKind::Xdg(popup.clone());
        let Ok(root) = find_popup_root_surface(&kind) else {
            return;
        };
        let scale = self.surface_scale(&root);
        let mut target = Rectangle::from_size(Size::from((
            (self.size.w as f64 / scale).round() as i32,
            (self.size.h as f64 / scale).round() as i32,
        )));
        target.loc -= get_popup_toplevel_coords(&kind);
        target.loc -= window_geometry(&root).loc;
        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }

    /// Configure a toplevel to fill the window, in the toplevel's own size units.
    pub fn configure_toplevel(&self, toplevel: &ToplevelSurface) {
        let scale = self.surface_scale(toplevel.wl_surface());
        let size = Size::from((
            (self.size.w as f64 / scale).round() as i32,
            (self.size.h as f64 / scale).round() as i32,
        ));
        // Full screen, as every toplevel here is: a client then takes the size as it is, where it
        // may choose its own for a plain window.
        toplevel.with_pending_state(|state| {
            state.size = Some(size);
            state.states.set(xdg_toplevel::State::Activated);
            state.states.set(xdg_toplevel::State::Fullscreen);
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

    fn new_popup(&mut self, surface: PopupSurface, positioner: PositionerState) {
        surface.with_pending_state(|state| state.geometry = positioner.get_geometry());
        self.place_popup(&surface);
        if let Err(error) = self.popups.track_popup(PopupKind::Xdg(surface)) {
            log::warn!("A popup went away before it was shown: {error:?}");
        }
    }

    fn grab(&mut self, surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {
        // Keyboard input goes to it (see `keyboard_target`); a press elsewhere closes it.
        self.popup_grab = Some(surface.wl_surface().clone());
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        self.place_popup(&surface);
        surface.send_repositioned(token);
    }

    fn popup_destroyed(&mut self, _surface: PopupSurface) {
        self.needs_redraw = true;
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

impl DataControlHandler for State {
    fn data_control_state(&self) -> &DataControlState {
        &self.data_control_state
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
        self.popups.commit(surface);
        self.enter_output(surface);
        // Also answers the frame callbacks and presentation feedback this commit asked for.
        self.needs_redraw = true;

        if let Some(PopupKind::Xdg(popup)) = self.popups.find_popup(surface) {
            // Its first commit, before it shows anything: where it goes.
            if !popup.is_initial_configure_sent() {
                let _ = popup.send_configure();
            }
        }

        let toplevel = self
            .xdg_shell_state
            .toplevel_surfaces()
            .iter()
            .find(|toplevel| toplevel.wl_surface() == surface)
            .cloned();
        if let Some(toplevel) = toplevel {
            if !toplevel.is_initial_configure_sent() {
                self.configure_toplevel(&toplevel);
            } else if has_buffer(surface)
                && with_states(surface, |states| {
                    states
                        .data_map
                        .insert_if_missing_threadsafe(|| ConfiguredWhenShown)
                })
            {
                // Once more when it first shows something, as other compositors do: some clients
                // (mpv) take the first configure's size for a hint in pixels, the later ones in
                // their own units.
                self.configure_toplevel(&toplevel);
            }
        }
    }
}

impl DmabufHandler for State {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        // The renderer imports the buffer when it first draws it.
        if dmabuf::can_import(&dmabuf) {
            let _ = notifier.successful::<State>();
        } else {
            notifier.failed();
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

    /// The program with keyboard focus is the one that gets the selection to paste.
    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let client = focused.and_then(|surface| self.display_handle.get_client(surface.id()).ok());
        set_data_device_focus(&self.display_handle, seat, client);
    }
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

/// Whether a surface has a buffer attached: it has drawn something.
fn has_buffer(surface: &WlSurface) -> bool {
    with_renderer_surface_state(surface, |state| state.buffer().is_some()).unwrap_or(false)
}

/// The window geometry a surface set (xdg_surface.set_window_geometry), if any.
fn window_geometry(surface: &WlSurface) -> Rectangle<i32, Logical> {
    with_states(surface, |states| {
        states
            .cached_state
            .get::<SurfaceCachedState>()
            .current()
            .geometry
    })
    .unwrap_or_default()
}

/// A toplevel's popups from the bottom up, each with where its surface's origin is and the area
/// it shows, relative to the toplevel surface's origin.
pub fn popups_of(
    root: &WlSurface,
) -> impl Iterator<Item = (PopupKind, Point<i32, Logical>, Rectangle<i32, Logical>)> {
    let base = window_geometry(root).loc;
    PopupManager::popups_for_surface(root).map(move |(popup, offset)| {
        let geometry = popup.geometry();
        let origin = base + offset - geometry.loc;
        (popup, origin, Rectangle::new(base + offset, geometry.size))
    })
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
    /// The client is the app itself (the guest link), not a program in the guest.
    from_app: bool,
}

/// Whether the other end of a client's socket is this process.
fn from_app(stream: &impl AsRawFd) -> bool {
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut size = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let got = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut size,
        )
    };
    got == 0 && credentials.pid == std::process::id() as libc::pid_t
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
delegate_ext_data_control!(State);
delegate_output!(State);
delegate_viewporter!(State);
delegate_single_pixel_buffer!(State);
delegate_pointer_constraints!(State);
delegate_presentation!(State);
delegate_fractional_scale!(State);
delegate_dmabuf!(State);

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
            data_control_state: data_control_state(&dh),
            seat_state,
            display_handle: dh.clone(),
            viewporter_state: ViewporterState::new::<State>(&dh),
            single_pixel_buffer_state: SinglePixelBufferState::new::<State>(&dh),
            pointer_constraints_state: PointerConstraintsState::new::<State>(&dh),
            presentation_state: PresentationState::new::<State>(
                &dh,
                smithay::utils::Clock::<Monotonic>::new().id() as u32,
            ),
            fractional_scale_state: FractionalScaleManagerState::new::<State>(&dh),
            dmabuf_state: DmabufState::new(),
            dmabuf_global: None,
            size: (1920, 1080).into(),
            client_scale: 1.0,
            scaled_surfaces: Vec::new(),
            needs_redraw: false,
            popups: PopupManager::default(),
            popup_grab: None,
            output: None,
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
                Ok(Some(stream)) => {
                    let data = ClientState {
                        from_app: from_app(&stream),
                        ..Default::default()
                    };
                    match self.display.handle().insert_client(stream, Arc::new(data)) {
                        Ok(client) => self.clients.push(client),
                        Err(error) => log::error!("Failed to insert Wayland client: {error}"),
                    }
                }
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

        // Keyboard input goes to what's on top from the moment it's there, not only after the
        // first tap, and moves with it.
        self.state.popups.cleanup();
        let target = self.state.keyboard_target();
        if target != self.keyboard.current_focus() {
            self.keyboard
                .set_focus(&mut self.state, target, SERIAL_COUNTER.next_serial());
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

    /// Offer clients the dma-buf protocol once the renderer can draw their dma-bufs.
    pub fn offer_dmabufs(&mut self, renderer: &GlesRenderer) {
        if self.state.dmabuf_global.is_some() || !dmabuf::install(renderer.egl_context().display())
        {
            return;
        }
        let display = self.display.handle();
        let global = self
            .state
            .dmabuf_state
            .create_global::<State>(&display, dmabuf::formats());
        self.state.dmabuf_global = Some(global);
        log::info!("Graphics: clients may hand over dma-bufs");
    }

    /// The client waker woke the event loop; `service_clients` lets it poll again.
    pub fn clients_ready(&mut self) {
        self.waker_waiting = true;
    }

    /// Whether the client waker runs. Without it the event loop has to poll for clients.
    pub fn has_client_waker(&self) -> bool {
        self.waker_ack.is_some()
    }

    /// A new activity has a new event loop: wake that one from now on. The old waker stops once
    /// it finds its loop gone, or right away if it waits for this one.
    pub fn restart_client_waker(&mut self) {
        let display_fd = self.display.backend().poll_fd().as_raw_fd();
        let listener_fd = self.listener.as_fd().as_raw_fd();
        self.waker_waiting = false;
        self.waker_ack = accessibility::event_loop_proxy()
            .and_then(|proxy| spawn_client_waker(display_fd, listener_fd, proxy));
    }
}

/// ext-data-control for the app's own clients only: the guest link connects from this process,
/// the programs in the guest from theirs, and a program that can read the selection without
/// focus could watch everything that is copied.
fn data_control_state(dh: &DisplayHandle) -> DataControlState {
    DataControlState::new::<State, _>(dh, None, |client| {
        client
            .get_data::<ClientState>()
            .is_some_and(|data| data.from_app)
    })
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
