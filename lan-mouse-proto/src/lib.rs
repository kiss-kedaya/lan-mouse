use input_event::{Event as InputEvent, KeyboardEvent, PointerEvent, Rect};
use num_enum::{IntoPrimitive, TryFromPrimitive, TryFromPrimitiveError};
use paste::paste;
use sha2::{Digest, Sha256};
use std::{
    fmt::{Debug, Display, Formatter},
    mem::size_of,
};
use thiserror::Error;

pub mod geometry;

/// defines the maximum size an encoded event can take up
/// this is currently the announcement:
/// type: u8, hostname length: u32, hostname, port: u32, instance: u32, fingerprint: 32 * u8
/// (the geometry event is smaller: type, 8 * f64, absolute, serial)
pub const MAX_EVENT_SIZE: usize =
    size_of::<u8>() + 2 * size_of::<u32>() + MAX_HOSTNAME_LEN + 2 * size_of::<u32>() + 32 + 64;

/// maximum hostname length carried in an [`Announce`] datagram
pub const MAX_HOSTNAME_LEN: usize = 255;

/// maximum length of a pairing code carried in a [`ProtoEvent::PairRequest`]
pub const MAX_PAIRING_CODE_LEN: usize = 64;

/// upper bound of the screen edge of this device, used for clamping
pub const MAX_SCREEN_EDGE: f64 = 1_000_000.0;

/// Announcement of a device on the local network. It is sent as a DTLS
/// datagram from the listen port to the peer's listen port; the certificate
/// authenticates the sender, so the fingerprint cannot be spoofed by other
/// hosts on the network.
#[derive(Clone, Debug, PartialEq)]
pub struct Announce {
    /// hostname of the announcing device
    pub hostname: String,
    /// listen port of the announcing device
    pub port: u16,
    /// token that changes when the device stops / starts announcing
    pub instance: u32,
    /// sha256 fingerprint of the sending certificate
    pub fingerprint: [u8; 32],
}

/// Sent once after connect and whenever the local display layout changes.
/// It lets the peer map relative motion into a genuine absolute position so
/// the pointer keeps its position when crossing devices with different
/// screen heights / offsets (instead of guessing from edge deltas).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PeerGeometry {
    /// bounding box of the local screenspace in pixels
    pub screen: Rect,
    /// outermost coordinate on each edge, used as cursor anchor
    /// while the remote device drives the pointer
    pub left: f64,
    pub right: f64,
    pub top: f64,
    pub bottom: f64,
    /// whether the peer can warp the pointer to absolute coordinates
    pub absolute: bool,
}

impl PeerGeometry {
    fn clamp(mut self) -> Self {
        let c = |v: f64| {
            if v.is_finite() {
                v.clamp(-MAX_SCREEN_EDGE, MAX_SCREEN_EDGE)
            } else {
                0.0
            }
        };
        self.screen.x = c(self.screen.x);
        self.screen.y = c(self.screen.y);
        self.screen.width = c(self.screen.width).abs();
        self.screen.height = c(self.screen.height).abs();
        self.left = c(self.left);
        self.right = c(self.right);
        self.top = c(self.top);
        self.bottom = c(self.bottom);
        self
    }

    /// build geometry where each edge is the outermost coordinate the local
    /// pointer can be warped to (the backend reports one `Position` event at
    /// a time, so the caller grows this as more edges are reported)
    pub fn from_point(point: PointerEvent, absolute: bool) -> Self {
        let (x, y) = match point {
            PointerEvent::Position { x, y, .. } => (x, y),
            _ => (0.0, 0.0),
        };
        Self {
            screen: Rect::new(0.0, 0.0, x + 1.0, y + 1.0),
            left: 0.0,
            right: x,
            top: 0.0,
            bottom: y,
            absolute,
        }
        .clamp()
    }

    /// expand the anchors with an additional reported pointer position
    pub fn with_point(mut self, point: PointerEvent) -> Self {
        if let PointerEvent::Position { x, y, .. } = point {
            self.screen.width = self.screen.width.max(x + 1.0);
            self.screen.height = self.screen.height.max(y + 1.0);
            self.right = self.right.max(x);
            self.bottom = self.bottom.max(y);
        }
        self.clamp()
    }

