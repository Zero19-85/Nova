//! Echo side-channel — the control/telemetry RPC for Nova's native client.
//!
//! ## Why this lives in the Master, not next to the capture engine
//!
//! Since the Phase 16 Master/Worker split, the **Master** (LocalSystem, Session
//! 0) owns every socket Nova binds — RTSP, ENet control, pairing, RTP, audio —
//! and the **Worker** (elevated user, console session) owns capture, the VDD,
//! and NVENC. The Worker has no listening ports at all and is deliberately
//! disposable: it dies and respawns on sign-out, session swap, or crash, and
//! the Master replays the live session's `ConfigureStart` to whichever Worker
//! connects next.
//!
//! Putting the Echo listener in the Worker would therefore tie the control
//! channel's lifetime to the most volatile process in the system: every
//! sign-out would drop the client's RPC connection along with the Worker. So
//! the listener is Master-side, and a display command is not a function call —
//! it is an [`ipc::ControlMsg`] down `\\.\pipe\NovaControl`. That routing is
//! what makes Echo control survive a Worker respawn for free, the same way
//! `control_supervisor`'s `last_configure` replay already does for sessions.
//!
//! ## Why newline-delimited JSON and not WebSocket
//!
//! Echo is a native client we ship ourselves; there is no browser in the
//! picture and no origin/proxy story that needs HTTP upgrade semantics. A
//! WebSocket would add a handshake, a masking layer, and a dependency, and buy
//! nothing a length-delimited stream doesn't already give us — this codebase
//! hand-rolls every other wire format it owns (RTP, pairing, `ipc.rs`) for the
//! same reason. One JSON object per line, `\n`-terminated, both directions.
//! Requests carry an optional `id` which is echoed on the matching response;
//! unsolicited server→client messages carry `"event"` instead and never an
//! `id`, so a client can demultiplex without tracking state.
//!
//! JSON itself (rather than this codebase's usual hand-rolled binary framing)
//! is a deliberate exception: this is a *control* channel at human/UI cadence,
//! never on the frame path, and a hand-written parser on a network-facing port
//! owned by a SYSTEM service is exactly the kind of code that turns a typo into
//! a vulnerability. `serde_json` is pure Rust with no native linkage, so it
//! does not touch the NVENC/DXGI link line (Developer Rule #1).
//!
//! ## Security posture — the pairing certificate IS the Echo identity
//!
//! This is a command port on a LocalSystem process: `set_display` reconfigures
//! the host's display topology. Authentication is therefore **mutual TLS
//! against the existing pairing trust store** — the same mechanism, the same
//! `nova_paired.json`, the same fingerprints that authorize port 47984:
//!
//! 1. The listener presents Nova's own server certificate (published by
//!    `pairing.rs` as `SERVER_IDENTITY`, so both ports are provably the same
//!    host identity — no second cert to generate, distribute, or rotate).
//! 2. A client certificate is **mandatory** at handshake time. Self-signed is
//!    fine and expected; what the handshake proves is possession of the
//!    matching private key (the TLS CertificateVerify signature is checked for
//!    real — see `pairing::AcceptAnyClientCert`).
//! 3. After the handshake the peer certificate's SHA-256 fingerprint is looked
//!    up in the live trust store. No match ⇒ the connection is closed before a
//!    single command is read.
//!
//! Consequences worth knowing: an Echo device must complete the ordinary
//! pairing flow (PIN on 47989/47984) before it can open this port at all —
//! there is exactly one enrolment path — and the tray's "Clear Paired Devices"
//! revokes Echo access at the same instant it revokes streaming, because the
//! store is read per connection rather than cached. Because authorization can
//! never succeed without a paired certificate, the listener binds `0.0.0.0`:
//! unlike a shared secret, there is no configuration mistake that turns this
//! into an open command port.
//!
//! What this deliberately does *not* do yet is distinguish *which* paired
//! device is allowed to do *what*. Every paired device is currently equal, so
//! any device that can stream can also reconfigure displays. Per-device
//! capability grants are the natural next layer once multi-seat exists and
//! "this seat belongs to that device" is a real relationship.
//!
//! ## What this module does NOT do
//!
//! It does not apply anything. It parses, authenticates, validates, and routes
//! through [`DisplayOrchestrator`]. The production implementation
//! ([`WorkerOrchestrator`]) turns a validated request into
//! `ControlMsg::SetDisplayMode`; the Worker-side application of that message —
//! the graceful capture/encoder rebuild — is the next step and lives entirely
//! on the other side of that seam.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

use crate::config::EchoConfig;
use crate::echo::session::{HandoffError, MediaOwner, SessionManager, SessionRequest};
use crate::encoder::Codec;
use crate::ipc::{self, ControlMsg, WorkerLink};
use crate::rtsp::ClientInfo;
use crate::session_negotiate::WorkerCaps;

/// Protocol version reported by `hello`. Bump on any breaking change to the
/// request/response shape so an older Echo build can refuse cleanly instead of
/// misparsing.
pub const PROTOCOL_VERSION: u32 = 1;

/// Identifier of the one capture seat Nova can address today.
///
/// Multi-seat (a display per connected device, the "split-seat workstation"
/// goal) turns this constant into a registry: one entry per `DesktopManager` +
/// encoder + RTP sender triple. Everything in this module already routes by
/// `display_id` so that change stays confined to the orchestrator.
pub const PRIMARY_SEAT: &str = "primary";

/// Longest single request line accepted. A control channel's frames are tens
/// of bytes; anything approaching this is a peer that has lost framing (or is
/// probing), and an unbounded `read_line` on a SYSTEM service is an
/// out-of-memory primitive.
const MAX_LINE_BYTES: usize = 64 * 1024;

/// Simultaneous Echo connections. Generous for a handful of devices, bounded
/// so a connect flood cannot exhaust handles in the service process.
const MAX_CONNECTIONS: usize = 8;

/// How long a peer may take to complete the TLS handshake. A half-open TLS
/// connection costs a task and a socket; a real client finishes in
/// milliseconds on a LAN.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to wait at startup for `pairing.rs` to publish Nova's TLS identity.
/// The pairing server is spawned just before this module in
/// `start_master_network`, so this is a scheduling race of milliseconds — the
/// generous bound only matters on a fresh install, where the certificate is
/// generated on first run.
const IDENTITY_WAIT: Duration = Duration::from_secs(60);

/// Who is on the other end of an Echo connection, established by mutual TLS
/// before any command is read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EchoIdentity {
    /// SHA-256 hex of the client certificate DER — the trust store's key.
    pub fingerprint: String,
    /// The name this device paired under.
    pub device_name: String,
}

impl EchoIdentity {
    /// First 16 hex chars, matching how `pairing.rs` abbreviates fingerprints
    /// in its own logs so the two are greppable together.
    fn short_fingerprint(&self) -> &str {
        &self.fingerprint[..16.min(self.fingerprint.len())]
    }
}

/// The authorization decision, factored out of the TLS plumbing so it can be
/// tested without a live handshake.
///
/// `lookup` is `pairing::trusted_device_name` in production — it reads the
/// LIVE trust store, so a device revoked via the tray is refused on its very
/// next connection.
pub(crate) fn authorize(
    peer_cert_der: Option<&[u8]>,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<EchoIdentity, String> {
    // Unreachable in production (rustls enforces `client_auth_mandatory`), but
    // the check is the difference between "cannot happen" and "cannot happen,
    // and if it did we would still refuse".
    let der = peer_cert_der.ok_or_else(|| "no client certificate presented".to_string())?;
    let fingerprint = crate::pairing::fingerprint_of_cert(der);
    match lookup(&fingerprint) {
        Some(device_name) => Ok(EchoIdentity { fingerprint, device_name }),
        None => Err(format!(
            "client certificate {}… is not paired with this host",
            &fingerprint[..16.min(fingerprint.len())]
        )),
    }
}

// ── Wire types ──────────────────────────────────────────────────────────────

/// One request line. Parameters are read from the **top level** of the object
/// (`{"command":"set_display","res":"4K","hdr":true}`) rather than a nested
/// `params` object — it is what the client is naturally going to write, and a
/// flattened catch-all keeps the schema open for per-command fields without a
/// variant explosion here.
#[derive(Debug, Deserialize)]
struct RpcRequest {
    #[serde(default)]
    id: Option<u64>,
    command: String,
    #[serde(flatten)]
    params: Map<String, Value>,
}

#[derive(Debug, Serialize)]
struct RpcResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<u64>,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}

