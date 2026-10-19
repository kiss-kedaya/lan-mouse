//! LAN discovery and pairing.
//!
//! Every device broadcasts an [`Announce`] (hostname, listen port, instance id
//! and certificate fingerprint) from an ephemeral port to the listen port of
//! the local subnet. The announcement travels inside DTLS and carries the
//! digest of the sender's certificate, so a host can only announce a
//! fingerprint it owns.
//!
//! The same listen port accepts DTLS connections. Pairing opens such a
//! connection and exchanges a [`ProtoEvent::PairRequest`] /
//! [`ProtoEvent::PairAccept`] over it; the accept carries the fingerprint of
//! the peer's certificate. Announcements of our own device are ignored, and
//! a device is forgotten once it stops announcing.

use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    path::Path,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use lan_mouse_ipc::{connect_error_text, pairing_reject_text};
use lan_mouse_proto::{Announce, MAX_EVENT_SIZE, ProtoEvent};
use sha2::{Digest, Sha256};
use tokio::net::UdpSocket as TokioUdpSocket;
use tokio::task::JoinHandle;
use webrtc_dtls::{
    config::{ClientAuthType, Config as DtlsConfig, ExtendedMasterSecretType},
    conn::DTLSConn,
    crypto::Certificate,
};
use webrtc_util::{Conn, conn::Listener};

use crate::config::Config;
use crate::crypto;

/// how often the own announcement is broadcast
const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(2);
/// a device is forgotten after not being announced for this long
const PEER_TIMEOUT: Duration = Duration::from_secs(7);
/// application prefix separating announcements from other services
const APP_ID: &str = "lan-mouse";

pub(crate) enum DiscoveryEvent {
    /// owned by a newly discovered device which is known to be connectable
    Found {
        addr: SocketAddr,
        hostname: String,
        fingerprint: String,
    },
    /// a discovered device disappeared
    Lost { addr: SocketAddr },
    /// the local pairing code (shown to the user)
    Code(String),
    /// pairing finished, `Ok` holds the fingerprint of the verified peer
    Paired {
        hostname: String,
        addr: SocketAddr,
        result: Result<String, String>,
    },
}

pub(crate) enum DiscoveryRequest {
    /// set the pairing code; an empty or missing code generates a new one
    SetCode(Option<String>),
    /// pair with the device announcing at `addr` using `code` as the code
    /// displayed by the remote device
    Pair {
        addr: SocketAddr,
        code: Option<String>,
    },
}

#[derive(Clone)]
struct Peer {
    digest: u64,
    hostname: String,
    /// fingerprint is part of the digest, kept for logging / debugging
    #[allow(dead_code)]
    fingerprint: String,
    addrs: HashMap<IpAddr, Instant>,
    /// host port derived from the announced port
    port: u16,
}

/// handle to the background discovery task
pub(crate) struct Discovery {
    request_tx: local_channel::mpsc::Sender<DiscoveryRequest>,
    event_rx: local_channel::mpsc::Receiver<DiscoveryEvent>,
    task: JoinHandle<()>,
}

impl Discovery {
    pub(crate) async fn new(
        cert_path: &Path,
        fingerprint: String,
        port: u16,
        code: String,
    ) -> Result<Self, std::io::Error> {
        let cert = crypto::load_certificate(cert_path)
            .map_err(|e| std::io::Error::other(format!("pairing certificate: {e}")))?;
        let (request_tx, request_rx) = local_channel::mpsc::channel();
        let (event_tx, event_rx) = local_channel::mpsc::channel();
        let task_data = DiscoveryTask {
            cert,
            cert_path: cert_path.to_owned(),
            fingerprint,
            port,
            code,
            peers: HashMap::new(),
            request_rx,
            event_tx,
        };
        let task = tokio::task::spawn_local(async move {
            if let Err(e) = task_data.run().await {
                log::warn!("lan discovery stopped: {e}");
            }
        });
        Ok(Self {
            request_tx,
            event_rx,
            task,
        })
    }

