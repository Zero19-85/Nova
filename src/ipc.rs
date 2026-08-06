//! Master↔Worker IPC (Phase 1 skeleton of the Session-Survival Architecture —
//! see the approved plan, `transient-snuggling-cosmos.md`).
//!
//! Two named pipes, byte-mode, hand-rolled `[u32 LE length][u8 tag][payload]`
//! framing — matches this codebase's existing wire-protocol style (RTP,
//! pairing, control are all hand-rolled binary already; no serde-for-wire
//! dependency added here):
//!   - `control`: bidirectional, small/latency-critical (configure/stop/
//!     input/idr/congestion/PIN-relay/ready-status). Phase 1 only wires
//!     `WorkerReady`/`WorkerError`/`Stop` — the rest of `ControlMsg` grows in
//!     Phase 2 once the network layer actually moves.
//!   - `media`: Worker → Master only, large/bursty (encoded video/audio
//!     frames). Kept on its own pipe so a multi-MB HDR IDR frame in flight
//!     never head-of-line-blocks an input-injection packet on the control
//!     pipe — the same class of jitter Phase 15.4 already fixed once, by
//!     splitting RTP send onto its own thread. Not sent over yet in Phase 1
//!     (no real streaming); the transport is proven now so Phase 2/3 only
//!     have to wire producers/consumers, not build the pipe itself.
//!
//! Fixed pipe names, not per-boot-unique: only one Worker is ever alive at a
//! time, so a fresh server instance replacing a torn-down one is exactly the
//! same staleness story `service.rs`'s `Global\NovaHostShutdown` already
//! handles for its own named event.
//!
//! Security: `reject_remote_clients(true)` blocks SMB/network reachability.
//! A per-spawn-token-scoped DACL (rather than the Windows default SD, which
//! for a SYSTEM-created object generally admits the interactive user tier
//! too) is real hardening work still owed — tracked for Phase 4, not done
//! here. Phase 1's job is proving the transport; don't let that scope creep
//! into ACL design before the message protocol itself is even validated.

use std::io;
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer, PipeMode, ServerOptions};

pub const CONTROL_PIPE_NAME: &str = r"\\.\pipe\NovaControl";
pub const MEDIA_PIPE_NAME: &str = r"\\.\pipe\NovaMedia";
/// Master → **SYSTEM input helper** (`--system-input-helper`), created only
/// while the secure desktop is up. Separate from the control pipe because the
/// peer is a different, short-lived process with a different token — see
/// `input.rs`'s UIPI note for why that process has to exist at all.
pub const INPUT_PIPE_NAME: &str = r"\\.\pipe\NovaInput";

/// How long the Worker retries connecting before giving up. Generous: right
/// after spawn the Worker may win the race against Master finishing pipe
/// creation, and — same reasoning as `service.rs`'s NVENC-respawn-backoff
/// note in the approved plan — a slow retry beats a tight spin loop.
const CONNECT_RETRY_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_RETRY_INTERVAL: Duration = Duration::from_millis(50);

/// How long Master waits for the Worker to connect after creating the pipes.
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long Master waits for the SYSTEM input helper — see
/// [`accept_input_helper`] for why this is far shorter than [`ACCEPT_TIMEOUT`].
const INPUT_HELPER_ACCEPT_TIMEOUT: Duration = Duration::from_secs(3);

// ── Wire format ─────────────────────────────────────────────────────────────

mod tag {
    pub const WORKER_READY: u8 = 1;
    pub const WORKER_ERROR: u8 = 2;
    pub const STOP: u8 = 3;
    pub const CONFIGURE_START: u8 = 4;
    pub const WORKER_CONFIGURED: u8 = 5;
    pub const INJECT_INPUT: u8 = 6;
    pub const REQUEST_IDR: u8 = 7;
    pub const CONGESTION_REDUCE: u8 = 8;
    pub const PIN_RELAY: u8 = 9;
    pub const DEACTIVATE: u8 = 12;
    pub const OPEN_PAIR_DIALOG: u8 = 13;
    pub const SECURE_DESKTOP_CHANGED: u8 = 14;
    pub const VIDEO_FRAME: u8 = 10;
    pub const AUDIO_FRAME: u8 = 11;
}

