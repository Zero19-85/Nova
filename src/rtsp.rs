use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

use rand::Rng;

#[derive(Clone, Debug, Default)]
pub struct ClientInfo {
    pub ip: String,
    pub rtp_port: u16,
    pub rtcp_port: u16,
    pub session_id: String,
    pub streaming_active: bool,
    pub server_rtp_port: u16,
    pub rikey: [u8; 16],
    pub rikeyid: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// App ID currently streaming (0 = none); mirrored in /serverinfo currentgame
    pub app_id: u32,
    /// Negotiated video packet size from ANNOUNCE SDP (x-nv-video[0].packetSize).
    /// Moonlight uses 1392 on LAN, 1024 for remote. 0 = not announced yet.
    /// The RTP/FEC shard size MUST match this — the client reconstructs lost
    /// packets over shards of exactly this size.
    pub packet_size: u32,
    /// Client's x-nv-vqos[0].fec.minRequiredFecPackets (0 = not announced).
    pub min_fec_packets: u32,
    /// Client's requested video bitrate in Kbps (x-nv-vqos[0].bw.maximumBitrateKbps,
    /// same attribute Sunshine reads — rtsp.cpp:1003). 0 = not announced.
    /// The encoder MUST be reconfigured to this: with CBR the encoder holds
    /// its configured rate constantly, so exceeding what the client asked for
    /// saturates the link/client and Moonlight aborts ("lower your bitrate").
    pub bitrate_kbps: u32,
    /// Client requested audio encryption (x-nv-general.featureFlags bit 0x20,
    /// or x-ss-general.encryptionEnabled bit 0x1). Audio payloads must then be
    /// AES-128-CBC encrypted with the /launch rikey.
    pub audio_encryption: bool,
    /// x-nv-aqos.packetDuration in ms (0 = not announced; default 5).
    pub audio_packet_duration: u32,
    /// /launch localAudioPlayMode=1 — keep playing audio on the host speakers
    /// while streaming. Default false = client-only: audio is routed through a
    /// virtual sink so the host speakers stay silent (never muted).
    pub host_audio: bool,
    /// Outgoing 0x0001 control envelope sequence counter (control.rs
    /// send_control_reply). Sunshine: session->control.seq, incremented per
    /// host->client encrypted control message; used in the legacy 16-byte IV.
    pub control_out_seq: u32,
    /// Set once `VirtualDisplay::activate_for_stream` has run for this
    /// launch/resume cycle — lets the capture loop pre-activate during the
    /// /launch -> RTSP PLAY gap (see lib.rs) without redoing the slow
    /// devcon/CCD work again when the control stream actually connects.
    /// Reset to `false` on every /launch and /resume.
    pub activated: bool,
    /// Video format bitmask from /launch `videoFormat` param
    /// (1=H264, 2=HEVC Main, 0x102=HEVC Main10). 0 = not reported.
    /// Must match the encoder codec or the client will receive the wrong
    /// NAL type and display a black screen.
    pub video_format: u32,
    /// Client explicitly requested HDR via /launch `hdrMode=1`.
    pub hdr_requested: bool,
    /// `x-nv-vqos[0].bitStreamFormat` from the RTSP ANNOUNCE SDP — the codec
    /// the client will actually put on the wire: 0=H264, 1=HEVC, 2=AV1.
    /// Arrives after `/launch videoFormat` (which is the primary negotiation
    /// source); used as a belt-and-suspenders cross-check.
    pub bit_stream_format: u32,
    /// `x-nv-video[0].dynamicRangeMode` from the RTSP ANNOUNCE SDP.
    /// 0 = SDR, 1 = HDR10. This is the authoritative source for whether
    /// the client actually negotiated HDR — more reliable than the /launch
    /// hdrMode flag, which reflects what the user requested but not what
    /// the codec intersection actually produced. HDR encoding must only
    /// be activated when this field is 1.
    pub dynamic_range_mode: u32,
    /// Set by the /cancel HTTP handler; cleared by the capture loop after the
    /// full VDD teardown completes. Distinguishes an intentional "Quit App"
    /// (teardown VDD, restore host topology) from a natural network disconnect
    /// (suspend: keep VDD alive so /resume can reconnect without flicker).
    pub cancelled: bool,
    /// Monotonic session counter — bumped by every /launch and /resume in
    /// pairing.rs. The control thread stamps each connecting ENet peer with
    /// the generation current at connect time and ignores Disconnect events
    /// whose stamp no longer matches. Quitting Moonlight on Xbox never sends
    /// an ENet disconnect, so the old peer lingers until its 10–30 s timeout
    /// fires — without the stamp, that timeout lands mid-/resume and tears
    /// down the freshly started session (client kicked back to the app list).
    pub session_generation: u64,
    /// True after the 0x010e HDR mode control packet has been sent this session.
    /// Apollo sends this once at stream start; Nova sends it on the first
    /// PT_PERIODIC_PING (encoder is running by then). Without it, Moonlight
    /// never calls RequestSetCurrentDisplayModeAsync(Eotf2084) and the TV
    /// stays in SDR regardless of VUI or SEI settings.
    pub hdr_mode_sent: bool,
    /// Friendly name of the connected client device (e.g. "Xbox", "Steam Deck"),
    /// looked up from `nova_paired.json` via the `uniqueid` in `/launch`.
    /// Used to rename the virtual display devnode so Device Manager and Display
    /// Settings show the client name instead of "VDD by MTT".
    pub device_name: String,
}

