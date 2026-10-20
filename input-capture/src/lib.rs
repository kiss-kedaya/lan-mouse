use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt::Display,
    future::Future,
    mem::swap,
    pin::Pin,
    task::{Poll, ready},
};

use async_trait::async_trait;
use futures::StreamExt;
use futures_core::Stream;

use input_event::{Event, KeyboardEvent, Rect, scancode};

pub use error::{CaptureCreationError, CaptureError, InputCaptureError};

pub mod error;

#[cfg(libei)]
mod libei;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(layer_shell)]
mod layer_shell;

#[cfg(windows)]
mod windows;

#[cfg(x11)]
mod x11;

/// fallback input capture (does not produce events)
mod dummy;

pub type CaptureHandle = u64;

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum CaptureEvent {
    /// capture on this capture handle is now active.
    /// Carries the absolute position (local screenspace pixels) the pointer
    /// of the local device should be anchored at while a remote device
    /// drives it; `None` if the backend cannot report one.
    Begin(Option<(f64, f64)>),
    /// input event coming from capture handle.
    /// `pending` counts the further events already queued by the backend, so
    /// the pipeline can batch a burst of high-dpi motion into one frame.
    Input(Event),
}

impl Display for CaptureEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureEvent::Begin(pos) => match pos {
                Some((x, y)) => write!(f, "begin capture @ {x},{y}"),
                None => write!(f, "begin capture"),
            },
            CaptureEvent::Input(e) => write!(f, "{e}"),
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, Hash, PartialEq)]
pub enum Position {
    Left,
    Right,
    Top,
    Bottom,
}

impl Position {
    pub fn opposite(&self) -> Self {
        match self {
            Position::Left => Self::Right,
            Position::Right => Self::Left,
            Position::Top => Self::Bottom,
            Position::Bottom => Self::Top,
        }
    }
}

impl Display for Position {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pos = match self {
            Position::Left => "left",
            Position::Right => "right",
            Position::Top => "top",
            Position::Bottom => "bottom",
        };
        write!(f, "{pos}")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Backend {
    #[cfg(libei)]
    InputCapturePortal,
    #[cfg(layer_shell)]
    LayerShell,
    #[cfg(x11)]
    X11,
    #[cfg(windows)]
    Windows,
    #[cfg(target_os = "macos")]
    MacOs,
    Dummy,
}

impl Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(libei)]
            Backend::InputCapturePortal => write!(f, "input-capture-portal"),
            #[cfg(layer_shell)]
            Backend::LayerShell => write!(f, "layer-shell"),
            #[cfg(x11)]
            Backend::X11 => write!(f, "X11"),
            #[cfg(windows)]
            Backend::Windows => write!(f, "windows"),
            #[cfg(target_os = "macos")]
            Backend::MacOs => write!(f, "MacOS"),
            Backend::Dummy => write!(f, "dummy"),
        }
    }
}

pub struct InputCapture {
    /// capture backend
    capture: Box<dyn Capture>,
    /// keys pressed by active capture
    pressed_keys: HashSet<scancode::Linux>,
    /// map from position to ids
    position_map: HashMap<Position, Vec<CaptureHandle>>,
    /// map from id to position
    id_map: HashMap<CaptureHandle, Position>,
    /// pending events
    pending: VecDeque<(CaptureHandle, CaptureEvent)>,
}

impl InputCapture {
    /// create a new client with the given id
    pub async fn create(&mut self, id: CaptureHandle, pos: Position) -> Result<(), CaptureError> {
        assert!(!self.id_map.contains_key(&id));

        self.id_map.insert(id, pos);

        if let Some(v) = self.position_map.get_mut(&pos) {
            v.push(id);
            Ok(())
        } else {
            self.position_map.insert(pos, vec![id]);
            self.capture.create(pos).await
        }
    }

