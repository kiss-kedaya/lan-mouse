use crate::config::local_commit;
use crate::listen::{LanMouseListener, ListenEvent, ListenerCreationError};
use futures::StreamExt;
use input_emulation::{EmulationHandle, InputEmulation, InputEmulationError};
use input_event::Event;
use input_event::{PointerEvent, Rect};
use lan_mouse_proto::{PeerGeometry, Position, ProtoEvent, geometry::Edge};
use local_channel::mpsc::{Receiver, Sender, channel};
use std::{
    cell::Cell,
    collections::HashMap,
    net::SocketAddr,
    rc::Rc,
    time::{Duration, Instant},
};
use tokio::{
    select,
    task::{JoinHandle, spawn_local},
};

/// emulation handling events received from a listener
pub(crate) struct Emulation {
    task: JoinHandle<()>,
    request_tx: Sender<EmulationRequest>,
    event_rx: Receiver<EmulationEvent>,
}

pub(crate) enum EmulationEvent {
    Connected {
        addr: SocketAddr,
        fingerprint: String,
    },
    ConnectionAttempt {
        fingerprint: String,
    },
    /// the peer reported its screenspace geometry
    PeerGeometry {
        addr: SocketAddr,
        geometry: PeerGeometry,
    },
    /// new connection
    Entered {
        /// address of the connection
        addr: SocketAddr,
        /// position of the connection
        pos: lan_mouse_ipc::Position,
        /// certificate fingerprint of the connection
        fingerprint: String,
    },

    /// connection closed
    Disconnected {
        addr: SocketAddr,
    },
    /// the port of the listener has changed
    PortChanged(Result<u16, ListenerCreationError>),
    /// emulation was disabled, with the reason it could not be started
    EmulationDisabled(String),
    /// emulation was enabled
    EmulationEnabled,
    /// capture should be released
    ReleaseNotify,
    /// peer sent us a Hello with its build commit hash. Used to
    /// populate `client_manager.peer_commit` from the listen side
    /// too — without this, peer-version visibility silently fails
    /// whenever the outgoing connection in the *other* direction is
    /// broken (one-way setups, asymmetric NAT, peer's TCP listener
    /// down). The connect-side path stays as the primary source;
    /// this is the defensive fallback.
    PeerHello {
        addr: SocketAddr,
        commit: [u8; 8],
    },
}

enum EmulationRequest {
    Reenable,
    Release(SocketAddr),
    /// report our screenspace to a connected peer
    Geometry(SocketAddr, PeerGeometry),
    ChangePort(u16),
    Terminate,
}

impl Emulation {
    pub(crate) fn new(
        backend: Option<input_emulation::Backend>,
        listener: LanMouseListener,
    ) -> Self {
        let emulation_proxy = EmulationProxy::new(backend);
        let (request_tx, request_rx) = channel();
        let (event_tx, event_rx) = channel();
        let emulation_task = ListenTask {
            listener,
            emulation_proxy,
            request_rx,
            event_tx,
        };
        let task = spawn_local(emulation_task.run());
        Self {
            task,
            request_tx,
            event_rx,
        }
    }

    pub(crate) fn send_leave_event(&self, addr: SocketAddr) {
        self.request_tx
            .send(EmulationRequest::Release(addr))
            .expect("channel closed");
    }

    pub(crate) fn reenable(&self) {
        self.request_tx
            .send(EmulationRequest::Reenable)
            .expect("channel closed");
    }

    /// tell a connected peer where our screenspace is
    pub(crate) fn send_geometry(&self, addr: SocketAddr, geometry: PeerGeometry) {
        self.request_tx
            .send(EmulationRequest::Geometry(addr, geometry))
            .expect("channel closed");
    }

    pub(crate) fn request_port_change(&self, port: u16) {
        self.request_tx
            .send(EmulationRequest::ChangePort(port))
            .expect("channel closed")
    }

    pub(crate) async fn event(&mut self) -> EmulationEvent {
        self.event_rx.recv().await.expect("channel closed")
    }

    /// wait for termination
    pub(crate) async fn terminate(&mut self) {
        log::debug!("terminating emulation");
        self.request_tx
            .send(EmulationRequest::Terminate)
            .expect("channel closed");
        if let Err(e) = (&mut self.task).await {
            log::warn!("{e}");
        }
    }
}

