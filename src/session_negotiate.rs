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
/// `caps` constrains the result to what the Worker that will actually serve
/// this session can do. A Moonlight client fixes its decoder's resolution and
/// HDR profile when the session starts; changing either mid-session wrecks it
/// (live 2026-08-10: black frame + green region after a lock-screen session
/// was upgraded to 4K/HDR at sign-in). A SYSTEM-fallback Worker can never
/// drive the VDD (`SetDisplayConfig` is denied on the Winlogon desktop), so a
/// session negotiated while one is live is pinned to the physical monitor's
/// native size in SDR — and STAYS there for its whole life, including after
/// the sign-in handoff to a full Worker. Reconnecting once signed in
/// negotiates a fresh, unconstrained (4K/HDR) session.
pub fn negotiate(client: &ClientInfo, cfg: &StreamConfig, caps: Option<WorkerCaps>) -> NegotiatedParams {
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
    // A Worker that can't drive the VDD can't produce HDR either: HDR10 needs
    // the VDD flipped into FP16/Advanced Color, which is the same denied
    // SetDisplayConfig call. Pin such a session to SDR up front rather than
    // letting the Worker silently serve SDR into a session the client built an
    // HDR decoder for.
    let constrained = caps.is_some_and(|c| !c.vdd_capable);
    let hdr_confirmed = !constrained && (client.dynamic_range_mode == 1 || cfg.enable_hdr);
    let codec = if hdr_confirmed && raw_codec == Codec::H264 {
        Codec::Hevc
    } else {
        raw_codec
    };

    // Pin the session to what the serving Worker can actually capture. Never
    // UPscale the request (a client asking for 720p on a 1440p monitor still
    // gets 720p — the VDD-less path just captures the monitor and the encoder
    // follows the capture, so only the "client wants more than the monitor
    // has" direction needs clamping).
    let (width, height) = match caps {
        Some(c) if !c.vdd_capable && c.native_width > 0 && c.native_height > 0 => {
            let w = client.width.min(c.native_width);
            let h = client.height.min(c.native_height);
            if (w, h) != (client.width, client.height) {
                println!(
                    "📐 Master: session pinned to {w}x{h} SDR — the live Worker has no VDD \
                     (logon screen); this geometry holds for the whole session so a sign-in \
                     handoff can't break the client's decoder. Reconnect once signed in for \
                     {}x{}/HDR.",
                    client.width, client.height
                );
            }
            (w, h)
        }
        _ => (client.width, client.height),
    };

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

    /// A Worker with no VDD (the SYSTEM fallback covering the logon screen)
    /// can only serve the physical monitor in SDR. The session must be pinned
    /// to that up front, because a Moonlight client fixes its decoder at
    /// session start and a later change wrecks it (live 2026-08-10).
    #[test]
    fn vdd_less_worker_pins_session_to_its_native_size_and_sdr() {
        let mut client = base_client(); // 3840x2160
        client.video_format = 0x0002; // HEVC
        client.dynamic_range_mode = 1; // client confirmed HDR...
        let caps = WorkerCaps { vdd_capable: false, native_width: 2560, native_height: 1440 };
        let n = negotiate(&client, &StreamConfig::default(), Some(caps));
        assert_eq!((n.width, n.height), (2560, 1440), "clamped to the monitor");
        assert!(!n.hdr_confirmed, "...but HDR needs the VDD in FP16 — pin to SDR");
        assert_eq!(n.codec, Codec::Hevc, "codec choice is unaffected by the clamp");
    }

    /// The clamp only ever reduces: a client asking for LESS than the monitor
    /// keeps its own smaller request (the VDD-less path captures the monitor
    /// and the encoder follows the capture).
    #[test]
    fn vdd_less_worker_never_upscales_a_smaller_request() {
        let mut client = base_client();
        client.width = 1280;
        client.height = 720;
        let caps = WorkerCaps { vdd_capable: false, native_width: 2560, native_height: 1440 };
        let n = negotiate(&client, &StreamConfig::default(), Some(caps));
        assert_eq!((n.width, n.height), (1280, 720));
    }

    /// A full Worker imposes no constraint — the client's request stands.
    #[test]
    fn vdd_capable_worker_leaves_the_request_alone() {
        let mut client = base_client();
        client.video_format = 0x0002;
        client.dynamic_range_mode = 1;
        let caps = WorkerCaps { vdd_capable: true, native_width: 2560, native_height: 1440 };
        let n = negotiate(&client, &StreamConfig::default(), Some(caps));
        assert_eq!((n.width, n.height), (3840, 2160));
        assert!(n.hdr_confirmed);
    }
}
