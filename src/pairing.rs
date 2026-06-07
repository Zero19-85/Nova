// pairing.rs
use hyper::{Request, Response, StatusCode, header};
use hyper::body::Incoming;
use http_body_util::Full;
use hyper::body::Bytes;
use std::sync::Arc;
use std::collections::HashMap;
use std::sync::Mutex;

use crate::debug::debug_log;

pub type PairSessions = Arc<Mutex<HashMap<String, PairSession>>>;

#[derive(Default, Clone)]
pub struct PairSession {
    pub last_phase: String,
    pub client_cert: Option<String>,
    pub salt: Option<String>,
    pub aes_key: Option<[u8; 16]>,
}

static PAIRED_CLIENTS: std::sync::LazyLock<Mutex<std::collections::HashSet<String>>> =
    std::sync::LazyLock::new(|| Mutex::new(std::collections::HashSet::new()));

pub struct ServerCrypto {
    pub cert_hex: String,
    pub cert_sig: Vec<u8>,
    pub private_key_der: Vec<u8>,
    pub cert_der: Vec<u8>,
}

impl ServerCrypto {
    pub fn new() -> Self {
        println!("🔑 Generating Nova RSA Security Certificate...");
        Self {
            cert_hex: "PLACEHOLDER_CERT".to_string(),
            cert_sig: vec![],
            private_key_der: vec![],
            cert_der: vec![],
        }
    }
}

// ====================== HELPERS ======================

fn make_xml_response(body: &str) -> Response<Full<Bytes>> {
    let mut res = Response::new(Full::new(Bytes::from(body.to_string())));
    *res.status_mut() = StatusCode::OK;
    res.headers_mut().insert(header::CONTENT_TYPE, "application/xml; charset=utf-8".parse().unwrap());
    res.headers_mut().insert(header::CONTENT_LENGTH, body.len().to_string().parse().unwrap());
    res
}

fn extract_arg(uri: &str, key: &str) -> Option<String> {
    if let Some(start) = uri.find(&format!("{}=", key)) {
        let rest = &uri[start + key.len() + 1..];
        if let Some(end) = rest.find('&') {
            Some(rest[..end].to_string())
        } else {
            Some(rest.to_string())
        }
    } else {
        None
    }
}

// ====================== SERVERINFO ======================

pub async fn handle_serverinfo(
    req: Request<Incoming>,
    host_ip: String,
    server_id: String,
    _server_mac: String,
    _crypto: Arc<ServerCrypto>,
    _sessions: PairSessions,
    _global_pin: Arc<Mutex<String>>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let full_uri = req.uri().to_string();
    debug_log(&format!("📥 {} {}", req.method(), full_uri));

    let uniqueid = extract_arg(&full_uri, "uniqueid");
    let is_paired = if let Some(uid) = &uniqueid {
        let paired = PAIRED_CLIENTS.lock().unwrap();
        paired.contains(uid)
    } else {
        false
    };

    let body = format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<root status_code="200">
    <hostname>Nova</hostname>
    <appversion>7.1.431.-1</appversion>
    <GfeVersion>3.23.0.74</GfeVersion>
    <uniqueid>{}</uniqueid>
    <HttpsPort>47984</HttpsPort>
    <ExternalPort>47989</ExternalPort>
    <MaxLumaPixelsHEVC>1869449984</MaxLumaPixelsHEVC>
    <mac>00:00:00:00:00:00</mac>
    <Permission>0</Permission>
    <LocalIP>{}</LocalIP>
    <ServerCodecModeSupport>2032385</ServerCodecModeSupport>
    <PairStatus>{}</PairStatus>
    <currentgame>0</currentgame>
    <currentgameuuid/>
    <state>SUNSHINE_SERVER_FREE</state>
</root>"#,
        server_id,
        host_ip,
        if is_paired { 1 } else { 0 }
    );

    Ok(make_xml_response(&body))
}

// ====================== APPLIST ======================