struct ListenTask {
    listener: LanMouseListener,
    emulation_proxy: EmulationProxy,
    request_rx: Receiver<EmulationRequest>,
    event_tx: Sender<EmulationEvent>,
}

impl ListenTask {
    async fn run(mut self) {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        let mut last_response = HashMap::new();
        let mut rejected_connections = HashMap::new();
        // screenspace of this device and whether we can warp our pointer
        let local_space = self.emulation_proxy.screenspace();
        let local_absolute = self.emulation_proxy.supports_absolute();
        // the peer's screenspace (reported on connect)
        let mut peer_space: Option<Rect> = None;
        // where the local pointer was put for the active incoming capture
        let mut entry = LocalPtr::default();
        let mut entry_edge = lan_mouse_ipc::Position::Left;
        // last pointer position driven by the peer
        let mut pointer = LocalPtr::default();
        loop {
            select! {
                e = self.listener.next() => {match e {
                    Some(ListenEvent::Msg { event, addr }) => {
                        log::trace!("{event} <-<-<-<-<- {addr}");
                        last_response.insert(addr, Instant::now());
                        match event {
                            ProtoEvent::Enter(pos) => {
                                let entry_pos = pos;
                                if let Some(fingerprint) = self.listener.get_certificate_fingerprint(addr).await {
                                    log::info!("releasing capture: {addr} entered this device");
                                    // tell the peer where our screenspace is so it
                                    // sends absolute positions and knows the way back
                                    if let Some(space) = local_space {
                                        self.listener.reply(addr, ProtoEvent::Geometry(
                                            PeerGeometry::from_rect(space, local_absolute), 0)).await;
                                    }
                                    // walk into the screen from the entered barrier
                                    // and carry the pointer along, so it keeps moving
                                    // once the local capture is released
                                    if let Some(space) = local_space {
                                        let mid = match entry_pos {
                                            Position::Left => (space.x, space.y + space.height / 2.0),
                                            Position::Right => (space.right() - 1.0, space.y + space.height / 2.0),
                                            Position::Top => (space.x + space.width / 2.0, space.y),
                                            Position::Bottom => (space.x + space.width / 2.0, space.bottom() - 1.0),
                                        };
                                        let inside = match entry_pos {
                                            Position::Left => (mid.0 + 1.0, mid.1),
                                            Position::Right => (mid.0 - 1.0, mid.1),
                                            Position::Top => (mid.0, mid.1 + 1.0),
                                            Position::Bottom => (mid.0, mid.1 - 1.0),
                                        };
                                        entry = LocalPtr::point(inside.0, inside.1);
                                        entry_edge = to_ipc_pos(entry_pos);
                                        pointer = entry;
                                        self.emulation_proxy.warp(inside.0, inside.1);
                                    }
                                    self.event_tx.send(EmulationEvent::ReleaseNotify).expect("channel closed");
                                    self.listener.reply(addr, ProtoEvent::Ack(0)).await;
                                    self.event_tx.send(EmulationEvent::Entered{addr, pos: to_ipc_pos(pos), fingerprint}).expect("channel closed");
                                }
                            }
                            ProtoEvent::Leave(_) => {
                                let mut cursor = None;
                                // the peer's pointer left its screen: restore
                                // our pointer where it was before entering, or
                                // mirror the peer's path back onto our edge
                                if let Some((ex, ey)) = entry.take() {
                                    cursor = match (local_space, peer_space) {
                                        (Some(ours), Some(theirs)) => {
                                            edge_return(pointer, ours, theirs, entry_edge)
                                        }
                                        _ => None,
                                    }
                                    .or(Some((ex, ey)));
                                }
                                self.emulation_proxy.remove(addr);
                                if let Some((x, y)) = cursor {
                                    self.emulation_proxy.warp(x, y);
                                }
                                self.listener.reply(addr, ProtoEvent::Ack(0)).await;
                            }
                            ProtoEvent::Input(event) => {
                                if let Event::Pointer(PointerEvent::Position { x, y, .. }) = event {
                                    pointer = LocalPtr::point(x, y);
                                }
                                self.emulation_proxy.consume(event, addr)
                            }
                            ProtoEvent::Geometry(geometry, _) => {
                                log::debug!("peer geometry: {geometry:?}");
                                peer_space = Some(geometry.screen);
                                self.emulation_proxy.set_geometry(geometry.screen);
                                self.listener.reply(addr, ProtoEvent::GeometryAck(0)).await;
                                self.event_tx.send(EmulationEvent::PeerGeometry { addr, geometry }).expect("channel closed");
                            }
                            ProtoEvent::GeometryAck(_) => {}
                            ProtoEvent::Ping => self.listener.reply(addr, ProtoEvent::Pong(self.emulation_proxy.emulation_active.get())).await,
                            // Peer's version handshake. Echo our own
                            // commit back so the peer's connect-side
                            // receive_loop populates its `peer_commit`,
                            // AND publish a PeerHello upward so our
                            // service can populate ours from the listen
                            // side too — the connect side is the primary
                            // path, but if the outbound direction is
                            // broken (one-way setup, NAT, peer's TCP
                            // listener down) the version display would
                            // otherwise silently say "unknown" while
                            // the peer is in fact happily talking to us.
                            ProtoEvent::Hello { commit } => {
                                self.listener.reply(addr, ProtoEvent::Hello { commit: local_commit() }).await;
                                self.event_tx.send(EmulationEvent::PeerHello { addr, commit }).expect("channel closed");
                            }
                            _ => {}
                        }
                    }
                    Some(ListenEvent::Accept { addr, fingerprint }) => {
                        self.event_tx.send(EmulationEvent::Connected { addr, fingerprint }).expect("channel closed");
                    }
                    Some(ListenEvent::Rejected { fingerprint }) => {
                        if rejected_connections.insert(fingerprint.clone(), Instant::now())
                            .is_none_or(|i| i.elapsed() >= Duration::from_secs(2)) {
                                self.event_tx.send(EmulationEvent::ConnectionAttempt { fingerprint }).expect("channel closed");
                            }
                    }
                    None => break
                }}
                event = self.emulation_proxy.event() => {
                    self.event_tx.send(event).expect("channel closed");
                }
                request = self.request_rx.recv() => match request.expect("channel closed") {
                    // reenable emulation
                    EmulationRequest::Reenable => self.emulation_proxy.reenable(),
                    // notify the other end that we hit a barrier (should release capture)
                    EmulationRequest::Release(addr) => self.listener.reply(addr, ProtoEvent::Leave(0)).await,
                    EmulationRequest::Geometry(addr, geometry) => {
                        self.emulation_proxy.set_geometry(geometry.screen);
                        self.listener.reply(addr, ProtoEvent::Geometry(geometry, 0)).await
                    }
                    EmulationRequest::ChangePort(port) => {
                        self.listener.request_port_change(port);
                        let result = self.listener.port_changed().await;
                        self.event_tx.send(EmulationEvent::PortChanged(result)).expect("channel closed");
                    }
                    EmulationRequest::Terminate => break,
                },
                _ = interval.tick() => {
                    last_response.retain(|&addr,instant| {
                        if instant.elapsed() > Duration::from_secs(1) {
                            log::warn!("releasing keys: {addr} not responding!");
                            self.emulation_proxy.remove(addr);
                            self.event_tx.send(EmulationEvent::Disconnected { addr }).expect("channel closed");
                            false
                        } else {
                            true
                        }
                    });
                }
            }
        }
        self.listener.terminate().await;
        self.emulation_proxy.terminate().await;
    }
}