// Fixed session token, matches Sunshine's hardcoded "DEADBEEFCAFE".
const SESSION_ID: &str = "DEADBEEFCAFE";

pub fn start_rtsp_server(port: u16, client_info: Arc<Mutex<Option<ClientInfo>>>) {
    let listener = TcpListener::bind(("0.0.0.0", port)).expect("Failed to bind RTSP port 48010");
    println!("🎥 RTSP server listening on port {} (Moonlight/GameStream)", port);

    // Sunshine-style per-session ping/connect tokens, generated once at startup
    // and echoed back in SETUP responses via X-SS-Ping-Payload / X-SS-Connect-Data.
    let av_ping_payload = generate_ping_payload();
    let control_connect_data: u32 = rand::thread_rng().r#gen();

    for stream in listener.incoming() {
        if let Ok(stream) = stream {
            let _ = stream.set_nodelay(true);
            let info = client_info.clone();
            let ping = av_ping_payload.clone();
            thread::spawn(move || {
                if let Err(e) = handle_message(stream, info, &ping, control_connect_data) {
                    eprintln!("RTSP handler error: {}", e);
                }
            });
        }
    }
}

fn generate_ping_payload() -> String {
    let bytes: [u8; 8] = rand::thread_rng().r#gen();
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

// ── Raw-byte response builder — every header line ends with exactly 0x0D 0x0A ─

fn rtsp_send(stream: &mut TcpStream, label: &str, buf: &[u8]) -> std::io::Result<()> {
    println!("📤 RTSP {} ← {} bytes:\n{}", label, buf.len(),
        String::from_utf8_lossy(buf).trim_end());
    stream.write_all(buf)?;
    stream.flush()?;
    Ok(())
}

// Sunshine's cmd_option: only CSeq, nothing else.
fn resp_options(cseq: u32) -> Vec<u8> {
    let mut r = Vec::with_capacity(48);
    r.extend_from_slice(b"RTSP/1.0 200 OK\r\n");
    r.extend_from_slice(format!("CSeq: {}\r\n", cseq).as_bytes());
    r.extend_from_slice(b"\r\n");
    r
}

// Sunshine's cmd_describe: CSeq + a payload of bare "a=" lines (LF-terminated,
// not full SDP — moonlight-common-c already knows the fixed GameStream ports).
//
// `is_hdr`: when true, includes `a=x-nv-video[0].dynamicRangeMode:1` so the
// client allocates a 10-bit HDR (P010) decoder surface. Without this line the
// client defaults to SDR (NV12), receives our P010 bitstream, and the chroma
// planes corrupt immediately (green tint). This line must arrive in the DESCRIBE
// response (before the client's ANNOUNCE) so the surface is allocated correctly.
fn resp_describe(cseq: u32, is_hdr: bool) -> Vec<u8> {
    let mut sdp = Vec::new();
    sdp.extend_from_slice(b"a=x-ss-general.featureFlags:0\n");
    // SS_ENC_AUDIO=0x1, SS_ENC_CONTROL_V2=0x4 (Sunshine rtsp.cpp:768-769).
    // Requesting CONTROL_V2 is required: without it Moonlight encrypts the
    // 0x0001 control envelopes (IDR requests, etc.) with the legacy 16-byte
    // IV scheme, which AES-128-GCM (ring, 96-bit nonces only) can't decrypt —
    // every control message then fails AEAD verification.
    sdp.extend_from_slice(b"a=x-ss-general.encryptionSupported:5\n");
    sdp.extend_from_slice(b"a=x-ss-general.encryptionRequested:4\n");
    // HEVC capability signal that moonlight-common-c actually reads.
    // Sunshine rtsp.cpp:792-794: `if (active_hevc_mode != 1) ss << "sprop-parameter-sets=AAAAAU"`.
    // This bare line (no "a=" prefix) is what triggers clientSupportHevc:1 and
    // bitStreamFormat:1 (HEVC) in the client's ANNOUNCE. The "a=x-nv-video[0].clientSupportHevc:1"
    // attribute we were sending previously is NOT what Sunshine sends and is ignored by corever=1 clients.
    sdp.extend_from_slice(b"sprop-parameter-sets=AAAAAU\n");
    // AV1 capability (Sunshine rtsp.cpp:796-798: `if (active_av1_mode != 1) ss << "a=rtpmap:98 AV1/90000"`).
    sdp.extend_from_slice(b"a=rtpmap:98 AV1/90000\n");
    // stereo: channelCount=2, streams=1, coupledStreams=1, mapping=[0,1]
    sdp.extend_from_slice(b"a=fmtp:97 surround-params=21101\n");
    if is_hdr {
        // Declare HDR10 mode to the client. Moonlight reads this during DESCRIBE
        // to decide which decoder surface type to allocate (NV12 for SDR, P010
        // for HDR). If absent, the client defaults to SDR and immediately corrupts
        // a P010 stream because the chroma planes are sized for 8-bit NV12.
        sdp.extend_from_slice(b"a=x-nv-video[0].dynamicRangeMode:1\n");
    }

    let mut r = Vec::with_capacity(128 + sdp.len());
    r.extend_from_slice(b"RTSP/1.0 200 OK\r\n");
    r.extend_from_slice(format!("CSeq: {}\r\n", cseq).as_bytes());
    r.extend_from_slice(b"Content-Type: application/sdp\r\n");
    r.extend_from_slice(format!("Content-Length: {}\r\n", sdp.len()).as_bytes());
    r.extend_from_slice(b"\r\n");
    r.extend_from_slice(&sdp);
    r
}

// Sunshine's cmd_setup: CSeq, Session (with spaces around '='), Transport,
// and an X-SS-Ping-Payload (audio/video) or X-SS-Connect-Data (control) header.
fn resp_setup(cseq: u32, server_port: u16, ss_header: &str, ss_value: &str) -> Vec<u8> {
    let mut r = Vec::with_capacity(160);
    r.extend_from_slice(b"RTSP/1.0 200 OK\r\n");
    r.extend_from_slice(format!("CSeq: {}\r\n", cseq).as_bytes());
    r.extend_from_slice(format!("Session: {};timeout = 90\r\n", SESSION_ID).as_bytes());
    r.extend_from_slice(format!("Transport: server_port={}\r\n", server_port).as_bytes());
    r.extend_from_slice(format!("{}: {}\r\n", ss_header, ss_value).as_bytes());
    r.extend_from_slice(b"\r\n");
    r
}

// Sunshine's cmd_announce / cmd_play: only CSeq.
fn resp_cseq_only(cseq: u32) -> Vec<u8> {
    let mut r = Vec::with_capacity(48);
    r.extend_from_slice(b"RTSP/1.0 200 OK\r\n");
    r.extend_from_slice(format!("CSeq: {}\r\n", cseq).as_bytes());
    r.extend_from_slice(b"\r\n");
    r
}

fn resp_not_found(cseq: u32) -> Vec<u8> {
    let mut r = Vec::with_capacity(48);
    r.extend_from_slice(b"RTSP/1.0 404 NOT FOUND\r\n");
    r.extend_from_slice(format!("CSeq: {}\r\n", cseq).as_bytes());
    r.extend_from_slice(b"\r\n");
    r
}

// ── Main per-connection handler ───────────────────────────────────────────────
//
// Mirrors Sunshine's rtsp_server_t::handle_msg: handle exactly ONE RTSP
// message per TCP connection, then sock.shutdown(shutdown_both). Moonlight
// opens a fresh connection for each subsequent message (DESCRIBE, SETUP x N,
// ANNOUNCE, PLAY); our listener's accept loop spawns a new thread for each.

fn handle_message(
    mut stream: TcpStream,
    client_info: Arc<Mutex<Option<ClientInfo>>>,
    av_ping_payload: &str,
    control_connect_data: u32,
) -> std::io::Result<()> {
    let peer      = stream.peer_addr().map(|a| a.to_string()).unwrap_or_else(|_| "?".to_string());
    let client_ip = stream.peer_addr().map(|a| a.ip().to_string()).unwrap_or_else(|_| "127.0.0.1".to_string());

    let mut reader = BufReader::new(stream.try_clone()?);

    // ── Read request headers until blank line ────────────────────────────
    let mut request        = String::new();
    let mut content_length = 0usize;

    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            // Empty probe connection — nothing to do.
            return Ok(());
        }
        if line.to_ascii_lowercase().starts_with("content-length:") {
            content_length = line.splitn(2, ':').nth(1)
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0);
        }
        request.push_str(&line);
        if line == "\r\n" || line == "\n" { break; }
    }

    if request.trim().is_empty() {
        return Ok(());
    }

    println!("🎥 New RTSP connection from {}", peer);

    // ── Read body (ANNOUNCE carries an SDP payload) ──────────────────────
    let mut body = Vec::new();
    if content_length > 0 {
        body.resize(content_length, 0);
        reader.read_exact(&mut body)?;
    }

    let method = request.split_whitespace().next().unwrap_or("").to_string();
    let cseq   = extract_cseq(&request).unwrap_or(1);
    let (client_rtp, client_rtcp) = extract_client_ports(&request);

    println!("📥 RTSP {} {} (CSeq={}, client_port={}/{})",
        peer, method, cseq, client_rtp, client_rtcp);

    // ── Side-effects (state updates) before building the response ────────
    match method.as_str() {
        "SETUP" => {
            // Only latch the video SETUP client_port — audio/control use separate ports.
            let is_video = !request.contains("streamid=audio")
                && !request.contains("streamid=control");
            if is_video && client_rtp > 0 {
                let mut guard = client_info.lock().unwrap();
                let mut info  = guard.take().unwrap_or_default();
                info.ip              = client_ip.clone();
                info.rtp_port        = client_rtp;
                info.rtcp_port       = client_rtcp;
                info.session_id      = SESSION_ID.to_string();
                info.server_rtp_port = 47998;
                *guard = Some(info);
                println!("   ↳ video SETUP: will send RTP to {}:{}", client_ip, client_rtp);
            }
        }
        "PLAY" => {
            let mut guard = client_info.lock().unwrap();
            let mut info  = guard.take().unwrap_or_default();
            info.ip               = client_ip.clone();
            info.streaming_active = true;
            let rtp = info.rtp_port;
            *guard = Some(info);
            println!("🚀 PLAY — streaming_active=true  target={}:{}", client_ip, rtp);
        }
        "TEARDOWN" => {
            let mut guard = client_info.lock().unwrap();
            if let Some(ref mut info) = *guard {
                info.streaming_active = false;
                info.app_id = 0;
            }
            println!("🛑 TEARDOWN — streaming stopped");
        }
        "ANNOUNCE" => {
            // The ANNOUNCE SDP carries the client's negotiated stream params.
            let sdp = String::from_utf8_lossy(&body);
            let packet_size      = parse_sdp_u32(&sdp, "x-nv-video[0].packetSize");
            let fps              = parse_sdp_u32(&sdp, "x-nv-video[0].maxFPS");
            let width            = parse_sdp_u32(&sdp, "x-nv-video[0].clientViewportWd");
            let height           = parse_sdp_u32(&sdp, "x-nv-video[0].clientViewportHt");
            let min_fec          = parse_sdp_u32(&sdp, "x-nv-vqos[0].fec.minRequiredFecPackets");
            let bitrate          = parse_sdp_u32(&sdp, "x-nv-vqos[0].bw.maximumBitrateKbps");
            let feat_flags       = parse_sdp_u32(&sdp, "x-nv-general.featureFlags").unwrap_or(0);
            let enc_enabled      = parse_sdp_u32(&sdp, "x-ss-general.encryptionEnabled").unwrap_or(0);
            let pkt_dur          = parse_sdp_u32(&sdp, "x-nv-aqos.packetDuration");
            // bitStreamFormat: 0=H264, 1=HEVC, 2=AV1 — the codec the client
            // will actually put on the wire. Lives under the vqos[0] namespace
            // in all observed Xbox/Android Moonlight ANNOUNCE SDPs.
            let bit_stream_fmt   = parse_sdp_u32(&sdp, "x-nv-vqos[0].bitStreamFormat");
            // dynamicRangeMode: 0=SDR, 1=HDR10. Authoritative source for
            // whether HDR was actually negotiated (vs hdrMode in /launch which
            // is a user request). Only 1 when server advertised SCM_HEVC_MAIN10
            // (bit 0x100 in ServerCodecModeSupport) AND client supports HDR.
            let dynamic_range    = parse_sdp_u32(&sdp, "x-nv-video[0].dynamicRangeMode");
            // clientSupportHevc: 1 if the client supports and negotiated HEVC.
            let client_hevc      = parse_sdp_u32(&sdp, "x-nv-clientSupportHevc").unwrap_or(0);

            let mut guard = client_info.lock().unwrap();
            let mut info  = guard.take().unwrap_or_default();
            if let Some(v) = packet_size  { info.packet_size = v; }
            if let Some(v) = fps          { info.fps = v; }
            if let Some(v) = width        { info.width = v; }
            if let Some(v) = height       { info.height = v; }
            if let Some(v) = min_fec      { info.min_fec_packets = v; }
            if let Some(v) = bitrate      { info.bitrate_kbps = v; }
            if let Some(v) = pkt_dur      { info.audio_packet_duration = v; }
            if let Some(v) = bit_stream_fmt { info.bit_stream_format = v; }
            // dynamicRangeMode: if explicitly present in the ANNOUNCE, use it as-is.
            // If ABSENT (not 0 — just missing from the SDP), infer HDR when /launch
            // requested it AND the client confirmed HEVC (bitStreamFormat=1). Only HEVC
            // carries HDR10/Main10; an absent field with HEVC+hdrMode=1 is a reliable
            // proxy. If the client explicitly sent dynamicRangeMode:0 (Xbox), that hard
            // decline is preserved unchanged — absent ≠ declined.
            if let Some(v) = dynamic_range {
                info.dynamic_range_mode = v;
            } else if info.hdr_requested && bit_stream_fmt == Some(1) {
                info.dynamic_range_mode = 1;
                println!("   ↳ ANNOUNCE: dynamicRangeMode absent — inferred HDR10 \
                    (hdrMode=1 + bitStreamFormat=HEVC)");
            }
            // Sunshine rtsp.cpp:982-987 — legacy nv flag 0x20 or Sunshine
            // extension bit 0x1 both mean "encrypt audio".
            info.audio_encryption = (feat_flags & 0x20) != 0 || (enc_enabled & 0x1) != 0;
            let bsf_name = match info.bit_stream_format {
                1 => "HEVC", 2 => "AV1", _ => "H264",
            };
            let drm_name = if info.dynamic_range_mode == 1 { "HDR10" } else { "SDR" };
            println!("   ↳ ANNOUNCE codec: bitStreamFormat={} ({}) dynamicRangeMode={} ({}) clientSupportHevc={}",
                info.bit_stream_format, bsf_name, info.dynamic_range_mode, drm_name, client_hevc);
            // Log the authoritative HDR decision. dynamicRangeMode=0 with hdr_requested=true
            // means the client (e.g. Xbox Moonlight 1.18.0) cannot do HEVC/HDR10 — Nova will
            // revert the VDD to SDR and stream H264 regardless of /launch hdrMode.
            if info.hdr_requested && info.dynamic_range_mode == 0 {
                println!("   ⚠️  ANNOUNCE: client declined HDR (dynamicRangeMode=0) despite /launch hdrMode=1 \
                    — will revert VDD to SDR and stream H264 (client lacks HEVC/HDR10 decoder)");
            } else if info.dynamic_range_mode == 1 {
                println!("   ✅ ANNOUNCE: client confirmed HDR (dynamicRangeMode=1) — HEVC Main10 pipeline active");
            }
            println!("   ↳ ANNOUNCE audio: encryption={} packetDuration={:?}ms",
                info.audio_encryption, pkt_dur);
            println!("   ↳ ANNOUNCE: packetSize={:?} maxFPS={:?} viewport={:?}x{:?} minFec={:?} bitrateKbps={:?}",
                packet_size, fps, width, height, min_fec, bitrate);
            // Always print the full ANNOUNCE SDP so codec/capability issues are
            // immediately visible in the log during Xbox testing.
            println!("   📋 ANNOUNCE SDP:\n{}", sdp.trim_end());
            if packet_size.is_none() {
                println!("   ⚠️  packetSize missing from ANNOUNCE SDP (see dump above)");
            }
            *guard = Some(info);
        }
        _ => {}
    }

    // ── Build raw-byte response ───────────────────────────────────────────
    let response = match method.as_str() {
        "OPTIONS"  => resp_options(cseq),
        "DESCRIBE" => {
            // Use both hdr_requested (/launch hdrMode=1) AND dynamic_range_mode
            // (persisted from a previous ANNOUNCE in reconnect scenarios) so the
            // DESCRIBE surface-allocation hint is always accurate.
            //
            // hdr_requested: set by /launch before RTSP starts — primary source.
            // dynamic_range_mode: persists from a prior ANNOUNCE in the same
            //   client_info slot; covers reconnect paths where /launch isn't
            //   re-issued but the client still expects an HDR decoder surface.
            let is_hdr = client_info.lock().unwrap()
                .as_ref()
                .map(|info| info.hdr_requested || info.dynamic_range_mode == 1)
                .unwrap_or(false);
            if is_hdr {
                println!("   ↳ DESCRIBE: advertising dynamicRangeMode=1 (HDR10 decoder surface)");
            }
            resp_describe(cseq, is_hdr)
        }
        "SETUP" => {
            let (server_port, ss_header, ss_value): (u16, &str, String) =
                if request.contains("streamid=audio") {
                    (48000, "X-SS-Ping-Payload", av_ping_payload.to_string())
                } else if request.contains("streamid=control") {
                    (47999, "X-SS-Connect-Data", control_connect_data.to_string())
                } else {
                    (47998, "X-SS-Ping-Payload", av_ping_payload.to_string())
                };
            resp_setup(cseq, server_port, ss_header, &ss_value)
        }
        "ANNOUNCE" | "PLAY" | "TEARDOWN" => resp_cseq_only(cseq),
        _ => {
            eprintln!("⚠️  RTSP unknown method '{}' from {}", method, peer);
            resp_not_found(cseq)
        }
    };

    rtsp_send(&mut stream, &peer, &response)?;

    // Sunshine: sock.shutdown(shutdown_both) after every response.
    stream.shutdown(Shutdown::Both)?;
    println!("🔌 RTSP {} — closed after {}", peer, method);

    Ok(())
}

/// Extract a numeric SDP attribute value, e.g. "a=x-nv-video[0].packetSize:1392".
fn parse_sdp_u32(sdp: &str, key: &str) -> Option<u32> {
    let pos  = sdp.find(key)?;
    let rest = sdp[pos + key.len()..].strip_prefix(':')?;
    let end  = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

// ── Header extraction helpers ─────────────────────────────────────────────────

fn extract_cseq(req: &str) -> Option<u32> {
    req.lines()
        .find(|l| l.to_ascii_lowercase().starts_with("cseq:"))
        .and_then(|l| l.splitn(2, ':').nth(1))
        .and_then(|s| s.trim().parse().ok())
}

fn extract_client_ports(req: &str) -> (u16, u16) {
    for line in req.lines() {
        if line.to_ascii_lowercase().contains("client_port=") {
            if let Some(part) = line.split("client_port=").nth(1) {
                let seg = part.split(';').next().unwrap_or("");
                let mut it = seg.split('-');
                let rtp  = it.next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
                let rtcp = it.next().and_then(|s| s.trim().parse().ok()).unwrap_or(rtp + 1);
                return (rtp, rtcp);
            }
        }
    }
    (0, 0)
}