/// Codec as it crosses the wire — mirrors `encoder::Codec` (can't reuse that
/// type directly without giving `encoder.rs` a dependency on `ipc.rs`'s wire
/// format; this module stays the one place that knows the byte encoding).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireCodec {
    H264,
    Hevc,
    Av1,
}

impl WireCodec {
    fn to_byte(self) -> u8 {
        match self {
            WireCodec::H264 => 0,
            WireCodec::Hevc => 1,
            WireCodec::Av1 => 2,
        }
    }
    fn from_byte(b: u8) -> io::Result<Self> {
        match b {
            0 => Ok(WireCodec::H264),
            1 => Ok(WireCodec::Hevc),
            2 => Ok(WireCodec::Av1),
            other => Err(invalid_data(&format!("unknown WireCodec byte {other}"))),
        }
    }
}

/// Master -> Worker: everything needed to activate the VDD, (re)configure the
/// encoder, and start streaming — the payload of
/// `session_negotiate::NegotiatedParams` plus the passthrough fields the
/// Worker needs that aren't part of the negotiation decision itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigureStart {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub codec: WireCodec,
    pub hdr_confirmed: bool,
    pub bitrate_kbps: u32,
    pub app_id: u32,
    /// Session came from `/launch` — Worker starts the app's process (in the
    /// user session, after VDD activation) exactly once per session.
    pub launch_app: bool,
    pub device_name: String,
    pub rikey: [u8; 16],
    pub rikeyid: u32,
    pub host_audio: bool,
    pub audio_encryption: bool,
    pub audio_packet_duration_ms: u32,
    pub packet_size: u32,
    pub min_fec_packets: u32,
}

/// Worker -> Master: the ACTUAL params the encoder ended up running, once
/// `ConfigureStart` has been applied. Master's `RtpSender` must configure
/// against this, not against the `ConfigureStart` it sent — see the approved
/// plan's note on why the negotiated-params handoff has to be two-way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerConfigured {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub codec: WireCodec,
    pub is_hdr: bool,
}

/// Control-plane messages. Both directions share one enum — the message set
/// is small enough that splitting Master→Worker from Worker→Master isn't
/// worth the ceremony; each variant's doc comment says which way it flows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlMsg {
    /// Worker -> Master: connected and ready for a `ConfigureStart`.
    WorkerReady,
    /// Worker -> Master: something went wrong; human-readable reason.
    WorkerError(String),
    /// Master -> Worker: shut down cleanly.
    Stop,
    /// Master -> Worker: activate/reconfigure and start streaming.
    ConfigureStart(ConfigureStart),
    /// Worker -> Master: `ConfigureStart` applied; these are the real values
    /// (may differ from what was requested if e.g. a resolution re-snap
    /// landed somewhere else) — see [`WorkerConfigured`].
    WorkerConfigured(WorkerConfigured),
    /// Master -> Worker: raw Moonlight INPUT_DATA payload (the same bytes
    /// `control.rs` used to pass straight to `input::handle_input_packet`).
    InjectInput(Vec<u8>),
    /// Master -> Worker: client sent PT_REQUEST_IDR_FRAME /
    /// PT_INVALIDATE_REF_FRAMES.
    RequestIdr,
    /// Master -> Worker: client sent PT_LOSS_STATS with loss > 0.
    CongestionReduce,
    /// Worker -> Master: PIN + device name entered on the Worker's tray
    /// dialog, forwarded into Master-side pairing's `global_pin` slot (the
    /// same handshake point the monolithic host's tray uses in-process).
    /// The Worker sends this whenever its tray dialog completes — whether
    /// the dialog was opened by [`ControlMsg::OpenPairDialog`] or manually
    /// from the tray menu.
    PinRelay { pin: String, device: String },
    /// Worker -> Master: the input desktop crossed the secure/interactive
    /// boundary. Only the Worker can observe this (its `desktop_switch`
    /// monitor runs in the console session; Master is in Session 0, where
    /// `OpenInputDesktop` reports Session 0's own desktops and is therefore
    /// useless for this). Master reacts by starting/stopping the SYSTEM input
    /// helper and re-routing `InjectInput` to it — see `input.rs`'s UIPI note.
    SecureDesktopChanged { secure: bool },
    /// Master -> Worker: a pairing request arrived (`getservercert`) and the
    /// Master is blocked waiting for a PIN — pop the tray's pair dialog in
    /// the user session. The Master is headless in Session 0 and cannot show
    /// UI; the Worker's tray is the only session-visible surface left.
    OpenPairDialog,
    /// Master -> Worker: the client's session ended (disconnect, RTSP
    /// TEARDOWN, or /cancel) — deactivate VDD/audio/encoder WITHOUT exiting
    /// the process, so the Worker stays alive and ready for the next
    /// ConfigureStart. `cancelled` mirrors `ClientInfo::cancelled`: true =
    /// the user quit the app (full VDD teardown, monitor restored to
    /// primary); false = a bare disconnect (VDD stays at its current
    /// resolution for a fast /resume reconnect). Without this message the
    /// Worker had no way to learn a session ended short of the whole
    /// process dying — confirmed live (2026-07-20): closing the app left
    /// the virtual display primary and the physical monitor dark.
    Deactivate { cancelled: bool },
}

