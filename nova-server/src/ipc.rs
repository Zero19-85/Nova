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
    pub const CAPTURE_RECT: u8 = 15;
    pub const INVALIDATE_REF_FRAMES: u8 = 16;
    pub const WORKER_CAPABILITIES: u8 = 17;
    pub const END_SESSION: u8 = 18;
    pub const CLEAR_PAIRED: u8 = 19;
    pub const SET_DISPLAY_MODE: u8 = 20;
    pub const DISPLAY_INVENTORY: u8 = 21;
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

/// The one place `encoder::Codec` becomes its wire form. Two session paths now
/// build a `ConfigureStart` (Moonlight's `session_watcher` and Echo's
/// `SessionManager`), and a second hand-written match is a second place for
/// them to disagree.
impl From<crate::encoder::Codec> for WireCodec {
    fn from(c: crate::encoder::Codec) -> Self {
        match c {
            crate::encoder::Codec::H264 => WireCodec::H264,
            crate::encoder::Codec::Hevc => WireCodec::Hevc,
            crate::encoder::Codec::Av1 => WireCodec::Av1,
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
    /// First wire frame index the Worker must use (== its first NVENC
    /// timestamp). 1 for a fresh client session; a replacement Worker adopted
    /// MID-session (sign-in upgrade, sign-out fallback, crash respawn) gets
    /// `last index Master put on the wire + 1` instead — moonlight-common-c
    /// discards every frame whose index is before the next one it expects
    /// (`isBefore32`), so a respawned Worker restarting at 1 while the client
    /// sits at frame N blacked the stream out permanently. Master
    /// (`control_supervisor`) stamps this on every ConfigureStart it sends.
    pub start_frame_index: u32,
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

/// One display in the Worker's report of the live desktop topology.
///
/// Mirrors `virtual_display::DisplayInfo` (that type stays free of any wire
/// knowledge; `lib.rs` converts) — the same split as
/// `NegotiatedParams` → [`ConfigureStart`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayEntry {
    /// GDI device name (`\\.\DISPLAY1`) — the seat's targeting key.
    pub device_name: String,
    pub label: String,
    pub width: u32,
    pub height: u32,
    /// Committed refresh in whole Hz; 0 = unknown.
    pub refresh_hz: u32,
    pub is_primary: bool,
    pub is_virtual: bool,
    pub hdr_active: bool,
    pub hdr_capable: bool,
}

impl DisplayEntry {
    fn encode_into(&self, out: &mut Vec<u8>) {
        write_string(out, &self.device_name);
        write_string(out, &self.label);
        write_u32(out, self.width);
        write_u32(out, self.height);
        write_u32(out, self.refresh_hz);
        // One flags byte rather than four: the boolean set here is expected to
        // grow (per-seat capability bits), and a bitfield keeps that additive
        // without re-laying-out the record each time.
        let flags = (self.is_primary as u8)
            | (self.is_virtual as u8) << 1
            | (self.hdr_active as u8) << 2
            | (self.hdr_capable as u8) << 3;
        out.push(flags);
    }

    fn decode(bytes: &[u8], at: &mut usize) -> io::Result<Self> {
        let device_name = read_string(bytes, at)?;
        let label = read_string(bytes, at)?;
        let width = read_u32(bytes, at)?;
        let height = read_u32(bytes, at)?;
        let refresh_hz = read_u32(bytes, at)?;
        let flags = *bytes
            .get(*at)
            .ok_or_else(|| invalid_data("truncated DisplayEntry flags"))?;
        *at += 1;
        Ok(DisplayEntry {
            device_name,
            label,
            width,
            height,
            refresh_hz,
            is_primary: flags & 0b0001 != 0,
            is_virtual: flags & 0b0010 != 0,
            hdr_active: flags & 0b0100 != 0,
            hdr_capable: flags & 0b1000 != 0,
        })
    }
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
    /// Worker -> Master: what this Worker is physically able to stream, sent
    /// once its capture backend is up (before any session exists).
    ///
    /// A Moonlight session's resolution and HDR profile are fixed when the
    /// client builds its decoder; changing either mid-session wrecks it
    /// (confirmed live 2026-08-10: black frame with a green region). A
    /// SYSTEM-fallback Worker covering the logon screen CANNOT drive the VDD —
    /// `SetDisplayConfig` is denied on the Winlogon desktop regardless of
    /// token — so it can only ever serve the physical monitor's native size in
    /// SDR. Master therefore negotiates sessions down to what the CURRENT
    /// Worker can actually sustain (see `session_negotiate::negotiate`), and
    /// holds that geometry for the session's whole life so no handoff can
    /// change it.
    WorkerCapabilities {
        /// False while this Worker is the SYSTEM fallback: no VDD, no HDR,
        /// capture is whatever the physical primary happens to be.
        vdd_capable: bool,
        /// The capture size available right now (the physical primary, for a
        /// fallback Worker).
        native_width: u32,
        native_height: u32,
    },
    /// Master -> Worker: raw Moonlight INPUT_DATA payload (the same bytes
    /// `control.rs` used to pass straight to `input::handle_input_packet`).
    InjectInput(Vec<u8>),
    /// Master -> Worker: client sent PT_REQUEST_IDR_FRAME /
    /// PT_INVALIDATE_REF_FRAMES.
    RequestIdr,
    /// Master -> Worker: client sent PT_LOSS_STATS with loss > 0.
    CongestionReduce,
    /// Master -> Worker: client sent PT_INVALIDATE_REF_FRAMES for the given
    /// wire frame-index range. The Worker asks NVENC to invalidate those
    /// references so the next P-frame recovers without an IDR; on failure it
    /// falls back to forcing an IDR. See encoder::invalidate_ref_frames.
    InvalidateRefFrames { first: u32, last: u32 },
    /// Worker -> Master: PIN + device name entered on the Worker's tray
    /// dialog, forwarded into Master-side pairing's `global_pin` slot (the
    /// same handshake point the monolithic host's tray uses in-process).
    /// The Worker sends this whenever its tray dialog completes — whether
    /// the dialog was opened by [`ControlMsg::OpenPairDialog`] or manually
    /// from the tray menu.
    PinRelay { pin: String, device: String },
    /// Worker -> Master, then Master -> input helper: the desktop-coordinate
    /// rect of the surface currently being captured.
    ///
    /// `input.rs` maps Moonlight's fractional ABSOLUTE mouse positions onto
    /// this rect, and it lives in a PER-PROCESS static — so the SYSTEM input
    /// helper, which never runs a capture loop, had it at 0×0 and silently
    /// dropped every absolute mouse move (confirmed live 2026-08-06: the
    /// cursor froze mid-screen at every UAC prompt while clicks and keys kept
    /// working). Master caches the Worker's last report and replays it to each
    /// helper on connect, so injection in either process maps identically.
    CaptureRect { origin_x: i32, origin_y: i32, width: u32, height: u32 },
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
    /// Worker -> Master: the user picked "End Stream" on the tray menu — end
    /// the live session now.
    ///
    /// The Worker cannot do this itself, and deliberately does not try. The
    /// session lives in Master's `ClientInfo`, and a Worker that tore down its
    /// own capture would leave the Master still holding an active session,
    /// still sending it input, and replaying its `ConfigureStart` to the next
    /// Worker that connects. Master instead marks the session cancelled and
    /// lets the ORDINARY teardown path run (see `control_supervisor`), so this
    /// takes exactly the same route as a client-initiated "Quit App".
    EndSession,
    /// Worker -> Master: the live desktop topology, as the Worker's session
    /// sees it.
    ///
    /// The Master cannot obtain this itself at any privilege level:
    /// `QueryDisplayConfig` describes the *calling session's* displays, and
    /// Master runs in Session 0, whose desktop no human is looking at. So the
    /// Worker enumerates and reports, and Master caches the result for
    /// `echo_rpc`'s seat list.
    ///
    /// Sent on Worker startup and after anything that can change the topology
    /// (session configure, VDD teardown), rather than polled: display changes
    /// are events, and a poll would burn CCD queries on a TIME_CRITICAL
    /// process for a list that changes a few times an hour.
    DisplayInventory(Vec<DisplayEntry>),
    /// Master -> Worker: an Echo client asked for a display mode change
    /// (`echo_rpc.rs`'s `set_display`).
    ///
    /// Display state — the VDD, CCD topology, Advanced Color — is Worker-owned
    /// and unreachable from Session 0, so an Echo command has to cross the pipe
    /// exactly like a session does. Routing it through the same channel also
    /// means it inherits the split's existing durability: a Worker that
    /// respawns mid-command simply never applies it, and the client's next
    /// `get_status` shows the unchanged mode rather than a lie.
    ///
    /// Deliberately NOT a `ConfigureStart`: that message starts a *client
    /// session* (audio, input, rikey, wire-index continuity, app launch). This
    /// one changes only the display the capture loop is bound to, which is
    /// what makes it usable outside a session as well as inside one.
    SetDisplayMode {
        /// Which capture seat — `echo_rpc::PRIMARY_SEAT` today, a real seat id
        /// once multi-seat exists.
        display_id: String,
        width: u32,
        height: u32,
        refresh_hz: u32,
        hdr: bool,
    },
    /// Worker -> Master: the user picked "Clear Paired Devices" on the tray
    /// menu — drop every entry from the trust store.
    ///
    /// Also Master-only work: `pairing.rs`'s trust store is the live
    /// `TrustedClients` map in THAT process, and deleting `nova_paired.json`
    /// from the Worker would revoke nothing (the Master keeps authorising from
    /// memory and rewrites the file on the next successful pair).
    ClearPaired,
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
        write_u32(out, self.start_frame_index);
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
        let start_frame_index = read_u32(bytes, at)?;
        Ok(ConfigureStart {
            width, height, fps, codec, hdr_confirmed, bitrate_kbps, app_id, launch_app,
            device_name, rikey, rikeyid, host_audio, audio_encryption,
            audio_packet_duration_ms, packet_size, min_fec_packets, start_frame_index,
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
            ControlMsg::InvalidateRefFrames { first, last } => {
                let mut out = vec![tag::INVALIDATE_REF_FRAMES];
                write_u32(&mut out, *first);
                write_u32(&mut out, *last);
                out
            }
            ControlMsg::PinRelay { pin, device } => {
                let mut out = vec![tag::PIN_RELAY];
                write_string(&mut out, pin);
                write_string(&mut out, device);
                out
            }
            ControlMsg::DisplayInventory(displays) => {
                let mut out = vec![tag::DISPLAY_INVENTORY];
                write_u32(&mut out, displays.len() as u32);
                for d in displays {
                    d.encode_into(&mut out);
                }
                out
            }
            ControlMsg::SetDisplayMode { display_id, width, height, refresh_hz, hdr } => {
                let mut out = vec![tag::SET_DISPLAY_MODE];
                write_string(&mut out, display_id);
                write_u32(&mut out, *width);
                write_u32(&mut out, *height);
                write_u32(&mut out, *refresh_hz);
                out.push(*hdr as u8);
                out
            }
            ControlMsg::Deactivate { cancelled } => vec![tag::DEACTIVATE, *cancelled as u8],
            ControlMsg::EndSession => vec![tag::END_SESSION],
            ControlMsg::ClearPaired => vec![tag::CLEAR_PAIRED],
            ControlMsg::OpenPairDialog => vec![tag::OPEN_PAIR_DIALOG],
            ControlMsg::SecureDesktopChanged { secure } => {
                vec![tag::SECURE_DESKTOP_CHANGED, *secure as u8]
            }
            ControlMsg::WorkerCapabilities { vdd_capable, native_width, native_height } => {
                let mut out = vec![tag::WORKER_CAPABILITIES, *vdd_capable as u8];
                write_u32(&mut out, *native_width);
                write_u32(&mut out, *native_height);
                out
            }
            ControlMsg::CaptureRect { origin_x, origin_y, width, height } => {
                let mut out = vec![tag::CAPTURE_RECT];
                write_u32(&mut out, *origin_x as u32);
                write_u32(&mut out, *origin_y as u32);
                write_u32(&mut out, *width);
                write_u32(&mut out, *height);
                out
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
            tag::INVALIDATE_REF_FRAMES => {
                let at = &mut 0usize;
                let first = read_u32(rest, at)?;
                let last = read_u32(rest, at)?;
                Ok(ControlMsg::InvalidateRefFrames { first, last })
            }
            tag::PIN_RELAY => {
                let at = &mut 0usize;
                let pin = read_string(rest, at)?;
                let device = read_string(rest, at)?;
                Ok(ControlMsg::PinRelay { pin, device })
            }
            tag::DISPLAY_INVENTORY => {
                let at = &mut 0usize;
                let count = read_u32(rest, at)?;
                // A corrupt count must not make us pre-allocate wildly; the
                // frame itself is already bounded by MAX_FRAME_LEN, and each
                // entry costs at least 21 bytes on the wire.
                if count as usize > rest.len() {
                    return Err(invalid_data("DisplayInventory count exceeds payload"));
                }
                let mut displays = Vec::with_capacity(count as usize);
                for _ in 0..count {
                    displays.push(DisplayEntry::decode(rest, at)?);
                }
                Ok(ControlMsg::DisplayInventory(displays))
            }
            tag::SET_DISPLAY_MODE => {
                let at = &mut 0usize;
                let display_id = read_string(rest, at)?;
                let width = read_u32(rest, at)?;
                let height = read_u32(rest, at)?;
                let refresh_hz = read_u32(rest, at)?;
                let hdr = *rest
                    .get(*at)
                    .ok_or_else(|| invalid_data("truncated SetDisplayMode"))?
                    != 0;
                Ok(ControlMsg::SetDisplayMode { display_id, width, height, refresh_hz, hdr })
            }
            tag::DEACTIVATE => {
                let cancelled = rest.first().copied().unwrap_or(0) != 0;
                Ok(ControlMsg::Deactivate { cancelled })
            }
            tag::END_SESSION => Ok(ControlMsg::EndSession),
            tag::CLEAR_PAIRED => Ok(ControlMsg::ClearPaired),
            tag::OPEN_PAIR_DIALOG => Ok(ControlMsg::OpenPairDialog),
            tag::SECURE_DESKTOP_CHANGED => Ok(ControlMsg::SecureDesktopChanged {
                secure: rest.first().copied().unwrap_or(0) != 0,
            }),
            tag::WORKER_CAPABILITIES => {
                let (&vdd, sizes) = rest
                    .split_first()
                    .ok_or_else(|| invalid_data("truncated WorkerCapabilities"))?;
                let at = &mut 0usize;
                let native_width = read_u32(sizes, at)?;
                let native_height = read_u32(sizes, at)?;
                Ok(ControlMsg::WorkerCapabilities {
                    vdd_capable: vdd != 0,
                    native_width,
                    native_height,
                })
            }
            tag::CAPTURE_RECT => {
                let at = &mut 0usize;
                // origin_x/y are signed (a monitor left of / above the primary
                // has negative desktop coordinates) — round-tripped as u32.
                let origin_x = read_u32(rest, at)? as i32;
                let origin_y = read_u32(rest, at)? as i32;
                let width = read_u32(rest, at)?;
                let height = read_u32(rest, at)?;
                Ok(ControlMsg::CaptureRect { origin_x, origin_y, width, height })
            }
            other => Err(invalid_data(&format!("unknown ControlMsg tag {other}"))),
        }
    }
}

/// Media-plane messages (Worker -> Master only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaMsg {
    /// `frame_type`: 2 = IDR/keyframe, 1 = P-frame (matches `rtp::send_frame`'s
    /// existing convention — reused as-is, not redefined).
    ///
    /// `frame_index` is the wire frame index the Worker encoded this frame as
    /// (== the NVENC inputTimeStamp). Master puts it straight on the wire via
    /// `rtp::send_frame`. Video carries it — unlike audio — because
    /// reference-frame invalidation requires the index NVENC stamped and the
    /// index the client references to be identical, and only the Worker (which
    /// owns the encoder) knows it. A frame the Worker dropped after encode
    /// simply never arrives, leaving a gap the client recovers from.
    VideoFrame { frame_index: u32, frame_type: u8, data: Vec<u8> },
    /// Raw Opus-encoded audio, pre-encryption. No seq/timestamp on the wire —
    /// Master's `audio::AudioTxState` owns those independently.
    AudioFrame { data: Vec<u8> },
}

impl MediaMsg {
    fn encode(&self) -> Vec<u8> {
        match self {
            MediaMsg::VideoFrame { frame_index, frame_type, data } => {
                let mut out = Vec::with_capacity(6 + data.len());
                out.push(tag::VIDEO_FRAME);
                out.push(*frame_type);
                out.extend_from_slice(&frame_index.to_le_bytes());
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
                // [frame_type: u8][frame_index: u32 LE][data...]
                let (&frame_type, rest) = rest
                    .split_first()
                    .ok_or_else(|| invalid_data("truncated VideoFrame"))?;
                let idx = rest
                    .get(..4)
                    .ok_or_else(|| invalid_data("truncated VideoFrame index"))?;
                let frame_index = u32::from_le_bytes(idx.try_into().unwrap());
                Ok(MediaMsg::VideoFrame { frame_index, frame_type, data: rest[4..].to_vec() })
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
            // Negative origins are real (a monitor left of/above the primary)
            // and must survive the u32 round-trip on the wire.
            ControlMsg::CaptureRect { origin_x: 0, origin_y: 0, width: 3840, height: 2160 },
            ControlMsg::CaptureRect { origin_x: -1920, origin_y: -300, width: 1920, height: 1080 },
            ControlMsg::InjectInput(vec![0x22, 0, 0, 0, 0x0c, 0, 0, 0]),
            // Tray actions (Worker -> Master): payload-free, so the whole
            // contract is that their tags stay distinct from every other
            // variant's — which is exactly what this round-trip checks.
            ControlMsg::EndSession,
            ControlMsg::ClearPaired,
            // Echo display command (echo_rpc.rs). The variable-length id sits
            // before four fixed fields, so a framing slip here would decode as
            // a plausible-but-wrong mode — worth pinning.
            ControlMsg::SetDisplayMode {
                display_id: "primary".to_string(),
                width: 3840,
                height: 2160,
                refresh_hz: 120,
                hdr: true,
            },
            ControlMsg::SetDisplayMode {
                display_id: String::new(),
                width: 1280,
                height: 720,
                refresh_hz: 60,
                hdr: false,
            },
            // Display inventory (Worker -> Master). Variable-length records in
            // a variable-length list, with the booleans packed into a flags
            // byte — the most framing-sensitive message on the pipe.
            ControlMsg::DisplayInventory(vec![]),
            ControlMsg::DisplayInventory(vec![
                DisplayEntry {
                    device_name: r"\\.\DISPLAY1".to_string(),
                    label: "NVIDIA GeForce RTX 4090".to_string(),
                    width: 2560,
                    height: 1440,
                    refresh_hz: 165,
                    is_primary: true,
                    is_virtual: false,
                    hdr_active: false,
                    hdr_capable: true,
                },
                DisplayEntry {
                    device_name: r"\\.\DISPLAY9".to_string(),
                    label: "Virtual Display Driver".to_string(),
                    width: 3840,
                    height: 2160,
                    refresh_hz: 120,
                    is_primary: false,
                    is_virtual: true,
                    hdr_active: true,
                    hdr_capable: true,
                },
            ]),
        ] {
            let decoded = ControlMsg::decode(&msg.encode()).expect("decode");
            assert_eq!(decoded, msg);
        }
    }

    #[test]
    fn media_msg_round_trips_through_encode_decode() {
        for msg in [
            MediaMsg::VideoFrame { frame_index: 1, frame_type: 2, data: vec![0xDE, 0xAD, 0xBE, 0xEF] },
            MediaMsg::VideoFrame { frame_index: 4_000_000_000, frame_type: 1, data: vec![] },
            MediaMsg::AudioFrame { data: vec![1, 2, 3] },
        ] {
            let decoded = MediaMsg::decode(&msg.encode()).expect("decode");
            assert_eq!(decoded, msg);
        }
    }
}
