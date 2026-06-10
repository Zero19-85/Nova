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
fn resp_describe(cseq: u32) -> Vec<u8> {
    let mut sdp = Vec::new();
    sdp.extend_from_slice(b"a=x-ss-general.featureFlags:0\n");
    sdp.extend_from_slice(b"a=x-ss-general.encryptionSupported:0\n");
    sdp.extend_from_slice(b"a=x-ss-general.encryptionRequested:0\n");
    // stereo: channelCount=2, streams=1, coupledStreams=1, mapping=[0,1]
    sdp.extend_from_slice(b"a=fmtp:97 surround-params=21101\n");

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
        _ => {}
    }

    // ── Build raw-byte response ───────────────────────────────────────────
    let response = match method.as_str() {
        "OPTIONS"  => resp_options(cseq),
        "DESCRIBE" => resp_describe(cseq),
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
