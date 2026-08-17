//! Echo session lifecycle — turning a punched UDP path into a live media
//! session, and refusing to do so when that would trample an existing one.
//!
//! ## The problem this exists to solve
//!
//! [`crate::echo::wan`] ends with a confirmed path: a peer address that
//! answered a STUN blast, recorded and deliberately *not* installed as
//! `RtpSender`'s media target. That restraint is the reason this module is
//! needed rather than optional — reachability and entitlement are different
//! questions, and conflating them means anyone who can complete a punch can
//! redirect an in-flight stream. A punch says "packets can reach here". A
//! session says "and they should".
//!
//! ## The handoff gate
//!
//! Nova has exactly one capture pipeline, one encoder, and one RTP sender, so
//! exactly one client can be served at a time (multi-seat is N pipelines, not a
//! targeting change — see `echo::rpc`'s `PRIMARY_SEAT`). The gate enforces that
//! as an explicit ownership question rather than a race:
//!
//! - **A live Moonlight session wins.** If `ClientInfo::streaming_active` is
//!   set, an Echo start is refused outright. Not deferred, not queued: refused,
//!   with a reason the client can show a user. Someone is watching that stream.
//! - **A live Echo session wins against another device.** The second device is
//!   refused; the *same* device restarting is treated as a restart, because
//!   that is a client that lost its socket and came back, not a competitor.
//! - **Media ownership is published**, so the Moonlight path can see it too.
//!   Without that, the guard is one-directional: a Moonlight PLAY arriving
//!   during an Echo session would reconfigure the Worker and re-point the
//!   stream, which is the very hijack this module refuses in the other
//!   direction.
//!
//! ## What a grant contains
//!
//! Starting a session mints fresh [`SessionKeys`] and returns them to the
//! client over the RPC — which is mutual TLS against the pairing trust store,
//! so the key is delivered on an already-authenticated channel and never
//! touches the UDP path. See [`nova_core::media_crypto`] for why the sealing
//! itself is frame-level.
//!
//! ## Scope note, stated plainly
//!
//! This module owns the *decision* and the *retargeting*. Frame sealing is
//! implemented and tested in `nova-core`, and [`EchoSession::seal_video`] is
//! its host-side entry point — but the media path does not call it yet, and
//! `media_supervisor` still forwards frames verbatim. Inserting an encryption
//! transform into the live frame path before any receiver can decrypt would
//! break a working stream in exchange for nothing. The call site is marked in
//! `lib.rs`; flipping it on is a one-line change once Echo's decoder exists.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nova_core::media_crypto::{SessionKeys, STREAM_VIDEO};

use crate::echo::rpc::EchoIdentity;
use crate::encoder::Codec;
use crate::ipc::{self, ControlMsg, WireCodec, WorkerLink};
use crate::rtsp::ClientInfo;
use crate::session_negotiate::WorkerCaps;

/// Who currently owns the capture/encode/RTP pipeline.
///
/// Published by the manager so both session paths consult one answer. A
/// boolean ("is Echo streaming") would have been enough today and wrong
/// tomorrow: the interesting question is *which* owner, because the refusal
/// message differs and multi-seat turns this into a set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaOwner {
    /// Nothing is streaming.
    Idle,
    /// A GameStream/Moonlight client holds the pipeline.
    Moonlight,
    /// An Echo client holds the pipeline.
    Echo,
}

/// Why a sealed input datagram was not injected.
///
/// Deliberately not an [`HandoffError`]: nothing on this path can be reported
/// back to the sender. The datagram is unacknowledged by design, and answering
/// an unauthenticated one would tell an attacker probing the socket whether a
/// session exists. So these are for the host's own log and counters only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputRejection {
    /// No Echo session is running — nothing owns the keyboard.
    NoSession,
    /// The datagram failed to open: forged, corrupted, or sealed for a session
    /// that has since ended.
    Unopenable(String),
}

impl std::fmt::Display for InputRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSession => write!(f, "no Echo session holds the pipeline"),
            Self::Unopenable(why) => write!(f, "{why}"),
        }
    }
}

/// Why a handoff was refused. Each variant maps to a stable RPC error code, so
/// a client can branch on the reason rather than parse prose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandoffError {
    /// A Moonlight client is mid-stream. The anti-hijack rule.
    MoonlightActive,
    /// Another Echo device holds the session.
    HeldByAnotherDevice { device: String },
    /// No punch has completed, so there is nowhere to send media. Reaching
    /// this usually means the client called `start_session` before its own
    /// punch finished, or from a network where punching cannot succeed.
    NoPathLatched,
    /// The Worker cannot serve this session (none connected, or one too
    /// degraded — the SYSTEM fallback at the logon screen).
    WorkerUnavailable(String),
    /// The request itself was invalid.
    BadRequest(String),
    /// Stop/modify attempted by a device that does not hold the session.
    NotTheOwner,
}

impl HandoffError {
    /// Stable machine-readable code for the RPC layer.
    pub fn code(&self) -> &'static str {
        match self {
            Self::MoonlightActive => "moonlight_active",
            Self::HeldByAnotherDevice { .. } => "session_held",
            Self::NoPathLatched => "no_path",
            Self::WorkerUnavailable(_) => "worker_unavailable",
            Self::BadRequest(_) => "bad_request",
            Self::NotTheOwner => "not_the_owner",
        }
    }
}

impl std::fmt::Display for HandoffError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MoonlightActive => write!(
                f,
                "a Moonlight client is streaming right now; Echo will not take the pipeline \
                 out from under it. Disconnect that client and retry."
            ),
            Self::HeldByAnotherDevice { device } => {
                write!(f, "the session is held by \"{device}\"")
            }
            Self::NoPathLatched => write!(
                f,
                "no network path to this device has been confirmed — complete a hole punch \
                 first (the host records the peer only after a successful punch)"
            ),
            Self::WorkerUnavailable(why) => write!(f, "{why}"),
            Self::BadRequest(why) => write!(f, "{why}"),
            Self::NotTheOwner => write!(f, "this device does not hold the active session"),
        }
    }
}

/// What the client asked for. Validated into [`StreamParams`] before anything
/// is touched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRequest {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub hdr: bool,
    pub codec: Codec,
    pub bitrate_kbps: u32,
    /// Nova app to launch (see `app_launcher`); 1 = Desktop.
    pub app_id: u32,
    /// Send host audio to the speakers as well as the client.
    pub host_audio: bool,
}

impl Default for SessionRequest {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            fps: 60,
            hdr: false,
            codec: Codec::Hevc,
            bitrate_kbps: 20_000,
            app_id: 1,
            host_audio: false,
        }
    }
}

/// The negotiated shape of a live session. Distinct from [`SessionRequest`]
/// because what the client asked for and what the host committed to are not
/// the same thing, and the client is told which is which.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamParams {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub hdr: bool,
    pub codec: Codec,
    pub bitrate_kbps: u32,
    pub app_id: u32,
    pub host_audio: bool,
    /// Payload bytes per datagram — the value `rtp.rs` shards against.
    pub packet_size: u32,
    pub fec_percentage: u32,
    pub min_fec_packets: u32,
}

/// Bounds mirroring `echo::rpc`'s display validation: a sanity envelope, not a
/// capability claim.
const MIN_DIMENSION: u32 = 640;
const MAX_WIDTH: u32 = 7680;
const MAX_HEIGHT: u32 = 4320;
const MIN_FPS: u32 = 24;
const MAX_FPS: u32 = 360;
const MIN_BITRATE_KBPS: u32 = 500;
const MAX_BITRATE_KBPS: u32 = 500_000;

/// Datagram payload size for a WAN session.
///
/// Moonlight negotiates 1392 on a LAN and drops to 1024 for remote streaming,
/// because a 1392-byte payload plus headers exceeds the effective MTU of many
/// tunnels and PPPoE links, and IP fragmentation of a UDP video packet turns
/// one lost fragment into a lost shard. Every Echo session today arrives over a
/// punched WAN path, so it takes the conservative value rather than discovering
/// the problem as unexplained loss.
const WAN_PACKET_SIZE: u32 = 1024;
const DEFAULT_FEC_PERCENTAGE: u32 = 5;
const DEFAULT_MIN_FEC_PACKETS: u32 = 2;

/// Largest datagram Echo will put on a WAN path.
///
/// 1400 is the conventional safe ceiling: below the 1500-byte Ethernet MTU with
/// room for a PPPoE (8 B) or common tunnel (20-60 B) header. Exceeding it does
/// not fail loudly — it fragments, and a fragmented UDP video packet turns one
/// lost fragment into a lost shard, which shows up as inexplicable loss on
/// exactly the links least able to absorb it.
pub const WAN_MTU_BUDGET: usize = 1400;

/// The size of the datagram a `WAN_PACKET_SIZE` session actually sends.
///
/// `rtp.rs` builds `packet_size + MAX_RTP_HEADER_SIZE(16)` bytes per datagram,
/// of which 32 are headers. Neither of Echo's two additions changes this:
///
/// - The **demux tag** replaces byte 0 rather than being prepended (see
///   `rtp::TxEngine::demux_tag`), so it costs zero bytes.
/// - The **GCM tag** is 16 bytes per *frame*, not per packet. It is added
///   before sharding, so its only effect is that a frame occasionally needs one
///   more shard than it otherwise would — it can never make a datagram larger.
///
/// [`echo_datagrams_fit_the_wan_mtu`](tests) asserts the result rather than
/// leaving it to this comment.
pub const ECHO_DATAGRAM_SIZE: usize = WAN_PACKET_SIZE as usize + 16;