    pub(crate) fn set_code(&self, code: Option<String>) {
        let _ = self.request_tx.send(DiscoveryRequest::SetCode(code));
    }

    pub(crate) fn pair(&self, addr: SocketAddr, code: Option<String>) {
        let _ = self.request_tx.send(DiscoveryRequest::Pair { addr, code });
    }

    pub(crate) async fn event(&mut self) -> DiscoveryEvent {
        self.event_rx.recv().await.expect("channel closed")
    }

    pub(crate) async fn terminate(&mut self) {
        self.task.abort();
    }
}

/// hostname usable in an announcement (no `.local` suffix, bounded length)
pub(crate) fn local_hostname() -> String {
    // Windows puts the name into the announcement itself and answers mDNS;
    // on the other systems the host name is the best we know
    #[cfg(target_os = "windows")]
    if let Some(name) = windows_computer_name() {
        return name;
    }
    let name = hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_default();
    let name = name.strip_suffix(".local").unwrap_or(&name);
    let name: String = name
        .chars()
        .filter(|c| !c.is_control())
        .take(lan_mouse_proto::MAX_HOSTNAME_LEN)
        .collect();
    if name.trim().is_empty() {
        APP_ID.to_owned()
    } else {
        name
    }
}

/// the NetBIOS style computer name of this Windows machine
#[cfg(target_os = "windows")]
fn windows_computer_name() -> Option<String> {
    let name = std::env::var("COMPUTERNAME").ok()?;
    let name = name.trim();
    (!name.is_empty()).then(|| name.chars().take(lan_mouse_proto::MAX_HOSTNAME_LEN).collect())
}

/// a fresh random six digit pairing code (leading zeros kept)
pub(crate) fn generate_code() -> String {
    format!("{:06}", random_u32() % 1_000_000)
}

/// 32 random bits from the operating system, independent of the crypto
/// library in use
fn random_u32() -> u32 {
    use rand_core::RngCore;
    rand_core::OsRng.next_u32()
}

fn instance_id() -> u32 {
    SYSTEM_INSTANCE.with(|cell| *cell)
}

thread_local! {
    static SYSTEM_INSTANCE: u32 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as u32)
        .unwrap_or_default();
}

fn fingerprint_bytes(fingerprint: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, byte) in fingerprint
        .split(':')
        .filter_map(|b| u8::from_str_radix(b, 16).ok())
        .take(32)
        .enumerate()
    {
        out[i] = byte;
    }
    out
}