#[derive(Debug, Serialize)]
struct RpcError {
    /// Stable machine-readable code — clients branch on this, never on
    /// `message`, which is free to change.
    code: &'static str,
    message: String,
}

impl RpcResponse {
    fn ok(id: Option<u64>, result: Value) -> Self {
        Self { id, ok: true, result: Some(result), error: None }
    }
    fn err(id: Option<u64>, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            id,
            ok: false,
            result: None,
            error: Some(RpcError { code, message: message.into() }),
        }
    }
}

// Error codes. Kept as constants so the set is greppable and a typo in one
// arm can't silently invent a new code the client has never heard of.
const E_BAD_REQUEST: &str = "bad_request";

/// Ceiling on one input packet. The largest GameStream input packet is the
/// multi-controller one at a few dozen bytes; 256 leaves room for protocol
/// variants while keeping a hostile peer from pushing bulk data through a
/// command that is supposed to carry a keystroke.
const MAX_INPUT_PACKET: usize = 256;

/// Ceiling on packets in one `input` command, matching the client's batch size.
const MAX_INPUT_BATCH: usize = 64;
const E_UNKNOWN_COMMAND: &str = "unknown_command";
const E_UNKNOWN_DISPLAY: &str = "unknown_display";
const E_SESSION_LOCKED: &str = "session_locked";
const E_WORKER_UNAVAILABLE: &str = "worker_unavailable";
/// Streaming commands exist but this build has no session manager wired (the
/// WAN layer is off because no relay is configured). Distinct from
/// `unknown_command`: the command is real, the capability is not present.
const E_NO_SESSION_LAYER: &str = "session_layer_unavailable";

// ── Display orchestration model ─────────────────────────────────────────────

/// A capture target Echo can address. One today (the console session's active
/// capture); one per seat once multi-seat lands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DisplaySeat {
    /// The GDI device name (`\\.\DISPLAY1`) once a Worker has reported a real
    /// inventory. Stable for as long as the topology is, which is what makes
    /// it usable as a targeting key.
    pub id: String,
    /// Human label for the client's UI (the monitor's adapter/device string).
    pub label: String,
    pub width: u32,
    pub height: u32,
    /// Committed refresh in whole Hz; 0 when not known.
    pub refresh_hz: u32,
    pub is_primary: bool,
    /// Whether Nova can drive this display to an arbitrary mode. True only for
    /// the virtual display today: it is the one panel Nova owns outright.
    /// Physical monitors are listed (they are legitimate *capture* targets)
    /// but not presented as reconfigurable, because changing their mode
    /// changes what the person sitting at the machine is looking at. A
    /// SYSTEM-fallback Worker at the logon screen reports nothing
    /// reconfigurable at all — `SetDisplayConfig` is denied on the Winlogon
    /// desktop regardless of token.
    pub reconfigurable: bool,
    /// True when this seat is the virtual display.
    pub virtual_display: bool,
    /// Advanced Color currently ON for this display.
    pub hdr_active: bool,
    /// Advanced Color supported by this display (whether or not it is on).
    pub hdr_capable: bool,
}

impl DisplaySeat {
    /// Convert a Worker's inventory record into a seat Echo can address.
    ///
    /// `reconfigurable` is derived here rather than reported by the Worker
    /// because it is a *policy* decision, not a hardware fact: CCD would
    /// happily re-mode a physical monitor, but doing so changes what the
    /// person sitting at the machine sees. Nova owns the virtual display
    /// outright, so that is the one seat presented as freely reconfigurable.
    pub fn from_entry(e: &ipc::DisplayEntry) -> Self {
        Self {
            id: e.device_name.clone(),
            label: e.label.clone(),
            width: e.width,
            height: e.height,
            refresh_hz: e.refresh_hz,
            is_primary: e.is_primary,
            reconfigurable: e.is_virtual,
            virtual_display: e.is_virtual,
            hdr_active: e.hdr_active,
            hdr_capable: e.hdr_capable,
        }
    }
}

/// A parsed, bounds-checked display change. Producing one of these is all the
/// validation this module does; applying it is the orchestrator's job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayRequest {
    pub display_id: String,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub hdr: bool,
    /// Override the mid-session guard. See [`Handler::handle_set_display`] —
    /// changing geometry under a live Moonlight client destroys its decoder,
    /// so the guard refuses by default and the caller must opt in.
    pub force: bool,
}

/// The seam between "Echo said so" and "the display actually changed".
///
/// A trait rather than a direct `WorkerLink` call so that (a) the whole
/// command surface is testable without a Worker process, and (b) the
/// multi-seat rework replaces one implementation instead of editing the RPC
/// layer.
pub trait DisplayOrchestrator: Send + Sync + 'static {
    /// Currently addressable seats. Empty means no Worker has reported in yet.
    fn seats(&self) -> Vec<DisplaySeat>;
    /// Route a validated request toward the process that owns the display.
    fn apply(&self, req: &DisplayRequest) -> Result<(), String>;
}

/// Production orchestrator: routes to the live Worker over the control pipe.
///
/// `WorkerLink::send` is deliberately best-effort and never blocks (see its
/// doc comment) — if no Worker is connected at this instant the message is
/// dropped rather than queued. That is the right behaviour for input packets,
/// but wrong for a display command a user is watching for a reply to, so this
/// checks for a live Worker (via reported capabilities) and reports
/// `worker_unavailable` instead of pretending success.
pub struct WorkerOrchestrator {
    link: WorkerLink,
    caps: Arc<Mutex<Option<WorkerCaps>>>,
    seats: SeatCache,
}

/// Master-side cache of the Worker's last `ControlMsg::DisplayInventory`.
///
/// The Master cannot enumerate the console session's monitors itself:
/// `QueryDisplayConfig` called from Session 0 describes Session 0's own
/// headless desktop, so a Master-side enumeration would be confidently wrong.
/// Only the Worker can see the real topology, so it reports and the Master
/// caches. Empty until a Worker has reported — which is also exactly the state
/// during a sign-out handoff, and answering "not ready" then is more honest
/// than serving a stale seat list from a Worker that no longer exists.
pub type SeatCache = Arc<Mutex<Vec<DisplaySeat>>>;

impl WorkerOrchestrator {
    pub fn new(link: WorkerLink, caps: Arc<Mutex<Option<WorkerCaps>>>, seats: SeatCache) -> Self {
        Self { link, caps, seats }
    }
}

impl DisplayOrchestrator for WorkerOrchestrator {
    fn seats(&self) -> Vec<DisplaySeat> {
        // Preferred source: the Worker's real per-monitor inventory.
        let reported = self.seats.lock().unwrap().clone();
        if !reported.is_empty() {
            return reported;
        }
        // Fallback for the window between a Worker connecting (which publishes
        // `WorkerCapabilities`) and its first inventory report, and for any
        // Worker too degraded to enumerate — a SYSTEM fallback at the logon
        // screen can capture but cannot query CCD meaningfully. One honest
        // entry beats an empty list that reads as "Nova is broken".
        let Some(caps) = *self.caps.lock().unwrap() else {
            return Vec::new();
        };
        vec![DisplaySeat {
            id: PRIMARY_SEAT.to_string(),
            label: if caps.vdd_capable { "Virtual display".into() } else { "Physical primary".into() },
            width: caps.native_width,
            height: caps.native_height,
            refresh_hz: 0, // unknown from capabilities alone — never guessed
            is_primary: true,
            reconfigurable: caps.vdd_capable,
            virtual_display: caps.vdd_capable,
            hdr_active: false,
            hdr_capable: false,
        }]
    }