impl SessionRequest {
    /// `audio_reserve_kbps` comes from `[network]` in `nova.toml`, carried by the
    /// [`SessionManager`] — see [`crate::qos::video_budget`]. Echo goes through
    /// the same budget as a Moonlight session deliberately: the cap exists
    /// because of what a *resolution* can consume and what the audio pipeline
    /// needs, neither of which cares which protocol asked.
    fn validate(&self, audio_reserve_kbps: u32) -> Result<StreamParams, HandoffError> {
        let bad = |why: String| HandoffError::BadRequest(why);

        // NVENC requires even dimensions; round down rather than reject, for
        // the same reason `echo::rpc::parse_display_request` does — a client
        // that computed an odd width from a scale factor wants the nearest
        // workable mode.
        let width = self.width - self.width % 2;
        let height = self.height - self.height % 2;
        if width < MIN_DIMENSION || height < MIN_DIMENSION {
            return Err(bad(format!("{width}x{height} is below the {MIN_DIMENSION}px minimum")));
        }
        if width > MAX_WIDTH || height > MAX_HEIGHT {
            return Err(bad(format!("{width}x{height} exceeds {MAX_WIDTH}x{MAX_HEIGHT}")));
        }
        if !(MIN_FPS..=MAX_FPS).contains(&self.fps) {
            return Err(bad(format!("{} fps is outside {MIN_FPS}-{MAX_FPS}", self.fps)));
        }
        if !(MIN_BITRATE_KBPS..=MAX_BITRATE_KBPS).contains(&self.bitrate_kbps) {
            return Err(bad(format!(
                "{} kbps is outside {MIN_BITRATE_KBPS}-{MAX_BITRATE_KBPS}",
                self.bitrate_kbps
            )));
        }

        // H.264 Level 5.2 caps 4K at 60 fps; the same cap `session_negotiate`
        // applies to Moonlight sessions, applied here for the same reason —
        // exceeding it crashes decoders rather than degrading them.
        let mut fps = self.fps;
        if self.codec == Codec::H264 && width * height > 1920 * 1080 && fps > 60 {
            fps = 60;
        }

        // Bound what was asked for. Unlike the Moonlight path this is not
        // defending against a slider — an Echo client picks its own number — but
        // against the same two realities: a 1080p stream cannot consume 100 Mbps,
        // and audio needs its slice of whatever we do send. A WAN session has
        // more reason to care than a LAN one, not less.
        let budget = crate::qos::video_budget(
            self.bitrate_kbps, width, height, fps, audio_reserve_kbps,
        );
        if let Some(line) = budget.describe(width, height, fps) {
            println!("{line}");
        }

        Ok(StreamParams {
            width,
            height,
            fps,
            hdr: self.hdr,
            codec: self.codec,
            bitrate_kbps: budget.video_kbps,
            app_id: self.app_id,
            host_audio: self.host_audio,
            packet_size: WAN_PACKET_SIZE,
            fec_percentage: DEFAULT_FEC_PERCENTAGE,
            min_fec_packets: DEFAULT_MIN_FEC_PACKETS,
        })
    }
}

/// A live Echo session.
#[derive(Debug)]
pub struct EchoSession {
    /// Monotonic per-process id, echoed in logs and RPC replies so a client's
    /// report can be matched to a host-side session without guessing.
    pub id: u64,
    /// Fingerprint of the device holding it — the ownership key.
    pub device_fingerprint: String,
    pub device_name: String,
    /// The punched peer. Media goes here and nowhere else for this session's
    /// lifetime (`RtpSender::pin_target`).
    pub peer: SocketAddr,
    pub params: StreamParams,
    pub started: Instant,
    /// Last evidence that the device holding this session is still there.
    ///
    /// Updated from two sources, and neither is a heartbeat invented for the
    /// purpose. Authenticated input and microphone datagrams are *proof* — they
    /// opened under this session's key, so only the holder could have sent them.
    /// The liveness sweep supplies the other source: a granted session pings the
    /// media socket every 500 ms to hold its NAT pinhole open, which `rtp.rs`
    /// sees and nothing else does.
    ///
    /// Deliberately NOT judged from control traffic alone. A working session's
    /// steady state is media pings and nothing on the control channel, so a
    /// control-only measure calls a perfectly healthy stream dead — which is
    /// exactly what happened live on 2026-08-15, 30 s into a working session.
    last_seen: Instant,
    /// When this session was detached, if it has been.
    ///
    /// Detached means the holder stopped answering: encoding and transmission
    /// have already been stopped, but the virtual display and everything running
    /// on it are held so the holder can pick up where it left off. `None` = the
    /// session is live.
    detached_since: Option<Instant>,
    /// Media key material. Private: a session hands its keys out exactly once,
    /// at start, over TLS.
    keys: SessionKeys,
    /// The audio path still speaks GameStream's AES-CBC framing, which is keyed
    /// by `rikey`/`rikeyid` rather than by [`SessionKeys`]. Generated fresh per
    /// session and handed to the client with the grant.
    rikey: [u8; 16],
    rikeyid: u32,
    /// Deduplicating opener for this session's input datagrams.
    ///
    /// Per-session, and that is load-bearing twice over: it holds the sequence
    /// high-water mark that makes redundant repeats idempotent, and it is keyed
    /// to the keys minted at `start`, so input sealed for a previous session
    /// cannot be replayed into this one.
    input: nova_core::input_channel::InputReceiver,
    /// Deduplicating opener for this session's microphone datagrams.
    ///
    /// Per-session for the same two reasons as `input`, and keyed to the same
    /// session keys — so audio sealed for a previous session cannot be replayed
    /// into this one. Its window is separate from the input receiver's because
    /// the two streams have independent sequence spaces.
    mic: nova_core::mic_channel::MicReceiver,
    /// Sealer for this session's downstream game audio.
    ///
    /// Per-session like the two receivers above, and for one extra reason of its
    /// own: it owns the sequence counter for the Echo audio wire, which is
    /// deliberately **not** `audio::AudioTxState`'s. That one numbers the
    /// GameStream stream on port 48000 and keeps advancing whenever a Moonlight
    /// client is being served — sharing it would punch gaps in this stream that
    /// the client reads as packet loss it never suffered.
    audio: nova_core::audio_channel::AudioSender,
}

impl EchoSession {
    /// Seal one encoded video frame for this session.
    ///
    /// The host-side entry point for [`nova_core::media_crypto`]. Not yet
    /// called by the media path — see the module's scope note — but it is the
    /// exact function that path will call, so the nonce/AAD derivation lives
    /// here rather than being re-derived at the call site later.
    pub fn seal_video(&self, wire_index: u32, frame_type: u8, frame: &[u8]) -> Vec<u8> {
        self.keys.seal(STREAM_VIDEO, wire_index, frame_type, frame)
    }

    /// Seal one encoded audio packet for this session.
    ///
    /// Takes `&mut self` where [`seal_video`](Self::seal_video) does not, because
    /// the sequence lives here rather than being supplied by the caller. Video's
    /// wire index is chosen by the Worker's encoder and must be echoed exactly —
    /// it is the NVENC timestamp reference invalidation targets. Audio has no
    /// such external anchor, so this side owns the numbering.
    fn seal_audio(
        &mut self,
        payload: &[u8],
    ) -> Result<Vec<u8>, nova_core::audio_channel::AudioError> {
        self.audio.datagram(payload)
    }

    fn grant(&self) -> SessionGrant {
        SessionGrant {
            session_id: self.id,
            peer: self.peer,
            params: self.params.clone(),
            keys_hex: self.keys.to_hex(),
            rikey_hex: hex::encode(self.rikey),
            rikeyid: self.rikeyid,
        }
    }
}

/// What the client receives when a session starts: where media will arrive
/// from, what shape it takes, and the keys to open it.
#[derive(Debug, Clone)]
pub struct SessionGrant {
    pub session_id: u64,
    pub peer: SocketAddr,
    pub params: StreamParams,
    /// Hex [`SessionKeys`] — see that type for why hex over a TLS channel is
    /// the whole key exchange.
    pub keys_hex: String,
    pub rikey_hex: String,
    pub rikeyid: u32,
}

/// What ending a session should do to the virtual display.
///
/// Nova has always had both behaviours — `Deactivate { cancelled }` is exactly
/// this distinction on the wire — but until the tray needed to end an Echo
/// session, every caller here wanted the same one, so it was hardcoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndMode {
    /// Full teardown: the virtual display goes away and the physical monitor
    /// comes back. What a client disconnecting or quitting means — it is done
    /// with the desktop.
    TearDown,
    /// Stop the stream, leave the virtual display up and suspended, so a
    /// reconnect is instant. What the tray's first "End Stream" press means:
    /// the operator asked for the stream to stop, not for the desktop to be
    /// rearranged. A second press releases the display.
    KeepDisplay,
}

/// The seam between "the session manager decided" and "the pipeline actually
/// moved". A trait for the same two reasons `DisplayOrchestrator` is one: the
/// gate is testable without a Worker or a socket, and multi-seat replaces an
/// implementation rather than editing this logic.
pub trait MediaPlane: Send + Sync + 'static {
    /// Point the pipeline at `peer` and start encoding `params`.
    ///
    /// `device_name` is the paired name of the client, which the Worker uses to
    /// label the virtual monitor — the same Phase 14.2 behaviour a Moonlight
    /// session gets, so Device Manager shows "Xbox" rather than a generic tag.
    fn begin(
        &self,
        peer: SocketAddr,
        params: &StreamParams,
        device_name: &str,
        rikey: [u8; 16],
        rikeyid: u32,
    ) -> Result<(), HandoffError>;
    /// Stop encoding and release the target.
    ///
    /// `mode` decides what happens to the virtual display, which is a separate
    /// question from whether the session is over: a client that disconnected is
    /// done with the desktop, but a host operator who pressed "End Stream" has
    /// asked only for the stream to stop and may press again to release the
    /// display. See [`EndMode`].
    fn end(&self, mode: EndMode);
    /// Hand a GameStream input packet to the injection stack.
    ///
    /// Unparsed by design — see `echo::rpc::handle_input`.
    fn inject_input(&self, packet: Vec<u8>);
    /// Force the next encoded frame to be a keyframe.
    ///
    /// Nova runs an infinite GOP, so there is no scheduled IDR to wait for. A
    /// client whose reference chain broke — a dropped frame, a decoder reset —
    /// can decode nothing further until one is produced on request. Without
    /// this the picture freezes permanently on the last good frame while the
    /// host streams on, which is exactly what happened live on 2026-08-15.
    fn request_idr(&self);
}

/// Production plane: retargets `RtpSender` and configures the live Worker.
pub struct WorkerMediaPlane {
    rtp_sender: Arc<Mutex<crate::rtp::RtpSender>>,
    worker_link: WorkerLink,
    worker_caps: Arc<Mutex<Option<WorkerCaps>>>,
}

impl WorkerMediaPlane {
    pub fn new(
        rtp_sender: Arc<Mutex<crate::rtp::RtpSender>>,
        worker_link: WorkerLink,
        worker_caps: Arc<Mutex<Option<WorkerCaps>>>,
    ) -> Self {
        Self { rtp_sender, worker_link, worker_caps }
    }
}