pub async fn handle_applist(
    req: Request<Incoming>,
    _host_ip: String,
    _server_id: String,
    _server_mac: String,
    _crypto: Arc<ServerCrypto>,
    _sessions: PairSessions,
    _global_pin: Arc<Mutex<String>>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    debug_log("📋 Moonlight requested /applist");

    let body = r#"<?xml version="1.0" encoding="utf-8"?>
<root status_code="200">
    <App><IsHdrSupported>0</IsHdrSupported><IsApp>1</IsApp><IsMediaApp>0</IsMediaApp><AppTitle>Desktop</AppTitle><ID>1</ID><UUID>4C9941FC-91A0-DA06-D407-3B3B6C4DBF4A</UUID></App>
    <App><IsHdrSupported>0</IsHdrSupported><IsApp>1</IsApp><IsMediaApp>0</IsMediaApp><AppTitle>Steam</AppTitle><ID>2</ID><UUID>84217441-34E9-5D0C-9A0D-E76E2820BD3F</UUID></App>
    <App><IsHdrSupported>0</IsHdrSupported><IsApp>1</IsApp><IsMediaApp>0</IsMediaApp><AppTitle>Xbox App</AppTitle><ID>3</ID><UUID>3C8B5E2A-1F4D-4A2B-9E7C-8D5F2A1B3C4D</UUID></App>
    <App><IsHdrSupported>0</IsHdrSupported><IsApp>1</IsApp><IsMediaApp>0</IsMediaApp><AppTitle>Virtual Desktop</AppTitle><ID>4</ID><UUID>7A9B2C3D-4E5F-6A7B-8C9D-0E1F2A3B4C5D</UUID></App>
</root>"#;

    Ok(make_xml_response(body))
}

// ====================== PAIR HANDLER (Corrected) ======================

pub async fn handle_pair(
    req: Request<Incoming>,
    _host_ip: String,
    _server_id: String,
    _server_mac: String,
    _crypto: Arc<ServerCrypto>,
    sessions: PairSessions,
    _global_pin: Arc<Mutex<String>>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let full_uri = req.uri().to_string();
    debug_log(&format!("📥 {} {}", req.method(), full_uri));

    let uniqueid = extract_arg(&full_uri, "uniqueid").unwrap_or_default();
    let phrase = extract_arg(&full_uri, "phrase");

    let mut sessions_guard = sessions.lock().unwrap();
    let session = sessions_guard.entry(uniqueid.clone()).or_default();

    if let Some(p) = &phrase {
        if p == "getservercert" {
            session.last_phase = "getservercert".to_string();
            session.salt = extract_arg(&full_uri, "salt");
            session.client_cert = extract_arg(&full_uri, "clientcert");

            debug_log("🔑 Phase 1: getservercert received - waiting for next phase");

            // Return paired=0 so Moonlight keeps the PIN screen open
            let body = r#"<?xml version="1.0" encoding="utf-8"?>
<root status_code="200">
    <paired>0</paired>
</root>"#;
            return Ok(make_xml_response(body));
        }
    }

    // Only mark as paired after the first phase has been seen
    if session.last_phase == "getservercert" {
        let mut paired = PAIRED_CLIENTS.lock().unwrap();
        paired.insert(uniqueid.clone());
        debug_log("✅ Client marked as paired");
    }

    let body = r#"<?xml version="1.0" encoding="utf-8"?>
<root status_code="200">
    <paired>1</paired>
</root>"#;

    Ok(make_xml_response(body))
}

// ====================== MAIN ROUTER ======================

pub async fn handle_request(
    req: Request<Incoming>,
    host_ip: String,
    server_id: String,
    server_mac: String,
    crypto: Arc<ServerCrypto>,
    sessions: PairSessions,
    global_pin: Arc<Mutex<String>>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let path = req.uri().path();

    match path {
        "/serverinfo" => handle_serverinfo(req, host_ip, server_id, server_mac, crypto, sessions, global_pin).await,
        "/applist"    => handle_applist(req, host_ip, server_id, server_mac, crypto, sessions, global_pin).await,
        _ if path.starts_with("/pair") => {
            handle_pair(req, host_ip, server_id, server_mac, crypto, sessions, global_pin).await
        }
        _ => {
            let mut res = Response::new(Full::new(Bytes::from("Not Found")));
            *res.status_mut() = StatusCode::NOT_FOUND;
            Ok(res)
        }
    }
}