    fn apply(&self, req: &DisplayRequest) -> Result<(), String> {
        let caps = *self.caps.lock().unwrap();
        let Some(caps) = caps else {
            return Err("no worker has reported capabilities yet".to_string());
        };
        if !caps.vdd_capable {
            return Err(
                "the active worker cannot drive the virtual display (SYSTEM fallback at the \
                 logon screen); sign in and retry"
                    .to_string(),
            );
        }
        self.link.send(ControlMsg::SetDisplayMode {
            display_id: req.display_id.clone(),
            width: req.width,
            height: req.height,
            refresh_hz: req.refresh_hz,
            hdr: req.hdr,
        });
        Ok(())
    }
}

// ── Validation ──────────────────────────────────────────────────────────────

/// NVENC needs even dimensions, and both the shim's colour-conversion shaders
/// and the RTP shard maths assume a sane frame. These bounds are a sanity
/// envelope, not a capability claim — the Worker still refuses a mode the VDD
/// cannot commit.
const MIN_DIMENSION: u32 = 640;
const MAX_WIDTH: u32 = 7680;
const MAX_HEIGHT: u32 = 4320;
const MIN_REFRESH: u32 = 24;
const MAX_REFRESH: u32 = 360;
const DEFAULT_REFRESH: u32 = 60;

/// Resolve a resolution shorthand or explicit `WxH` string.
///
/// Accepts the labels a client UI naturally shows ("4K", "1440p", …) as well
/// as `3840x2160`. Returns `None` for anything unrecognised — never a guess,
/// since silently streaming a different resolution than the user selected is
/// worse than an error they can see.
pub fn parse_resolution(text: &str) -> Option<(u32, u32)> {
    let t = text.trim().to_ascii_lowercase();
    match t.as_str() {
        "8k" | "4320p" => return Some((7680, 4320)),
        "4k" | "uhd" | "2160p" => return Some((3840, 2160)),
        "1440p" | "qhd" | "2k" => return Some((2560, 1440)),
        "1080p" | "fhd" => return Some((1920, 1080)),
        "900p" => return Some((1600, 900)),
        "720p" | "hd" => return Some((1280, 720)),
        _ => {}
    }
    // "3840x2160", "3840X2160", "3840*2160"
    let sep = t.find(['x', '*'])?;
    let w: u32 = t[..sep].trim().parse().ok()?;
    let h: u32 = t[sep + 1..].trim().parse().ok()?;
    Some((w, h))
}