    /// derive the edge anchors from an axis aligned bounding box
    pub fn from_rect(screen: Rect, absolute: bool) -> Self {
        Self {
            screen,
            left: screen.x,
            right: screen.right() - 1.0,
            top: screen.y,
            bottom: screen.bottom() - 1.0,
            absolute,
        }
        .clamp()
    }
}

/// error type for protocol violations
#[derive(Debug, Error)]
pub enum ProtocolError {
    /// event type does not exist
    #[error("invalid event id: `{0}`")]
    InvalidEventId(#[from] TryFromPrimitiveError<EventType>),
    /// position type does not exist
    #[error("invalid event id: `{0}`")]
    InvalidPosition(#[from] TryFromPrimitiveError<Position>),
    /// hostname in an announcement exceeded the size limit
    #[error("invalid announcement: hostname length {0}")]
    InvalidAnnouncement(usize),
}

/// Position of a client
#[derive(Clone, Copy, Debug, PartialEq, Eq, TryFromPrimitive, IntoPrimitive)]
#[repr(u8)]
pub enum Position {
    Left,
    Right,
    Top,
    Bottom,
}

impl Display for Position {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let pos = match self {
            Position::Left => "left",
            Position::Right => "right",
            Position::Top => "top",
            Position::Bottom => "bottom",
        };
        write!(f, "{pos}")
    }
}

/// main lan-mouse protocol event type
#[derive(Clone, Debug, PartialEq)]
pub enum ProtoEvent {
    /// notify a client that the cursor entered its region at the given position
    /// [`ProtoEvent::Ack`] with the same serial is used for synchronization between devices
    Enter(Position),
    /// notify a client that the cursor left its region
    /// [`ProtoEvent::Ack`] with the same serial is used for synchronization between devices
    Leave(u32),
    /// acknowledge of an [`ProtoEvent::Enter`] or [`ProtoEvent::Leave`] event
    Ack(u32),
    /// Input event
    Input(InputEvent),
    /// Report the geometry of the sending device's screenspace.
    /// `serial` is chosen by the sender and echoed back in
    /// [`ProtoEvent::GeometryAck`] by the peer.
    Geometry(PeerGeometry, u32),
    /// acknowledgemente of a [`ProtoEvent::Geometry`] (its serial)
    GeometryAck(u32),
    /// Ping event for tracking unresponsive clients.
    /// A client has to respond with [`ProtoEvent::Pong`].
    Ping,
    /// Response to [`ProtoEvent::Ping`], true if emulation is enabled / available
    Pong(bool),
    /// Build identification for the sending peer. Sent by the
    /// connect side once after the connection authenticates, and
    /// echoed back by the listen side in reply, so each end can
    /// display the peer's build hash and warn (soft) on mismatch.
    /// `commit` is the 8-byte ASCII short commit hash from
    /// `shadow_rs`'s `SHORT_COMMIT`. Old peers that don't
    /// recognize the event type silently skip it per the
    /// forward-compat handling in the receive loop.
    Hello { commit: [u8; 8] },
    /// announce this device on the local network (discovery / pairing)
    Announce(Announce),
    /// ask the peer to pair with us; `code` is the code the peer displays
    PairRequest { code: String },
    /// pairing accepted, carries the fingerprint of our certificate
    PairAccept([u8; 32]),
    /// the pairing code did not match
    PairReject,
}

impl Display for ProtoEvent {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtoEvent::Enter(s) => write!(f, "Enter({s})"),
            ProtoEvent::Leave(s) => write!(f, "Leave({s})"),
            ProtoEvent::Ack(s) => write!(f, "Ack({s})"),
            ProtoEvent::Input(e) => write!(f, "{e}"),
            ProtoEvent::Geometry(g, _) => write!(f, "Geometry({:?})", g.screen),
            ProtoEvent::GeometryAck(_) => write!(f, "GeometryAck"),
            ProtoEvent::Ping => write!(f, "ping"),
            ProtoEvent::Pong(alive) => {
                write!(
                    f,
                    "pong: {}",
                    if *alive { "alive" } else { "not available" }
                )
            }
            ProtoEvent::Hello { commit } => {
                let s = std::str::from_utf8(commit).unwrap_or("????????");
                write!(f, "Hello({s})")
            }
            ProtoEvent::Announce(a) => write!(f, "Announce({}, port {})", a.hostname, a.port),
            ProtoEvent::PairRequest { .. } => write!(f, "PairRequest"),
            ProtoEvent::PairAccept(_) => write!(f, "PairAccept"),
            ProtoEvent::PairReject => write!(f, "PairReject"),
        }
    }
}