fn format_fingerprint(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

type VerifyPeer = Arc<
    dyn Fn(
            &[Vec<u8>],
            &[rustls::pki_types::CertificateDer<'static>],
        ) -> Result<(), webrtc_dtls::Error>
        + Send
        + Sync,
>;

fn dtls_config(cert: &Certificate, verify: Option<VerifyPeer>) -> DtlsConfig {
    DtlsConfig {
        certificates: vec![cert.clone()],
        server_name: "ignored".to_owned(),
        insecure_skip_verify: true,
        extended_master_secret: ExtendedMasterSecretType::Require,
        client_auth: ClientAuthType::RequireAnyClientCert,
        verify_peer_certificate: verify,
        ..Default::default()
    }
}

struct DiscoveryTask {
    cert: Certificate,
    cert_path: std::path::PathBuf,
    fingerprint: String,
    port: u16,
    code: String,
    peers: HashMap<IpAddr, Peer>,
    request_rx: local_channel::mpsc::Receiver<DiscoveryRequest>,
    event_tx: local_channel::mpsc::Sender<DiscoveryEvent>,
}

#[cfg(not(target_os = "ios"))]
impl DiscoveryTask {
    async fn run(mut self) -> Result<(), std::io::Error> {
        let instance = instance_id();
        let own = Announce {
            hostname: local_hostname(),
            port: self.port,
            instance,
            fingerprint: fingerprint_bytes(&self.fingerprint),
        };
        let own_digest = own.digest();
        let (packet, len): ([u8; MAX_EVENT_SIZE], usize) = ProtoEvent::Announce(own).into();
        let mut datagram = APP_ID.as_bytes().to_vec();
        datagram.push(0);
        datagram.extend_from_slice(&packet[..len]);

        let udp =
            TokioUdpSocket::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)).await?;
        udp.set_broadcast(true)?;

        let listener = DtlsListener::new(self.cert.clone(), self.code.clone(), self.port).await;
        // a busy port (e.g. another instance) must not stop discovery, only
        // the announcement receiver and pairing depend on it
        let mut listener = match listener {
            Ok(listener) => Some(listener),
            Err(e) => {
                log::warn!(
                    "{}",
                    crate::i18n::gettext("pairing listener is not available: {error}")
                        .replace("{error}", &e.to_string())
                );
                None
            }
        };

        self.emit(DiscoveryEvent::Code(self.code.clone()));
        let mut announce_interval = tokio::time::interval(ANNOUNCE_INTERVAL);
        let mut expire_interval = tokio::time::interval(Duration::from_secs(1));
        let mut seen: HashMap<u64, Instant> = HashMap::new();

        loop {
            tokio::select! {
                request = self.request_rx.recv() => match request.expect("channel closed") {
                    DiscoveryRequest::SetCode(code) => self.set_code(code),
                    DiscoveryRequest::Pair { addr, code } => {
                        self.pair_with(addr, code.as_deref()).await;
                    }
                },
                _ = announce_interval.tick() => {
                    for target in [IpAddr::V4(Ipv4Addr::BROADCAST), IpAddr::V4(Ipv4Addr::new(255, 255, 255, 255))] {
                        let target = SocketAddr::new(target, self.port);
                        if let Err(e) = udp.send_to(&datagram, target).await {
                            log::debug!("announcement to {target} failed: {e}");
                        }
                    }
                    // devices we already know get the announcement directly
                    // as well, broadcast packets get dropped more often
                    for addr in self.known_addrs() {
                        let _ = udp.send_to(&datagram, addr).await;
                    }
                }
                msg = listener_next(&mut listener) => match msg {
                    Some(msg) => self.handle_announcement(msg, own_digest, &mut seen),
                    None => {}
                },
                _ = expire_interval.tick() => self.expire(),
            }
        }
    }

    fn emit(&self, event: DiscoveryEvent) {
        if self.event_tx.send(event).is_err() {
            log::debug!("discovery frontend channel closed");
        }
    }

    fn set_code(&mut self, code: Option<String>) {
        // a changed code is stored on disk and picked up on the next start,
        // pairing requests of a running session keep using the stored code
        let code = store_code_persistent(&self.cert_path, code);
        log::info!("pairing code: {code}");
        self.emit(DiscoveryEvent::Code(code));
    }

    fn known_addrs(&self) -> HashSet<SocketAddr> {
        self.peers
            .values()
            .flat_map(|p| p.addrs.keys().map(|ip| SocketAddr::new(*ip, p.port)))
            .collect()
    }

    fn handle_announcement(
        &mut self,
        msg: Announcement,
        own_digest: u64,
        seen: &mut HashMap<u64, Instant>,
    ) {
        let announce = msg.announce;
        let digest = announce.digest();
        if digest == own_digest {
            // our own broadcast came back through the loopback
            return;
        }
        // multiple interfaces of one device repeat the same digest
        match seen.entry(digest) {
            std::collections::hash_map::Entry::Occupied(mut e) => {
                if e.get().elapsed() < ANNOUNCE_INTERVAL {
                    return;
                }
                *e.get_mut() = Instant::now();
            }
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(Instant::now());
                // keep the de-duplication table small on busy networks
                if seen.len() > 256 {
                    let now = Instant::now();
                    seen.retain(|_, at| now.duration_since(*at) < PEER_TIMEOUT);
                }
            }
        }

        let fingerprint = format_fingerprint(&announce.fingerprint);
        let addr = SocketAddr::new(msg.from.ip(), announce.port);
        let now = Instant::now();
        let known = self.peers.get(&msg.from.ip());
        let is_new = known.is_none_or(|p| p.digest != digest);
        let peer = self.peers.entry(msg.from.ip()).or_insert(Peer {
            digest,
            hostname: announce.hostname.clone(),
            fingerprint: fingerprint.clone(),
            addrs: HashMap::new(),
            port: announce.port,
        });
        if is_new {
            *peer = Peer {
                digest,
                hostname: announce.hostname.clone(),
                fingerprint: fingerprint.clone(),
                addrs: HashMap::new(),
                port: announce.port,
            };
        }
        peer.addrs.insert(msg.from.ip(), now);
        if is_new {
            log::info!("discovered \"{}\" at {addr}", announce.hostname);
            self.emit(DiscoveryEvent::Found {
                addr,
                hostname: announce.hostname,
                fingerprint,
            });
        }
    }

    fn expire(&mut self) {
        let mut lost = vec![];
        let now = Instant::now();
        self.peers.retain(|ip, peer| {
            peer.addrs
                .retain(|_, at| now.duration_since(*at) < PEER_TIMEOUT);
            if peer.addrs.is_empty() {
                lost.push(SocketAddr::new(*ip, peer.port));
                false
            } else {
                true
            }
        });
        for addr in lost {
            log::info!("device {addr} is no longer announcing itself");
            self.emit(DiscoveryEvent::Lost { addr });
        }
    }

    /// pair with the device at `addr`: dial its DTLS listener and let it
    /// decide whether `code` is correct
    async fn pair_with(&mut self, addr: SocketAddr, code: Option<&str>) {
        let hostname = self
            .peers
            .get(&addr.ip())
            .map(|p| p.hostname.clone())
            .unwrap_or_else(|| addr.ip().to_string());
        let result = pair_dial(&self.cert, addr, code.unwrap_or_default()).await;
        log::info!("pairing with {hostname} ({addr}): {result:?}");
        self.emit(DiscoveryEvent::Paired {
            hostname,
            addr,
            result,
        });
    }
}