fn param_u32(params: &Map<String, Value>, key: &str) -> Option<u32> {
    match params.get(key)? {
        Value::Number(n) => n.as_u64().and_then(|v| u32::try_from(v).ok()),
        // Clients that build the frame from UI text fields send strings.
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn param_bool(params: &Map<String, Value>, key: &str) -> Option<bool> {
    match params.get(key)? {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "on" | "yes" => Some(true),
            "false" | "0" | "off" | "no" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn param_str<'a>(params: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    params.get(key)?.as_str()
}

/// Turn a `set_display` parameter bag into a validated [`DisplayRequest`].
///
/// Geometry may be given either as `res` (shorthand or `WxH`) or as explicit
/// `width`/`height`; explicit values win when both are present. Odd dimensions
/// are rounded **down** to even rather than rejected — NVENC requires even
/// dimensions, and a client that computed 1919 from a scale factor wants the
/// nearest workable mode, not a failure.
pub fn parse_display_request(
    params: &Map<String, Value>,
    default_seat: &str,
) -> Result<DisplayRequest, String> {
    let display_id = param_str(params, "display_id")
        .or_else(|| param_str(params, "device_id"))
        .unwrap_or(default_seat)
        .trim()
        .to_string();
    if display_id.is_empty() {
        return Err("display_id must not be empty".into());
    }

    let explicit = match (param_u32(params, "width"), param_u32(params, "height")) {
        (Some(w), Some(h)) => Some((w, h)),
        (Some(_), None) | (None, Some(_)) => {
            return Err("width and height must be given together".into())
        }
        (None, None) => None,
    };
    let (mut width, mut height) = match explicit {
        Some(wh) => wh,
        None => {
            let res = param_str(params, "res")
                .or_else(|| param_str(params, "resolution"))
                .ok_or("missing resolution: pass \"res\" (e.g. \"4K\" or \"3840x2160\") or width+height")?;
            parse_resolution(res)
                .ok_or_else(|| format!("unrecognised resolution \"{res}\""))?
        }
    };
    // Even-align before the bounds check so a rounded value is still judged
    // against the same envelope.
    width -= width % 2;
    height -= height % 2;

    if width < MIN_DIMENSION || height < MIN_DIMENSION {
        return Err(format!("{width}x{height} is below the {MIN_DIMENSION}px minimum"));
    }
    if width > MAX_WIDTH || height > MAX_HEIGHT {
        return Err(format!("{width}x{height} exceeds the {MAX_WIDTH}x{MAX_HEIGHT} maximum"));
    }

    let refresh_hz = match param_u32(params, "refresh_hz").or_else(|| param_u32(params, "fps")) {
        Some(r) if (MIN_REFRESH..=MAX_REFRESH).contains(&r) => r,
        Some(r) => return Err(format!("refresh {r}Hz is outside {MIN_REFRESH}-{MAX_REFRESH}Hz")),
        None => DEFAULT_REFRESH,
    };

    let hdr = match params.get("hdr") {
        Some(_) => param_bool(params, "hdr").ok_or("hdr must be a boolean")?,
        None => false,
    };
    let force = param_bool(params, "force").unwrap_or(false);

    Ok(DisplayRequest { display_id, width, height, refresh_hz, hdr, force })
}

/// Map a handoff refusal onto this module's error-code vocabulary.
///
/// The session layer keeps its own codes rather than importing the RPC's: it
/// has to be usable (and testable) without a JSON layer at all. This is the one
/// place the two vocabularies meet.
fn handoff_code(e: &HandoffError) -> &'static str {
    match e {
        HandoffError::BadRequest(_) => E_BAD_REQUEST,
        HandoffError::WorkerUnavailable(_) => E_WORKER_UNAVAILABLE,
        other => other.code(),
    }
}

/// Parse a `start_session` parameter bag.
///
/// Geometry accepts the same forms as `set_display` (`res` shorthand or
/// explicit `width`/`height`) so a client builds one resolution picker, not
/// two. Everything else falls back to [`SessionRequest::default`] — a client
/// that sends `{"command":"start_session"}` gets a sane 1080p60 HEVC desktop
/// session rather than an error, because the common case should not require a
/// full parameter set.
pub fn parse_session_request(params: &Map<String, Value>) -> Result<SessionRequest, String> {
    let mut req = SessionRequest::default();

    match (param_u32(params, "width"), param_u32(params, "height")) {
        (Some(w), Some(h)) => {
            req.width = w;
            req.height = h;
        }
        (Some(_), None) | (None, Some(_)) => {
            return Err("width and height must be given together".into())
        }
        (None, None) => {
            if let Some(text) = param_str(params, "res").or_else(|| param_str(params, "resolution")) {
                let (w, h) = parse_resolution(text)
                    .ok_or_else(|| format!("unrecognised resolution \"{text}\""))?;
                req.width = w;
                req.height = h;
            }
        }
    }

    if let Some(fps) = param_u32(params, "fps").or_else(|| param_u32(params, "refresh_hz")) {
        req.fps = fps;
    }
    if params.contains_key("hdr") {
        req.hdr = param_bool(params, "hdr").ok_or("hdr must be a boolean")?;
    }
    if let Some(text) = param_str(params, "codec") {
        req.codec = match text.trim().to_ascii_lowercase().as_str() {
            "h264" | "avc" => Codec::H264,
            "hevc" | "h265" => Codec::Hevc,
            "av1" => Codec::Av1,
            other => return Err(format!("unknown codec \"{other}\" (h264, hevc, av1)")),
        };
    }
    if let Some(kbps) = param_u32(params, "bitrate_kbps").or_else(|| param_u32(params, "bitrate")) {
        req.bitrate_kbps = kbps;
    }
    if let Some(app) = param_u32(params, "app_id") {
        req.app_id = app;
    }
    if params.contains_key("host_audio") {
        req.host_audio = param_bool(params, "host_audio").ok_or("host_audio must be a boolean")?;
    }

    Ok(req)
}

// ── Session view ────────────────────────────────────────────────────────────

/// What the Master knows about the live session, flattened for `get_status`.
///
/// Everything here is Master-side state. Encoder-side telemetry (real encode
/// fps, output bitrate) lives in the Worker's `stats.rs` process-global and is
/// deliberately **not** invented here — surfacing it over Echo needs a
/// Worker→Master push, which is its own change.
fn session_status(client_info: &Arc<Mutex<Option<ClientInfo>>>) -> Value {
    let Some(info) = client_info.lock().unwrap().clone() else {
        return json!({ "active": false });
    };
    json!({
        "active": info.streaming_active,
        "session_generation": info.session_generation,
        "client_ip": info.ip,
        "app_id": info.app_id,
        "width": info.width,
        "height": info.height,
        "fps": info.fps,
        "bitrate_kbps": info.bitrate_kbps,
        "hdr": info.dynamic_range_mode,
    })
}

/// True when a client is mid-stream right now.
fn session_is_live(client_info: &Arc<Mutex<Option<ClientInfo>>>) -> bool {
    client_info
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|i| i.streaming_active)
}

// ── Command handling ────────────────────────────────────────────────────────

/// The command surface, independent of how a client reached it.
///
/// One instance serves both transports — the LAN TCP listener below and the
/// WAN tunnel in [`crate::echo::transport`] — so a command cannot behave
/// differently depending on which door it came through, and neither can the
/// anti-hijack gate. That is the reason this is a shared object rather than
/// two parallel dispatchers.
pub(crate) struct Handler {
    orchestrator: Arc<dyn DisplayOrchestrator>,
    client_info: Arc<Mutex<Option<ClientInfo>>>,
    /// `None` on installs with no relay configured: without WAN there is no
    /// punched path, so there is nothing for a session to be handed off to.
    /// The streaming commands then answer [`E_NO_SESSION_LAYER`] rather than
    /// pretending the command does not exist.
    sessions: Option<Arc<SessionManager>>,
}

impl Handler {
    /// `identity` is established by the TLS handshake before this is ever
    /// called — an unauthorized peer never reaches a command at all, so there
    /// is no in-band auth state to track here.
    fn dispatch(&self, req: RpcRequest, identity: &EchoIdentity) -> RpcResponse {
        let id = req.id;
        if req.command == "hello" {
            return self.handle_hello(id, identity);
        }
        match req.command.as_str() {
            "get_status" => RpcResponse::ok(
                id,
                json!({
                    "protocol_version": PROTOCOL_VERSION,
                    "session": session_status(&self.client_info),
                    "echo_session": self.echo_session_status(),
                    "media_owner": self.media_owner_label(),
                    "displays": self.orchestrator.seats(),
                }),
            ),
            // Telemetry from the client, logged beside the host's own numbers.
            //
            // Purely diagnostic and deliberately trusted for nothing: it is
            // printed and discarded, never used to make a decision, so a client
            // reporting nonsense can mislead a reader of the log and nothing
            // else. It exists because every question about input latency has
            // had one half of its answer on each machine.
            "client_stats" => {
                println!(
                    "📱 Echo client \"{}\": rtt {}ms (best {}ms), capture {}, \
                     input peak {}/s x{} samples, batch {} (worst {})",
                    identity.device_name,
                    req.params.get("rtt_ms").and_then(|v| v.as_u64()).unwrap_or(0),
                    req.params.get("rtt_best_ms").and_then(|v| v.as_u64()).unwrap_or(0),
                    if req.params.get("capture_held").and_then(|v| v.as_bool()).unwrap_or(false) {
                        "held"
                    } else {
                        "off"
                    },
                    req.params.get("peak_events_per_sec").and_then(|v| v.as_u64()).unwrap_or(0),
                    req.params.get("peak_samples_per_event").and_then(|v| v.as_u64()).unwrap_or(0),
                    req.params.get("input_batch").and_then(|v| v.as_u64()).unwrap_or(0),
                    req.params.get("input_batch_worst").and_then(|v| v.as_u64()).unwrap_or(0),
                );
                RpcResponse::ok(id, json!({}))
            }
            "start_session" => self.handle_start_session(id, &req.params, identity),
            "stop_session" => self.handle_stop_session(id, identity),
            "request_idr" => self.handle_request_idr(id, identity),
            "input" => self.handle_input(id, &req.params, identity),
            "list_displays" => {
                let seats = self.orchestrator.seats();
                if seats.is_empty() {
                    RpcResponse::err(
                        id,
                        E_WORKER_UNAVAILABLE,
                        "no worker has reported its capture capabilities yet",
                    )
                } else {
                    RpcResponse::ok(id, json!({ "displays": seats }))
                }
            }
            "set_display" => self.handle_set_display(id, &req.params),
            other => RpcResponse::err(
                id,
                E_UNKNOWN_COMMAND,
                format!("unknown command \"{other}\""),
            ),
        }
    }

    /// `hello` is a version/identity handshake, NOT an authentication step —
    /// the TLS layer already decided that. It exists so a client can discover
    /// the protocol version and what this host actually implements, and can
    /// confirm which paired identity the host resolved it to.
    fn handle_hello(&self, id: Option<u64>, identity: &EchoIdentity) -> RpcResponse {
        RpcResponse::ok(
            id,
            json!({
                "server": "nova",
                "protocol_version": PROTOCOL_VERSION,
                "authenticated": true,
                "device_name": identity.device_name,
                "fingerprint": identity.fingerprint,
                "capabilities": {
                    // Advertised so an Echo build can light up UI only for what
                    // this host actually implements. `hot_format_change` stays
                    // false until the Worker-side rebuild + client
                    // renegotiation handshake exists — a client that trusted it
                    // early would ask for a mid-session change that lands as a
                    // black frame.
                    "set_display": true,
                    "hot_format_change": false,
                    "multi_seat": false,
                    "telemetry_stream": false,
                    // Streaming commands exist only where a session layer is
                    // wired (i.e. a relay is configured). A client that sees
                    // false must not offer a "connect" button.
                    "sessions": self.sessions.is_some(),
                    // Media is sealed per frame with AES-128-GCM (see
                    // nova_core::media_crypto). Advertised as a capability
                    // because a client that cannot decrypt must not connect —
                    // it would render noise rather than fail cleanly.
                    "media_encryption": "aes-128-gcm",
                }
            }),
        )
    }

    fn media_owner_label(&self) -> &'static str {
        match self.sessions.as_ref().map(|s| s.owner()) {
            Some(MediaOwner::Moonlight) => "moonlight",
            Some(MediaOwner::Echo) => "echo",
            Some(MediaOwner::Idle) => "idle",
            // With no session layer, only Moonlight can own the pipeline.
            None if session_is_live(&self.client_info) => "moonlight",
            None => "idle",
        }
    }

    /// The live Echo session, if any. Never includes key material: keys are
    /// handed out once, in the reply to `start_session`, and a status endpoint
    /// that re-served them would turn every subsequent poll into another
    /// chance to leak them.
    fn echo_session_status(&self) -> Value {
        let Some(mgr) = &self.sessions else { return json!({ "active": false }) };
        match mgr.status() {
            None => json!({ "active": false }),
            Some((session_id, device, peer, params, uptime_secs)) => json!({
                "active": true,
                "session_id": session_id,
                "device_name": device,
                "peer": peer.to_string(),
                "uptime_secs": uptime_secs,
                "width": params.width,
                "height": params.height,
                "fps": params.fps,
                "hdr": params.hdr,
                "codec": params.codec.as_str(),
                "bitrate_kbps": params.bitrate_kbps,
            }),
        }
    }

    /// The media handoff. Every refusal here comes from
    /// [`SessionManager::start`], which checks before it touches anything —
    /// see that method for why "denied" and "partially applied" can never be
    /// the same outcome.
    fn handle_start_session(
        &self,
        id: Option<u64>,
        params: &Map<String, Value>,
        identity: &EchoIdentity,
    ) -> RpcResponse {
        let Some(mgr) = &self.sessions else {
            return RpcResponse::err(
                id,
                E_NO_SESSION_LAYER,
                "this host has no signaling relay configured, so no punched path can exist \
                 for a session to use",
            );
        };
        let request = match parse_session_request(params) {
            Ok(r) => r,
            Err(e) => return RpcResponse::err(id, E_BAD_REQUEST, e),
        };

        match mgr.start(identity, request) {
            Ok(grant) => RpcResponse::ok(
                id,
                json!({
                    "session_id": grant.session_id,
                    // Where media will arrive FROM, so the client can reject
                    // datagrams from anywhere else without waiting to fail a
                    // tag check on each one.
                    "peer": grant.peer.to_string(),
                    "width": grant.params.width,
                    "height": grant.params.height,
                    "fps": grant.params.fps,
                    "hdr": grant.params.hdr,
                    "codec": grant.params.codec.as_str(),
                    "bitrate_kbps": grant.params.bitrate_kbps,
                    "packet_size": grant.params.packet_size,
                    "fec_percentage": grant.params.fec_percentage,
                    // Media key material. Safe here and only here: this
                    // channel is mutual TLS against the pairing trust store,
                    // so the transport is already authenticated and encrypted
                    // end to end. See nova_core::media_crypto.
                    "media_key": grant.keys_hex,
                    "rikey": grant.rikey_hex,
                    "rikeyid": grant.rikeyid,
                }),
            ),
            Err(e) => RpcResponse::err(id, handoff_code(&e), e.to_string()),
        }
    }

    fn handle_stop_session(&self, id: Option<u64>, identity: &EchoIdentity) -> RpcResponse {
        let Some(mgr) = &self.sessions else {
            return RpcResponse::err(id, E_NO_SESSION_LAYER, "no session layer on this host");
        };
        match mgr.stop(identity) {
            Ok(session_id) => RpcResponse::ok(id, json!({ "session_id": session_id, "stopped": true })),
            Err(e) => RpcResponse::err(id, handoff_code(&e), e.to_string()),
        }
    }

    /// The client's only repair path.
    ///
    /// With an infinite GOP nothing else will ever produce a keyframe, so a
    /// client that loses its reference chain is stuck forever without this —
    /// and its own keyframe gate will (correctly) refuse to feed the decoder
    /// anything in the meantime.
    fn handle_request_idr(&self, id: Option<u64>, identity: &EchoIdentity) -> RpcResponse {
        let Some(mgr) = &self.sessions else {
            return RpcResponse::err(id, E_NO_SESSION_LAYER, "no session layer on this host");
        };
        match mgr.request_idr(identity) {
            Ok(session_id) => {
                println!("🔑 Echo: \"{}\" requested a keyframe", identity.device_name);
                RpcResponse::ok(id, json!({ "session_id": session_id, "requested": true }))
            }
            Err(e) => RpcResponse::err(id, handoff_code(&e), e.to_string()),
        }
    }

    /// Forward a GameStream input packet to the injection stack.
    ///
    /// The payload is passed through **unparsed**. That is deliberate: the
    /// Master's routing already inspects these bytes to decide between the
    /// SYSTEM input helper (secure desktop) and the Worker (everything else,
    /// including gamepads), and `input.rs` owns every question about what a
    /// packet means. Re-deciding any of that here would create a second
    /// injection path that could drift from Moonlight's.
    ///
    /// Owner-checked: injecting input is the most powerful thing this surface
    /// can do, so it is restricted to the device that actually holds the
    /// session — being paired is not enough.
    fn handle_input(
        &self,
        id: Option<u64>,
        params: &Map<String, Value>,
        identity: &EchoIdentity,
    ) -> RpcResponse {
        let Some(mgr) = &self.sessions else {
            return RpcResponse::err(id, E_NO_SESSION_LAYER, "no session layer on this host");
        };

        // Two shapes: a single `data` packet, or a `packets` array. The array
        // exists because the control channel is one-command-at-a-time and
        // pointer devices generate events far faster than a round trip — a
        // burst has to cost one trip, not one each.
        let encoded: Vec<&str> = match (params.get("packets"), params.get("data")) {
            (Some(Value::Array(items)), _) => {
                if items.len() > MAX_INPUT_BATCH {
                    return RpcResponse::err(id, E_BAD_REQUEST, "too many packets in one batch");
                }
                match items.iter().map(Value::as_str).collect::<Option<Vec<_>>>() {
                    Some(v) => v,
                    None => {
                        return RpcResponse::err(id, E_BAD_REQUEST, "\"packets\" must be hex strings")
                    }
                }
            }
            (_, Some(Value::String(one))) => vec![one.as_str()],
            _ => {
                return RpcResponse::err(
                    id,
                    E_BAD_REQUEST,
                    "input needs a hex \"data\" field or a \"packets\" array",
                )
            }
        };

        // Decode and validate the whole batch BEFORE injecting any of it, so a
        // malformed tail cannot leave half a burst applied — a half-applied
        // batch can strand a mouse button or modifier key held down.
        let mut packets = Vec::with_capacity(encoded.len());
        for hexed in encoded {
            let Ok(packet) = hex::decode(hexed) else {
                return RpcResponse::err(id, E_BAD_REQUEST, "input packet is not valid hex");
            };
            // The injector reads a magic at bytes 4..8 and returns early below
            // 8 bytes; refusing here makes a malformed sender an error it can
            // see rather than input that silently does nothing.
            if packet.len() < 8 {
                return RpcResponse::err(id, E_BAD_REQUEST, "input packet is shorter than its header");
            }
            // Bounded so a hostile peer cannot push bulk data through a command
            // expected to carry a keystroke.
            if packet.len() > MAX_INPUT_PACKET {
                return RpcResponse::err(id, E_BAD_REQUEST, "input packet is implausibly large");
            }
            packets.push(packet);
        }

        let count = packets.len();
        for packet in packets {
            if let Err(e) = mgr.inject_input(identity, packet) {
                return RpcResponse::err(id, handoff_code(&e), e.to_string());
            }
        }
        RpcResponse::ok(id, json!({ "accepted": count }))
    }

    fn handle_set_display(&self, id: Option<u64>, params: &Map<String, Value>) -> RpcResponse {
        let req = match parse_display_request(params, PRIMARY_SEAT) {
            Ok(r) => r,
            Err(e) => return RpcResponse::err(id, E_BAD_REQUEST, e),
        };

        let seats = self.orchestrator.seats();
        if seats.is_empty() {
            return RpcResponse::err(
                id,
                E_WORKER_UNAVAILABLE,
                "no worker has reported its capture capabilities yet",
            );
        }
        let Some(seat) = seats.iter().find(|s| s.id == req.display_id) else {
            let known: Vec<&str> = seats.iter().map(|s| s.id.as_str()).collect();
            return RpcResponse::err(
                id,
                E_UNKNOWN_DISPLAY,
                format!("no display \"{}\"; known: {}", req.display_id, known.join(", ")),
            );
        };
        if !seat.reconfigurable {
            return RpcResponse::err(
                id,
                E_WORKER_UNAVAILABLE,
                format!("display \"{}\" cannot be reconfigured right now", seat.id),
            );
        }

        // The mid-session guard. A Moonlight client fixes its decoder's
        // resolution and dynamic range when the session starts; changing either
        // underneath it produces a black frame with a green region (confirmed
        // live 2026-08-10 — it is why `session_negotiate` pins geometry for a
        // session's whole life and why `WorkerCapabilities` exists at all).
        // Echo will be able to renegotiate its decoder, but that handshake does
        // not exist yet, so the safe default is to refuse and say why.
        if !req.force && session_is_live(&self.client_info) {
            return RpcResponse::err(
                id,
                E_SESSION_LOCKED,
                "a stream is live; changing resolution or HDR mid-session breaks a client's \
                 decoder. Retry with \"force\": true only from a client that can rebuild its \
                 decoder on a format-change event.",
            );
        }

        match self.orchestrator.apply(&req) {
            Ok(()) => {
                println!(
                    "🎛️  Echo: set_display {} → {}x{}@{}Hz{}{}",
                    req.display_id,
                    req.width,
                    req.height,
                    req.refresh_hz,
                    if req.hdr { " HDR10" } else { "" },
                    if req.force { " (forced over a live session)" } else { "" },
                );
                RpcResponse::ok(
                    id,
                    json!({
                        "display_id": req.display_id,
                        "width": req.width,
                        "height": req.height,
                        "refresh_hz": req.refresh_hz,
                        "hdr": req.hdr,
                        // The command is queued to the Worker, not confirmed by
                        // it. Saying "accepted" rather than "applied" keeps the
                        // client honest until the Worker reports back — that
                        // acknowledgement is part of the apply step.
                        "status": "accepted",
                    }),
                )
            }
            Err(e) => RpcResponse::err(id, E_WORKER_UNAVAILABLE, e),
        }
    }
}