fn write_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn read_u32(bytes: &[u8], at: &mut usize) -> io::Result<u32> {
    let end = *at + 4;
    let slice = bytes.get(*at..end).ok_or_else(|| invalid_data("truncated u32"))?;
    *at = end;
    Ok(u32::from_le_bytes(slice.try_into().unwrap()))
}
fn write_string(out: &mut Vec<u8>, s: &str) {
    write_u32(out, s.len() as u32);
    out.extend_from_slice(s.as_bytes());
}
fn read_string(bytes: &[u8], at: &mut usize) -> io::Result<String> {
    let len = read_u32(bytes, at)? as usize;
    let end = *at + len;
    let slice = bytes.get(*at..end).ok_or_else(|| invalid_data("truncated string"))?;
    *at = end;
    Ok(String::from_utf8_lossy(slice).into_owned())
}

impl ConfigureStart {
    fn encode_into(&self, out: &mut Vec<u8>) {
        write_u32(out, self.width);
        write_u32(out, self.height);
        write_u32(out, self.fps);
        out.push(self.codec.to_byte());
        out.push(self.hdr_confirmed as u8);
        write_u32(out, self.bitrate_kbps);
        write_u32(out, self.app_id);
        out.push(self.launch_app as u8);
        write_string(out, &self.device_name);
        out.extend_from_slice(&self.rikey);
        write_u32(out, self.rikeyid);
        out.push(self.host_audio as u8);
        out.push(self.audio_encryption as u8);
        write_u32(out, self.audio_packet_duration_ms);
        write_u32(out, self.packet_size);
        write_u32(out, self.min_fec_packets);
    }

    fn decode(bytes: &[u8]) -> io::Result<Self> {
        let at = &mut 0usize;
        let width = read_u32(bytes, at)?;
        let height = read_u32(bytes, at)?;
        let fps = read_u32(bytes, at)?;
        let codec = WireCodec::from_byte(*bytes.get(*at).ok_or_else(|| invalid_data("truncated ConfigureStart"))?)?;
        *at += 1;
        let hdr_confirmed = *bytes.get(*at).ok_or_else(|| invalid_data("truncated ConfigureStart"))? != 0;
        *at += 1;
        let bitrate_kbps = read_u32(bytes, at)?;
        let app_id = read_u32(bytes, at)?;
        let launch_app = *bytes.get(*at).ok_or_else(|| invalid_data("truncated ConfigureStart"))? != 0;
        *at += 1;
        let device_name = read_string(bytes, at)?;
        let rikey_slice = bytes.get(*at..*at + 16).ok_or_else(|| invalid_data("truncated rikey"))?;
        let mut rikey = [0u8; 16];
        rikey.copy_from_slice(rikey_slice);
        *at += 16;
        let rikeyid = read_u32(bytes, at)?;
        let host_audio = *bytes.get(*at).ok_or_else(|| invalid_data("truncated ConfigureStart"))? != 0;
        *at += 1;
        let audio_encryption = *bytes.get(*at).ok_or_else(|| invalid_data("truncated ConfigureStart"))? != 0;
        *at += 1;
        let audio_packet_duration_ms = read_u32(bytes, at)?;
        let packet_size = read_u32(bytes, at)?;
        let min_fec_packets = read_u32(bytes, at)?;
        Ok(ConfigureStart {
            width, height, fps, codec, hdr_confirmed, bitrate_kbps, app_id, launch_app,
            device_name, rikey, rikeyid, host_audio, audio_encryption,
            audio_packet_duration_ms, packet_size, min_fec_packets,
        })
    }
}