impl MediaPlane for WorkerMediaPlane {
    fn begin(
        &self,
        peer: SocketAddr,
        params: &StreamParams,
        device_name: &str,
        rikey: [u8; 16],
        rikeyid: u32,
    ) -> Result<(), HandoffError> {
        // Refuse before touching anything if no Worker can serve this. A
        // half-applied start (RTP retargeted, Worker never configured) would
        // leave the sender pinned at a peer nothing ever encodes for.
        if self.worker_caps.lock().unwrap().is_none() {
            return Err(HandoffError::WorkerUnavailable(
                "no worker has reported its capture capabilities yet".to_string(),
            ));
        }

        // Ordering here is load-bearing:
        //
        //   1. `reset()` first — it clears the previous session's wire index,
        //      sequence numbers, and any pin, and flushes stale pings out of
        //      the receive buffer. It must come before the pin, because reset
        //      deliberately clears pins (a finished session must never leave
        //      the sender deaf to the next client).
        //   2. `configure`/`set_fps`/`set_codec` next — parameters the send
        //      thread needs before the first frame.
        //   3. `pin_target` last, which is the instant this session becomes
        //      the media destination.
        //
        // Every one of these is a message on the send thread's ordered command
        // channel, so they land in this order relative to any frame already
        // queued — the retarget can never split a frame across two peers.
        {
            let mut rtp = self.rtp_sender.lock().unwrap();
            rtp.reset();
            rtp.configure(
                params.packet_size as usize,
                params.fec_percentage as usize,
                params.min_fec_packets as usize,
            );
            rtp.set_fps(params.fps);
            rtp.set_codec(params.codec == Codec::Hevc, params.codec == Codec::Av1);
            rtp.pin_target(peer);
            // The pin takes effect now; the Worker's reconfiguration does not.
            // Everything it encodes in between belongs to the previous session's
            // format and must not reach this client. Released by the Master when
            // `WorkerConfigured` arrives, and self-releasing if it never does.
            rtp.hold_until_configured();
        }

        self.worker_link.send(ControlMsg::ConfigureStart(ipc::ConfigureStart {
            width: params.width,
            height: params.height,
            fps: params.fps,
            codec: WireCodec::from(params.codec),
            hdr_confirmed: params.hdr,
            bitrate_kbps: params.bitrate_kbps,
            app_id: params.app_id,
            launch_app: params.app_id != 1, // Desktop needs no process started
            device_name: device_name.to_string(),
            rikey,
            rikeyid,
            host_audio: params.host_audio,
            audio_encryption: true,
            // 20 ms, where a Moonlight session negotiates 5 ms.
            //
            // GameStream picks 5 ms to keep audio latency under its FEC, and
            // that is the right trade on a LAN socket with no per-packet crypto.
            // Echo's audio is a sealed datagram: every packet pays a 16-byte GCM
            // tag plus 6 bytes of header, so 5 ms frames would put 200
            // datagrams/second on the punched path to carry roughly 80 bytes of
            // Opus each — more overhead than payload. 20 ms matches what the
            // microphone already proved comfortable in the other direction and
            // cuts the packet rate fourfold.
            //
            // This is per-Worker-session, not per-client: a Moonlight client
            // sharing the pipeline with an Echo session gets 20 ms frames too.
            // Accepted deliberately — 20 ms is a normal Opus frame and
            // moonlight-common-c handles it; the alternative is transcoding the
            // same audio twice.
            audio_packet_duration_ms: 20,
            packet_size: params.packet_size,
            min_fec_packets: params.min_fec_packets,
            start_frame_index: 1,
        }));
        Ok(())
    }

    fn inject_input(&self, packet: Vec<u8>) {
        // The exact message the ENet control path sends, so Echo's input meets
        // the Master's helper-vs-Worker routing at the identical place as
        // Moonlight's — including the gamepad exception.
        self.worker_link.send(ControlMsg::InjectInput(packet));
    }

    fn request_idr(&self) {
        self.worker_link.send(ControlMsg::RequestIdr);
    }

    fn end(&self, mode: EndMode) {
        // Deactivate first, then reset: the Worker stops producing frames
        // before the sender forgets where they were going, so no frame is
        // encoded into a sender with no target.
        //
        // `cancelled` IS the display decision on this wire: true tears the VDD
        // down and restores the monitor, false suspends it for a fast
        // reconnect (see `deactivate_worker`). Either way the session is over
        // and the RTP pin is dropped below.
        self.worker_link.send(ControlMsg::Deactivate {
            cancelled: matches!(mode, EndMode::TearDown),
        });
        let mut rtp = self.rtp_sender.lock().unwrap();
        rtp.reset(); // clears the pin — learning resumes for the next Moonlight client
    }
}

/// Where a confirmed punch is published. `echo::wan::GatherHandle` writes it;
/// this reads it.
pub type LatchedPeer = Arc<Mutex<Option<SocketAddr>>>;

/// Operator settings from `nova.toml` that this manager needs.
///
/// A struct rather than two `u32` parameters because they would otherwise sit
/// adjacent and positional at every construction site, where swapping them
/// compiles cleanly and produces a 512-second grace period and a 600 Kbps audio
/// reservation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionPolicy {
    /// `[network] audio_reserve_kbps` — see [`crate::qos::video_budget`].
    pub audio_reserve_kbps: u32,
    /// `[stream] detach_grace_secs` — how long a detached session is held before
    /// its display is released. `Duration::ZERO` = hold indefinitely, matching
    /// the Moonlight path's reading of a configured 0.
    pub detach_grace: Duration,
}

/// What one [`SessionManager::sweep`] decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sweep {
    /// Nothing to do: no session, a healthy one, or a detached one still inside
    /// its grace period.
    Nothing,
    /// A live session stopped answering. Encoding and transmission have been
    /// stopped; the display is held.
    Detached,
    /// A detached session outlived its grace period and has been torn down.
    Reaped,
}

/// Owns the one Echo session Nova can run, and the gate protecting it.
pub struct SessionManager {
    plane: Arc<dyn MediaPlane>,
    client_info: Arc<Mutex<Option<ClientInfo>>>,
    latched: LatchedPeer,
    active: Mutex<Option<EchoSession>>,
    next_id: AtomicU64,
    /// Lock-free mirror of `active.is_some()`, for the frame path.
    ///
    /// `media_supervisor` consults this for every frame at up to 120 fps on a
    /// stream that is usually Moonlight's. Taking the session mutex there would
    /// put a lock on the hot path of the *legacy* stream purely to ask a
    /// question whose answer is almost always "no" — the kind of tax that is
    /// invisible in a test and measurable in frame pacing. Written only while
    /// `active`'s mutex is held, so it can never disagree with it for longer
    /// than one instruction.
    echo_active: std::sync::atomic::AtomicBool,
    /// Operator settings. Held here rather than read from config at the call
    /// sites so the gate stays testable without a `nova.toml`.
    policy: SessionPolicy,
}

impl SessionManager {
    pub fn new(
        plane: Arc<dyn MediaPlane>,
        client_info: Arc<Mutex<Option<ClientInfo>>>,
        latched: LatchedPeer,
        policy: SessionPolicy,
    ) -> Self {
        Self {
            plane,
            client_info,
            latched,
            active: Mutex::new(None),
            next_id: AtomicU64::new(1),
            echo_active: std::sync::atomic::AtomicBool::new(false),
            policy,
        }
    }

    /// Seal one encoded video frame if an Echo session owns the pipeline.
    ///
    /// Returns `None` when it does not — which is the answer on every frame of
    /// every Moonlight session, reached without taking a lock. Called from
    /// `media_supervisor` immediately before the frame goes to `RtpSender`,
    /// which is the correct place for exactly one reason: sealing must happen
    /// **before** sharding, so Reed-Solomon parity is computed over the
    /// ciphertext and the client can repair loss without holding the key. See
    /// [`nova_core::media_crypto`].
    pub fn seal_video(&self, wire_index: u32, frame_type: u8, frame: &[u8]) -> Option<Vec<u8>> {
        if !self.echo_active.load(Ordering::Relaxed) {
            return None;
        }
        let guard = self.active.lock().unwrap();
        let session = guard.as_ref()?;
        Some(session.seal_video(wire_index, frame_type, frame))
    }

    /// Seal one encoded audio packet if an Echo session owns the pipeline,
    /// returning it with the address to send it to.
    ///
    /// Returns `None` when no Echo session holds the pipeline — the answer on
    /// every packet of every Moonlight session, reached without taking a lock.
    ///
    /// **Returns the datagram rather than sending it**, which is the same
    /// discipline `open_sealed_mic` follows and for the same reason: the mutex
    /// this takes is the one `seal_video` takes for every video frame, so
    /// holding it across a socket write would put the network's worst moment
    /// inside the frame path. The caller sends after the guard drops.
    ///
    /// The sealing itself stays inside the lock because it must: it advances the
    /// session's sequence counter, and two callers interleaving there would
    /// either reuse a GCM nonce or emit sequences out of order.
    pub fn seal_audio(&self, payload: &[u8]) -> Option<(Vec<u8>, SocketAddr)> {
        if !self.echo_active.load(Ordering::Relaxed) {
            return None;
        }
        let mut guard = self.active.lock().unwrap();
        let session = guard.as_mut()?;
        let peer = session.peer;
        match session.seal_audio(payload) {
            Ok(datagram) => Some((datagram, peer)),
            Err(why) => {
                // Reachable only for an empty or oversized payload — a bug in
                // the encoder's output, not a network event. No sequence was
                // consumed, so this shows up as a missing packet rather than as
                // loss (see `AudioSender::datagram`).
                println!("🔇 Echo audio packet not sealed: {why}");
                None
            }
        }
    }

    /// Who owns the pipeline right now.
    ///
    /// Moonlight is reported ahead of Echo when both look active, because the
    /// gate below guarantees that cannot happen for long and reporting the
    /// louder truth is the safer default during the overlap.
    pub fn owner(&self) -> MediaOwner {
        if self.moonlight_is_live() {
            return MediaOwner::Moonlight;
        }
        if self.active.lock().unwrap().is_some() {
            return MediaOwner::Echo;
        }
        MediaOwner::Idle
    }

    /// True while an Echo session holds the pipeline — the flag the Moonlight
    /// path checks before configuring a Worker (see `lib.rs::session_watcher`).
    pub fn echo_holds_media(&self) -> bool {
        self.echo_active.load(Ordering::Relaxed)
    }

