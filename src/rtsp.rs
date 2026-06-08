use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

#[derive(Clone, Debug, Default)]
pub struct ClientInfo {
    pub ip: String,
    pub rtp_port: u16,
    pub rtcp_port: u16,
    pub session_id: String,
    /// Set to true once PLAY is received — main loop can use this as "go" signal for RTP
    pub streaming_active: bool,
    /// Server-side RTP port we told the client (future hook for your rtp.rs)
    pub server_rtp_port: u16,
}

pub fn start_rtsp_server(port: u16, client_info: Arc<Mutex<Option<ClientInfo>>>) {
    let listener = TcpListener::bind(("0.0.0.0", port)).expect("Failed to bind RTSP port 48010");
    println!("🎥 RTSP server listening on port {} (Moonlight/GameStream)", port);

    for stream in listener.incoming() {
        if let Ok(stream) = stream {
            let info = client_info.clone();
            thread::spawn(move || {
                if let Err(e) = handle_client(stream, info) {
                    eprintln!("RTSP client handler error: {}", e);
                }
            });
        }
    }
}

fn handle_client(mut stream: TcpStream, client_info: Arc<Mutex<Option<ClientInfo>>>) -> std::io::Result<()> {
    let peer       = stream.peer_addr().map(|a| a.to_string()).unwrap_or_else(|_| "unknown".to_string());
    let client_ip  = stream.peer_addr().map(|a| a.ip().to_string()).unwrap_or_else(|_| "127.0.0.1".to_string());
    println!("🎥 New RTSP connection from {}", peer);

    let mut reader     = BufReader::new(stream.try_clone()?);
    let mut session_id = "12345678".to_string();

    // Persistent-connection loop — handles OPTIONS → DESCRIBE → SETUP → PLAY on the same TCP stream.
    loop {
        let mut request = String::new();

        loop {
            let mut line = String::new();
            let bytes_read = reader.read_line(&mut line)?;
            if bytes_read == 0 {
                return Ok(()); // client closed connection
            }
            request.push_str(&line);
            if line.trim().is_empty() {
                break; // end of RTSP headers
            }
        }

        if request.trim().is_empty() {
            return Ok(());
        }

        println!("📥 RTSP from {}:\n{}", peer, request.trim());

        let cseq                    = extract_cseq(&request).unwrap_or(1);
        let req_session             = extract_session_id(&request);
        let (client_rtp, client_rtcp) = extract_client_ports(&request);

        // Keep session_id stable across SETUP/PLAY/TEARDOWN on this connection.
        if let Some(s) = req_session {
            if !s.is_empty() { session_id = s; }
        }

        if request.contains("SETUP") {
            let mut guard = client_info.lock().unwrap();
            let mut info  = guard.take().unwrap_or_default();
            info.ip              = client_ip.clone();
            info.rtp_port        = client_rtp;
            info.rtcp_port       = client_rtcp;
            info.session_id      = session_id.clone();
            info.server_rtp_port = 47998;
            *guard = Some(info);
        }

        if request.contains("PLAY") {
            let mut guard = client_info.lock().unwrap();
            let mut info  = guard.take().unwrap_or_default();
            info.ip              = client_ip.clone();
            info.rtp_port        = client_rtp.max(info.rtp_port); // keep port from SETUP if PLAY has 0
            info.streaming_active = true;
            *guard = Some(info);
            println!("🚀 PLAY received — streaming_active = true");
        }

        if request.contains("TEARDOWN") {
            let mut guard = client_info.lock().unwrap();
            if let Some(ref mut info) = *guard {
                info.streaming_active = false;
            }
            println!("🛑 TEARDOWN — streaming stopped");
        }

        let response = if request.contains("OPTIONS") {
            format!(
                "RTSP/1.0 200 OK\r\nCSeq: {}\r\nPublic: OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN\r\n\r\n",
                cseq
            )
        } else if request.contains("DESCRIBE") {
            // profile-level-id 42001e = Baseline 3.0, widely compatible.
            // sprop-parameter-sets carries a placeholder SPS/PPS; Moonlight will use
            // the in-band SPS/PPS from the first IDR (NVENC repeatSPSPPS=1).
            let sdp = "v=0\r\n\
o=- 0 0 IN IP4 0.0.0.0\r\n\
s=Nova Server\r\n\
t=0 0\r\n\
a=control:*\r\n\
m=video 47998 RTP/AVP 96\r\n\
a=rtpmap:96 H264/90000\r\n\
a=fmtp:96 packetization-mode=1;profile-level-id=42001e\r\n\
a=control:streamid=0\r\n";
            format!(
                "RTSP/1.0 200 OK\r\nCSeq: {}\r\nContent-Base: rtsp://0.0.0.0:48010/\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{}",
                cseq, sdp.len(), sdp
            )
        } else if request.contains("SETUP") {
            format!(
                "RTSP/1.0 200 OK\r\nCSeq: {}\r\nTransport: RTP/AVP;unicast;client_port={}-{};server_port=47998-47999\r\nSession: {};timeout=60\r\n\r\n",
                cseq, client_rtp, client_rtcp, session_id
            )
        } else if request.contains("PLAY") {
            format!("RTSP/1.0 200 OK\r\nCSeq: {}\r\nSession: {}\r\n\r\n", cseq, session_id)
        } else if request.contains("TEARDOWN") {
            stream.write_all(
                format!("RTSP/1.0 200 OK\r\nCSeq: {}\r\nSession: {}\r\n\r\n", cseq, session_id).as_bytes()
            )?;
            return Ok(()); // connection intentionally closed after TEARDOWN
        } else {
            format!("RTSP/1.0 404 Not Found\r\nCSeq: {}\r\n\r\n", cseq)
        };

        stream.write_all(response.as_bytes())?;
        println!("📤 RTSP → {} (CSeq={})", peer, cseq);
    }
}

// === Helpers (robust extraction) ===
fn extract_cseq(request: &str) -> Option<u32> {
    for line in request.lines() {
        let lower = line.to_lowercase();
        if lower.starts_with("cseq:") {
            return line.split(':').nth(1).and_then(|s| s.trim().parse().ok());
        }
    }
    None
}

fn extract_session_id(request: &str) -> Option<String> {
    for line in request.lines() {
        let lower = line.to_lowercase();
        if lower.starts_with("session:") {
            return line.split(':').nth(1).map(|s| s.trim().to_string());
        }
    }
    None
}

fn extract_client_ports(request: &str) -> (u16, u16) {
    for line in request.lines() {
        let lower = line.to_lowercase();
        if lower.contains("client_port=") {
            if let Some(ports_part) = line.split("client_port=").nth(1) {
                let ports = ports_part.split(';').next().unwrap_or("");
                let mut parts = ports.split('-');
                let rtp = parts.next().and_then(|p| p.trim().parse().ok()).unwrap_or(50000);
                let rtcp = parts.next().and_then(|p| p.trim().parse().ok()).unwrap_or(50001);
                return (rtp, rtcp);
            }
        }
    }
    (50000, 50001)
}