//! Pure Master-side codec/HDR/fps negotiation — decides what the Worker
//! should encode, given what the client asked for and what `nova.toml`
//! allows. Extracted from `lib.rs`'s PLAY-handling block (Session-Survival
//! Architecture, Phase 2 — see the approved plan,
//! `transient-snuggling-cosmos.md`) with the arithmetic itself UNCHANGED.
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

use crate::config::StreamConfig;
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
pub fn negotiate(client: &ClientInfo, cfg: &StreamConfig, caps: Option<WorkerCaps>) -> NegotiatedParams {
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

    NegotiatedParams {
        width,
        height,
        fps,
        codec,
        hdr_confirmed,
        bitrate_kbps: client.bitrate_kbps,
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
        let n = negotiate(&client, &StreamConfig::default(), None);
        assert_eq!(n.codec, Codec::H264);
        assert!(n.fps < 120, "expected an fps cap below 120, got {}", n.fps);
    }

    #[test]
    fn hevc_is_never_capped() {
        let mut client = base_client();
        client.video_format = 0x0002; // HEVC Main
        let n = negotiate(&client, &StreamConfig::default(), None);
        assert_eq!(n.codec, Codec::Hevc);
        assert_eq!(n.fps, 120);
    }

    #[test]
    fn confirmed_hdr_upgrades_h264_to_hevc() {
        let mut client = base_client();
        client.video_format = 0x0001; // client asked for H264
        client.dynamic_range_mode = 1; // but ANNOUNCE confirmed HDR
        let n = negotiate(&client, &StreamConfig::default(), None);
        assert_eq!(n.codec, Codec::Hevc);
        assert!(n.hdr_confirmed);
    }

    #[test]
    fn unconfirmed_hdr_does_not_upgrade_codec() {
        let mut client = base_client();
        client.video_format = 0x0001;
        client.hdr_requested = true; // user asked...
        client.dynamic_range_mode = 0; // ...but ANNOUNCE declined
        let n = negotiate(&client, &StreamConfig::default(), None);
        assert_eq!(n.codec, Codec::H264);
        assert!(!n.hdr_confirmed);
    }

    #[test]
    fn av1_selected_via_bit_stream_format_when_video_format_unset() {
        let mut client = base_client();
        client.video_format = 0; // old-protocol client never set it
        client.bit_stream_format = 2; // AV1
        let n = negotiate(&client, &StreamConfig::default(), None);
        assert_eq!(n.codec, Codec::Av1);
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
        let n = negotiate(&client, &StreamConfig::default(), Some(caps));
        assert_eq!((n.width, n.height), (3840, 2160), "session keeps the client's geometry");
        assert!(n.hdr_confirmed, "session keeps HDR — the shim converts an SDR capture");
        assert_eq!(n.codec, Codec::Hevc);

        // A fully-capable Worker must of course reach the same conclusion.
        let caps = WorkerCaps { vdd_capable: true, native_width: 2560, native_height: 1440 };
        let full = negotiate(&client, &StreamConfig::default(), Some(caps));
        assert_eq!((full.width, full.height), (n.width, n.height));
        assert_eq!(full.hdr_confirmed, n.hdr_confirmed);
    }
}