// ── Server ──────────────────────────────────────────────────────────────────

/// Start the Echo RPC listener. Call from within the Master's tokio runtime;
/// returns immediately.
///
/// Binds `0.0.0.0` unconditionally — see the module docs: every connection
/// must present a paired client certificate, so there is no configuration
/// state in which this port accepts an unauthenticated command.
pub(crate) fn build_handler(
    orchestrator: Arc<dyn DisplayOrchestrator>,
    client_info: Arc<Mutex<Option<ClientInfo>>>,
    sessions: Option<Arc<SessionManager>>,
) -> Arc<Handler> {
    Arc::new(Handler { orchestrator, client_info, sessions })
}

/// Peers allowed to reach the TCP listener.
///
/// Since the WAN control channel moved onto the punched UDP path
/// ([`crate::echo::transport`]), this port has exactly one remaining job:
/// convenience on a trusted LAN. Restricting it to private address space is
/// therefore free — it removes an internet-facing TCP surface on a LocalSystem
/// service without costing any capability.
///
/// Mutual TLS is still required and still decides authorization; this is a
/// second, cruder fence in front of it. Defence in depth is the point: a flaw
/// in the TLS stack or the trust store is exactly the kind of thing that
/// should not be reachable from the open internet.
fn is_lan_peer(addr: &SocketAddr) -> bool {
    match addr.ip() {
        std::net::IpAddr::V4(ip) => {
            ip.is_loopback() || ip.is_private() || ip.is_link_local()
        }
        std::net::IpAddr::V6(ip) => {
            // Loopback, link-local (fe80::/10), or unique-local (fc00::/7).
            ip.is_loopback()
                || (ip.segments()[0] & 0xffc0) == 0xfe80
                || (ip.segments()[0] & 0xfe00) == 0xfc00
        }
    }
}