/// where discovery is not available, the task simply idles
#[cfg(target_os = "ios")]
impl DiscoveryTask {
    async fn run(mut self) -> Result<(), std::io::Error> {
        log::info!("lan discovery is not available on this platform");
        loop {
            match self.request_rx.recv().await.expect("channel closed") {
                DiscoveryRequest::SetCode(code) => self.set_code(code),
                DiscoveryRequest::Pair { mut addr, .. } => {
                    addr.set_port(lan_mouse_ipc::DEFAULT_PORT);
                    self.emit(DiscoveryEvent::Paired {
                        hostname: addr.ip().to_string(),
                        addr,
                        result: Err(connect_error_text("discovery unavailable")),
                    });
                }
            }
        }
    }

    fn set_code(&mut self, code: Option<String>) {
        self.code = code
            .map(|c| c.trim().to_owned())
            .filter(|c| !c.is_empty())
            .unwrap_or_else(generate_code);
        self.emit(DiscoveryEvent::Code(self.code.clone()));
    }

    fn emit(&self, event: DiscoveryEvent) {
        let _ = self.event_tx.send(event);
    }
}

/// store a pairing code (generating one when `code` is empty) in the config
pub(crate) fn store_code(config: &mut Config, code: Option<String>) -> String {
    let code = normalize_code(code);
    config.set_pairing_code(Some(code.clone()));
    if let Err(e) = config.write_back() {
        log::warn!("could not persist the pairing code: {e}");
    }
    code
}