/// proxy handling the actual input emulation,
/// discarding events when it is disabled
pub(crate) struct EmulationProxy {
    emulation_active: Rc<Cell<bool>>,
    exit_requested: Rc<Cell<bool>>,
    screenspace: Rc<Cell<Option<Rect>>>,
    absolute: Rc<Cell<bool>>,
    request_tx: Sender<ProxyRequest>,
    event_rx: Receiver<EmulationEvent>,
    task: JoinHandle<()>,
}

enum ProxyRequest {
    Input(Event, SocketAddr),
    Warp(f64, f64),
    SetGeometry(Rect),
    Remove(SocketAddr),
    Terminate,
    Reenable,
}

impl EmulationProxy {
    fn new(backend: Option<input_emulation::Backend>) -> Self {
        let (request_tx, request_rx) = channel();
        let (event_tx, event_rx) = channel();
        let emulation_active = Rc::new(Cell::new(false));
        let exit_requested = Rc::new(Cell::new(false));
        let screenspace = Rc::new(Cell::new(None));
        let absolute = Rc::new(Cell::new(false));
        let emulation_task = EmulationTask {
            backend,
            exit_requested: exit_requested.clone(),
            request_rx,
            event_tx,
            handles: Default::default(),
            next_id: 0,
            screenspace: screenspace.clone(),
            absolute: absolute.clone(),
            peer_screen: None,
            pointer: None,
            deferred: Default::default(),
        };
        let task = spawn_local(emulation_task.run());
        Self {
            emulation_active,
            exit_requested,
            request_tx,
            task,
            event_rx,
            screenspace,
            absolute,
        }
    }