#[cfg(test)]
pub(crate) fn is_lan_peer_for_test(addr: &SocketAddr) -> bool {
    is_lan_peer(addr)
}

pub fn spawn(cfg: &EchoConfig, handler: Arc<Handler>) {
    if !cfg.enabled {
        println!("🎛️  Echo RPC disabled by config");
        return;
    }
    let addr = SocketAddr::from(([0, 0, 0, 0], cfg.port));

    tokio::spawn(async move {
        // Wait for pairing.rs to publish Nova's TLS identity. Binding first and
        // configuring TLS afterwards would open a window in which the port
        // exists but the client-cert policy does not — the exact ordering
        // mistake pairing.rs's own "ports open only after TLS is fully
        // configured" comment warns about.
        let Some(tls_config) = await_tls_config().await else {
            println!(
                "❌ Echo RPC: no TLS identity published after {IDENTITY_WAIT:?} — listener not \
                 started (is the pairing server running in this process?)"
            );
            return;
        };
        let acceptor = TlsAcceptor::from(Arc::new(tls_config));

        let listener = match TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                println!("❌ Echo RPC could not bind {addr}: {e}");
                return;
            }
        };
        println!(
            "🎛️  Echo RPC listening on {addr} — LAN only (mutual TLS; WAN clients use the \
             punched path)"
        );

        let live = Arc::new(AtomicUsize::new(0));
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(v) => v,
                Err(e) => {
                    println!("⚠️  Echo RPC accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    continue;
                }
            };
            if !is_lan_peer(&peer) {
                // Dropped before the TLS handshake, so a remote scanner cannot
                // even make this process do certificate work.
                println!("⛔ Echo RPC refusing {peer}: this port is LAN-only — connect over the \
                          Echo WAN path instead");
                drop(stream);
                continue;
            }
            if live.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
                println!("⚠️  Echo RPC refusing {peer}: {MAX_CONNECTIONS} connections already open");
                drop(stream);
                continue;
            }
            // Nagle would coalesce small replies on a channel whose whole point
            // is being responsive.
            let _ = stream.set_nodelay(true);
            live.fetch_add(1, Ordering::Relaxed);
            let handler = handler.clone();
            let acceptor = acceptor.clone();
            let live = live.clone();
            tokio::spawn(async move {
                match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
                    Ok(Ok(tls)) => {
                        // Authorize against the live pairing trust store before
                        // reading a single byte of protocol.
                        let peer_der = {
                            let (_, conn) = tls.get_ref();
                            conn.peer_certificates()
                                .and_then(|c| c.first())
                                .map(|c| c.as_ref().to_vec())
                        };
                        match authorize(peer_der.as_deref(), crate::pairing::trusted_device_name) {
                            Ok(identity) => {
                                println!(
                                    "🔓 Echo RPC {peer} authenticated as \"{}\" (cert {}…)",
                                    identity.device_name,
                                    identity.short_fingerprint()
                                );
                                match serve_connection(tls, handler, &identity).await {
                                    Ok(()) => println!("🔌 Echo RPC {peer} disconnected"),
                                    Err(e) => println!("🔌 Echo RPC {peer} closed: {e}"),
                                }
                            }
                            Err(why) => {
                                println!("⛔ Echo RPC {peer} denied — {why}");
                            }
                        }
                    }
                    Ok(Err(e)) => println!("⚠️  Echo RPC {peer} TLS handshake failed: {e}"),
                    Err(_) => println!("⚠️  Echo RPC {peer} TLS handshake timed out"),
                }
                live.fetch_sub(1, Ordering::Relaxed);
            });
        }
    });
}

/// Poll for the pairing server's published TLS identity. Polling (rather than
/// a notification) because the producer is a `tokio::spawn`ed task started
/// moments earlier in `start_master_network` — this resolves on the first or
/// second tick in every case except a first-run certificate generation.
async fn await_tls_config() -> Option<tokio_rustls::rustls::ServerConfig> {
    let deadline = tokio::time::Instant::now() + IDENTITY_WAIT;
    loop {
        if let Some(cfg) = crate::pairing::client_auth_tls_config() {
            return Some(cfg);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Read one `\n`-terminated line, refusing to buffer more than `max` bytes.
///
/// Hand-rolled over `fill_buf`/`consume` rather than `AsyncBufReadExt::
/// read_line` because that call is unbounded: a peer that never sends a
/// newline would grow the buffer until the SYSTEM service ran out of memory,
/// and `AsyncReadExt::take` — the obvious cap — returns a reader that no
/// longer implements `AsyncBufRead`, so it cannot be combined with line
/// reading at all. Returns `Ok(None)` at a clean EOF.
async fn read_line_bounded(
    reader: &mut (impl AsyncBufRead + Unpin),
    max: usize,
) -> std::io::Result<Option<String>> {
    let mut collected: Vec<u8> = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(if collected.is_empty() {
                None
            } else {
                // Peer closed mid-line: treat the partial as a complete frame
                // so it fails JSON parsing with a clear error instead of
                // vanishing.
                Some(String::from_utf8_lossy(&collected).into_owned())
            });
        }
        match available.iter().position(|&b| b == b'\n') {
            Some(idx) => {
                collected.extend_from_slice(&available[..idx]);
                reader.consume(idx + 1);
                if collected.len() > max {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "request line exceeds the maximum length",
                    ));
                }
                return Ok(Some(String::from_utf8_lossy(&collected).into_owned()));
            }
            None => {
                let n = available.len();
                collected.extend_from_slice(available);
                reader.consume(n);
                if collected.len() > max {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "request line exceeds the maximum length",
                    ));
                }
            }
        }
    }
}