    /// destroy the client with the given id, if it exists
    pub async fn destroy(&mut self, id: CaptureHandle) -> Result<(), CaptureError> {
        let pos = self
            .id_map
            .remove(&id)
            .expect("no position for this handle");

        log::debug!("destroying capture {id} @ {pos}");
        let remaining = self.position_map.get_mut(&pos).expect("id vector");
        remaining.retain(|&i| i != id);

        log::debug!("remaining ids @ {pos}: {remaining:?}");
        if remaining.is_empty() {
            log::debug!("destroying capture @ {pos} - no remaining ids");
            self.position_map.remove(&pos);
            self.capture.destroy(pos).await?;
        }
        Ok(())
    }

    /// release mouse
    pub async fn release(&mut self) -> Result<(), CaptureError> {
        self.pressed_keys.clear();
        self.capture.release().await
    }

    /// whether the capture backend can place the local pointer at an
    /// absolute position (needed to enter this device at the matching spot)
    pub fn supports_absolute(&self) -> bool {
        self.capture.supports_absolute()
    }

    /// move the hidden local pointer to `(x, y)` in screenspace pixels.
    /// The backend reports the result as a [`CaptureEvent::Input`] event.
    pub async fn warp(&mut self, x: f64, y: f64) -> Result<(), CaptureError> {
        self.capture.warp(x, y).await
    }

    /// bounding box of the local screenspace in pixels
    pub fn screenspace(&self) -> Option<Rect> {
        self.capture.screenspace()
    }

    /// Take every event the backend has queued right now. The very next
    /// motion event already coalesces the burst, so one poll per frame is
    /// enough even for high-dpi devices; buttons and keys keep their order.
    pub fn drain(&mut self, cx: &mut std::task::Context<'_>) -> Vec<(CaptureHandle, CaptureEvent)> {
        let mut out = Vec::new();
        while out.len() < 256 {
            match std::pin::Pin::new(&mut *self).poll_next(cx) {
                std::task::Poll::Ready(Some(Ok(event))) => out.push(event),
                _ => break,
            }
        }
        out
    }

    /// Drain and return every key the capture has forwarded as
    /// down-but-not-up. The caller is expected to synthesize key-up
    /// events to the remote peer for each — otherwise the peer
    /// retains phantom-held keys after capture is released. The
    /// canonical case is the release-bind chord
    /// (Ctrl+Shift+Alt+Meta): the down events were sent while
    /// capture was active, but the matching up events arrive after
    /// the local tap has flipped to passthrough and never reach
    /// the peer.
    pub fn take_pressed_keys(&mut self) -> HashSet<scancode::Linux> {
        std::mem::take(&mut self.pressed_keys)
    }

    /// destroy the input capture
    pub async fn terminate(&mut self) -> Result<(), CaptureError> {
        self.capture.terminate().await
    }

    /// creates a new [`InputCapture`]
    pub async fn new(backend: Option<Backend>) -> Result<Self, CaptureCreationError> {
        let capture = create(backend).await?;
        Ok(Self {
            capture,
            id_map: Default::default(),
            pending: Default::default(),
            position_map: Default::default(),
            pressed_keys: HashSet::new(),
        })
    }

    /// check whether the given keys are pressed
    pub fn keys_pressed(&self, keys: &[scancode::Linux]) -> bool {
        keys.iter().all(|k| self.pressed_keys.contains(k))
    }

    fn update_pressed_keys(&mut self, key: u32, state: u8) {
        if let Ok(scancode) = scancode::Linux::try_from(key) {
            log::debug!("key: {key}, state: {state}, scancode: {scancode:?}");
            match state {
                1 => self.pressed_keys.insert(scancode),
                _ => self.pressed_keys.remove(&scancode),
            };
        }
    }
}

impl Stream for InputCapture {
    type Item = Result<(CaptureHandle, CaptureEvent), CaptureError>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        if let Some(e) = self.pending.pop_front() {
            return Poll::Ready(Some(Ok(e)));
        }

        // ready
        let event = ready!(self.capture.poll_next_unpin(cx));

        // stream closed
        let event = match event {
            Some(e) => e,
            None => return Poll::Ready(None),
        };

        // error occurred
        let (pos, event) = match event {
            Ok(e) => e,
            Err(e) => return Poll::Ready(Some(Err(e))),
        };