    async fn event(&mut self) -> EmulationEvent {
        let event = self.event_rx.recv().await.expect("channel closed");
        if let EmulationEvent::EmulationEnabled = event {
            self.emulation_active.replace(true);
        }
        if let EmulationEvent::EmulationDisabled(_) = event {
            self.emulation_active.replace(false);
        }
        event
    }

    fn consume(&self, event: Event, addr: SocketAddr) {
        // ignore events if emulation is currently disabled
        if self.emulation_active.get() {
            self.request_tx
                .send(ProxyRequest::Input(event, addr))
                .expect("channel closed");
        }
    }

    fn warp(&self, x: f64, y: f64) {
        self.request_tx
            .send(ProxyRequest::Warp(x, y))
            .expect("channel closed");
    }

    fn screenspace(&self) -> Option<Rect> {
        self.screenspace.get()
    }

    fn supports_absolute(&self) -> bool {
        self.absolute.get()
    }

    fn set_geometry(&self, screen: Rect) {
        self.request_tx
            .send(ProxyRequest::SetGeometry(screen))
            .expect("channel closed");
    }

    fn remove(&self, addr: SocketAddr) {
        self.request_tx
            .send(ProxyRequest::Remove(addr))
            .expect("channel closed");
    }

    fn reenable(&self) {
        self.request_tx
            .send(ProxyRequest::Reenable)
            .expect("channel closed");
    }

    async fn terminate(&mut self) {
        self.exit_requested.replace(true);
        self.request_tx
            .send(ProxyRequest::Terminate)
            .expect("channel closed");
        let _ = (&mut self.task).await;
    }
}

struct EmulationTask {
    backend: Option<input_emulation::Backend>,
    exit_requested: Rc<Cell<bool>>,
    request_rx: Receiver<ProxyRequest>,
    event_tx: Sender<EmulationEvent>,
    handles: HashMap<SocketAddr, EmulationHandle>,
    next_id: EmulationHandle,
    screenspace: Rc<Cell<Option<Rect>>>,
    absolute: Rc<Cell<bool>>,
    /// peer screenspace, used to scale relative motion of peers that cannot
    /// warp their pointer (e.g. X11) into the local one
    peer_screen: Option<Rect>,
    /// position of the peer-driven pointer, kept for the return trip
    pointer: Option<(f64, f64)>,
    /// requests of the current burst, handled after its motion
    deferred: std::collections::VecDeque<ProxyRequest>,
}

impl EmulationTask {
    /// peers with an absolute pointer get absolute positions; peers that
    /// only produce relative motion get absolute positions split into
    /// screen-relative deltas, so screens of different sizes can be driven
    /// without the pointer drifting against an edge.
    fn map_event(&mut self, event: Event, emulation: &mut InputEmulation) -> Event {
        let Event::Pointer(PointerEvent::Position { time, x, y }) = event else {
            return event;
        };
        if emulation.supports_absolute() {
            self.pointer = Some((x, y));
            return Event::Pointer(PointerEvent::Position { time, x, y });
        }
        let previous = self.pointer.unwrap_or((x, y));
        let local = emulation.screenspace();
        let peer = self.peer_screen;
        let (dx, dy) = match (local, peer) {
            (Some(local), Some(peer)) if local.width > 0.0 && peer.width > 0.0 => (
                (x - previous.0) * local.width / peer.width,
                (y - previous.1) * local.height / peer.height,
            ),
            _ => (x - previous.0, y - previous.1),
        };
        self.pointer = Some((x, y));
        Event::Pointer(PointerEvent::Motion { time, dx, dy })
    }

