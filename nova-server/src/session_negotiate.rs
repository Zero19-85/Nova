//! Pure Master-side codec/HDR/fps negotiation — decides what the Worker
//! should encode, given what the client asked for and what `nova.toml`
//! allows. Extracted from `lib.rs`'s PLAY-handling block (Session-Survival
//! Architecture, Phase 2 — see the approved plan,
//! `transient-snuggling-cosmos.md`) with the arithmetic itself UNCHANGED.
//!
//! One thing HAS since been added to it: the bitrate the client requests is no
//! longer passed straight through, but bounded by [`crate::qos::video_budget`]
//! (resolution cap, then audio reservation). That belongs here rather than in
//! the Worker for the same reason the codec decision does — Master decides once,
//! and every Worker that adopts the session inherits the same ceiling.
//!
//! This exists specifically because `RtpSender::set_codec()` needs the
//! *actual* codec/fps the Worker ends up running, not the client's raw
//! request — Master must make this decision ONCE and ship the Worker an
//! unambiguous instruction, rather than have both sides guess independently
//! (which is exactly how a Worker respawn could silently diverge from what
//! Master's `RtpSender` is configured for).
//!
//! Deliberately narrower than the old lib.rs code it replaces: this only
//! covers the PLAY-time decision, when ANNOUNCE's `dynamic_range_mode` is
//! authoritative. The pre-activation latency-hiding optimization (starting
//! the VDD/encoder during the `/launch`→PLAY gap, before ANNOUNCE arrives)
//! is a known, deliberate simplification of this first Phase 2 pass, not an
//! oversight — see the approved plan's Phase 2 scope note. A fast-follow can
//! reintroduce it once the core IPC path is proven live.

use crate::config::{NetworkConfig, StreamConfig};
use crate::encoder::Codec;
use crate::rtsp::ClientInfo;

#[derive(Debug, Clone)]
pub struct NegotiatedParams {
    pub width: u32,
    pub height: u32,
    /// Already H264-Level-5.2-capped when `codec == Codec::H264` — this is
    /// the fps the encoder/RTP layer should actually run at, not necessarily
    /// what the client asked for (see [`negotiate`]'s cap logic).
    pub fps: u32,
    pub codec: Codec,
    /// ANNOUNCE-confirmed HDR (or the `nova.toml` operator override) — NOT
    /// the same as `ClientInfo::hdr_requested`, which only reflects what the
    /// user asked for, not what the client can actually decode.
    pub hdr_confirmed: bool,
    /// What the video encoder is allowed to run at — already
    /// resolution-capped and audio-reserved by [`crate::qos::video_budget`], NOT
    /// the raw `maximumBitrateKbps` the client asked for. This becomes NVENC's
    /// CBR ceiling and `QosController`'s ramp target, so the cap reaches the
    /// congestion controller for free.
    pub bitrate_kbps: u32,
    pub app_id: u32,
    /// This session came from `/launch` (not `/resume`) — the Worker should
    /// start the app's process once the VDD is active.
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

/// What the currently-connected Worker can physically sustain, as reported by
/// `ControlMsg::WorkerCapabilities`. `None` = no Worker has reported yet;
/// negotiate then assumes a fully-capable one (the pre-existing behavior).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerCaps {
    pub vdd_capable: bool,
    pub native_width: u32,
    pub native_height: u32,
}