        // handle key presses
        if let CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Key { key, state, .. })) = event {
            self.update_pressed_keys(key, state);
        }

        let len = self
            .position_map
            .get(&pos)
            .map(|ids| ids.len())
            .unwrap_or(0);

        match len {
            0 => Poll::Pending,
            1 => Poll::Ready(Some(Ok((
                self.position_map.get(&pos).expect("no id")[0],
                event,
            )))),
            _ => {
                let mut position_map = HashMap::new();
                swap(&mut self.position_map, &mut position_map);
                {
                    for &id in position_map.get(&pos).expect("position") {
                        self.pending.push_back((id, event));
                    }
                }
                swap(&mut self.position_map, &mut position_map);

                Poll::Ready(Some(Ok(self.pending.pop_front().expect("event"))))
            }
        }
    }
}

#[async_trait]
trait Capture: Stream<Item = Result<(Position, CaptureEvent), CaptureError>> + Unpin {
    /// create a new client with the given id
    async fn create(&mut self, pos: Position) -> Result<(), CaptureError>;

    /// destroy the client with the given id, if it exists
    async fn destroy(&mut self, pos: Position) -> Result<(), CaptureError>;

    /// release mouse
    async fn release(&mut self) -> Result<(), CaptureError>;

    /// destroy the input capture
    async fn terminate(&mut self) -> Result<(), CaptureError>;

    /// whether [`Self::warp`] is implemented
    fn supports_absolute(&self) -> bool {
        false
    }

    /// place the local pointer at an absolute screenspace position.
    /// The backends run on the same thread as the caller, hence no `Send`.
    fn warp(
        &mut self,
        _x: f64,
        _y: f64,
    ) -> Pin<Box<dyn Future<Output = Result<(), CaptureError>> + '_>>;

    /// bounding box of the local screenspace in pixels
    fn screenspace(&self) -> Option<Rect> {
        None
    }
}

async fn create_backend(
    backend: Backend,
) -> Result<
    Box<dyn Capture<Item = Result<(Position, CaptureEvent), CaptureError>>>,
    CaptureCreationError,
> {
    // the hooks place the cursor at screen edges, which needs a display
    #[cfg(windows)]
    {
        log::info!("checking the displays of this session");
        if !windows::displays_available() {
            log::warn!("no display is reported for this session");
            return Err(CaptureCreationError::NoDisplays);
        }
        log::info!("displays available, starting the windows capture backend");
    }
    match backend {
        #[cfg(libei)]
        Backend::InputCapturePortal => Ok(Box::new(libei::LibeiInputCapture::new().await?)),
        #[cfg(layer_shell)]
        Backend::LayerShell => Ok(Box::new(layer_shell::LayerShellInputCapture::new()?)),
        #[cfg(x11)]
        Backend::X11 => Ok(Box::new(x11::X11InputCapture::new()?)),
        #[cfg(windows)]
        Backend::Windows => Ok(Box::new(windows::WindowsInputCapture::new())),
        #[cfg(target_os = "macos")]
        Backend::MacOs => Ok(Box::new(macos::MacOSInputCapture::new().await?)),
        Backend::Dummy => Ok(Box::new(dummy::DummyInputCapture::new())),
    }
}

async fn create(
    backend: Option<Backend>,
) -> Result<
    Box<dyn Capture<Item = Result<(Position, CaptureEvent), CaptureError>>>,
    CaptureCreationError,
> {
    if let Some(backend) = backend {
        let b = create_backend(backend).await;
        if b.is_ok() {
            log::info!("using capture backend: {backend}");
        }
        return b;
    }

    for backend in [
        #[cfg(libei)]
        Backend::InputCapturePortal,
        #[cfg(layer_shell)]
        Backend::LayerShell,
        #[cfg(x11)]
        Backend::X11,
        #[cfg(windows)]
        Backend::Windows,
        #[cfg(target_os = "macos")]
        Backend::MacOs,
    ] {
        match create_backend(backend).await {
            Ok(b) => {
                log::info!("using capture backend: {backend}");
                return Ok(b);
            }
            Err(e) if e.cancelled_by_user() => return Err(e),
            Err(e) => log::warn!("{backend} input capture backend unavailable: {e}"),
        }
    }
    Err(CaptureCreationError::NoAvailableBackend)
}