impl WorkerConfigured {
    fn encode_into(&self, out: &mut Vec<u8>) {
        write_u32(out, self.width);
        write_u32(out, self.height);
        write_u32(out, self.fps);
        out.push(self.codec.to_byte());
        out.push(self.is_hdr as u8);
    }

    fn decode(bytes: &[u8]) -> io::Result<Self> {
        let at = &mut 0usize;
        let width = read_u32(bytes, at)?;
        let height = read_u32(bytes, at)?;
        let fps = read_u32(bytes, at)?;
        let codec = WireCodec::from_byte(*bytes.get(*at).ok_or_else(|| invalid_data("truncated WorkerConfigured"))?)?;
        *at += 1;
        let is_hdr = *bytes.get(*at).ok_or_else(|| invalid_data("truncated WorkerConfigured"))? != 0;
        Ok(WorkerConfigured { width, height, fps, codec, is_hdr })
    }
}

impl ControlMsg {
    fn encode(&self) -> Vec<u8> {
        match self {
            ControlMsg::WorkerReady => vec![tag::WORKER_READY],
            ControlMsg::WorkerError(reason) => {
                let mut out = vec![tag::WORKER_ERROR];
                out.extend_from_slice(reason.as_bytes());
                out
            }
            ControlMsg::Stop => vec![tag::STOP],
            ControlMsg::ConfigureStart(cs) => {
                let mut out = vec![tag::CONFIGURE_START];
                cs.encode_into(&mut out);
                out
            }
            ControlMsg::WorkerConfigured(wc) => {
                let mut out = vec![tag::WORKER_CONFIGURED];
                wc.encode_into(&mut out);
                out
            }
            ControlMsg::InjectInput(payload) => {
                let mut out = Vec::with_capacity(1 + payload.len());
                out.push(tag::INJECT_INPUT);
                out.extend_from_slice(payload);
                out
            }
            ControlMsg::RequestIdr => vec![tag::REQUEST_IDR],
            ControlMsg::CongestionReduce => vec![tag::CONGESTION_REDUCE],
            ControlMsg::PinRelay { pin, device } => {
                let mut out = vec![tag::PIN_RELAY];
                write_string(&mut out, pin);
                write_string(&mut out, device);
                out
            }
            ControlMsg::Deactivate { cancelled } => vec![tag::DEACTIVATE, *cancelled as u8],
            ControlMsg::OpenPairDialog => vec![tag::OPEN_PAIR_DIALOG],
            ControlMsg::SecureDesktopChanged { secure } => {
                vec![tag::SECURE_DESKTOP_CHANGED, *secure as u8]
            }
        }
    }

    fn decode(bytes: &[u8]) -> io::Result<Self> {
        let (&t, rest) = bytes
            .split_first()
            .ok_or_else(|| invalid_data("empty ControlMsg"))?;
        match t {
            tag::WORKER_READY => Ok(ControlMsg::WorkerReady),
            tag::WORKER_ERROR => Ok(ControlMsg::WorkerError(String::from_utf8_lossy(rest).into_owned())),
            tag::STOP => Ok(ControlMsg::Stop),
            tag::CONFIGURE_START => Ok(ControlMsg::ConfigureStart(ConfigureStart::decode(rest)?)),
            tag::WORKER_CONFIGURED => Ok(ControlMsg::WorkerConfigured(WorkerConfigured::decode(rest)?)),
            tag::INJECT_INPUT => Ok(ControlMsg::InjectInput(rest.to_vec())),
            tag::REQUEST_IDR => Ok(ControlMsg::RequestIdr),
            tag::CONGESTION_REDUCE => Ok(ControlMsg::CongestionReduce),
            tag::PIN_RELAY => {
                let at = &mut 0usize;
                let pin = read_string(rest, at)?;
                let device = read_string(rest, at)?;
                Ok(ControlMsg::PinRelay { pin, device })
            }
            tag::DEACTIVATE => {
                let cancelled = rest.first().copied().unwrap_or(0) != 0;
                Ok(ControlMsg::Deactivate { cancelled })
            }
            tag::OPEN_PAIR_DIALOG => Ok(ControlMsg::OpenPairDialog),
            tag::SECURE_DESKTOP_CHANGED => Ok(ControlMsg::SecureDesktopChanged {
                secure: rest.first().copied().unwrap_or(0) != 0,
            }),
            other => Err(invalid_data(&format!("unknown ControlMsg tag {other}"))),
        }
    }
}