    async fn run(mut self) {
        let screenspace = self.screenspace.clone();
        let absolute = self.absolute.clone();
        let on_ready = move |emulation: &InputEmulation| {
            screenspace.set(emulation.screenspace());
            absolute.set(emulation.supports_absolute());
        };
        loop {
            if let Err(e) = self.do_emulation(&on_ready).await {
                log::warn!("input emulation exited: {e}");
                let _ = self
                    .event_tx
                    .send(EmulationEvent::EmulationDisabled(e.to_string()));
            }
            if self.exit_requested.get() {
                break;
            }
            // wait for reenable request
            loop {
                match self.request_rx.recv().await.expect("channel closed") {
                    ProxyRequest::Reenable => break,
                    ProxyRequest::Terminate => return,
                    ProxyRequest::Input(..) => { /* emulation inactive => ignore */ }
                    ProxyRequest::SetGeometry(..) | ProxyRequest::Warp(..) => { /* nothing to do while inactive */
                    }
                    ProxyRequest::Remove(..) => { /* emulation inactive => ignore */ }
                }
            }
        }
    }

    async fn do_emulation<F: Fn(&InputEmulation)>(
        &mut self,
        on_ready: &F,
    ) -> Result<(), InputEmulationError> {
        log::info!("creating input emulation ...");
        let mut emulation = tokio::select! {
            r = InputEmulation::new(self.backend) => r?,
            // allow termination event while requesting input emulation
            _ = wait_for_termination(&mut self.request_rx) => return Ok(()),
        };
        on_ready(&emulation);

        // used to send enabled and disabled events
        let _emulation_guard = DropGuard::new(
            self.event_tx.clone(),
            EmulationEvent::EmulationEnabled,
            EmulationEvent::EmulationDisabled(String::new()),
        );

        // create active handles
        if let Err(e) = self.create_clients(&mut emulation).await {
            emulation.terminate().await;
            return Err(e);
        }

        let res = self.do_emulation_session(&mut emulation).await;
        // FIXME replace with async drop when stabilized
        emulation.terminate().await;
        res
    }

    async fn create_clients(
        &mut self,
        emulation: &mut InputEmulation,
    ) -> Result<(), InputEmulationError> {
        for handle in self.handles.values() {
            tokio::select! {
                _ = emulation.create(*handle) => {},
                _ = wait_for_termination(&mut self.request_rx) => return Ok(()),
            }
        }
        Ok(())
    }

    /// Coalesce the motion events that queued up while the previous batch was
    /// applied. A high-dpi mouse produces several motion events per display
    /// frame; forwarding them one by one only adds latency and scheduling
    /// overhead, so only the newest position (or the summed relative motion)
    /// of a burst is kept. Buttons and keys are never dropped, and a burst is
    /// limited in size so lifecycle requests are not starved.
    fn coalesce(&mut self, first: Event, addr: SocketAddr) -> (Event, SocketAddr) {
        enum Kind {
            Position,
            Relative(f64, f64),
        }
        let (mut motion, mut kind) = match first {
            Event::Pointer(event @ PointerEvent::Motion { dx, dy, .. }) => {
                (Event::Pointer(event), Kind::Relative(dx, dy))
            }
            Event::Pointer(event @ PointerEvent::Position { .. }) => {
                (Event::Pointer(event), Kind::Position)
            }
            other => return (other, addr),
        };
        let mut drained = 0;
        while drained < 128 {
            let next = {
                use futures::Stream;
                use std::task::{Context, Poll};
                // the channel is a plain in-memory stream, polling it once
                // never blocks and never yields
                let mut cx = Context::from_waker(futures::task::noop_waker_ref());
                match std::pin::Pin::new(&mut self.request_rx).poll_next(&mut cx) {
                    Poll::Ready(Some(request)) => request,
                    _ => break,
                }
            };
            match next {
                ProxyRequest::Input(Event::Pointer(p), a) if a == addr => match p {
                    PointerEvent::Motion { time, dx, dy } => {
                        if let Kind::Relative(sx, sy) = &mut kind {
                            *sx += dx;
                            *sy += dy;
                            motion = Event::Pointer(PointerEvent::Motion {
                                time,
                                dx: *sx,
                                dy: *sy,
                            });
                        }
                    }
                    PointerEvent::Position { time, x, y } => {
                        kind = Kind::Position;
                        motion = Event::Pointer(PointerEvent::Position { time, x, y });
                    }
                    // buttons and scroll are individual events
                    other => self
                        .deferred
                        .push_back(ProxyRequest::Input(Event::Pointer(other), a)),
                },
                other => self.deferred.push_back(other),
            }
            drained += 1;
        }
        (motion, addr)
    }

