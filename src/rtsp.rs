use std::net::TcpListener;
use std::io::{BufRead, BufReader, Write};
use std::thread;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, Default)]
pub struct ClientInfo {
    pub ip: String,
    pub rtp_port: u16,
    pub rtcp_port: u16,
    pub session_id: String,
}

pub fn start_rtsp_server(port: u16, client_info: Arc<Mutex<Option<ClientInfo>>>) {
    let listener = TcpListener::bind(("0.0.0.0", port)).expect("Failed to bind RTSP port");
    println!("RTSP server listening on port {}", port);

    for stream in listener.incoming() {
        if let Ok(stream) = stream {
            let info = client_info.clone();
            thread::spawn(move || {
                handle_client(stream, info);
            });
        }
    }
}

fn handle_client(mut stream: std::net::TcpStream, client_info: Arc<Mutex<Option<ClientInfo>>>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request = String::new();

    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
            break;
        }
        request.push_str(&line);
    }

    let session_id = extract_session_id(&request).unwrap_or_else(|| "12345678".to_string());
    let (client_rtp, client_rtcp) = extract_client_ports(&request);
    let client_ip = stream.peer_addr().map(|a| a.ip().to_string()).unwrap_or_else(|_| "127.0.0.1".to_string());

    if request.contains("SETUP") || request.contains("PLAY") {
        let mut info = client_info.lock().unwrap();
        *info = Some(ClientInfo {
            ip: client_ip,
            rtp_port: client_rtp,
            rtcp_port: client_rtcp,
            session_id: session_id.clone(),
        });
    }

    let response = if request.contains("OPTIONS") {
        "RTSP/1.0 200 OK\r\nPublic: OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN\r\n\r\n"
    } else if request.contains("DESCRIBE") {
        let sdp = "v=0\r\n\
                   o=- 0 0 IN IP4 0.0.0.0\r\n\
                   s=Nova\r\n\
                   t=0 0\r\n\
                   a=control:*\r\n\
                   m=video 0 RTP/AVP 96\r\n\
                   a=rtpmap:96 H264/90000\r\n\
                   a=fmtp:96 packetization-mode=1;profile-level-id=42e01f\r\n\
                   a=control:streamid=0\r\n";
        &format!(
            "RTSP/1.0 200 OK\r\nContent-Base: rtsp://0.0.0.0:48010/\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{}",
            sdp.len(), sdp
        )
    } else if request.contains("SETUP") {
        &format!(
            "RTSP/1.0 200 OK\r\nTransport: RTP/AVP;unicast;client_port={}-{};server_port=50002-50003\r\nSession: {};timeout=60\r\n\r\n",
            client_rtp, client_rtcp, session_id
        )
    } else if request.contains("PLAY") {
        &format!("RTSP/1.0 200 OK\r\nSession: {}\r\n\r\n", session_id)
    } else if request.contains("TEARDOWN") {
        &format!("RTSP/1.0 200 OK\r\nSession: {}\r\n\r\n", session_id)
    } else {
        "RTSP/1.0 404 Not Found\r\n\r\n"
    };

    let _ = stream.write_all(response.as_bytes());
}

fn extract_session_id(request: &str) -> Option<String> {
    for line in request.lines() {
        if line.to_lowercase().starts_with("session:") {
            return line.split(':').nth(1).map(|s| s.trim().to_string());
        }
    }
    None
}

fn extract_client_ports(request: &str) -> (u16, u16) {
    for line in request.lines() {
        if line.to_lowercase().contains("client_port=") {
            if let Some(ports_part) = line.split("client_port=").nth(1) {
                let ports = ports_part.split(';').next().unwrap_or("");
                let mut parts = ports.split('-');
                let rtp = parts.next().and_then(|p| p.parse().ok()).unwrap_or(50000);
                let rtcp = parts.next().and_then(|p| p.parse().ok()).unwrap_or(50001);
                return (rtp, rtcp);
            }
        }
    }
    (50000, 50001)
}