/// Media-plane messages (Worker -> Master only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaMsg {
    /// `frame_type`: 2 = IDR/keyframe, 1 = P-frame (matches `rtp::send_frame`'s
    /// existing convention — reused as-is, not redefined).
    VideoFrame { frame_type: u8, data: Vec<u8> },
    /// Raw Opus-encoded audio, pre-encryption. No seq/timestamp on the wire —
    /// Master's `audio::AudioTxState` owns those independently (mirrors
    /// `RtpSender` owning `frame_index` for video instead of trusting
    /// whatever a respawned Worker thinks its own count is).
    AudioFrame { data: Vec<u8> },
}

impl MediaMsg {
    fn encode(&self) -> Vec<u8> {
        match self {
            MediaMsg::VideoFrame { frame_type, data } => {
                let mut out = Vec::with_capacity(2 + data.len());
                out.push(tag::VIDEO_FRAME);
                out.push(*frame_type);
                out.extend_from_slice(data);
                out
            }
            MediaMsg::AudioFrame { data } => {
                let mut out = Vec::with_capacity(1 + data.len());
                out.push(tag::AUDIO_FRAME);
                out.extend_from_slice(data);
                out
            }
        }
    }

    fn decode(bytes: &[u8]) -> io::Result<Self> {
        let (&t, rest) = bytes
            .split_first()
            .ok_or_else(|| invalid_data("empty MediaMsg"))?;
        match t {
            tag::VIDEO_FRAME => {
                let (&frame_type, data) = rest
                    .split_first()
                    .ok_or_else(|| invalid_data("truncated VideoFrame"))?;
                Ok(MediaMsg::VideoFrame { frame_type, data: data.to_vec() })
            }
            tag::AUDIO_FRAME => Ok(MediaMsg::AudioFrame { data: rest.to_vec() }),
            other => Err(invalid_data(&format!("unknown MediaMsg tag {other}"))),
        }
    }
}

fn invalid_data(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

/// 32 MB guard: a corrupt/desynced length prefix must never wedge the reader
/// into allocating an unbounded buffer or blocking forever on `read_exact`.
const MAX_FRAME_LEN: u32 = 32 * 1024 * 1024;

async fn write_frame(io: &mut (impl AsyncWrite + Unpin), payload: &[u8]) -> io::Result<()> {
    io.write_all(&(payload.len() as u32).to_le_bytes()).await?;
    io.write_all(payload).await?;
    Ok(())
}

async fn read_frame(io: &mut (impl AsyncRead + Unpin)) -> io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    io.read_exact(&mut len_buf).await?;
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME_LEN {
        return Err(invalid_data(&format!("IPC frame length {len} exceeds {MAX_FRAME_LEN}")));
    }
    let mut buf = vec![0u8; len as usize];
    io.read_exact(&mut buf).await?;
    Ok(buf)
}

pub async fn send_control(io: &mut (impl AsyncWrite + Unpin), msg: &ControlMsg) -> io::Result<()> {
    write_frame(io, &msg.encode()).await
}

pub async fn recv_control(io: &mut (impl AsyncRead + Unpin)) -> io::Result<ControlMsg> {
    ControlMsg::decode(&read_frame(io).await?)
}

pub async fn send_media(io: &mut (impl AsyncWrite + Unpin), msg: &MediaMsg) -> io::Result<()> {
    write_frame(io, &msg.encode()).await
}

pub async fn recv_media(io: &mut (impl AsyncRead + Unpin)) -> io::Result<MediaMsg> {
    MediaMsg::decode(&read_frame(io).await?)
}