/// Serve NDJSON commands over any authenticated stream.
///
/// Generic over the transport because there are now two: a TLS-over-TCP
/// connection on the LAN, and TLS over the reliable-UDP tunnel on the WAN. The
/// protocol above the stream is byte-for-byte the same in both cases, which is
/// the property worth preserving — a WAN client and a LAN client are the same
/// client to everything downstream of here.
pub(crate) async fn serve_connection<S>(
    stream: S,
    handler: Arc<Handler>,
    identity: &EchoIdentity,
) -> std::io::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);

    loop {
        // No idle deadline: an authenticated client is allowed to sit quiet
        // between commands — this is a control channel, not a request/response
        // API. The unauthenticated case that needed a deadline no longer
        // exists; TLS settles identity before this function is reached.
        let line = match read_line_bounded(&mut reader, MAX_LINE_BYTES).await {
            Ok(Some(l)) => l,
            Ok(None) => return Ok(()), // peer closed
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                let resp = RpcResponse::err(None, E_BAD_REQUEST, e.to_string());
                let _ = write_response(&mut write_half, &resp).await;
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue; // keepalive newline
        }

        // A request with no id is a notification: acted on, not answered.
        // Input rides this path, because a reply per pointer event serialises
        // the client's whole input stream behind a round trip each.
        let (expects_reply, response) = match serde_json::from_str::<RpcRequest>(trimmed) {
            Ok(req) => {
                let expects_reply = req.id.is_some();
                (expects_reply, handler.dispatch(req, identity))
            }
            Err(e) => (true, RpcResponse::err(None, E_BAD_REQUEST, format!("malformed request: {e}"))),
        };

        // Failures are always reported, even for notifications — an input burst
        // refused for lack of a session must not vanish silently. The response
        // carries no id, and the client skips such lines when matching replies.
        if expects_reply || !response.ok {
            write_response(&mut write_half, &response).await?;
        }
    }
}

