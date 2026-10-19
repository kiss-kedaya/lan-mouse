use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::{Duration, Instant},
};

use futures::StreamExt;
use input_capture::{
    CaptureError, CaptureEvent, CaptureHandle, InputCapture, InputCaptureError, Position,
};
use input_event::{Event, KeyboardEvent, PointerEvent, Rect, scancode};
use lan_mouse_proto::{PeerGeometry, ProtoEvent, geometry::Edge};
use local_channel::mpsc::{Receiver, Sender, channel};
use std::task::Context;
use tokio::task::{JoinHandle, spawn_local};
use tokio_util::sync::CancellationToken;

use crate::connect::LanMouseConnection;

pub(crate) struct Capture {
    screenspace: Rc<Cell<Option<Rect>>>,
    absolute: Rc<Cell<bool>>,
    cancellation_token: CancellationToken,
    request_tx: Sender<CaptureRequest>,
    task: JoinHandle<()>,
    event_rx: Receiver<ICaptureEvent>,
}

pub(crate) enum ICaptureEvent {
    /// a client was entered
    CaptureBegin(CaptureHandle),
    /// capture disabled
    CaptureDisabled,
    /// capture disabled
    CaptureEnabled,
    /// A (new) client was entered.
    /// In contrast to [`ICaptureEvent::CaptureBegin`] this
    /// event is only triggered when the capture was
    /// explicitly released in the meantime by
    /// either the remote client leaving its device region,
    /// a new device entering the screen or the release bind.
    ClientEntered(u64),
    /// The previously active client was left, i.e. capture
    /// was released for the given handle. Mirrors
    /// [`ICaptureEvent::ClientEntered`] for the leave side
    /// and fires on every release path (release-bind chord,
    /// remote `Leave`, explicit `Release` request, send
    /// failure, or destroy of the active capture).
    ClientLeft(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CaptureType {
    /// a normal input capture
    Default,
    /// A capture only interested in [`CaptureEvent::Begin`] events.
    /// The capture is released immediately, if there is no
    /// Default capture at the same position.
    EnterOnly,
}

#[derive(Clone, Debug)]
enum CaptureRequest {
    /// capture must release the mouse
    Release,
    /// add a capture client
    Create(CaptureHandle, Position, CaptureType),
    /// destory a capture client
    Destroy(CaptureHandle),
    /// reenable input capture
    Reenable,
    /// set release bind
    SetReleaseBind(Vec<scancode::Linux>),
    /// the peer reported its screenspace
    PeerGeometry(PeerGeometry),
    /// place the local pointer at an absolute position
    Warp(f64, f64),
    /// drop hooks of edges without active clients
    ReleaseUnusedHooks,
}

impl Capture {
    pub(crate) fn new(
        backend: Option<input_capture::Backend>,
        conn: LanMouseConnection,
        release_bind: Vec<scancode::Linux>,
    ) -> Self {
        let (request_tx, request_rx) = channel();
        let (event_tx, event_rx) = channel();
        let cancellation_token = CancellationToken::new();
        let screenspace = Rc::new(Cell::new(None));
        let absolute = Rc::new(Cell::new(false));
        let capture_task = CaptureTask {
            published_space: screenspace.clone(),
            published_absolute: absolute.clone(),
            active_client: None,
            backend,
            cancellation_token: cancellation_token.clone(),
            captures: Default::default(),
            conn,
            event_tx,
            request_rx,
            release_bind: Rc::new(RefCell::new(release_bind)),
            state: Default::default(),
            wanted: Default::default(),
            local_geometry: None,
            peer_geometry: None,
            sent_geometry: None,
            hook_handles: Default::default(),
        };
        let task = spawn_local(capture_task.run());
        Self {
            screenspace,
            absolute,
            cancellation_token,
            request_tx,
            task,
            event_rx,
        }
    }

    pub(crate) fn reenable(&self) {
        self.request_tx
            .send(CaptureRequest::Reenable)
            .expect("channel closed");
    }

    pub(crate) async fn terminate(&mut self) {
        self.cancellation_token.cancel();
        log::debug!("terminating capture");
        if let Err(e) = (&mut self.task).await {
            log::warn!("{e}");
        }
    }

    pub(crate) fn create(
        &self,
        handle: CaptureHandle,
        pos: lan_mouse_ipc::Position,
        capture_type: CaptureType,
    ) {
        let pos = to_capture_pos(pos);
        self.request_tx
            .send(CaptureRequest::Create(handle, pos, capture_type))
            .expect("channel closed");
    }

    pub(crate) fn destroy(&self, handle: CaptureHandle) {
        self.request_tx
            .send(CaptureRequest::Destroy(handle))
            .expect("channel closed");
    }

    pub(crate) fn release(&self) {
        self.request_tx
            .send(CaptureRequest::Release)
            .expect("channel closed");
    }

    pub(crate) async fn event(&mut self) -> ICaptureEvent {
        self.event_rx.recv().await.expect("channel closed")
    }

    pub(crate) fn set_release_bind(&mut self, bind: Vec<scancode::Linux>) {
        let _ = self.request_tx.send(CaptureRequest::SetReleaseBind(bind));
    }

    /// remove hooks for edges that no longer have an active client
    pub(crate) fn release_unused_hooks(&self) -> Result<(), CaptureError> {
        self.request_tx
            .send(CaptureRequest::ReleaseUnusedHooks)
            .map_err(|_| CaptureError::ActivationClosed)
    }

    /// bounding box of the local screenspace (empty until the backend is up)
    pub(crate) fn screenspace(&self) -> Option<Rect> {
        self.screenspace.get()
    }

    pub(crate) fn supports_absolute(&self) -> bool {
        self.absolute.get()
    }

    /// move the local pointer to an absolute position
    pub(crate) fn warp(&self, x: f64, y: f64) {
        let _ = self.request_tx.send(CaptureRequest::Warp(x, y));
    }

    /// the peer reported its screenspace
    pub(crate) fn set_peer_geometry(&self, geometry: PeerGeometry) {
        let _ = self.request_tx.send(CaptureRequest::PeerGeometry(geometry));
    }
}

/// debounce a statement `$st`, i.e. the statement is executed only if the
/// time since the previous execution is at least `$dur`.
/// `$prev` is used to keep track of this timestamp
macro_rules! debounce {
    ($prev:ident, $dur:expr, $st:stmt) => {
        let exec = match $prev.get() {
            None => true,
            Some(instant) if instant.elapsed() > $dur => true,
            _ => false,
        };
        if exec {
            $prev.replace(Some(Instant::now()));
            $st
        }
    };
}

struct CaptureTask {
    active_client: Option<CaptureHandle>,
    backend: Option<input_capture::Backend>,
    cancellation_token: CancellationToken,
    captures: Vec<(CaptureHandle, Position, CaptureType)>,
    conn: LanMouseConnection,
    event_tx: Sender<ICaptureEvent>,
    release_bind: Rc<RefCell<Vec<scancode::Linux>>>,
    request_rx: Receiver<CaptureRequest>,
    state: State,
    /// positions the service wants to watch, including the hook-only binds
    wanted: [bool; 4],
    /// local screenspace, refreshed whenever the capture (re)starts
    local_geometry: Option<PeerGeometry>,
    /// last geometry sent to the active client
    sent_geometry: Option<PeerGeometry>,
    /// handles of the hook-only captures (one per edge)
    hook_handles: [Option<CaptureHandle>; 4],
    /// screenspace published to the service layer
    published_space: Rc<Cell<Option<Rect>>>,
    published_absolute: Rc<Cell<bool>>,
    peer_geometry: Option<PeerGeometry>,
}

/// index of a position in the fixed-size position tables
fn pos_index(pos: Position) -> usize {
    match pos {
        Position::Left => 0,
        Position::Right => 1,
        Position::Top => 2,
        Position::Bottom => 3,
    }
}

impl CaptureTask {
    fn add_capture(&mut self, handle: CaptureHandle, pos: Position, capture_type: CaptureType) {
        self.captures.push((handle, pos, capture_type));
    }

    fn remove_capture(&mut self, handle: CaptureHandle) {
        self.captures.retain(|&(h, ..)| handle != h);
    }

    fn is_default_capture_at(&self, pos: Position) -> bool {
        self.captures
            .iter()
            .any(|&(_, p, t)| p == pos && t == CaptureType::Default)
    }

    fn get_pos_opt(&self, handle: CaptureHandle) -> Option<Position> {
        self.captures
            .iter()
            .find(|(h, ..)| *h == handle)
            .map(|(_, p, _)| *p)
    }

    fn get_type_opt(&self, handle: CaptureHandle) -> Option<CaptureType> {
        self.captures
            .iter()
            .find(|(h, ..)| *h == handle)
            .map(|(_, _, t)| *t)
    }

    fn get_pos(&self, handle: CaptureHandle) -> Position {
        self.captures
            .iter()
            .find(|(h, ..)| *h == handle)
            .expect("no such capture")
            .1
    }

    fn get_type(&self, handle: CaptureHandle) -> CaptureType {
        self.captures
            .iter()
            .find(|(h, ..)| *h == handle)
            .expect("no such capture")
            .2
    }

    async fn run(mut self) {
        loop {
            if let Err(e) = self.do_capture().await {
                log::warn!("input capture exited: {e}");
            }
            loop {
                tokio::select! {
                    r = self.request_rx.recv() => match r.expect("channel closed") {
                        CaptureRequest::Reenable => break,
                        CaptureRequest::Create(h, p, t) => self.add_capture(h, p, t),
                        CaptureRequest::Destroy(h) => self.remove_capture(h),
                        CaptureRequest::Release => { /* nothing to do */ }
                        CaptureRequest::SetReleaseBind(bind) => {
                            self.release_bind.borrow_mut().clone_from(&bind);
                        }
                        CaptureRequest::Warp(..)
                        | CaptureRequest::ReleaseUnusedHooks
                        | CaptureRequest::PeerGeometry(..) => {}
                    },
                    _ = self.cancellation_token.cancelled() => return,
                }
            }
        }
    }

    async fn do_capture(&mut self) -> Result<(), InputCaptureError> {
        /* allow cancelling capture request */
        let mut capture = tokio::select! {
            r = InputCapture::new(self.backend) => r?,
            _ = self.cancellation_token.cancelled() => return Ok(()),
        };

        let _capture_guard = DropGuard::new(
            self.event_tx.clone(),
            ICaptureEvent::CaptureEnabled,
            ICaptureEvent::CaptureDisabled,
        );

        self.sent_geometry = None;
        self.hook_handles = Default::default();
        self.local_geometry = capture
            .screenspace()
            .map(|space| PeerGeometry::from_rect(space, capture.supports_absolute()));
        self.publish_geometry();
        log::debug!("local screenspace: {:?}", self.local_geometry);

        /* create barriers for active clients */
        let r = self.create_captures(&mut capture).await;
        if let Err(e) = r {
            capture.terminate().await?;
            return Err(e.into());
        }

        let r = self.do_capture_session(&mut capture).await;

        // FIXME replace with async drop when stabilized
        capture.terminate().await?;

        r
    }

    async fn create_captures(&mut self, capture: &mut InputCapture) -> Result<(), CaptureError> {
        let captures = self.captures.clone();
        for (handle, pos, _type) in captures {
            tokio::select! {
                r = capture.create(handle, pos) => r?,
                _ = self.cancellation_token.cancelled() => return Ok(()),
            }
        }
        Ok(())
    }

    async fn do_capture_session(
        &mut self,
        capture: &mut InputCapture,
    ) -> Result<(), InputCaptureError> {
        loop {
            tokio::select! {
                event = capture.next() => match event {
                    Some(event) => {
                        let first = event?;
                        // a burst of motion is applied in one go, then the
                        // peer gets a single absolute position for the frame
                        let mut burst = capture.drain(&mut Context::from_waker(futures::task::noop_waker_ref()));
                        self.handle_capture_event(capture, first).await?;
                        for event in burst.drain(..) {
                            self.handle_capture_event(capture, event).await?;
                        }
                    }
                    None => return Ok(()),
                },
                (handle, event) = self.conn.recv() => {
                    if let Some(active) = self.active_client {
                        if handle != active {
                            // we only care about events coming from the client we are currently connected to
                            // only `Ack` and `Leave` are relevant
                            continue
                        }
                    }

                    match event {
                        // connection acknowlegded => set state to Sending
                        ProtoEvent::Ack(_) => {
                            log::info!("client {handle} acknowledged the connection!");
                            self.state = State::Sending;
                        }
                        // client disconnected
                        ProtoEvent::Leave(_) => {
                            log::info!("releasing capture: left remote client device region");
                            self.release_capture(capture).await?;
                        },
                        _ => {}
                    }
                },
                e = self.request_rx.recv() => match e.expect("channel closed") {
                    CaptureRequest::Reenable => { /* already active */ },
                    CaptureRequest::Release => self.release_capture(capture).await?,
                    CaptureRequest::Create(h, p, t) => {
                        self.add_capture(h, p, t);
                        capture.create(h, p).await?;
                        self.wanted[pos_index(p)] = true;
                        self.ensure_hook(capture, p).await?;
                    }
                    CaptureRequest::Destroy(h) => {
                        // If the capture we're tearing down is the
                        // currently-active one, treat this as a
                        // release for hook purposes. The release_capture
                        // path also clears active_client and flushes
                        // pressed-key state to the peer; without this,
                        // `cli deactivate` (or a hostname change
                        // re-creating the client) would skip leave_hook.
                        if self.active_client == Some(h) {
                            self.release_capture(capture).await?;
                        }
                        self.remove_capture(h);
                        if let (Some(CaptureType::Default), Some(pos)) =
                            (self.get_type_opt(h), self.get_pos_opt(h))
                        {
                            capture.destroy(h).await?;
                            // keep the hook when another client shares the edge
                            self.wanted[pos_index(pos)] = self
                                .captures
                                .iter()
                                .any(|&(_, p, t)| p == pos && t == CaptureType::Default);
                        }
                    }
                    CaptureRequest::SetReleaseBind(bind) => {
                        self.release_bind.borrow_mut().clone_from(&bind);
                    }
                    CaptureRequest::Warp(x, y) => {
                        capture.warp(x, y).await?;
                    }
                    CaptureRequest::ReleaseUnusedHooks => {
                        self.prune_hooks(capture).await?;
                    }
                    CaptureRequest::PeerGeometry(geometry) => {
                        log::debug!("peer geometry: {geometry:?}");
                        self.peer_geometry = Some(geometry);
                    }
                },
                _ = self.cancellation_token.cancelled() => break,
            }
        }
        Ok(())
    }

    /// drop hook-only captures on edges that no client uses anymore and
    /// (re)create hooks for edges that gained one
    async fn prune_hooks(&mut self, capture: &mut InputCapture) -> Result<(), CaptureError> {
        for pos in [
            Position::Left,
            Position::Right,
            Position::Top,
            Position::Bottom,
        ] {
            let idx = pos_index(pos);
            let used = self.wanted[idx];
            match (used, self.hook_handles[idx]) {
                (false, Some(handle)) => {
                    log::debug!("removing hook for {pos}");
                    self.hook_handles[idx] = None;
                    capture.destroy(handle).await?;
                }
                (true, None) => {
                    let handle = u64::MAX - idx as u64;
                    self.hook_handles[idx] = Some(handle);
                    capture.create(handle, pos).await?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    async fn handle_capture_event(
        &mut self,
        capture: &mut InputCapture,
        event: (CaptureHandle, CaptureEvent),
    ) -> Result<(), CaptureError> {
        let (handle, event) = event;
        log::trace!("({handle}): {event:?}");

        if capture.keys_pressed(&self.release_bind.borrow()) {
            log::info!("releasing capture: release-bind pressed");
            return self.release_capture(capture).await;
        }

        // the backend handed out a synthetic absolute position for one of
        // its hook captures: grow the local geometry with it
        if self.hook_handles.iter().any(|h| *h == Some(handle)) {
            if let CaptureEvent::Input(Event::Pointer(p @ PointerEvent::Position { .. })) = event {
                let absolute = capture.supports_absolute();
                self.local_geometry = Some(match self.local_geometry {
                    Some(g) => g.with_point(p),
                    None => PeerGeometry::from_point(p, absolute),
                });
                self.publish_geometry();
                self.push_geometry(capture).await;
            }
            return Ok(());
        }

        if matches!(event, CaptureEvent::Begin(_)) {
            self.event_tx
                .send(ICaptureEvent::CaptureBegin(handle))
                .expect("channel closed");
        }

        // enter only capture (for incoming connections)
        if self.get_type(handle) == CaptureType::EnterOnly {
            // if there is no active outgoing connection at the current capture,
            // we release the capture
            if !self.is_default_capture_at(self.get_pos(handle)) {
                log::info!("releasing capture: no active client at this position");
                capture.release().await?;
            }
            // we dont care about events from incoming handles except for releasing the capture
            return Ok(());
        }

        // activated a new client
        if matches!(event, CaptureEvent::Begin(_)) {
            self.push_geometry(capture).await;
        }

        let edge = to_edge(self.get_pos(handle));
        // the remote device reports its position on the edge that faces us
        let remote_edge = opposite_edge(edge);
        let mut coordinates = None;
        if let CaptureEvent::Begin(entry) = event {
            if Some(handle) != self.active_client {
                self.state = State::WaitingForAck;
                self.active_client.replace(handle);
                self.event_tx
                    .send(ICaptureEvent::ClientEntered(handle))
                    .expect("channel closed");
            }
            // absolute pointer position where the local device entered the
            // barrier (the backends also report the full edge extents as
            // synthetic positions, which keep the geometry up to date)
            if let (Some((_, y)), Some(local), Some(peer)) =
                (entry, &self.local_geometry, &self.peer_geometry)
            {
                if peer.absolute {
                    let t = edge.fraction_at(
                        if edge.is_horizontal() { 0.0 } else { y },
                        if edge.is_horizontal() { y } else { 0.0 },
                        local,
                    );
                    let (x, y) = lan_mouse_proto::geometry::edge_point(remote_edge, t, peer);
                    coordinates = Some(PointerEvent::Position { time: 0, x, y });
                }
            }
        }

        let event = match event {
            CaptureEvent::Begin(_) => match coordinates {
                Some(p) => ProtoEvent::Input(Event::Pointer(p)),
                None => ProtoEvent::Enter(to_proto_edge(edge)),
            },
            CaptureEvent::Input(e) => match self.state {
                // connection not acknowledged, repeat `Enter` event
                State::WaitingForAck => ProtoEvent::Enter(to_proto_edge(edge)),
                State::Sending => match (e, &self.local_geometry, &self.peer_geometry) {
                    // Tell the peer where it should put the pointer and how
                    // far it may move before coming back. Only browsers with
                    // an absolute pointer can honour it; the rest gets
                    // relative motion as before.
                    (
                        Event::Pointer(PointerEvent::Position { time, x: _, y }),
                        Some(local),
                        Some(peer),
                    ) if peer.absolute => {
                        // our pointer walks along our edge while the peer's
                        // screen is driven; map its fraction to the peer edge
                        let (cx, cy) = if edge.is_horizontal() {
                            (0.0, y)
                        } else {
                            (y, 0.0)
                        };
                        let t = edge.fraction_at(cx, cy, local);
                        let (x, y) = lan_mouse_proto::geometry::edge_point(remote_edge, t, peer);
                        ProtoEvent::Input(Event::Pointer(PointerEvent::Position { time, x, y }))
                    }
                    _ => ProtoEvent::Input(e),
                },
            },
        };

        if let Err(e) = self.conn.send(event, handle).await {
            const DUR: Duration = Duration::from_millis(500);
            debounce!(PREV_LOG, DUR, log::warn!("releasing capture: {e}"));
            // Funnel through release_capture so the leave_hook
            // fires and active_client is cleared (without this the
            // active_client field would stay stale until the next
            // Begin from a different handle).
            self.release_capture(capture).await?;
        }
        Ok(())
    }

    fn publish_geometry(&self) {
        self.published_space
            .set(self.local_geometry.map(|g| g.screen));
        self.published_absolute
            .set(capture_absolute(&self.local_geometry));
    }

    /// tell the active client where our screenspace is so it can map
    /// absolute positions. Called on entry and whenever our geometry was
    /// refreshed (e.g. after a display change).
    async fn push_geometry(&mut self, capture: &InputCapture) {
        let Some(handle) = self.active_client else {
            return;
        };
        let geometry = match capture.screenspace() {
            Some(space) => PeerGeometry::from_rect(space, capture.supports_absolute()),
            None => match self.local_geometry {
                Some(g) => g,
                None => return,
            },
        };
        if self.local_geometry != Some(geometry) {
            self.local_geometry = Some(geometry);
        }
        if self.sent_geometry == Some(geometry) {
            return;
        }
        if let Err(e) = self.conn.send_geometry(handle, geometry).await {
            log::debug!("sending geometry failed: {e}");
            return;
        }
        self.sent_geometry = Some(geometry);
    }

    /// register a hook-only capture for `pos` if no client is active there
    /// and no hook exists yet
    async fn ensure_hook(
        &mut self,
        capture: &mut InputCapture,
        pos: Position,
    ) -> Result<(), CaptureError> {
        let idx = pos_index(pos);
        if !self.wanted[idx] && self.hook_handles[idx].is_none() {
            log::debug!("registering hook for {pos}");
            let handle = u64::MAX - idx as u64;
            self.hook_handles[idx] = Some(handle);
            capture.create(handle, pos).await?;
        }
        Ok(())
    }

    async fn release_capture(&mut self, capture: &mut InputCapture) -> Result<(), CaptureError> {
        // If we have an active client, notify them we're leaving
        if let Some(handle) = self.active_client.take() {
            // Surface the leave to the service layer so it can fire
            // the per-client leave_hook. Sent before the network
            // teardown below so we never race against the peer
            // disappearing.
            self.event_tx
                .send(ICaptureEvent::ClientLeft(handle))
                .expect("channel closed");
            // Synthesize key-up events for every key still held in the
            // capture's pressed_keys set BEFORE sending Leave. Without
            // this, pressing the release-bind chord (typically all four
            // modifiers) leaves the peer with phantom held modifiers:
            // the down events were forwarded while capture was active,
            // but the matching up events arrive after the local tap
            // flips to passthrough and never reach the peer. The peer
            // then runs every subsequent keystroke through those held
            // mods until its watchdog times out (1+ s) or our Leave
            // arrives — and Leave can be lost over UDP/DTLS.
            for key in capture.take_pressed_keys() {
                let key_up = ProtoEvent::Input(Event::Keyboard(KeyboardEvent::Key {
                    time: 0,
                    key: key as u32,
                    state: 0,
                }));
                if let Err(e) = self.conn.send(key_up, handle).await {
                    log::warn!("failed to send key-up to client {handle}: {e}");
                }
            }
            // Reset the modifier mask too. The peer's input-emulation
            // layer keeps a separate XKB-style modifier state that's
            // updated by KeyboardEvent::Modifiers, distinct from the
            // pressed_keys set drained above. Without this, an
            // already-locked CapsLock would survive the release.
            let mods_zero = ProtoEvent::Input(Event::Keyboard(KeyboardEvent::Modifiers {
                depressed: 0,
                latched: 0,
                locked: 0,
                group: 0,
            }));
            if let Err(e) = self.conn.send(mods_zero, handle).await {
                log::warn!("failed to reset modifiers on client {handle}: {e}");
            }

            // Hand control back with the position of our pointer: the peer
            // then puts *its* pointer back where it left the screen instead
            // of wherever the OS clamped it.
            log::info!("sending Leave event to client {handle}");
            if let Err(e) = self.conn.send(ProtoEvent::Leave(0), handle).await {
                log::warn!("failed to send Leave to client {handle}: {e}");
            }
        }
        capture.release().await
    }
}

thread_local! {
    static PREV_LOG: Cell<Option<Instant>> = const { Cell::new(None) };
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    WaitingForAck,
    Sending,
}

fn capture_absolute(geometry: &Option<PeerGeometry>) -> bool {
    geometry.map(|g| g.absolute).unwrap_or(false)
}

/// the edge a peer on the opposite side sees
fn opposite_edge(edge: Edge) -> Edge {
    match edge {
        Edge::Left => Edge::Right,
        Edge::Right => Edge::Left,
        Edge::Top => Edge::Bottom,
        Edge::Bottom => Edge::Top,
    }
}

fn to_edge(pos: Position) -> Edge {
    match pos {
        Position::Left => Edge::Left,
        Position::Right => Edge::Right,
        Position::Top => Edge::Top,
        Position::Bottom => Edge::Bottom,
    }
}

fn to_proto_edge(edge: Edge) -> lan_mouse_proto::Position {
    match edge {
        Edge::Left => lan_mouse_proto::Position::Left,
        Edge::Right => lan_mouse_proto::Position::Right,
        Edge::Top => lan_mouse_proto::Position::Top,
        Edge::Bottom => lan_mouse_proto::Position::Bottom,
    }
}

fn to_capture_pos(pos: lan_mouse_ipc::Position) -> input_capture::Position {
    match pos {
        lan_mouse_ipc::Position::Left => input_capture::Position::Left,
        lan_mouse_ipc::Position::Right => input_capture::Position::Right,
        lan_mouse_ipc::Position::Top => input_capture::Position::Top,
        lan_mouse_ipc::Position::Bottom => input_capture::Position::Bottom,
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