#[derive(TryFromPrimitive, IntoPrimitive)]
#[repr(u8)]
pub enum EventType {
    PointerMotion,
    PointerPosition,
    PointerButton,
    PointerAxis,
    PointerAxisValue120,
    KeyboardKey,
    KeyboardModifiers,
    Ping,
    Pong,
    Enter,
    Leave,
    Ack,
    Hello,
    Geometry,
    GeometryAck,
    Announce,
    PairRequest,
    PairAccept,
    PairReject,
}

impl Announce {
    /// digest used to recognize our own announcements
    pub fn digest(&self) -> u64 {
        let mut hash = Sha256::new();
        hash.update(&self.fingerprint);
        hash.update(self.instance.to_be_bytes());
        hash.update(self.port.to_be_bytes());
        hash.update(self.hostname.as_bytes());
        let out = hash.finalize();
        u64::from_be_bytes(out[..8].try_into().expect("8 bytes"))
    }
}

impl ProtoEvent {
    fn event_type(&self) -> EventType {
        match self {
            ProtoEvent::Geometry(..) => EventType::Geometry,
            ProtoEvent::GeometryAck(_) => EventType::GeometryAck,
            ProtoEvent::Input(e) => match e {
                InputEvent::Pointer(p) => match p {
                    PointerEvent::Motion { .. } => EventType::PointerMotion,
                    PointerEvent::Position { .. } => EventType::PointerPosition,
                    PointerEvent::Button { .. } => EventType::PointerButton,
                    PointerEvent::Axis { .. } => EventType::PointerAxis,
                    PointerEvent::AxisDiscrete120 { .. } => EventType::PointerAxisValue120,
                },
                InputEvent::Keyboard(k) => match k {
                    KeyboardEvent::Key { .. } => EventType::KeyboardKey,
                    KeyboardEvent::Modifiers { .. } => EventType::KeyboardModifiers,
                },
            },
            ProtoEvent::Ping => EventType::Ping,
            ProtoEvent::Pong(_) => EventType::Pong,
            ProtoEvent::Enter(_) => EventType::Enter,
            ProtoEvent::Leave(_) => EventType::Leave,
            ProtoEvent::Ack(_) => EventType::Ack,
            ProtoEvent::Hello { .. } => EventType::Hello,
            ProtoEvent::Announce(_) => EventType::Announce,
            ProtoEvent::PairRequest { .. } => EventType::PairRequest,
            ProtoEvent::PairAccept(_) => EventType::PairAccept,
            ProtoEvent::PairReject => EventType::PairReject,
        }
    }
}

impl TryFrom<[u8; MAX_EVENT_SIZE]> for ProtoEvent {
    type Error = ProtocolError;