/// store a pairing code in the config file next to `cert_path`, used by the
/// running discovery task which does not own the service config
fn store_code_persistent(cert_path: &std::path::Path, code: Option<String>) -> String {
    let code = normalize_code(code);
    let config_path = cert_path.with_file_name("config.toml");
    if let Err(e) = update_config_file(&config_path, &code) {
        log::warn!("could not persist the pairing code: {e}");
    }
    code
}

fn normalize_code(code: Option<String>) -> String {
    code.map(|c| c.trim().to_owned())
        .filter(|c| !c.is_empty() && c.len() <= 16 && c.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or_else(generate_code)
}

/// write just the pairing code into the config, keeping everything else
fn update_config_file(path: &std::path::Path, code: &str) -> std::io::Result<()> {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = content.parse().map_err(std::io::Error::other)?;
    doc["pairing_code"] = toml_edit::value(code);
    std::fs::write(path, doc.to_string())
}

struct Announcement {
    announce: Announce,
    from: SocketAddr,
}

/// whether a UDP datagram starts with a DTLS ClientHello handshake record
fn is_client_hello(packet: &[u8]) -> bool {
    // record header: content type (22 = handshake), version, epoch, sequence, length
    // handshake header: msg type (1 = client_hello) at offset 13
    packet.len() > 13
        && packet[0] == 22
        && packet[1] == 0xfe
        && (packet[2] == 0xfd || packet[2] == 0xff)
        && packet[13] == 1
}

/// decrypts announcements and answers pairing requests on the listen port
struct DtlsListener {
    cert: Certificate,
    pending: tokio::sync::mpsc::Receiver<Announcement>,
    task: JoinHandle<()>,
}

impl DtlsListener {
    async fn new(cert: Certificate, code: String, port: u16) -> Result<Self, std::io::Error> {
        // the listener accepts every UDP packet as a new connection until the
        // DTLS handshake starts, so only ClientHellos may create one: our own
        // announcements and connection attempts to the main listener would
        // otherwise be torn down mid handshake
        let filter: webrtc_util::conn::conn_udp_listener::AcceptFilterFn =
            Box::new(|packet: &[u8]| {
                let is_hello = is_client_hello(packet);
                Box::pin(async move { is_hello })
            });
        let listener = webrtc_util::conn::conn_udp_listener::ListenConfig {
            backlog: 64,
            accept_filter: Some(filter),
        }
        .listen(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port))
        .await
        .map_err(|e| std::io::Error::other(format!("pairing listener: {e}")))?;
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        // the listener is owned by the accept loop; it closes its UDP socket
        // when the task is aborted on shutdown
        let code: Arc<str> = code.into();
        let task = tokio::task::spawn_local(accept_loop(listener, code, tx));
        Ok(Self {
            cert,
            pending: rx,
            task,
        })
    }

    async fn next(&mut self) -> Option<Announcement> {
        self.pending.recv().await
    }
}

impl Drop for DtlsListener {
    fn drop(&mut self) {
        self.task.abort();
        let _ = &self.cert;
    }
}

/// answer a single encrypted request: pairing requests get an accept/reject,
/// announcements are forwarded after their certificate was verified
async fn serve_conn(
    conn: Arc<dyn Conn + Send + Sync>,
    addr: SocketAddr,
    tx: tokio::sync::mpsc::Sender<Announcement>,
    expected_code: &str,
) -> Result<(), String> {
    let mut buf = [0u8; MAX_EVENT_SIZE];
    tokio::time::timeout(Duration::from_secs(5), conn.recv(&mut buf))
        .await
        .map_err(|_| "timeout".to_owned())?
        .map_err(|e| e.to_string())?;
    let payload = decode_datagram(&buf)?;
    let dtls: &DTLSConn = conn.as_any().downcast_ref().ok_or("no dtls state")?;
    let peer_cert = dtls
        .connection_state()
        .await
        .peer_certificates
        .first()
        .cloned();
    match payload {
        ProtoEvent::Announce(announce) => {
            let Some(peer_cert) = peer_cert else {
                return Ok(());
            };
            // the announcement must come from the owner of the certificate
            if announce.fingerprint == fingerprint_bytes(&digest(&peer_cert)) {
                let _ = tx
                    .send(Announcement {
                        announce,
                        from: addr,
                    })
                    .await;
            } else {
                log::debug!("ignoring announcement with a mismatching certificate from {addr}");
            }
        }
        ProtoEvent::PairRequest { code } => {
            let peer_cert = peer_cert.ok_or("peer sent no certificate")?;
            let reply = pairing_reply_from(&peer_cert, expected_code, &code);
            let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = reply.into();
            let _ = conn.send(&buf[..len]).await;
        }
        _ => {}
    }
    let _ = conn.close().await;
    Ok(())
}