// ── Pipe lifecycle ──────────────────────────────────────────────────────────

fn server_options() -> ServerOptions {
    let mut opts = ServerOptions::new();
    opts.pipe_mode(PipeMode::Byte).reject_remote_clients(true);
    opts
}

/// (Master side) Create both pipe servers. Must be called BEFORE spawning the
/// Worker process — the child's connect attempt races pipe creation
/// otherwise (mitigated further by the Worker's own retry loop, but starting
/// the server first avoids relying on that retry in the common case).
pub fn create_master_pipes() -> io::Result<(NamedPipeServer, NamedPipeServer)> {
    let control = server_options().create(CONTROL_PIPE_NAME)?;
    let media = server_options().create(MEDIA_PIPE_NAME)?;
    Ok((control, media))
}

/// (Master side) Block until the Worker connects to both pipes, or time out.
pub async fn accept_worker(control: &NamedPipeServer, media: &NamedPipeServer) -> io::Result<()> {
    tokio::time::timeout(ACCEPT_TIMEOUT, async {
        control.connect().await?;
        media.connect().await
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "worker did not connect to both IPC pipes in time"))?
}

/// (Master side) Create the input-helper pipe server. Created fresh for each
/// secure-desktop interlude and dropped when it ends, so a stale helper can
/// never hold the name — the same create-per-peer model as the Worker pipes.
pub fn create_input_pipe() -> io::Result<NamedPipeServer> {
    server_options().create(INPUT_PIPE_NAME)
}

/// (Master side) Wait for the SYSTEM input helper to connect. Much shorter
/// than [`ACCEPT_TIMEOUT`]: the helper is a bare process with no VDD/capture
/// bring-up, and a user is waiting to type their PIN — if it hasn't connected
/// by now, fall back to Worker-side injection rather than stalling input.
pub async fn accept_input_helper(server: &NamedPipeServer) -> io::Result<()> {
    tokio::time::timeout(INPUT_HELPER_ACCEPT_TIMEOUT, server.connect())
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "SYSTEM input helper did not connect in time",
            )
        })?
}

/// (Input-helper side) Connect to Master's input pipe.
pub async fn connect_to_master_input() -> io::Result<NamedPipeClient> {
    open_with_retry(INPUT_PIPE_NAME).await
}

/// (Worker side) Connect to both pipes Master has already created, retrying
/// briefly if Master hasn't finished creating them yet.
pub async fn connect_to_master() -> io::Result<(NamedPipeClient, NamedPipeClient)> {
    let control = open_with_retry(CONTROL_PIPE_NAME).await?;
    let media = open_with_retry(MEDIA_PIPE_NAME).await?;
    Ok((control, media))
}

async fn open_with_retry(name: &str) -> io::Result<NamedPipeClient> {
    let deadline = Instant::now() + CONNECT_RETRY_TIMEOUT;
    let mut client_opts = ClientOptions::new();
    client_opts.pipe_mode(PipeMode::Byte);
    loop {
        match client_opts.open(name) {
            Ok(client) => return Ok(client),
            Err(e) if Instant::now() < deadline => {
                tokio::time::sleep(CONNECT_RETRY_INTERVAL).await;
                let _ = e; // transient (pipe not created yet / busy) — keep retrying
            }
            Err(e) => return Err(e),
        }
    }
}

// ── Master-side bridge for synchronous callers (control.rs) ────────────────

/// Cheap, cloneable handle letting `control.rs`'s ENet thread (synchronous —
/// no tokio context of its own) reach whichever Worker is currently
/// connected, without needing to know about Worker spawn/respawn lifecycle
/// itself. Backed by an unbounded channel into `lib.rs`'s `control_supervisor`
/// task, which is the SOLE owner of the actual pipe (both directions —
/// reading `WorkerConfigured`/`WorkerError`/`PinRelay` replies as well as
/// writing) and adopts a fresh pipe each time `service.rs` hands one off
/// after a successful `WorkerReady` handshake. Routing sends through a
/// channel rather than a shared `Mutex<Option<NamedPipeServer>>` means this
/// type never touches tokio I/O directly — `send` is a plain, non-blocking,
/// non-async call, safe from any thread.
#[derive(Clone)]
pub struct WorkerLink {
    tx: tokio::sync::mpsc::UnboundedSender<ControlMsg>,
}