    fn try_from(buf: [u8; MAX_EVENT_SIZE]) -> Result<Self, Self::Error> {
        let mut buf = &buf[..];
        let event_type = decode_u8(&mut buf)?;
        match EventType::try_from(event_type)? {
            EventType::PointerMotion => {
                Ok(Self::Input(InputEvent::Pointer(PointerEvent::Motion {
                    time: decode_u32(&mut buf)?,
                    dx: decode_f64(&mut buf)?,
                    dy: decode_f64(&mut buf)?,
                })))
            }
            EventType::PointerPosition => {
                Ok(Self::Input(InputEvent::Pointer(PointerEvent::Position {
                    time: decode_u32(&mut buf)?,
                    x: decode_f64(&mut buf)?,
                    y: decode_f64(&mut buf)?,
                })))
            }
            EventType::PointerButton => {
                Ok(Self::Input(InputEvent::Pointer(PointerEvent::Button {
                    time: decode_u32(&mut buf)?,
                    button: decode_u32(&mut buf)?,
                    state: decode_u32(&mut buf)?,
                })))
            }
            EventType::PointerAxis => Ok(Self::Input(InputEvent::Pointer(PointerEvent::Axis {
                time: decode_u32(&mut buf)?,
                axis: decode_u8(&mut buf)?,
                value: decode_f64(&mut buf)?,
            }))),
            EventType::PointerAxisValue120 => Ok(Self::Input(InputEvent::Pointer(
                PointerEvent::AxisDiscrete120 {
                    axis: decode_u8(&mut buf)?,
                    value: decode_i32(&mut buf)?,
                },
            ))),
            EventType::KeyboardKey => Ok(Self::Input(InputEvent::Keyboard(KeyboardEvent::Key {
                time: decode_u32(&mut buf)?,
                key: decode_u32(&mut buf)?,
                state: decode_u8(&mut buf)?,
            }))),
            EventType::KeyboardModifiers => Ok(Self::Input(InputEvent::Keyboard(
                KeyboardEvent::Modifiers {
                    depressed: decode_u32(&mut buf)?,
                    latched: decode_u32(&mut buf)?,
                    locked: decode_u32(&mut buf)?,
                    group: decode_u32(&mut buf)?,
                },
            ))),
            EventType::Announce => {
                let hostname_len = decode_u32(&mut buf)? as usize;
                if hostname_len > MAX_HOSTNAME_LEN {
                    return Err(ProtocolError::InvalidAnnouncement(hostname_len));
                }
                let mut hostname = String::with_capacity(hostname_len);
                for _ in 0..hostname_len {
                    hostname.push(decode_u8(&mut buf)? as char);
                }
                let port = decode_u32(&mut buf)? as u16;
                let instance = decode_u32(&mut buf)?;
                let mut fingerprint = [0u8; 32];
                for b in fingerprint.iter_mut() {
                    *b = decode_u8(&mut buf)?;
                }
                Ok(Self::Announce(Announce {
                    hostname,
                    port,
                    instance,
                    fingerprint,
                }))
            }
            EventType::PairRequest => {
                let len = decode_u32(&mut buf)? as usize;
                if len > MAX_PAIRING_CODE_LEN {
                    return Err(ProtocolError::InvalidAnnouncement(len));
                }
                let mut code = String::with_capacity(len);
                for _ in 0..len {
                    code.push(decode_u8(&mut buf)? as char);
                }
                Ok(Self::PairRequest { code })
            }
            EventType::PairAccept => {
                let mut fingerprint = [0u8; 32];
                for b in fingerprint.iter_mut() {
                    *b = decode_u8(&mut buf)?;
                }
                Ok(Self::PairAccept(fingerprint))
            }
            EventType::PairReject => Ok(Self::PairReject),
            EventType::Ping => Ok(Self::Ping),
            EventType::Pong => Ok(Self::Pong(decode_u8(&mut buf)? != 0)),
            EventType::Enter => Ok(Self::Enter(decode_u8(&mut buf)?.try_into()?)),
            EventType::Leave => Ok(Self::Leave(decode_u32(&mut buf)?)),
            EventType::Ack => Ok(Self::Ack(decode_u32(&mut buf)?)),
            EventType::Hello => {
                let mut commit = [0u8; 8];
                for b in commit.iter_mut() {
                    *b = decode_u8(&mut buf)?;
                }
                Ok(Self::Hello { commit })
            }
            EventType::GeometryAck => Ok(Self::GeometryAck(decode_u32(&mut buf)?)),
            EventType::Geometry => Ok(Self::Geometry(
                PeerGeometry {
                    screen: input_event::Rect::new(
                        decode_f64(&mut buf)?,
                        decode_f64(&mut buf)?,
                        decode_f64(&mut buf)?,
                        decode_f64(&mut buf)?,
                    ),
                    left: decode_f64(&mut buf)?,
                    right: decode_f64(&mut buf)?,
                    top: decode_f64(&mut buf)?,
                    bottom: decode_f64(&mut buf)?,
                    absolute: decode_u8(&mut buf)? != 0,
                }
                .clamp(),
                decode_u32(&mut buf)?,
            )),
        }
    }
}