/// answer a pairing request: accept only with the code this device displays
fn pairing_reply_from(peer_cert: &[u8], expected_code: &str, code: &str) -> ProtoEvent {
    if pairing_code_matches(expected_code, code) {
        ProtoEvent::PairAccept(fingerprint_bytes(&digest(peer_cert)))
    } else {
        ProtoEvent::PairReject
    }
}

/// announcement packet as sent by the broadcaster (application id + event)
#[cfg(test)]
fn announcement_packet(announce: &Announce) -> Vec<u8> {
    let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = ProtoEvent::Announce(announce.clone()).into();
    let mut packet = APP_ID.as_bytes().to_vec();
    packet.push(0);
    packet.extend_from_slice(&buf[..len]);
    packet
}

/// strip the application id in front of announcements and decode the event
fn decode_datagram(buf: &[u8]) -> Result<ProtoEvent, String> {
    let payload = if let Some(rest) = buf.strip_prefix(APP_ID.as_bytes()) {
        rest.strip_prefix(&[0u8]).unwrap_or(rest)
    } else {
        buf
    };
    let mut event = [0u8; MAX_EVENT_SIZE];
    if payload.len() > MAX_EVENT_SIZE {
        return Err("datagram too large".to_owned());
    }
    event[..payload.len()].copy_from_slice(payload);
    ProtoEvent::try_from(event).map_err(|e| e.to_string())
}

/// accept connections, decrypt one datagram each and route it: announcements
/// are forwarded, pairing requests are answered, everything else is dropped
/// wait for the next announcement, never finishes without a listener
async fn listener_next(listener: &mut Option<DtlsListener>) -> Option<Announcement> {
    match listener {
        Some(listener) => listener.next().await,
        None => std::future::pending().await,
    }
}

async fn accept_loop(
    listener: impl Listener,
    expected_code: Arc<str>,
    tx: tokio::sync::mpsc::Sender<Announcement>,
) {
    // webrtc-dtls multiplexes everything on one UDP socket (see
    // webrtc-rs/webrtc#614), give the accept loop some breathing room
    loop {
        let sleep = tokio::time::sleep(Duration::from_millis(10));
        tokio::pin!(sleep);
        tokio::select! {
            _ = &mut sleep => {}
            accepted = listener.accept() => {
                let Ok((conn, addr)) = accepted else { continue };
                let code = expected_code.clone();
                let tx = tx.clone();
                tokio::task::spawn_local(async move {
                    if let Err(e) = serve_conn(conn, addr, tx, &code).await {
                        log::debug!("pairing datagram from {addr}: {e}");
                    }
                });
            }
        }
    }
}

fn digest(cert: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(cert);
    format_fingerprint(&hash.finalize())
}