/// Computes what the Worker should stream, from the client's negotiated
/// [`ClientInfo`] (populated by `rtsp.rs`/`pairing.rs` by the time RTSP PLAY
/// arrives) and the operator's `nova.toml`. Pure — no VDD/encoder/capture
/// side effects; the Worker performs those once it receives the resulting
/// `ConfigureStart` IPC message built from this.
///
/// `caps` is informational only (Master logs what the live Worker can capture).
/// It does NOT constrain the result: the shim decouples the encoder's geometry
/// and colour space from the capture's, so any Worker can serve any session —
/// which is precisely what makes a mid-session Worker handoff invisible to the
/// client's decoder.
pub fn negotiate(
    client: &ClientInfo,
    cfg: &StreamConfig,
    net: &NetworkConfig,
    caps: Option<WorkerCaps>,
) -> NegotiatedParams {
    let _ = caps;
    // Derive codec from /launch videoFormat. Old-protocol clients (Xbox
    // Moonlight <= 1.18.0) never set videoFormat — it arrives as 0. For
    // those, use bitStreamFormat from the ANNOUNCE SDP instead: set by
    // moonlight-common-c from (client caps ∩ server ServerCodecModeSupport),
    // authoritative for the wire codec regardless of protocol version.
    let raw_codec = if client.video_format != 0 {
        Codec::from_video_format(client.video_format)
    } else {
        match client.bit_stream_format {
            1 => Codec::Hevc,
            2 => Codec::Av1,
            _ => Codec::H264,
        }
    };

    // HDR10 requires HEVC Main10. dynamic_range_mode==1 (ANNOUNCE-confirmed)
    // or nova.toml's enable_hdr override upgrades to HEVC when the client
    // would otherwise land on H264 — NEVER use hdr_requested alone, it
    // reflects user intent, not decode capability (forcing HEVC on a client
    // with no HEVC decoder is a guaranteed 10s watchdog timeout).
    let hdr_confirmed = client.dynamic_range_mode == 1 || cfg.enable_hdr;
    let codec = if hdr_confirmed && raw_codec == Codec::H264 {
        Codec::Hevc
    } else {
        raw_codec
    };

    // The client always gets exactly the geometry it asked for. What the host
    // can capture at any given moment (the physical monitor at the logon
    // screen, the VDD once signed in) is now independent of it: the shim scales
    // the capture into the encoder's session-sized surface and cross-converts
    // SDR↔HDR as needed. That decoupling is what lets a Worker handoff happen
    // mid-session without touching the client's decoder.
    let (width, height) = (client.width, client.height);

    // H264 Level 5.2 fps cap: Xbox Moonlight 1.18.0 (corever=1) hardwires
    // H264 and cannot negotiate HEVC server-side; at 4K/1440p@120fps that
    // exceeds Level 5.2 (983,040 MB/s) and crashes the Xbox hardware H264
    // decoder. Cap fps to what Level 5.2 allows instead of streaming garbage.
    let fps = {
        let mb_per_frame = ((width + 15) / 16) as u64 * ((height + 15) / 16) as u64;
        let mb_per_sec = mb_per_frame * client.fps as u64;
        if codec == Codec::H264 && mb_per_sec > 983_040 {
            (983_040u64 / mb_per_frame.max(1)).max(1) as u32
        } else {
            client.fps
        }
    };

    // What the client asked for is a request, not an entitlement: Moonlight's
    // bitrate slider reaches 150 Mbps at every resolution, and honouring that at
    // 1080p spends bandwidth no amount of detail can consume — then teaches the
    // congestion controller to keep climbing back to it. Cap by mode, reserve
    // audio's slice, and hand the remainder to the encoder. Computed against the
    // NEGOTIATED fps (already H264-Level-5.2-capped above), never the client's
    // request, so a capped session is not also budgeted for frames it will never
    // send.
    let budget = crate::qos::video_budget(
        client.bitrate_kbps, width, height, fps, net.audio_reserve_kbps,
    );
    if let Some(line) = budget.describe(width, height, fps) {
        println!("{line}");
    }

    NegotiatedParams {
        width,
        height,
        fps,
        codec,
        hdr_confirmed,
        bitrate_kbps: budget.video_kbps,
        app_id: client.app_id,
        launch_app: client.pending_app_launch,
        device_name: client.device_name.clone(),
        rikey: client.rikey,
        rikeyid: client.rikeyid,
        host_audio: client.host_audio,
        audio_encryption: client.audio_encryption,
        audio_packet_duration_ms: client.audio_packet_duration,
        packet_size: client.packet_size,
        min_fec_packets: client.min_fec_packets,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_client() -> ClientInfo {
        ClientInfo { width: 3840, height: 2160, fps: 120, ..Default::default() }
    }

    #[test]
    fn h264_over_level_5_2_gets_capped() {
        let mut client = base_client();
        client.video_format = 0x0001; // H264
        let n = negotiate(&client, &StreamConfig::default(), &NetworkConfig::default(), None);
        assert_eq!(n.codec, Codec::H264);
        assert!(n.fps < 120, "expected an fps cap below 120, got {}", n.fps);
    }

    #[test]
    fn hevc_is_never_capped() {
        let mut client = base_client();
        client.video_format = 0x0002; // HEVC Main
        let n = negotiate(&client, &StreamConfig::default(), &NetworkConfig::default(), None);
        assert_eq!(n.codec, Codec::Hevc);
        assert_eq!(n.fps, 120);
    }

    #[test]
    fn confirmed_hdr_upgrades_h264_to_hevc() {
        let mut client = base_client();
        client.video_format = 0x0001; // client asked for H264
        client.dynamic_range_mode = 1; // but ANNOUNCE confirmed HDR
        let n = negotiate(&client, &StreamConfig::default(), &NetworkConfig::default(), None);
        assert_eq!(n.codec, Codec::Hevc);
        assert!(n.hdr_confirmed);
    }

    #[test]
    fn unconfirmed_hdr_does_not_upgrade_codec() {
        let mut client = base_client();
        client.video_format = 0x0001;
        client.hdr_requested = true; // user asked...
        client.dynamic_range_mode = 0; // ...but ANNOUNCE declined
        let n = negotiate(&client, &StreamConfig::default(), &NetworkConfig::default(), None);
        assert_eq!(n.codec, Codec::H264);
        assert!(!n.hdr_confirmed);
    }

    #[test]
    fn av1_selected_via_bit_stream_format_when_video_format_unset() {
        let mut client = base_client();
        client.video_format = 0; // old-protocol client never set it
        client.bit_stream_format = 2; // AV1
        let n = negotiate(&client, &StreamConfig::default(), &NetworkConfig::default(), None);
        assert_eq!(n.codec, Codec::Av1);
    }

    /// The bitrate the client asks for must arrive at the Worker already
    /// bounded — this is the seam where the budget either takes effect or
    /// silently doesn't. Checked here rather than only in `qos` because
    /// `NegotiatedParams::bitrate_kbps` is what becomes NVENC's ceiling AND
    /// `QosController`'s ramp target: if the raw request leaked through, the
    /// congestion controller would spend every session climbing back to it.
    #[test]
    fn the_negotiated_bitrate_is_budgeted_not_the_raw_request() {
        let mut client = base_client();
        client.width = 1920;
        client.height = 1080;
        client.fps = 60;
        client.video_format = 0x0002; // HEVC
        client.bitrate_kbps = 100_000; // the slider went to 100 Mbps at 1080p

        let net = NetworkConfig { fec_percentage: 5, audio_reserve_kbps: 512 };
        let n = negotiate(&client, &StreamConfig::default(), &net, None);
        assert_eq!(n.bitrate_kbps, 40_000 - 512, "1080p cap, less the audio reserve");

        // With no reservation configured, the cap alone applies.
        let net = NetworkConfig { fec_percentage: 5, audio_reserve_kbps: 0 };
        let n = negotiate(&client, &StreamConfig::default(), &net, None);
        assert_eq!(n.bitrate_kbps, 40_000);
    }

    /// The budget must be computed against the fps the session will ACTUALLY
    /// run at. An Xbox asking for 4K120 H264 is capped to ~24 fps by Level 5.2,
    /// so budgeting it for 120 fps would hand the encoder bandwidth for frames
    /// it is never going to send.
    #[test]
    fn the_budget_follows_the_capped_fps_not_the_requested_one() {
        let mut client = base_client(); // 3840x2160@120
        client.video_format = 0x0001; // H264 ⇒ Level 5.2 fps cap applies
        client.bitrate_kbps = 500_000;

        let net = NetworkConfig { fec_percentage: 5, audio_reserve_kbps: 0 };
        let n = negotiate(&client, &StreamConfig::default(), &net, None);
        assert!(n.fps < 120, "precondition: fps was capped, got {}", n.fps);
        assert_eq!(
            n.bitrate_kbps,
            crate::qos::resolution_ceiling(3840, 2160, n.fps),
            "budget must use the negotiated fps"
        );
        assert!(
            n.bitrate_kbps < crate::qos::resolution_ceiling(3840, 2160, 120),
            "a 24 fps session must not be budgeted like a 120 fps one"
        );
    }

    /// The client's requested geometry and HDR profile survive regardless of
    /// what the serving Worker can capture. A VDD-less Worker (the SYSTEM
    /// fallback covering the logon screen) captures the physical monitor in
    /// SDR, and the shim scales + converts it into the session's 4K HDR10
    /// surface — so the session is NOT downgraded, which is what allows a
    /// Worker handoff mid-session without disturbing the client's decoder.
    #[test]
    fn worker_capability_never_downgrades_the_session() {
        let mut client = base_client(); // 3840x2160
        client.video_format = 0x0002; // HEVC
        client.dynamic_range_mode = 1; // ANNOUNCE-confirmed HDR
        let caps = WorkerCaps { vdd_capable: false, native_width: 2560, native_height: 1440 };
        let n = negotiate(&client, &StreamConfig::default(), &NetworkConfig::default(), Some(caps));
        assert_eq!((n.width, n.height), (3840, 2160), "session keeps the client's geometry");
        assert!(n.hdr_confirmed, "session keeps HDR — the shim converts an SDR capture");
        assert_eq!(n.codec, Codec::Hevc);

        // A fully-capable Worker must of course reach the same conclusion.
        let caps = WorkerCaps { vdd_capable: true, native_width: 2560, native_height: 1440 };
        let full = negotiate(&client, &StreamConfig::default(), &NetworkConfig::default(), Some(caps));
        assert_eq!((full.width, full.height), (n.width, n.height));
        assert_eq!(full.hdr_confirmed, n.hdr_confirmed);
    }
}