impl WorkerLink {
    /// Returns the link plus the receiver `lib.rs`'s `control_supervisor`
    /// task consumes for as long as Master is alive.
    pub fn new() -> (Self, tokio::sync::mpsc::UnboundedReceiver<ControlMsg>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (Self { tx }, rx)
    }

    /// Best-effort send: if `control_supervisor` has no Worker pipe connected
    /// right now (e.g. input arriving in the gap between a sign-out and the
    /// replacement Worker's handshake completing), it drops the message
    /// rather than blocking — same "never worse than dropping one packet"
    /// contract as before. If Master itself is shutting down (receiver
    /// dropped), this is silently a no-op rather than a panic.
    pub fn send(&self, msg: ControlMsg) {
        let _ = self.tx.send(msg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Distinct pipe names per test so `cargo test`'s parallel test threads
    // don't fight over the same real pipe names this module also exports as
    // the production constants.
    const TEST_CONTROL: &str = r"\\.\pipe\NovaControlTest-round-trip";
    const TEST_MEDIA: &str = r"\\.\pipe\NovaMediaTest-round-trip";

    /// End-to-end proof of the transport this module is for: a server task
    /// and a client task, in the same process, exercising the exact
    /// create→accept / connect / send / recv path `service.rs`'s
    /// `spawn_host_with_ipc` and `lib.rs`'s `run_worker` use for real.
    /// Doesn't touch cross-process/cross-session concerns (security
    /// descriptor, `CreateProcessAsUserW`) — those are proven by live
    /// deployment once the Worker actually does something (Phase 2+); this
    /// proves the framing/message logic can't be the thing that's wrong.
    #[tokio::test]
    async fn control_pipe_round_trip() {
        let control_server = server_options().create(TEST_CONTROL).expect("create control pipe");
        let media_server = server_options().create(TEST_MEDIA).expect("create media pipe");

        let server_task = tokio::spawn(async move {
            accept_worker(&control_server, &media_server).await.expect("accept worker");
            let mut control_server = control_server;
            let msg = recv_control(&mut control_server).await.expect("recv WorkerReady");
            assert_eq!(msg, ControlMsg::WorkerReady);
            send_control(&mut control_server, &ControlMsg::Stop).await.expect("send Stop");
        });

        let mut control_client = open_with_retry(TEST_CONTROL).await.expect("connect control pipe");
        let _media_client = open_with_retry(TEST_MEDIA).await.expect("connect media pipe");
        send_control(&mut control_client, &ControlMsg::WorkerReady).await.expect("send WorkerReady");
        let reply = recv_control(&mut control_client).await.expect("recv Stop");
        assert_eq!(reply, ControlMsg::Stop);

        server_task.await.expect("server task panicked");
    }

    #[test]
    fn control_msg_round_trips_through_encode_decode() {
        for msg in [
            ControlMsg::WorkerReady,
            ControlMsg::WorkerError("something went sideways".to_string()),
            ControlMsg::Stop,
            ControlMsg::OpenPairDialog,
            ControlMsg::PinRelay { pin: "1234".to_string(), device: "PiXeL".to_string() },
            ControlMsg::SecureDesktopChanged { secure: true },
            ControlMsg::SecureDesktopChanged { secure: false },
            ControlMsg::InjectInput(vec![0x22, 0, 0, 0, 0x0c, 0, 0, 0]),
        ] {
            let decoded = ControlMsg::decode(&msg.encode()).expect("decode");
            assert_eq!(decoded, msg);
        }
    }

    #[test]
    fn media_msg_round_trips_through_encode_decode() {
        for msg in [
            MediaMsg::VideoFrame { frame_type: 2, data: vec![0xDE, 0xAD, 0xBE, 0xEF] },
            MediaMsg::VideoFrame { frame_type: 1, data: vec![] },
            MediaMsg::AudioFrame { data: vec![1, 2, 3] },
        ] {
            let decoded = MediaMsg::decode(&msg.encode()).expect("decode");
            assert_eq!(decoded, msg);
        }
    }
}