impl From<ProtoEvent> for ([u8; MAX_EVENT_SIZE], usize) {
    fn from(event: ProtoEvent) -> Self {
        let mut buf = [0u8; MAX_EVENT_SIZE];
        let mut len = 0usize;
        {
            let mut buf = &mut buf[..];
            let buf = &mut buf;
            let len = &mut len;
            encode_u8(buf, len, event.event_type() as u8);
            match event {
                ProtoEvent::Input(event) => match event {
                    InputEvent::Pointer(p) => match p {
                        PointerEvent::Motion { time, dx, dy } => {
                            encode_u32(buf, len, time);
                            encode_f64(buf, len, dx);
                            encode_f64(buf, len, dy);
                        }
                        PointerEvent::Position { time, x, y } => {
                            encode_u32(buf, len, time);
                            encode_f64(buf, len, x);
                            encode_f64(buf, len, y);
                        }
                        PointerEvent::Button {
                            time,
                            button,
                            state,
                        } => {
                            encode_u32(buf, len, time);
                            encode_u32(buf, len, button);
                            encode_u32(buf, len, state);
                        }
                        PointerEvent::Axis { time, axis, value } => {
                            encode_u32(buf, len, time);
                            encode_u8(buf, len, axis);
                            encode_f64(buf, len, value);
                        }
                        PointerEvent::AxisDiscrete120 { axis, value } => {
                            encode_u8(buf, len, axis);
                            encode_i32(buf, len, value);
                        }
                    },
                    InputEvent::Keyboard(k) => match k {
                        KeyboardEvent::Key { time, key, state } => {
                            encode_u32(buf, len, time);
                            encode_u32(buf, len, key);
                            encode_u8(buf, len, state);
                        }
                        KeyboardEvent::Modifiers {
                            depressed,
                            latched,
                            locked,
                            group,
                        } => {
                            encode_u32(buf, len, depressed);
                            encode_u32(buf, len, latched);
                            encode_u32(buf, len, locked);
                            encode_u32(buf, len, group);
                        }
                    },
                },
                ProtoEvent::GeometryAck(serial) => encode_u32(buf, len, serial),
                ProtoEvent::PairRequest { code } => {
                    let code = &code.as_bytes()[..code.len().min(MAX_PAIRING_CODE_LEN)];
                    encode_u32(buf, len, code.len() as u32);
                    for b in code {
                        encode_u8(buf, len, *b);
                    }
                }
                ProtoEvent::PairAccept(fingerprint) => {
                    for b in fingerprint.iter() {
                        encode_u8(buf, len, *b);
                    }
                }
                ProtoEvent::PairReject => {}
                ProtoEvent::Announce(a) => {
                    let hostname = &a.hostname.as_bytes()[..a.hostname.len().min(MAX_HOSTNAME_LEN)];
                    encode_u32(buf, len, hostname.len() as u32);
                    for b in hostname {
                        encode_u8(buf, len, *b);
                    }
                    encode_u32(buf, len, a.port as u32);
                    encode_u32(buf, len, a.instance);
                    for b in a.fingerprint.iter() {
                        encode_u8(buf, len, *b);
                    }
                }
                ProtoEvent::Geometry(g, serial) => {
                    encode_f64(buf, len, g.screen.x);
                    encode_f64(buf, len, g.screen.y);
                    encode_f64(buf, len, g.screen.width);
                    encode_f64(buf, len, g.screen.height);
                    encode_f64(buf, len, g.left);
                    encode_f64(buf, len, g.right);
                    encode_f64(buf, len, g.top);
                    encode_f64(buf, len, g.bottom);
                    encode_u8(buf, len, g.absolute as u8);
                    encode_u32(buf, len, serial);
                }
                ProtoEvent::Ping => {}
                ProtoEvent::Pong(alive) => encode_u8(buf, len, alive as u8),
                ProtoEvent::Enter(pos) => encode_u8(buf, len, pos as u8),
                ProtoEvent::Leave(serial) => encode_u32(buf, len, serial),
                ProtoEvent::Ack(serial) => encode_u32(buf, len, serial),
                ProtoEvent::Hello { commit } => {
                    for b in commit.iter() {
                        encode_u8(buf, len, *b);
                    }
                }
            }
        }
        (buf, len)
    }
}