    fn moonlight_is_live(&self) -> bool {
        self.client_info
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|i| i.streaming_active)
    }

    /// Summary for `get_status`, safe to serialise (never contains keys).
    pub fn status(&self) -> Option<(u64, String, SocketAddr, StreamParams, u64)> {
        let guard = self.active.lock().unwrap();
        let s = guard.as_ref()?;
        Some((
            s.id,
            s.device_name.clone(),
            s.peer,
            s.params.clone(),
            s.started.elapsed().as_secs(),
        ))
    }

    /// The handoff gate.
    ///
    /// Every refusal happens **before** anything is retargeted, so a denied
    /// request leaves the pipeline byte-for-byte as it was. That is what makes
    /// "deny while Moonlight is streaming" a real guarantee rather than a
    /// window: there is no partial application to unwind.
    pub fn start(
        &self,
        device: &EchoIdentity,
        request: SessionRequest,
    ) -> Result<SessionGrant, HandoffError> {
        // 1. Validate before consulting any state — a malformed request should
        //    not be able to report on whether someone else is streaming.
        let params = request.validate(self.policy.audio_reserve_kbps)?;

        // 2. Anti-hijack: a live Moonlight client owns the pipeline outright.
        if self.moonlight_is_live() {
            println!(
                "⛔ Echo: \"{}\" asked to start a session while a Moonlight client is streaming — denied",
                device.device_name
            );
            return Err(HandoffError::MoonlightActive);
        }

        // 3. One Echo session at a time. The same device asking again is a
        //    restart (its socket died and it reconnected), which is a
        //    materially different situation from a second device barging in.
        let mut guard = self.active.lock().unwrap();
        if let Some(existing) = guard.as_ref() {
            let detached = existing.detached_since.is_some();
            let same_device = existing.device_fingerprint == device.fingerprint;

            if !same_device && !detached {
                return Err(HandoffError::HeldByAnotherDevice {
                    device: existing.device_name.clone(),
                });
            }

            if detached {
                // Nothing is running: the sweep already stopped the encoder and
                // the transmission, and left the display up for exactly this.
                // Calling `end` again would be pointless work at the moment
                // latency matters most, and `end(TearDown)` would destroy the
                // very thing being reclaimed.
                //
                // A DIFFERENT device is allowed to take a detached session over.
                // The alternative — holding the seat for the whole grace period
                // — means one phone losing signal locks every other paired
                // device out of the host for ten minutes, to protect a stream
                // nobody is watching. The display it inherits is the same
                // display, and the keys minted below are new either way.
                if same_device {
                    println!(
                        "⚡ Echo: \"{}\" reclaiming its detached session {} — display still up, resuming",
                        device.device_name, existing.id
                    );
                } else {
                    println!(
                        "🔄 Echo: \"{}\" taking over session {}, detached by \"{}\" — display still up",
                        device.device_name, existing.id, existing.device_name
                    );
                }
            } else {
                println!(
                    "🔁 Echo: \"{}\" restarting its session {} — ending the old one first",
                    device.device_name, existing.id
                );
                // KeepDisplay: the same device is reconnecting and a new `begin`
                // follows immediately, so tearing the VDD down here would make a
                // restart pay a full display cycle it is about to undo.
                self.plane.end(EndMode::KeepDisplay);
            }
            *guard = None;
            self.echo_active.store(false, Ordering::Relaxed);
        }

        // 4. A path must already be proven. Reachability is not something this
        //    layer can establish on demand — the punch either happened or it
        //    did not.
        let Some(peer) = *self.latched.lock().unwrap() else {
            return Err(HandoffError::NoPathLatched);
        };

        // 5. Mint keys and apply. `begin` is the first line that changes any
        //    state outside this struct.
        let keys = SessionKeys::generate();
        let (rikey, rikeyid) = generate_audio_key();
        self.plane.begin(peer, &params, &device.device_name, rikey, rikeyid)?;

        let session = EchoSession {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            device_fingerprint: device.fingerprint.clone(),
            device_name: device.device_name.clone(),
            peer,
            params,
            started: Instant::now(),
            // A brand-new session counts as alive now. Without this the very
            // first sweep would measure against a zero mark and detach a session
            // whose client has not had time to send its first ping.
            last_seen: Instant::now(),
            detached_since: None,
            input: nova_core::input_channel::InputReceiver::new(keys.clone()),
            mic: nova_core::mic_channel::MicReceiver::new(keys.clone()),
            audio: nova_core::audio_channel::AudioSender::new(keys.clone()),
            keys,
            rikey,
            rikeyid,
        };
        println!(
            "🎬 Echo session {} started for \"{}\" → {} : {}x{}@{}fps {}{}",
            session.id,
            session.device_name,
            peer,
            session.params.width,
            session.params.height,
            session.params.fps,
            session.params.codec.as_str(),
            if session.params.hdr { "/HDR10" } else { "" },
        );
        let grant = session.grant();
        *guard = Some(session);
        // Ordered after the session is installed: the frame path reads this
        // flag without the lock, so it must never say "yes" before there is a
        // session to seal with.
        self.echo_active.store(true, Ordering::Relaxed);
        Ok(grant)
    }

    /// End the session held by `device`. Only its owner may — otherwise any
    /// paired device could end anyone's stream, which is a denial-of-service
    /// dressed up as a feature.
    ///
    /// `Ok(None)` means there was nothing running and the display was released
    /// anyway. That case used to return `NotTheOwner`, which was both wrong (no
    /// owner exists to not be) and user-visible: pressing "End Stream" a second
    /// time in the Echo app got an error, so the button looked dead and the
    /// only way to make it respond again was to close and reopen the app
    /// (reported 2026-08-16). Ending something already ended is a request that
    /// has been satisfied, so it answers yes — and, since the whole point of
    /// the press is "put my desktop back", it makes sure of that too.
    ///
    /// The owner check still stands where an owner exists, and a live Moonlight
    /// session is refused rather than torn down: a paired-but-idle device
    /// asking to release the display must not be able to end someone else's
    /// stream through the back door.
    pub fn stop(&self, device: &EchoIdentity) -> Result<Option<u64>, HandoffError> {
        let mut guard = self.active.lock().unwrap();
        let Some(session) = guard.as_ref() else {
            if self.moonlight_is_live() {
                return Err(HandoffError::MoonlightActive);
            }
            self.plane.end(EndMode::TearDown);
            return Ok(None);
        };
        if session.device_fingerprint != device.fingerprint {
            return Err(HandoffError::NotTheOwner);
        }
        let id = session.id;
        // Cleared before the session is dropped, so no frame is sealed with a
        // key the client has already stopped listening for.
        self.echo_active.store(false, Ordering::Relaxed);
        // The client said it is done (or its tunnel died), so the desktop goes
        // back to the monitor. Unchanged behaviour — only the host-initiated
        // path below asks for anything else.
        self.plane.end(EndMode::TearDown);
        *guard = None;
        println!("🛑 Echo session {id} ended by \"{}\"", device.device_name);
        Ok(Some(id))
    }

    /// Inject input on behalf of the session's owner.
    ///
    /// Owner-checked, and the check matters more here than anywhere else on
    /// this surface: input injection drives the host's keyboard and mouse. A
    /// device that is merely paired — not streaming — must never reach it.
    pub fn inject_input(&self, device: &EchoIdentity, packet: Vec<u8>) -> Result<(), HandoffError> {
        let guard = self.active.lock().unwrap();
        let Some(session) = guard.as_ref() else {
            return Err(HandoffError::NotTheOwner);
        };
        if session.device_fingerprint != device.fingerprint {
            return Err(HandoffError::NotTheOwner);
        }
        self.plane.inject_input(packet);
        Ok(())
    }

    /// Inject input from a sealed datagram that arrived on the media socket.
    ///
    /// The counterpart to [`inject_input`](Self::inject_input) for the
    /// unreliable path, and the authorization works differently on purpose.
    /// There is no TLS connection here to have authenticated a device and no
    /// `EchoIdentity` to compare, because this datagram arrived raw on a socket
    /// anyone can write to. What stands in for the owner check is the session
    /// key: it was minted at `start` and handed to exactly one device over
    /// mutual TLS, so a datagram that opens under it came from that device.
    /// Anything else fails the tag and is counted.
    ///
    /// Returns how many packets were injected — zero is normal and means every
    /// packet in the datagram was a redundant repeat of one already applied.
    pub fn inject_sealed_input(&self, datagram: &[u8]) -> Result<usize, InputRejection> {
        // The lock is released before anything is injected, and that boundary
        // is deliberate: `seal_video` takes this same mutex for **every video
        // frame**, so any work done while holding it is work the media thread
        // can block on. Decrypting and deduplicating genuinely need the
        // session's state; handing packets onward does not. Keeping the
        // injection inside would couple the frame path to whatever the Worker
        // link happens to cost that instant, which is a hitch nobody would
        // think to look for here.
        let packets = {
            let mut guard = self.active.lock().unwrap();
            let Some(session) = guard.as_mut() else {
                return Err(InputRejection::NoSession);
            };
            let packets = session
                .input
                .open(datagram)
                .map_err(|e| InputRejection::Unopenable(e.to_string()))?;
            // Proof of life, and the best kind available: this datagram opened
            // under the session's own key, so only the device holding it could
            // have sent it. Recorded even for a repeat — a duplicate still had
            // to be transmitted by someone who is there.
            session.last_seen = Instant::now();
            packets
        };

        let count = packets.len();
        for packet in packets {
            self.plane.inject_input(packet);
        }
        Ok(count)
    }

    /// Open a sealed microphone datagram that arrived on the media socket.
    ///
    /// Authorization works exactly as it does for
    /// [`inject_sealed_input`](Self::inject_sealed_input), and for the same
    /// reason: there is no TLS connection here and no `EchoIdentity` to
    /// compare, so possession of the session key — minted at `start` and handed
    /// to one device over mutual TLS — is what proves the sender.
    ///
    /// Returns the packet, or `None` when the datagram was authentic but
    /// carried nothing new (a duplicate, or an arrival too late to place).
    ///
    /// Note what this does **not** do: render. The lock below is the same one
    /// `seal_video` takes for every video frame, so this returns the packet and
    /// lets the caller hand it to the renderer with nothing held — the same
    /// discipline `inject_sealed_input` documents.
    pub fn open_sealed_mic(
        &self,
        datagram: &[u8],
    ) -> Result<Option<nova_core::mic_channel::MicPacket>, InputRejection> {
        let mut guard = self.active.lock().unwrap();
        let Some(session) = guard.as_mut() else {
            return Err(InputRejection::NoSession);
        };
        let opened = session
            .mic
            .open(datagram)
            .map_err(|e| InputRejection::Unopenable(e.to_string()))?;
        // Proof of life for the same reason as `inject_sealed_input`: it opened
        // under this session's key.
        session.last_seen = Instant::now();
        Ok(opened)
    }

    /// Address of the session's peer, if one is live and not already detached.
    ///
    /// The caller uses it to ask `rtp.rs` how long that peer has been silent.
    /// Returned rather than having this type query `rtp` itself, so the two
    /// locks are taken one after another instead of nested: every existing path
    /// takes the session lock and then the RTP lock (`start` → `plane.begin`,
    /// `force_end` → `plane.end`), and a sweep that read RTP while holding the
    /// session lock would be the one place doing it the other way round.
    pub fn live_peer(&self) -> Option<SocketAddr> {
        let guard = self.active.lock().unwrap();
        let session = guard.as_ref()?;
        session.detached_since.is_none().then_some(session.peer)
    }

    /// One liveness sweep: detach a session whose holder has gone quiet, and
    /// tear down a detached one that was never reclaimed.
    ///
    /// `media_idle` is how long the media socket has been silent from the peer
    /// [`live_peer`](Self::live_peer) reported, or `None` when that is unknown —
    /// `rtp.rs` tracks only the single most recent sender, so a stray datagram
    /// from anywhere else erases the reading. `None` therefore means "no news",
    /// never "dead": treating it as evidence of death would let one packet from
    /// a port scanner detach a healthy session.
    ///
    /// ## Why both halves live here
    ///
    /// Nothing else ends an Echo session. `stop` requires a client that is well
    /// enough to ask, and `force_end` requires an operator at the tray — so a
    /// phone that lost signal left the host encoding and transmitting to an
    /// address nobody was listening at, indefinitely. The tunnel sweep reclaimed
    /// its *slot* (transport.rs) but never told the session, which is why this
    /// is called from there.
    pub fn sweep(&self, media_idle: Option<Duration>, idle_timeout: Duration) -> Sweep {
        let mut guard = self.active.lock().unwrap();
        let Some(session) = guard.as_mut() else {
            return Sweep::Nothing;
        };

        // ── Already detached: is the grace period up? ────────────────────────
        if let Some(detached_at) = session.detached_since {
            let grace = self.policy.detach_grace;
            if grace.is_zero() || detached_at.elapsed() < grace {
                return Sweep::Nothing;
            }
            let id = session.id;
            let name = session.device_name.clone();
            *guard = None;

            // A Moonlight client may have claimed the pipeline while this
            // session sat detached — `echo_active` went false at detach
            // precisely so it could. Ending the plane now would send a
            // cancelling Deactivate and tear down THAT session instead, which
            // would look exactly like a stream dying at random ten minutes after
            // an unrelated phone lost signal. Drop the record and touch nothing.
            if self.moonlight_is_live() {
                println!(
                    "🧹 Echo: detached session {id} (\"{name}\") expired, but Moonlight now holds \
                     the pipeline — forgetting the session without touching the display"
                );
                return Sweep::Reaped;
            }

            println!(
                "🕐 Echo: detached session {id} (\"{name}\") was not reclaimed within {}s — \
                 tearing down and restoring the display",
                grace.as_secs()
            );
            self.plane.end(EndMode::TearDown);
            return Sweep::Reaped;
        }

        // ── Live: has the holder stopped answering? ──────────────────────────
        //
        // A fresh reading is proof of life and moves the mark forward; a stale
        // or absent one leaves whatever authenticated input and microphone
        // datagrams have already recorded.
        if let Some(idle) = media_idle {
            if idle < idle_timeout {
                // `now - idle` rather than `now`: the reading says the peer was
                // there THEN, and stamping it as now would push detection out by
                // up to a full timeout. checked_sub because Instant is
                // QPC-since-boot on Windows and plain subtraction panics near
                // boot (the Phase 15.3 crash-loop); the guard also keeps the
                // mark monotonic, since an authenticated input datagram may have
                // already recorded something fresher.
                if let Some(seen_at) = Instant::now().checked_sub(idle) {
                    if seen_at > session.last_seen {
                        session.last_seen = seen_at;
                    }
                }
            }
        }
        if session.last_seen.elapsed() < idle_timeout {
            return Sweep::Nothing;
        }

        let id = session.id;
        let name = session.device_name.clone();
        let silent_for = session.last_seen.elapsed().as_secs();
        session.detached_since = Some(Instant::now());

        // Cleared before the plane is touched, for the same reason `stop` does
        // it: no frame may be sealed with a key for a session that is no longer
        // receiving. It also un-blocks the Moonlight path, which defers to Echo
        // while this flag is set — a detached session must not hold the whole
        // pipeline hostage against a client that is actually present.
        self.echo_active.store(false, Ordering::Relaxed);

        let grace = self.policy.detach_grace;
        if self.moonlight_is_live() {
            // Defensive: the gate should make this unreachable. Detaching still
            // has to happen, but touching the plane would suspend someone else's
            // live stream.
            println!("⏸️  Echo session {id} (\"{name}\") DETACHED — Moonlight holds the pipeline, leaving it alone");
        } else {
            // KeepDisplay, not TearDown: the encoder stops and the frames stop,
            // which is the whole point of noticing — but the desktop stays
            // exactly as the user left it so a reconnect resumes into it. The
            // Worker's `resume_suspended` is what makes that reconnect instant.
            self.plane.end(EndMode::KeepDisplay);
            if grace.is_zero() {
                println!(
                    "⏸️  Echo session {id} (\"{name}\") DETACHED after {silent_for}s of silence — \
                     encoder and transmission stopped, display held indefinitely"
                );
            } else {
                println!(
                    "⏸️  Echo session {id} (\"{name}\") DETACHED after {silent_for}s of silence — \
                     encoder and transmission stopped, display held for {}s",
                    grace.as_secs()
                );
            }
        }
        Sweep::Detached
    }

    /// Microphone-datagram counters for the live session, for diagnostics.
    pub fn mic_stats(&self) -> Option<(nova_core::mic_channel::MicStats, u32)> {
        let guard = self.active.lock().unwrap();
        let session = guard.as_ref()?;
        Some((session.mic.stats(), session.mic.highest_sequence()))
    }

    /// Input-datagram counters for the live session, for diagnostics.
    pub fn input_stats(&self) -> Option<nova_core::input_channel::InputStats> {
        Some(self.active.lock().unwrap().as_ref()?.input.stats())
    }

    /// Ask the encoder for a keyframe on behalf of the session's owner.
    ///
    /// Owner-checked like [`stop`](Self::stop): a keyframe request is cheap but
    /// not free — it costs a full intra-coded frame — so an authenticated device
    /// that holds no session must not be able to make the host produce them.
    pub fn request_idr(&self, device: &EchoIdentity) -> Result<u64, HandoffError> {
        let guard = self.active.lock().unwrap();
        let Some(session) = guard.as_ref() else {
            return Err(HandoffError::NotTheOwner);
        };
        if session.device_fingerprint != device.fingerprint {
            return Err(HandoffError::NotTheOwner);
        }
        self.plane.request_idr();
        Ok(session.id)
    }

    /// Drop the session without an owner check — for host-side teardown
    /// (shutdown, a Worker that will never come back, the tray's "End
    /// Stream"), never for a remote request.
    ///
    /// Returns whether there was a session to end, which is what lets the tray
    /// distinguish "I stopped your stream" from "there was nothing running" —
    /// and the absence of that answer is why "End Stream" reported *"no active
    /// session"* while an Echo client was streaming: the Master only ever
    /// consulted `ClientInfo`, which describes the GameStream session and knows
    /// nothing about this one.
    pub fn force_end(&self, why: &str, mode: EndMode) -> bool {
        let mut guard = self.active.lock().unwrap();
        self.echo_active.store(false, Ordering::Relaxed);
        match guard.take() {
            Some(session) => {
                println!("🛑 Echo session {} force-ended: {why}", session.id);
                self.plane.end(mode);
                true
            }
            None => false,
        }
    }
}