    async fn do_emulation_session(
        &mut self,
        emulation: &mut InputEmulation,
    ) -> Result<(), InputEmulationError> {
        loop {
            tokio::select! {
                e = self.request_rx.recv() => match e.expect("channel closed") {
                    ProxyRequest::Input(event, addr) => {
                        let handle = match self.handles.get(&addr) {
                            Some(&handle) => handle,
                            None => {
                                let handle = self.next_id;
                                self.next_id += 1;
                                emulation.create(handle).await;
                                self.handles.insert(addr, handle);
                                handle
                            }
                        };
                        let (event, _) = self.coalesce(event, addr);
                        let event = self.map_event(event, emulation);
                        emulation.consume(event, handle).await?;
                        // events of the burst that are not motion, in order
                        while let Some(request) = self.deferred.pop_front() {
                            match request {
                                ProxyRequest::Input(event, _) => {
                                    let event = self.map_event(event, emulation);
                                    emulation.consume(event, handle).await?;
                                }
                                other => {
                                    self.deferred.push_front(other);
                                    break;
                                }
                            }
                        }
                    },
                    ProxyRequest::Warp(x, y) => {
                        if let Some(&handle) = self.handles.values().next() {
                            emulation.warp(x, y, handle).await?;
                        }
                    }
                    ProxyRequest::SetGeometry(screen) => self.peer_screen = Some(screen),
                    ProxyRequest::Remove(addr) => {
                        if let Some(handle) = self.handles.remove(&addr) {
                            emulation.destroy(handle).await;
                        }
                    }
                    ProxyRequest::Terminate => break Ok(()),
                    ProxyRequest::Reenable => continue,
                },
            }
        }
    }
}

/// the local pointer position we track for incoming captures
#[derive(Clone, Copy, Default)]
struct LocalPtr(Option<(f64, f64)>);

impl LocalPtr {
    fn point(x: f64, y: f64) -> Self {
        Self(Some((x, y)))
    }

    fn take(&mut self) -> Option<(f64, f64)> {
        self.0.take()
    }
}

/// Mirror the pointer path of the peer back onto our edge: the peer clamps
/// its pointer at the barrier, so its position tells us where along the edge
/// we re-enter after the incoming capture is released.
fn edge_return(
    pointer: LocalPtr,
    ours: Rect,
    theirs: Rect,
    pos: lan_mouse_ipc::Position,
) -> Option<(f64, f64)> {
    let (px, py) = pointer.0?;
    let edge = match pos {
        lan_mouse_ipc::Position::Left => Edge::Left,
        lan_mouse_ipc::Position::Right => Edge::Right,
        lan_mouse_ipc::Position::Top => Edge::Top,
        lan_mouse_ipc::Position::Bottom => Edge::Bottom,
    };
    let local = PeerGeometry::from_rect(ours, true);
    let peer = PeerGeometry::from_rect(theirs, true);
    let t = edge.fraction_at(px, py, &peer);
    Some(edge.point_on(t, &local))
}

fn to_ipc_pos(pos: Position) -> lan_mouse_ipc::Position {
    match pos {
        Position::Left => lan_mouse_ipc::Position::Left,
        Position::Right => lan_mouse_ipc::Position::Right,
        Position::Top => lan_mouse_ipc::Position::Top,
        Position::Bottom => lan_mouse_ipc::Position::Bottom,
    }
}

async fn wait_for_termination(rx: &mut Receiver<ProxyRequest>) {
    loop {
        match rx.recv().await.expect("channel closed") {
            ProxyRequest::Terminate => return,
            ProxyRequest::Input(_, _) => continue,
            ProxyRequest::SetGeometry(_) | ProxyRequest::Warp(..) => continue,
            ProxyRequest::Remove(_) => continue,
            ProxyRequest::Reenable => continue,
        }
    }
}

struct DropGuard<T> {
    tx: Sender<T>,
    on_drop: Option<T>,
}

impl<T> DropGuard<T> {
    fn new(tx: Sender<T>, on_new: T, on_drop: T) -> Self {
        tx.send(on_new).expect("channel closed");
        let on_drop = Some(on_drop);
        Self { tx, on_drop }
    }
}

impl<T> Drop for DropGuard<T> {
    fn drop(&mut self) {
        self.tx
            .send(self.on_drop.take().expect("item"))
            .expect("channel closed");
    }
}