macro_rules! decode_impl {
    ($t:ty) => {
        paste! {
            fn [<decode_ $t>](data: &mut &[u8]) -> Result<$t, ProtocolError> {
                let (int_bytes, rest) = data.split_at(size_of::<$t>());
                *data = rest;
                Ok($t::from_be_bytes(int_bytes.try_into().unwrap()))
            }
        }
    };
}

decode_impl!(u8);
decode_impl!(u32);
decode_impl!(i32);
decode_impl!(f64);

macro_rules! encode_impl {
    ($t:ty) => {
        paste! {
            fn [<encode_ $t>](buf: &mut &mut [u8], amt: &mut usize, n: $t) {
                let src = n.to_be_bytes();
                let data = std::mem::take(buf);
                let (int_bytes, rest) = data.split_at_mut(size_of::<$t>());
                int_bytes.copy_from_slice(&src);
                *amt += size_of::<$t>();
                *buf = rest
            }
        }
    };
}

encode_impl!(u8);
encode_impl!(u32);
encode_impl!(i32);
encode_impl!(f64);

#[cfg(test)]
mod tests {
    use super::*;
    use input_event::Rect;

    fn roundtrip(event: ProtoEvent) -> ProtoEvent {
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.into();
        ProtoEvent::try_from(buf)
            .expect("decodes") // trailing bytes are ignored by the decoder
            .clone_for_len(len)
    }

    impl ProtoEvent {
        /// sanity check that the encoded payload fits the reported length
        fn clone_for_len(self, len: usize) -> ProtoEvent {
            assert!(len <= MAX_EVENT_SIZE);
            self
        }
    }

    #[test]
    fn geometry_roundtrip() {
        let geometry = PeerGeometry::from_rect(Rect::new(-1280.0, 0.0, 1280.0, 800.0), true);
        let decoded = roundtrip(ProtoEvent::Geometry(geometry, 7));
        match decoded {
            ProtoEvent::Geometry(g, serial) => {
                assert_eq!(g, geometry);
                assert_eq!(serial, 7);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn absolute_position_roundtrip() {
        let event = ProtoEvent::Input(InputEvent::Pointer(PointerEvent::Position {
            time: 3,
            x: 12.5,
            y: -4.25,
        }));
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.into();
        assert_eq!(len, 1 + 4 + 8 + 8);
        assert!(matches!(
            ProtoEvent::try_from(buf).unwrap(),
            ProtoEvent::Input(InputEvent::Pointer(PointerEvent::Position { x, y, .. }))
                if (x - 12.5).abs() < f64::EPSILON && (y + 4.25).abs() < f64::EPSILON
        ));
    }

    #[test]
    fn announce_roundtrip() {
        let announce = Announce {
            hostname: "example-host".to_owned(),
            port: 4242,
            instance: 17,
            fingerprint: [7u8; 32],
        };
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) =
            ProtoEvent::Announce(announce.clone()).into();
        assert!(len <= MAX_EVENT_SIZE);
        assert_eq!(
            ProtoEvent::try_from(buf).unwrap(),
            ProtoEvent::Announce(announce)
        );
    }

    #[test]
    fn announce_hostname_is_capped() {
        let announce = Announce {
            hostname: "x".repeat(MAX_HOSTNAME_LEN * 2),
            port: 1,
            instance: 0,
            fingerprint: [0u8; 32],
        };
        let (_, len): ([u8; MAX_EVENT_SIZE], usize) = ProtoEvent::Announce(announce).into();
        assert!(len <= MAX_EVENT_SIZE);
    }

    #[test]
    fn geometry_values_are_clamped() {
        let geometry = PeerGeometry {
            screen: Rect::new(f64::NAN, 0.0, 1e12, 1.0),
            left: f64::INFINITY,
            right: 0.0,
            top: 0.0,
            bottom: 0.0,
            absolute: false,
        }
        .clamp();
        assert!(geometry.screen.x.is_finite());
        assert!(geometry.left.is_finite());
    }
}