/// Fresh GameStream audio key for this session.
fn generate_audio_key() -> ([u8; 16], u32) {
    use rand::RngCore;
    let mut rikey = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut rikey);
    (rikey, rand::rngs::OsRng.next_u32())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records what the gate decided to do, so the decision can be asserted
    /// without a Worker, a socket, or a capture pipeline.
    #[derive(Default)]
    struct MockPlane {
        begun: Mutex<Vec<(SocketAddr, StreamParams)>>,
        ended: Mutex<usize>,
        /// Every end's mode, in order — the tray's two-stage "End Stream"
        /// turns on which one each caller asks for.
        end_modes: Mutex<Vec<EndMode>>,
        idrs: Mutex<usize>,
        injected: Mutex<Vec<Vec<u8>>>,
        fail: Option<HandoffError>,
    }

    impl MediaPlane for Arc<MockPlane> {
        fn begin(
            &self,
            peer: SocketAddr,
            params: &StreamParams,
            _device_name: &str,
            _rikey: [u8; 16],
            _rikeyid: u32,
        ) -> Result<(), HandoffError> {
            if let Some(e) = &self.fail {
                return Err(e.clone());
            }
            self.begun.lock().unwrap().push((peer, params.clone()));
            Ok(())
        }
        fn end(&self, mode: EndMode) {
            *self.ended.lock().unwrap() += 1;
            self.end_modes.lock().unwrap().push(mode);
        }
        fn request_idr(&self) {
            *self.idrs.lock().unwrap() += 1;
        }
        fn inject_input(&self, packet: Vec<u8>) {
            self.injected.lock().unwrap().push(packet);
        }
    }

    fn device(name: &str, fp: u8) -> EchoIdentity {
        EchoIdentity {
            fingerprint: hex::encode([fp; 32]),
            device_name: name.to_string(),
        }
    }

    fn peer() -> SocketAddr {
        "203.0.113.9:47998".parse().unwrap()
    }

    struct Fixture {
        mgr: SessionManager,
        plane: Arc<MockPlane>,
        client_info: Arc<Mutex<Option<ClientInfo>>>,
    }

    fn fixture(latched: Option<SocketAddr>) -> Fixture {
        // No audio reservation and a long grace: these tests are about the gate,
        // and the ones that care about detachment set their own policy.
        Fixture::with_policy(
            latched,
            SessionPolicy { audio_reserve_kbps: 0, detach_grace: Duration::from_secs(600) },
        )
    }

    impl Fixture {
        fn with_policy(latched: Option<SocketAddr>, policy: SessionPolicy) -> Fixture {
            let plane = Arc::new(MockPlane::default());
            let client_info = Arc::new(Mutex::new(None));
            let mgr = SessionManager::new(
                Arc::new(plane.clone()),
                client_info.clone(),
                Arc::new(Mutex::new(latched)),
                policy,
            );
            Fixture { mgr, plane, client_info }
        }
    }

    #[test]
    fn a_punched_path_becomes_a_session_with_keys() {
        let f = fixture(Some(peer()));
        let grant = f
            .mgr
            .start(&device("Xbox", 1), SessionRequest::default())
            .expect("start");

        assert_eq!(grant.peer, peer());
        assert_eq!(f.mgr.owner(), MediaOwner::Echo);
        // The plane was retargeted exactly once, at the punched address.
        let begun = f.plane.begun.lock().unwrap();
        assert_eq!(begun.len(), 1);
        assert_eq!(begun[0].0, peer());
        // A WAN session must not inherit the LAN packet size.
        assert_eq!(begun[0].1.packet_size, WAN_PACKET_SIZE);

        // The grant carries usable key material, and the session keeps its own
        // copy — sealing with it must produce something the client can open.
        let keys = SessionKeys::from_hex(&grant.keys_hex).expect("grant keys parse");
        let sealed = f
            .mgr
            .active
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .seal_video(1, 2, b"frame");
        assert_eq!(keys.open(STREAM_VIDEO, 1, 2, &sealed).unwrap(), b"frame");
    }

    /// The anti-hijack rule, and the property that makes it meaningful: a
    /// refusal must leave the pipeline untouched, not partially retargeted.
    #[test]
    fn a_live_moonlight_session_blocks_the_handoff_without_touching_the_pipeline() {
        let f = fixture(Some(peer()));
        *f.client_info.lock().unwrap() = Some(ClientInfo {
            streaming_active: true,
            ..Default::default()
        });

        let err = f
            .mgr
            .start(&device("Xbox", 1), SessionRequest::default())
            .expect_err("must refuse");
        assert_eq!(err, HandoffError::MoonlightActive);
        assert_eq!(err.code(), "moonlight_active");
        assert!(f.plane.begun.lock().unwrap().is_empty(), "nothing may be retargeted");
        assert_eq!(f.mgr.owner(), MediaOwner::Moonlight);

        // When that client disconnects, the same request succeeds.
        f.client_info.lock().unwrap().as_mut().unwrap().streaming_active = false;
        assert!(f.mgr.start(&device("Xbox", 1), SessionRequest::default()).is_ok());
    }

    #[test]
    fn a_second_device_cannot_take_a_live_echo_session() {
        let f = fixture(Some(peer()));
        f.mgr.start(&device("Xbox", 1), SessionRequest::default()).expect("first");

        let err = f
            .mgr
            .start(&device("Pixel", 2), SessionRequest::default())
            .expect_err("must refuse");
        assert_eq!(err, HandoffError::HeldByAnotherDevice { device: "Xbox".into() });
        assert_eq!(f.plane.begun.lock().unwrap().len(), 1, "no second retarget");
    }

    /// A client whose socket died and reconnected is not a competitor. It gets
    /// a clean restart — old session torn down first, so the pipeline is never
    /// configured twice without an intervening stop.
    #[test]
    fn the_same_device_reconnecting_restarts_rather_than_being_refused() {
        let f = fixture(Some(peer()));
        let first = f.mgr.start(&device("Xbox", 1), SessionRequest::default()).unwrap();
        let second = f.mgr.start(&device("Xbox", 1), SessionRequest::default()).unwrap();

        assert_ne!(first.session_id, second.session_id, "a restart is a new session");
        assert_ne!(first.keys_hex, second.keys_hex, "…with fresh keys");
        assert_eq!(*f.plane.ended.lock().unwrap(), 1, "the old session was ended");
        assert_eq!(f.plane.begun.lock().unwrap().len(), 2);
    }

    #[test]
    fn a_session_cannot_start_without_a_confirmed_path() {
        let f = fixture(None);
        let err = f
            .mgr
            .start(&device("Xbox", 1), SessionRequest::default())
            .expect_err("no path");
        assert_eq!(err, HandoffError::NoPathLatched);
        assert!(f.plane.begun.lock().unwrap().is_empty());
    }

    #[test]
    fn only_the_owner_can_stop_a_session() {
        let f = fixture(Some(peer()));
        f.mgr.start(&device("Xbox", 1), SessionRequest::default()).unwrap();

        assert_eq!(f.mgr.stop(&device("Pixel", 2)), Err(HandoffError::NotTheOwner));
        assert_eq!(*f.plane.ended.lock().unwrap(), 0, "a stranger's stop is a no-op");

        assert_eq!(f.mgr.stop(&device("Xbox", 1)), Ok(Some(1)));
        assert_eq!(f.mgr.owner(), MediaOwner::Idle);
        assert_eq!(*f.plane.ended.lock().unwrap(), 1);

        // Stopping an ALREADY-stopped session succeeds and releases the
        // display, rather than erroring. The old `NotTheOwner` here was
        // user-visible: the Echo app's "End Stream" button did nothing on a
        // second press and only recovered after closing and reopening the app.
        assert_eq!(f.mgr.stop(&device("Xbox", 1)), Ok(None));
        assert_eq!(
            f.plane.end_modes.lock().unwrap().as_slice(),
            &[EndMode::TearDown, EndMode::TearDown],
            "both the real stop and the repeat put the desktop back"
        );
    }

    /// The one thing the idempotent stop must never become: a way for a paired
    /// device that holds nothing to reach across and end somebody else's
    /// stream. An Echo device asking to stop while MOONLIGHT is streaming is
    /// refused — the display it would release is in use.
    #[test]
    fn an_idle_device_cannot_release_a_display_moonlight_is_using() {
        let f = fixture(Some(peer()));
        *f.client_info.lock().unwrap() = Some(ClientInfo {
            streaming_active: true,
            ..Default::default()
        });

        assert_eq!(f.mgr.stop(&device("Pixel", 2)), Err(HandoffError::MoonlightActive));
        assert_eq!(*f.plane.ended.lock().unwrap(), 0, "the Moonlight session is untouched");
    }

    /// A keyframe request is the client's ONLY repair path under an infinite
    /// GOP, so it must reach the encoder — and it costs a full intra-coded
    /// frame, so a device that holds no session must not be able to demand one.
    #[test]
    fn only_the_owner_can_ask_for_a_keyframe() {
        let f = fixture(Some(peer()));

        // No session at all: nobody is the owner yet.
        assert_eq!(f.mgr.request_idr(&device("Xbox", 1)), Err(HandoffError::NotTheOwner));
        assert_eq!(*f.plane.idrs.lock().unwrap(), 0);

        f.mgr.start(&device("Xbox", 1), SessionRequest::default()).unwrap();

        assert_eq!(f.mgr.request_idr(&device("Pixel", 2)), Err(HandoffError::NotTheOwner));
        assert_eq!(
            *f.plane.idrs.lock().unwrap(),
            0,
            "an authenticated stranger must not be able to make the host encode keyframes"
        );

        assert!(f.mgr.request_idr(&device("Xbox", 1)).is_ok());
        assert_eq!(*f.plane.idrs.lock().unwrap(), 1);

        // Repeatable: recovery may need more than one attempt on a bad link.
        assert!(f.mgr.request_idr(&device("Xbox", 1)).is_ok());
        assert_eq!(*f.plane.idrs.lock().unwrap(), 2);

        f.mgr.stop(&device("Xbox", 1)).unwrap();
        assert_eq!(
            f.mgr.request_idr(&device("Xbox", 1)),
            Err(HandoffError::NotTheOwner),
            "a finished session grants no further keyframes"
        );
    }

    /// Input injection drives the host's real keyboard and mouse, so holding
    /// the session — not merely being paired — is the bar.
    #[test]
    fn only_the_owner_can_inject_input() {
        let f = fixture(Some(peer()));
        let packet = vec![0u8; 12];

        assert_eq!(
            f.mgr.inject_input(&device("Xbox", 1), packet.clone()),
            Err(HandoffError::NotTheOwner),
            "no session means no injection, even for a paired device"
        );

        f.mgr.start(&device("Xbox", 1), SessionRequest::default()).unwrap();

        assert_eq!(
            f.mgr.inject_input(&device("Pixel", 2), packet.clone()),
            Err(HandoffError::NotTheOwner)
        );
        assert!(
            f.plane.injected.lock().unwrap().is_empty(),
            "a stranger must never reach the injection stack"
        );

        assert!(f.mgr.inject_input(&device("Xbox", 1), packet.clone()).is_ok());
        assert_eq!(
            f.plane.injected.lock().unwrap().as_slice(),
            &[packet.clone()],
            "the packet must arrive byte-for-byte — the host parses it, not us"
        );

        f.mgr.stop(&device("Xbox", 1)).unwrap();
        assert_eq!(
            f.mgr.inject_input(&device("Xbox", 1), packet),
            Err(HandoffError::NotTheOwner),
            "a finished session injects nothing"
        );
    }

    /// The unreliable input path has no TLS connection behind it and no
    /// `EchoIdentity` to check, so the session key carries the entire
    /// authorization burden. These datagrams arrive on a socket anyone can
    /// write to and end at `SendInput` on a host whose Master runs as
    /// LocalSystem, which makes this the single most security-sensitive seam
    /// Echo has.
    #[test]
    fn only_the_session_key_can_inject_over_the_unreliable_path() {
        use nova_core::input_channel::InputSender;
        use nova_core::media_crypto::SessionKeys;

        let f = fixture(Some(peer()));
        let packet = vec![7u8; 12];

        // Nothing running: a well-formed datagram from nowhere injects nothing.
        let mut orphan = InputSender::new(SessionKeys::generate());
        let stray = orphan.datagrams(vec![packet.clone()]).unwrap().remove(0);
        assert_eq!(f.mgr.inject_sealed_input(&stray), Err(InputRejection::NoSession));

        let grant = f.mgr.start(&device("Xbox", 1), SessionRequest::default()).unwrap();
        let keys = SessionKeys::from_hex(&grant.keys_hex).expect("grant keys parse");

        // A different key — the shape an attacker on the punched path has.
        assert!(
            matches!(f.mgr.inject_sealed_input(&stray), Err(InputRejection::Unopenable(_))),
            "a foreign key must not reach the injector"
        );
        assert!(f.plane.injected.lock().unwrap().is_empty());

        // The granted key does.
        let mut owner = InputSender::new(keys);
        let datagram = owner.datagrams(vec![packet.clone()]).unwrap().remove(0);
        assert_eq!(f.mgr.inject_sealed_input(&datagram), Ok(1));
        assert_eq!(f.plane.injected.lock().unwrap().as_slice(), &[packet.clone()]);

        // Replaying it injects nothing a second time — the property that stops
        // an observer replaying a captured click.
        assert_eq!(f.mgr.inject_sealed_input(&datagram), Ok(0));
        assert_eq!(f.plane.injected.lock().unwrap().len(), 1);

        // A finished session takes no more input, even from its own key: the
        // receiver dies with the session rather than outliving it.
        f.mgr.stop(&device("Xbox", 1)).unwrap();
        let next = owner.datagrams(vec![packet]).unwrap().remove(0);
        assert_eq!(f.mgr.inject_sealed_input(&next), Err(InputRejection::NoSession));
    }

    /// A new session must not accept input sealed for the previous one, even
    /// for the same device — otherwise input queued during a reconnect could
    /// arrive against a session the user did not intend it for.
    #[test]
    fn input_sealed_for_a_previous_session_is_refused_by_the_next() {
        use nova_core::input_channel::InputSender;
        use nova_core::media_crypto::SessionKeys;

        let f = fixture(Some(peer()));
        let first = f.mgr.start(&device("Xbox", 1), SessionRequest::default()).unwrap();
        let mut stale =
            InputSender::new(SessionKeys::from_hex(&first.keys_hex).expect("keys parse"));
        let queued = stale.datagrams(vec![vec![9u8; 12]]).unwrap().remove(0);

        f.mgr.stop(&device("Xbox", 1)).unwrap();
        f.mgr.start(&device("Xbox", 1), SessionRequest::default()).unwrap();

        assert!(matches!(
            f.mgr.inject_sealed_input(&queued),
            Err(InputRejection::Unopenable(_))
        ));
        assert!(f.plane.injected.lock().unwrap().is_empty());
    }

    /// The tray's "End Stream" must reach an Echo session, and it must be able
    /// to say whether it ended anything.
    ///
    /// This is the bug the two-stage End Stream exists to fix: the Master
    /// judged "is anything streaming?" from `ClientInfo` alone, which describes
    /// the GameStream session and is empty during an Echo one — so with a phone
    /// mid-stream the tray reported "no active session — nothing to end" and
    /// the stream carried on (live log, 2026-08-16).
    #[test]
    fn the_host_can_end_an_echo_session_and_is_told_whether_it_did() {
        let f = fixture(Some(peer()));

        // Nothing running: the host must learn that, not silently succeed —
        // it is what lets the tray fall through to releasing the display.
        assert!(!f.mgr.force_end("tray", EndMode::KeepDisplay), "no session to end");
        assert_eq!(*f.plane.ended.lock().unwrap(), 0, "nothing to end means nothing ended");

        f.mgr.start(&device("Pixel", 1), SessionRequest::default()).unwrap();
        assert!(f.mgr.echo_holds_media());

        assert!(f.mgr.force_end("tray", EndMode::KeepDisplay), "a live session was ended");
        assert!(!f.mgr.echo_holds_media(), "the pipeline must be released for the next client");
        assert_eq!(
            f.plane.end_modes.lock().unwrap().as_slice(),
            &[EndMode::KeepDisplay],
            "the first press stops the stream but leaves the display up"
        );

        // And it stays ended — a second press has nothing left to stop, which
        // is precisely the signal the tray turns into "release the display".
        assert!(!f.mgr.force_end("tray", EndMode::TearDown));
        assert_eq!(*f.plane.ended.lock().unwrap(), 1, "the plane is not ended twice");
    }

    // ── 1C: detach, reap, reclaim ────────────────────────────────────────────

    /// Force a session into the detached state without waiting out a real
    /// timeout: rewind its liveness mark past the timeout and sweep.
    fn detach_now(f: &Fixture, idle_timeout: Duration) -> Sweep {
        {
            let mut guard = f.mgr.active.lock().unwrap();
            let session = guard.as_mut().expect("a session to detach");
            session.last_seen = session.last_seen.checked_sub(idle_timeout * 2).unwrap();
        }
        f.mgr.sweep(None, idle_timeout)
    }

    /// The core of 1C. A client that stops answering must stop costing the host
    /// an encoder and an uplink — but must NOT cost the user their desktop
    /// arrangement, which is the whole point of detaching rather than ending.
    #[test]
    fn a_silent_client_detaches_and_keeps_the_display() {
        let timeout = Duration::from_secs(30);
        let f = fixture(Some(peer()));
        f.mgr.start(&device("Pixel", 1), SessionRequest::default()).unwrap();
        assert!(f.mgr.echo_holds_media());

        assert_eq!(detach_now(&f, timeout), Sweep::Detached);

        // Encoding and transmission stopped, display held.
        assert_eq!(f.plane.end_modes.lock().unwrap().as_slice(), &[EndMode::KeepDisplay]);
        // The frame path must stop sealing at once: a detached session's client
        // is not listening, and the keys are about to be replaced.
        assert!(!f.mgr.echo_holds_media(), "a detached session must release the frame path");
        // ...which also unblocks Moonlight. A detached Echo session holding the
        // whole pipeline hostage against a client that IS present would be a
        // worse bug than the one this fixes.
        assert!(!f.mgr.echo_holds_media());

        // Sweeping again inside the grace window changes nothing — no repeated
        // `end` calls, no repeated log lines.
        assert_eq!(f.mgr.sweep(None, timeout), Sweep::Nothing);
        assert_eq!(*f.plane.ended.lock().unwrap(), 1, "the plane must not be ended twice");
    }

    /// Evidence of life keeps a session alive, and `None` is not evidence of
    /// death. `rtp.rs` reports only its most recent sender, so a single stray
    /// datagram from anywhere else erases the reading — treating that as a dead
    /// client would let a port scanner detach a healthy stream.
    #[test]
    fn an_unknown_idle_reading_never_detaches_a_healthy_session() {
        let timeout = Duration::from_secs(30);
        let f = fixture(Some(peer()));
        f.mgr.start(&device("Pixel", 1), SessionRequest::default()).unwrap();

        // Unknown, repeatedly, on a session that has just been seen.
        for _ in 0..100 {
            assert_eq!(f.mgr.sweep(None, timeout), Sweep::Nothing);
        }
        // A fresh media reading is proof of life.
        assert_eq!(f.mgr.sweep(Some(Duration::from_millis(500)), timeout), Sweep::Nothing);
        // A stale one is not, and detaches.
        assert_eq!(detach_now(&f, timeout), Sweep::Detached);
    }

    /// The grace period ends with the display coming back — and `0` means hold
    /// forever, exactly as it does on the Moonlight path. Getting that backwards
    /// would turn the opt-out into an instant teardown.
    #[test]
    fn a_detached_session_is_reaped_when_its_grace_expires() {
        let timeout = Duration::from_secs(30);

        // Grace 0: detached forever, never reaped.
        let f = Fixture::with_policy(
            Some(peer()),
            SessionPolicy { audio_reserve_kbps: 0, detach_grace: Duration::ZERO },
        );
        f.mgr.start(&device("Pixel", 1), SessionRequest::default()).unwrap();
        assert_eq!(detach_now(&f, timeout), Sweep::Detached);
        for _ in 0..100 {
            assert_eq!(f.mgr.sweep(None, timeout), Sweep::Nothing, "grace 0 must never reap");
        }
        assert_eq!(f.plane.end_modes.lock().unwrap().as_slice(), &[EndMode::KeepDisplay]);

        // A real grace period, already elapsed: the display comes back.
        let f = Fixture::with_policy(
            Some(peer()),
            SessionPolicy { audio_reserve_kbps: 0, detach_grace: Duration::from_millis(1) },
        );
        f.mgr.start(&device("Pixel", 1), SessionRequest::default()).unwrap();
        assert_eq!(detach_now(&f, timeout), Sweep::Detached);
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(f.mgr.sweep(None, timeout), Sweep::Reaped);
        assert_eq!(
            f.plane.end_modes.lock().unwrap().as_slice(),
            &[EndMode::KeepDisplay, EndMode::TearDown],
            "detach holds the display; the reap releases it"
        );
        // Reaped means gone: nothing further to sweep, and no second teardown.
        assert_eq!(f.mgr.sweep(None, timeout), Sweep::Nothing);
        assert_eq!(*f.plane.ended.lock().unwrap(), 2);
    }

    /// The cross-protocol hazard. `echo_active` goes false at detach precisely
    /// so a Moonlight client can claim the idle pipeline — but the detached
    /// session's grace clock is still running, and its expiry would otherwise
    /// send a cancelling Deactivate that tears down THAT session. From the
    /// operator's chair it would look like a Moonlight stream dying at random,
    /// ten minutes after an unrelated phone lost signal.
    #[test]
    fn reaping_a_detached_session_never_ends_a_moonlight_stream() {
        let timeout = Duration::from_secs(30);
        let f = Fixture::with_policy(
            Some(peer()),
            SessionPolicy { audio_reserve_kbps: 0, detach_grace: Duration::from_millis(1) },
        );
        f.mgr.start(&device("Pixel", 1), SessionRequest::default()).unwrap();
        assert_eq!(detach_now(&f, timeout), Sweep::Detached);

        // Moonlight takes the pipeline while the phone is away.
        *f.client_info.lock().unwrap() =
            Some(ClientInfo { streaming_active: true, ..Default::default() });

        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(f.mgr.sweep(None, timeout), Sweep::Reaped, "the record is still forgotten");
        assert_eq!(
            f.plane.end_modes.lock().unwrap().as_slice(),
            &[EndMode::KeepDisplay],
            "the reap must NOT end the plane while Moonlight is streaming"
        );
    }

    /// Reclaiming is the fast path: nothing is ended, because nothing is
    /// running and the display being reused is the one an `end` would destroy.
    /// Fresh keys are minted regardless — a session's keys are never reused,
    /// which is what stops a datagram sealed for the old session from being
    /// replayed into the new one.
    #[test]
    fn the_owner_reclaims_a_detached_session_without_touching_the_display() {
        let timeout = Duration::from_secs(30);
        let f = fixture(Some(peer()));
        let pixel = device("Pixel", 1);
        let first = f.mgr.start(&pixel, SessionRequest::default()).unwrap();
        assert_eq!(detach_now(&f, timeout), Sweep::Detached);
        let ends_after_detach = *f.plane.ended.lock().unwrap();

        let second = f.mgr.start(&pixel, SessionRequest::default()).expect("owner may reclaim");

        assert_eq!(
            *f.plane.ended.lock().unwrap(), ends_after_detach,
            "reclaiming must not end the plane — that would tear down the display it is reusing"
        );
        assert_ne!(second.keys_hex, first.keys_hex, "every session mints fresh keys");
        assert_ne!(second.session_id, first.session_id);
        assert!(f.mgr.echo_holds_media(), "the frame path is live again");
        assert_eq!(f.plane.begun.lock().unwrap().len(), 2);
    }

    /// A detached session must not lock every other paired device out of the
    /// host for the whole grace period. It is holding a display nobody is
    /// watching, on behalf of a device that is not answering — while a LIVE
    /// session is still defended, which is the distinction that matters.
    #[test]
    fn another_device_may_take_over_a_detached_session_but_not_a_live_one() {
        let timeout = Duration::from_secs(30);
        let f = fixture(Some(peer()));
        f.mgr.start(&device("Pixel", 1), SessionRequest::default()).unwrap();

        // Live: defended.
        let err = f
            .mgr
            .start(&device("Tablet", 2), SessionRequest::default())
            .expect_err("a live session belongs to its owner");
        assert!(matches!(err, HandoffError::HeldByAnotherDevice { .. }));

        // Detached: available.
        assert_eq!(detach_now(&f, timeout), Sweep::Detached);
        let taken = f.mgr.start(&device("Tablet", 2), SessionRequest::default());
        assert!(taken.is_ok(), "a detached session may be taken over: {taken:?}");
        assert!(f.mgr.echo_holds_media());
    }

    /// A client that disconnects is done with the desktop; only the host's
    /// first press is the "stop the stream, keep the display" case. Asserted
    /// because the two callers sit three lines apart and the wrong mode here
    /// would either strand the VDD after every client quit or make the tray's
    /// two-stage press collapse into one.
    #[test]
    fn a_client_ending_its_own_session_tears_the_display_down() {
        let f = fixture(Some(peer()));
        f.mgr.start(&device("Pixel", 1), SessionRequest::default()).unwrap();
        f.mgr.stop(&device("Pixel", 1)).expect("owner may stop");
        assert_eq!(f.plane.end_modes.lock().unwrap().as_slice(), &[EndMode::TearDown]);
    }

    #[test]
    fn requests_are_validated_before_any_state_is_consulted() {
        let f = fixture(None); // no path — but validation must fail first
        let err = f
            .mgr
            .start(
                &device("Xbox", 1),
                SessionRequest { width: 100, height: 100, ..Default::default() },
            )
            .expect_err("invalid geometry");
        assert!(matches!(err, HandoffError::BadRequest(_)));

        for bad in [
            SessionRequest { fps: 1000, ..Default::default() },
            SessionRequest { bitrate_kbps: 1, ..Default::default() },
            SessionRequest { width: 99_999, height: 4320, ..Default::default() },
        ] {
            assert!(matches!(
                f.mgr.start(&device("Xbox", 1), bad),
                Err(HandoffError::BadRequest(_))
            ));
        }
    }

    /// The same H.264 Level 5.2 ceiling `session_negotiate` applies to
    /// Moonlight sessions: exceeding it crashes client decoders rather than
    /// degrading them, so the cap belongs on every path that reaches NVENC.
    #[test]
    fn h264_above_1080p_is_capped_to_60fps() {
        let f = fixture(Some(peer()));
        let grant = f
            .mgr
            .start(
                &device("Xbox", 1),
                SessionRequest {
                    width: 3840,
                    height: 2160,
                    fps: 120,
                    codec: Codec::H264,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(grant.params.fps, 60);

        // HEVC has no such limit and must not be capped.
        f.mgr.stop(&device("Xbox", 1)).unwrap();
        let grant = f
            .mgr
            .start(
                &device("Xbox", 1),
                SessionRequest {
                    width: 3840,
                    height: 2160,
                    fps: 120,
                    codec: Codec::Hevc,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(grant.params.fps, 120);
    }

    /// The MTU claim, asserted rather than reasoned about in a comment.
    ///
    /// Fragmented UDP is the failure mode being avoided: a lost fragment
    /// discards the whole datagram, so an over-MTU video packet turns ordinary
    /// loss into shard loss on exactly the links least able to absorb it.
    #[test]
    fn echo_datagrams_fit_the_wan_mtu() {
        assert!(
            ECHO_DATAGRAM_SIZE <= WAN_MTU_BUDGET,
            "an Echo datagram is {ECHO_DATAGRAM_SIZE} bytes, over the {WAN_MTU_BUDGET} budget"
        );
        // The demux tag replaces byte 0 rather than being prepended, so it must
        // cost nothing at all. If someone later makes it a real prefix, this
        // assertion is what should stop them silently eating the headroom.
        assert_eq!(
            ECHO_DATAGRAM_SIZE,
            WAN_PACKET_SIZE as usize + 16,
            "the demux tag must not add bytes to a datagram"
        );
        // Control shares the same budget and must also fit unfragmented.
        assert!(nova_core::rudp::MAX_PAYLOAD + nova_core::rudp::HEADER_LEN <= WAN_MTU_BUDGET);
    }

    /// The GCM tag is per frame, so it can only ever change how many shards a
    /// frame needs — never how big a datagram is. Worth pinning down, because
    /// "the tag adds 16 bytes" invites exactly the opposite assumption.
    #[test]
    fn the_gcm_tag_costs_shards_not_datagram_size() {
        let keys = SessionKeys::generate();
        let frame = vec![0u8; 4000];
        let sealed = keys.seal(STREAM_VIDEO, 1, 2, &frame);
        assert_eq!(sealed.len(), frame.len() + 16);

        let payload_per_packet = WAN_PACKET_SIZE as usize + 16 - 32;
        let shards_plain = (frame.len() + 8).div_ceil(payload_per_packet);
        let shards_sealed = (sealed.len() + 8).div_ceil(payload_per_packet);
        assert!(
            shards_sealed - shards_plain <= 1,
            "sealing may cost at most one extra shard per frame"
        );
    }

    /// The frame path asks this question up to 120 times a second on a stream
    /// that is usually Moonlight's, so "no session" must be both correct and
    /// free of the session lock.
    #[test]
    fn sealing_is_a_no_op_without_an_echo_session() {
        let f = fixture(Some(peer()));
        assert!(f.mgr.seal_video(1, 2, b"moonlight frame").is_none());

        f.mgr.start(&device("Xbox", 1), SessionRequest::default()).unwrap();
        let sealed = f.mgr.seal_video(1, 2, b"echo frame").expect("sealed while active");
        assert_ne!(sealed, b"echo frame");

        f.mgr.stop(&device("Xbox", 1)).unwrap();
        assert!(
            f.mgr.seal_video(2, 2, b"moonlight again").is_none(),
            "a finished session must stop sealing immediately"
        );
    }

    /// Audio sealing follows video's gate exactly: silent for Moonlight, active
    /// for Echo, and stops the instant the session does.
    #[test]
    fn audio_sealing_is_a_no_op_without_an_echo_session() {
        let f = fixture(Some(peer()));
        assert!(f.mgr.seal_audio(b"moonlight opus").is_none());

        f.mgr.start(&device("Xbox", 1), SessionRequest::default()).unwrap();
        let (datagram, to) = f.mgr.seal_audio(b"echo opus").expect("sealed while active");
        assert_eq!(datagram[0], nova_core::demux::ECHO_AUDIO);
        assert_eq!(to, peer(), "audio must go to the session's punched peer");
        assert!(
            !datagram.windows(9).any(|w| w == b"echo opus"),
            "the payload must not appear in the clear"
        );

        f.mgr.stop(&device("Xbox", 1)).unwrap();
        assert!(f.mgr.seal_audio(b"after the end").is_none());
    }

    /// The property the separate counter exists for, asserted end-to-end rather
    /// than trusted: audio numbering must start at 1 and advance by 1 per packet
    /// **regardless of how many video frames were sealed in between**. If the two
    /// ever shared a counter, interleaving video would punch gaps in the audio
    /// sequence, and the client would read those gaps as packet loss and conceal
    /// audio that was never lost.
    #[test]
    fn audio_sequence_is_independent_of_the_video_wire() {
        let f = fixture(Some(peer()));
        let grant = f.mgr.start(&device("Xbox", 1), SessionRequest::default()).unwrap();

        // The client's own view: opened with the keys it was granted, so this
        // exercises the real cross-process contract rather than internal state.
        let keys = SessionKeys::from_hex(&grant.keys_hex).expect("grant keys parse");
        let mut rx = nova_core::audio_channel::AudioReceiver::new(keys);

        for wire_index in 1..=10u32 {
            // A burst of video between every audio packet — the realistic ratio
            // is roughly two video frames per 20 ms audio packet.
            f.mgr.seal_video(wire_index, 1, b"a video frame").expect("echo session seals");
            f.mgr.seal_video(wire_index + 100, 1, b"another").expect("echo session seals");

            let (datagram, _) = f.mgr.seal_audio(b"opus packet").expect("sealed");
            let opened = rx.open(&datagram).expect("opens").expect("is new");
            assert_eq!(
                opened.seq, wire_index,
                "audio sequence must count audio packets, not video frames"
            );
        }

        let stats = rx.stats();
        assert_eq!(stats.accepted, 10);
        assert_eq!(stats.lost(rx.highest_sequence()), 0, "a gap here is invented loss");
        assert_eq!(stats.reordered, 0);
    }

    /// A failure inside the media plane must not leave a phantom session
    /// recorded — the manager would then refuse every future start while
    /// nothing was actually streaming.
    #[test]
    fn a_plane_failure_leaves_no_session_behind() {
        let plane = Arc::new(MockPlane {
            fail: Some(HandoffError::WorkerUnavailable("no worker".into())),
            ..Default::default()
        });
        let mgr = SessionManager::new(
            Arc::new(plane.clone()),
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(Some(peer()))),
            SessionPolicy { audio_reserve_kbps: 0, detach_grace: Duration::from_secs(600) },
        );

        assert!(mgr.start(&device("Xbox", 1), SessionRequest::default()).is_err());
        assert_eq!(mgr.owner(), MediaOwner::Idle);
        assert!(!mgr.echo_holds_media());
    }
}