/// the code typed by the pairing device must match the code we display.
/// Compared in constant time, the codes are short enough for an exhaustive
/// guess otherwise.
fn pairing_code_matches(expected: &str, code: &str) -> bool {
    let (a, b) = (code.trim().as_bytes(), expected.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// perform one request/answer exchange over DTLS with the server at `addr`
async fn dtls_request(
    cert: &Certificate,
    addr: SocketAddr,
    request: ProtoEvent,
) -> Result<(Vec<u8>, ProtoEvent), String> {
    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
    socket.set_nonblocking(true).map_err(|e| e.to_string())?;
    let socket = Arc::new(TokioUdpSocket::from_std(socket).map_err(|e| e.to_string())?);
    socket.connect(addr).await.map_err(|e| e.to_string())?;
    let conn = tokio::time::timeout(
        Duration::from_secs(5),
        DTLSConn::new(socket, dtls_config(cert, None), true, None),
    )
    .await
    .map_err(|_| connect_error_text("timeout"))?
    .map_err(|e| format!("dtls: {e}"))?;

    let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = request.into();
    conn.send(&buf[..len]).await.map_err(|e| e.to_string())?;
    let mut reply = [0u8; MAX_EVENT_SIZE];
    let _ = tokio::time::timeout(Duration::from_secs(5), conn.recv(&mut reply))
        .await
        .map_err(|_| connect_error_text("timeout"))?
        .map_err(|e| e.to_string())?;
    // the peer certificate comes from the authenticated connection
    let peer: Vec<u8> = conn
        .connection_state()
        .await
        .peer_certificates
        .first()
        .cloned()
        .ok_or("peer sent no certificate")?;
    let reply = ProtoEvent::try_from(reply).map_err(|e| format!("protocol: {e}"))?;
    let _ = conn.close().await;
    Ok((peer, reply))
}

/// open a DTLS connection to the pairing listener at `addr` and request it to
/// authorize us with `code`. Returns the fingerprint of the peer certificate.
async fn pair_dial(cert: &Certificate, addr: SocketAddr, code: &str) -> Result<String, String> {
    let (peer, reply) = dtls_request(
        cert,
        addr,
        ProtoEvent::PairRequest {
            code: code.trim().to_owned(),
        },
    )
    .await?;
    match reply {
        ProtoEvent::PairAccept(_) => {
            // the fingerprint is taken from the authenticated certificate of
            // the connection, never from the payload
            Ok(digest(&peer))
        }
        ProtoEvent::PairReject => Err(pairing_reject_text()),
        other => Err(format!("unexpected answer: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_roundtrip() {
        let fp = "ab:cd:ef:01:23:45:67:89:ab:cd:ef:01:23:45:67:89:ab:cd:ef:01:23:45:67:89:ab:cd:ef:01:23:45:67:89";
        let bytes = fingerprint_bytes(fp);
        assert_eq!(bytes[0], 0xab);
        assert_eq!(bytes[31], 0x89);
        assert_eq!(format_fingerprint(&bytes), fp);
    }

    #[test]
    fn pairing_code_is_checked() {
        assert!(pairing_code_matches("123456", "123456"));
        assert!(pairing_code_matches("123456", " 123456 "));
        assert!(!pairing_code_matches("123456", "123457"));
        assert!(!pairing_code_matches("123456", "12345"));
        // an empty code matches an empty configuration only
        assert!(pairing_code_matches("", ""));
        assert!(!pairing_code_matches("123456", ""));
    }

    #[test]
    fn pairing_answer_rejects_wrong_code() {
        let cert =
            Certificate::generate_self_signed(["lan-mouse-test".to_owned()]).expect("certificate");
        let peer = cert.certificate.first().expect("peer cert").clone();
        match pairing_reply_from(&peer, "123456", "654321") {
            ProtoEvent::PairReject => {}
            other => panic!("unexpected reply: {other}"),
        }
        match pairing_reply_from(&peer, "123456", "123456") {
            ProtoEvent::PairAccept(fp) => {
                assert_eq!(format_fingerprint(&fp), digest(&peer))
            }
            other => panic!("unexpected reply: {other}"),
        }
    }

    #[test]
    fn generated_codes_are_six_digits() {
        for _ in 0..32 {
            let code = generate_code();
            assert_eq!(code.len(), 6);
            assert!(code.chars().all(|c| c.is_ascii_digit()));
        }
    }
}