async fn write_response(
    out: &mut (impl AsyncWriteExt + Unpin),
    resp: &RpcResponse,
) -> std::io::Result<()> {
    // Serialization of our own types cannot fail in practice; if it somehow
    // did, dropping the reply silently would leave the client hanging on an id
    // that never comes back, so send a minimal well-formed error instead.
    let mut buf = serde_json::to_vec(resp)
        .unwrap_or_else(|_| br#"{"ok":false,"error":{"code":"internal","message":"encode failed"}}"#.to_vec());
    buf.push(b'\n');
    out.write_all(&buf).await?;
    out.flush().await
}

// Re-export so callers constructing the orchestrator don't also need to import
// `ipc` just to name the message this module sends.
pub use ipc::ControlMsg as EchoControlMsg;

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct MockOrchestrator {
        seats: Vec<DisplaySeat>,
        applied: Mutex<Vec<DisplayRequest>>,
        fail_with: Option<String>,
    }

    impl MockOrchestrator {
        fn with_primary() -> Arc<Self> {
            Arc::new(Self {
                seats: vec![seat(PRIMARY_SEAT, true)],
                ..Default::default()
            })
        }
        /// A virtual (reconfigurable) or physical (capture-only) seat.
        fn with_seats(seats: Vec<DisplaySeat>) -> Arc<Self> {
            Arc::new(Self { seats, ..Default::default() })
        }
    }

    fn seat(id: &str, virtual_display: bool) -> DisplaySeat {
        DisplaySeat {
            id: id.to_string(),
            label: "Test display".into(),
            width: 1920,
            height: 1080,
            refresh_hz: 60,
            is_primary: true,
            reconfigurable: virtual_display,
            virtual_display,
            hdr_active: false,
            hdr_capable: false,
        }
    }

    impl DisplayOrchestrator for Arc<MockOrchestrator> {
        fn seats(&self) -> Vec<DisplaySeat> {
            self.seats.clone()
        }
        fn apply(&self, req: &DisplayRequest) -> Result<(), String> {
            if let Some(e) = &self.fail_with {
                return Err(e.clone());
            }
            self.applied.lock().unwrap().push(req.clone());
            Ok(())
        }
    }

    fn handler_with(orch: Arc<MockOrchestrator>, info: Option<ClientInfo>) -> Handler {
        Handler {
            orchestrator: Arc::new(orch),
            client_info: Arc::new(Mutex::new(info)),
            sessions: None,
        }
    }

    fn test_identity() -> EchoIdentity {
        EchoIdentity {
            fingerprint: "a".repeat(64),
            device_name: "Xbox".to_string(),
        }
    }

    fn request(json_text: &str) -> RpcRequest {
        serde_json::from_str(json_text).expect("parse request")
    }

    fn live_session() -> ClientInfo {
        ClientInfo { streaming_active: true, ..Default::default() }
    }

    #[test]
    fn resolution_shorthand_and_explicit_forms_parse() {
        assert_eq!(parse_resolution("4K"), Some((3840, 2160)));
        assert_eq!(parse_resolution(" 1440p "), Some((2560, 1440)));
        assert_eq!(parse_resolution("1280x720"), Some((1280, 720)));
        assert_eq!(parse_resolution("2560X1440"), Some((2560, 1440)));
        // Never guess: an unrecognised label is an error the user can see, not
        // a silent fallback to some default mode.
        assert_eq!(parse_resolution("cinema"), None);
        assert_eq!(parse_resolution(""), None);
    }

    #[test]
    fn set_display_params_validate_and_normalise() {
        let params = |t: &str| -> Map<String, Value> {
            serde_json::from_str(t).expect("params")
        };

        // The exact shape from the Echo spec.
        let req = parse_display_request(
            &params(r#"{"device_id":"xbox_1","res":"4K","hdr":true}"#),
            PRIMARY_SEAT,
        )
        .expect("valid");
        assert_eq!(req.display_id, "xbox_1");
        assert_eq!((req.width, req.height), (3840, 2160));
        assert!(req.hdr);
        assert_eq!(req.refresh_hz, DEFAULT_REFRESH);

        // Explicit width/height beats `res`, and odd dimensions round down to
        // even for NVENC rather than failing.
        let req = parse_display_request(
            &params(r#"{"res":"720p","width":1921,"height":1081,"fps":120}"#),
            PRIMARY_SEAT,
        )
        .expect("valid");
        assert_eq!((req.width, req.height), (1920, 1080));
        assert_eq!(req.refresh_hz, 120);

        for bad in [
            r#"{"width":1920}"#,                     // height missing
            r#"{"res":"potato"}"#,                   // unrecognised
            r#"{"res":"320x240"}"#,                  // below the floor
            r#"{"res":"99999x4320"}"#,               // above the ceiling
            r#"{"res":"1080p","fps":1000}"#,         // refresh out of range
            r#"{"res":"1080p","hdr":"maybe"}"#,      // non-boolean hdr
            r#"{}"#,                                 // no geometry at all
        ] {
            assert!(
                parse_display_request(&params(bad), PRIMARY_SEAT).is_err(),
                "expected rejection for {bad}"
            );
        }
    }

    /// Authorization is a pure function of the presented certificate and the
    /// trust store, so it is testable without a TLS handshake — which is the
    /// whole reason it was factored out of the accept loop.
    #[test]
    fn only_a_paired_certificate_is_authorized() {
        // The fingerprint the lookup recognises is whatever pairing computes
        // for these DER bytes — derive it the same way rather than hardcoding.
        let paired_der = b"pretend-certificate-der";
        let paired_fp = crate::pairing::fingerprint_of_cert(paired_der);
        let store = {
            let fp = paired_fp.clone();
            move |presented: &str| {
                (presented == fp).then(|| "Living Room Xbox".to_string())
            }
        };

        let ok = authorize(Some(paired_der), &store).expect("paired cert must be authorized");
        assert_eq!(ok.device_name, "Living Room Xbox");
        assert_eq!(ok.fingerprint, paired_fp);

        // A cert Nova has never paired with.
        assert!(authorize(Some(b"some-other-cert"), &store).is_err());
        // No client cert at all (rustls should prevent this; refuse anyway).
        assert!(authorize(None, &store).is_err());
    }

    /// A device that was paired and then revoked (tray "Clear Paired
    /// Devices") must be refused on its next connection — the store is
    /// consulted per connection, never cached.
    #[test]
    fn revoked_devices_lose_access_on_the_next_connection() {
        let der = b"cert-der";
        let revoked = |_: &str| None::<String>;
        assert!(authorize(Some(der), &revoked).is_err());
    }

    #[test]
    fn hello_reports_the_authenticated_identity() {
        let orch = MockOrchestrator::with_primary();
        let h = handler_with(orch, None);
        let resp = h.dispatch(request(r#"{"id":2,"command":"hello"}"#), &test_identity());
        assert!(resp.ok);
        assert_eq!(resp.id, Some(2));
        let result = resp.result.expect("hello result");
        assert_eq!(result["device_name"], "Xbox");
        // Hot format change stays advertised as unavailable until the Worker
        // apply path and the client's decoder-rebuild handshake both exist.
        assert_eq!(result["capabilities"]["hot_format_change"], false);
    }

    #[test]
    fn set_display_routes_a_validated_request_to_the_orchestrator() {
        let orch = MockOrchestrator::with_primary();
        let h = handler_with(orch.clone(), None);

        let resp = h.dispatch(
            request(r#"{"id":7,"command":"set_display","res":"1440p","hdr":true,"fps":120}"#),
            &test_identity(),
        );
        assert!(resp.ok, "expected success, got {:?}", resp.error);
        assert_eq!(resp.id, Some(7));

        let applied = orch.applied.lock().unwrap();
        assert_eq!(applied.len(), 1);
        assert_eq!(
            applied[0],
            DisplayRequest {
                display_id: PRIMARY_SEAT.to_string(),
                width: 2560,
                height: 1440,
                refresh_hz: 120,
                hdr: true,
                force: false,
            }
        );
    }

    #[test]
    fn unknown_display_is_rejected_without_reaching_the_orchestrator() {
        let orch = MockOrchestrator::with_primary();
        let h = handler_with(orch.clone(), None);

        let resp = h.dispatch(
            request(r#"{"command":"set_display","device_id":"seat_2","res":"1080p"}"#),
            &test_identity(),
        );
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, E_UNKNOWN_DISPLAY);
        assert!(orch.applied.lock().unwrap().is_empty());
    }

    /// Multi-seat targeting: with a real inventory the client selects a seat
    /// by its GDI device name, and a physical monitor is listed (it is a valid
    /// capture target) but refused for mode changes.
    #[test]
    fn seats_are_addressed_by_device_name_and_physical_ones_are_not_reconfigurable() {
        let orch = MockOrchestrator::with_seats(vec![
            seat(r"\\.\DISPLAY1", false),
            seat(r"\\.\DISPLAY9", true),
        ]);
        let h = handler_with(orch.clone(), None);

        let resp = h.dispatch(
            request(r#"{"command":"set_display","display_id":"\\\\.\\DISPLAY9","res":"4K"}"#),
            &test_identity(),
        );
        assert!(resp.ok, "virtual seat should accept a mode change: {:?}", resp.error);
        assert_eq!(orch.applied.lock().unwrap()[0].display_id, r"\\.\DISPLAY9");

        let resp = h.dispatch(
            request(r#"{"command":"set_display","display_id":"\\\\.\\DISPLAY1","res":"4K"}"#),
            &test_identity(),
        );
        assert!(!resp.ok, "physical monitor must not be re-moded by a remote client");
        assert_eq!(resp.error.unwrap().code, E_WORKER_UNAVAILABLE);
        assert_eq!(orch.applied.lock().unwrap().len(), 1, "no second apply");
    }

    /// The Worker's inventory is what makes a seat targetable; check the
    /// conversion preserves identity and the reconfigurable policy.
    #[test]
    fn inventory_entries_become_addressable_seats() {
        let entry = ipc::DisplayEntry {
            device_name: r"\\.\DISPLAY9".to_string(),
            label: "Virtual Display Driver".to_string(),
            width: 3840,
            height: 2160,
            refresh_hz: 120,
            is_primary: true,
            is_virtual: true,
            hdr_active: true,
            hdr_capable: true,
        };
        let s = DisplaySeat::from_entry(&entry);
        assert_eq!(s.id, r"\\.\DISPLAY9");
        assert!(s.reconfigurable && s.virtual_display && s.hdr_active);

        let physical = ipc::DisplayEntry { is_virtual: false, ..entry };
        let s = DisplaySeat::from_entry(&physical);
        assert!(!s.reconfigurable, "physical displays are capture targets, not Nova's to re-mode");
    }

    /// The guard that keeps this RPC from being a way to black out a live
    /// Moonlight client — see `handle_set_display`.
    #[test]
    fn a_live_session_blocks_set_display_unless_forced() {
        let orch = MockOrchestrator::with_primary();
        let h = handler_with(orch.clone(), Some(live_session()));

        let resp = h.dispatch(
            request(r#"{"command":"set_display","res":"4K"}"#),
            &test_identity(),
        );
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, E_SESSION_LOCKED);
        assert!(orch.applied.lock().unwrap().is_empty());

        let resp = h.dispatch(
            request(r#"{"command":"set_display","res":"4K","force":true}"#),
            &test_identity(),
        );
        assert!(resp.ok, "force should override the guard");
        assert_eq!(orch.applied.lock().unwrap().len(), 1);
    }

    /// A client that sends nothing but the command must get a usable session,
    /// not an error — the common case should not require a full parameter set.
    #[test]
    fn start_session_defaults_to_a_sane_desktop_session() {
        let empty = Map::new();
        let req = parse_session_request(&empty).expect("bare start_session is valid");
        assert_eq!(req, SessionRequest::default());

        let params: Map<String, Value> = serde_json::from_str(
            r#"{"res":"4K","fps":120,"codec":"av1","bitrate_kbps":80000,"hdr":true,"host_audio":true}"#,
        )
        .unwrap();
        let req = parse_session_request(&params).expect("valid");
        assert_eq!((req.width, req.height), (3840, 2160));
        assert_eq!(req.fps, 120);
        assert_eq!(req.codec, Codec::Av1);
        assert_eq!(req.bitrate_kbps, 80_000);
        assert!(req.hdr && req.host_audio);

        for bad in [
            r#"{"width":1920}"#,
            r#"{"res":"potato"}"#,
            r#"{"codec":"vp9"}"#,
            r#"{"hdr":"sometimes"}"#,
        ] {
            let params: Map<String, Value> = serde_json::from_str(bad).unwrap();
            assert!(parse_session_request(&params).is_err(), "expected rejection for {bad}");
        }
    }

    /// On a host with no relay there is no punched path, so the streaming
    /// commands answer with a distinct code rather than pretending to be
    /// unknown — a client can tell "not configured" from "not supported".
    #[test]
    fn streaming_commands_report_a_missing_session_layer_distinctly() {
        let h = handler_with(MockOrchestrator::with_primary(), None);

        for command in [r#"{"command":"start_session"}"#, r#"{"command":"stop_session"}"#] {
            let resp = h.dispatch(request(command), &test_identity());
            assert!(!resp.ok);
            assert_eq!(resp.error.unwrap().code, E_NO_SESSION_LAYER);
        }

        // …and `hello` says so up front, so a client never offers the button.
        let resp = h.dispatch(request(r#"{"command":"hello"}"#), &test_identity());
        assert_eq!(resp.result.unwrap()["capabilities"]["sessions"], false);
    }

    #[test]
    fn unknown_commands_and_malformed_ids_do_not_panic() {
        let orch = MockOrchestrator::with_primary();
        let h = handler_with(orch, None);
        let resp = h.dispatch(request(r#"{"id":3,"command":"reboot_host"}"#), &test_identity());
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, E_UNKNOWN_COMMAND);
    }
}
