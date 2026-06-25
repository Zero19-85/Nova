use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode, header};
use hyper::body::Incoming;
use http_body_util::Full;
use hyper::body::Bytes;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use hyper_util::rt::TokioIo;
use std::sync::Arc;
use std::collections::HashMap;
use url::Url;
use std::sync::Mutex;
use crate::rtsp;

// TLS/HTTPS Imports
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::{self, ServerConfig};
use rustls_pki_types::{CertificateDer, PrivateKeyDer};

// Crypto imports
use aes::Aes128;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes::cipher::generic_array::GenericArray; 
use sha2::{Sha256, Digest};
use rand::RngCore;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};

// RSA Generation Imports 
use rsa::{RsaPrivateKey, pkcs8::EncodePrivateKey};
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, SanType, PKCS_RSA_SHA256};

type PairSessions = Arc<Mutex<HashMap<String, PairSession>>>;

const PAIRED_PATH:        &str = "nova_paired.json";
const CERT_VERSION_PATH: &str = "nova_cert.version";
const CERT_VERSION:       u8  = 8;

/// All data files are stored next to the running executable so the paths are
/// stable regardless of the current working directory (service runs, shells
/// started from different locations, etc.).  Relative paths break when Nova is
/// launched as a Windows service because the SCM sets CWD to System32.
fn data_dir() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

fn data_file(name: &str) -> std::path::PathBuf { data_dir().join(name) }

// ── JSON paired-device store ───────────────────────────────────────────────
// Format: { "UNIQUEID": { "name": "Xbox" }, ... }
// Written one entry per line for easy manual inspection and diff-friendly diffs.
// No external crate required — the structure is fixed so hand-written
// serialisation/deserialisation is sufficient.

fn load_paired_json() -> HashMap<String, String> {
    let path = data_file(PAIRED_PATH);
    let text  = match std::fs::read_to_string(&path) {
        Ok(s)  => s,
        Err(_) => return HashMap::new(),
    };
    let mut map = HashMap::new();
    for line in text.lines() {
        // Each data line looks like:   "UNIQUEID": { "name": "VALUE" },
        let line = line.trim().trim_end_matches(',');
        if !line.starts_with('"') { continue; }
        // Extract uniqueid (first quoted token)
        let inner = &line[1..]; // skip opening "
        let id_end = match inner.find('"') { Some(i) => i, None => continue };
        let id = &inner[..id_end];
        // Extract name field from the rest of the line
        let rest = &inner[id_end..];
        let name_marker = "\"name\":";
        let nm = match rest.find(name_marker) { Some(i) => i, None => continue };
        let after = rest[nm + name_marker.len()..].trim_start();
        if !after.starts_with('"') { continue; }
        let val_inner = &after[1..];
        let val_end   = match val_inner.find('"') { Some(i) => i, None => continue };
        let name      = &val_inner[..val_end];
        if !id.is_empty() {
            map.insert(id.to_string(), name.to_string());
        }
    }
    map
}

fn save_paired_json(map: &HashMap<String, String>) {
    let path = data_file(PAIRED_PATH);
    let mut out = String::from("{\n");
    let mut entries: Vec<_> = map.iter().collect();
    entries.sort_by_key(|(k, _)| k.as_str());
    for (i, (id, name)) in entries.iter().enumerate() {
        let comma     = if i + 1 < entries.len() { "," } else { "" };
        let name_esc  = name.replace('\\', "\\\\").replace('"', "\\\"");
        out.push_str(&format!("  \"{}\": {{ \"name\": \"{}\" }}{}\n", id, name_esc, comma));
    }
    out.push('}');
    let _ = std::fs::write(&path, out);
}

/// Upsert a paired device record (uniqueid → name).
fn persist_paired_client(client_id: &str, name: &str) {
    let mut map = load_paired_json();
    map.insert(client_id.to_string(), name.to_string());
    save_paired_json(&map);
}

/// Remove a device record by uniqueid.
fn remove_paired_client(client_id: &str) {
    let mut map = load_paired_json();
    map.remove(client_id);
    save_paired_json(&map);
}

/// Load all persisted device records and mark them as paired in the sessions map.
fn load_paired_clients(sessions: &PairSessions) {
    let map = load_paired_json();
    if map.is_empty() {
        println!("📂 No paired-clients file at {} — starting fresh", data_file(PAIRED_PATH).display());
        return;
    }
    let mut lock = sessions.lock().unwrap();
    for id in map.keys() {
        let entry = lock.entry(id.clone()).or_default();
        entry.last_phase = "CLIENTPAIRINGSECRET".to_string();
    }
    println!("📂 Loaded {} paired client(s) from {}:", lock.len(), data_file(PAIRED_PATH).display());
    for (id, name) in &map {
        println!("   • {} → \"{}\"", id, name);
    }
}

#[derive(Default)]
struct PairSession {
    last_phase: String,
    client_cert: Option<String>,
    salt: Option<String>,
    aes_key: Option<[u8; 16]>,
    server_secret: Option<Vec<u8>>,
    server_challenge: Option<Vec<u8>>,
    client_hash: Option<Vec<u8>>,
    /// The pairing PIN entered by the user.  Collected during getservercert
    /// (which Android Moonlight waits for without a read timeout) so that
    /// clientchallenge (7-second read timeout) can respond instantly.
    pin: Option<String>,
    /// Device name entered by the user during the PIN dialog (stored here so it
    /// survives from the getservercert phase to the clientpairingsecret phase).
    name: Option<String>,
}

pub struct ServerCrypto {
    /// Hex-encoded DER — this is what goes in `plaincert`.
    /// moonlight-qt parses `plaincert` with OpenSSL d2i_X509() (DER-only).
    /// Moonlight Android uses CertificateFactory which accepts both, but
    /// hex-DER is the canonical format Sunshine uses and works everywhere.
    /// Note: Moonlight sends its *own* clientcert as hex-PEM — asymmetric but correct.
    pub cert_hex: String,
    /// Last 256 bytes of the DER cert — used as the signature blob in Phase 2.
    pub cert_sig: Vec<u8>,
    pub private_key_der: Vec<u8>,
    pub cert_der: Vec<u8>,
}

const CERT_PATH:     &str = "nova_cert.der";
const CERT_PEM_PATH: &str = "nova_cert.pem";
const KEY_PATH:      &str = "nova_key.der";

/// Encode DER bytes as a standard PEM certificate string (no extra dependencies).
fn der_to_pem(der: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut b64 = String::with_capacity((der.len() + 2) / 3 * 4);
    for chunk in der.chunks(3) {
        let b0 = chunk[0] as usize;
        let b1 = if chunk.len() > 1 { chunk[1] as usize } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as usize } else { 0 };
        b64.push(CHARS[b0 >> 2] as char);
        b64.push(CHARS[((b0 & 3) << 4) | (b1 >> 4)] as char);
        b64.push(if chunk.len() > 1 { CHARS[((b1 & 15) << 2) | (b2 >> 6)] as char } else { '=' });
        b64.push(if chunk.len() > 2 { CHARS[b2 & 63] as char } else { '=' });
    }
    let mut pem = String::from("-----BEGIN CERTIFICATE-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        pem.push_str(std::str::from_utf8(chunk).unwrap());
        pem.push('\n');
    }
    pem.push_str("-----END CERTIFICATE-----\n");
    pem
}

fn sha256_fingerprint(der: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(der);
    let hash = hasher.finalize();
    hash.iter().map(|b| format!("{:02X}", b)).collect::<Vec<_>>().join(":")
}

impl ServerCrypto {
    /// Load from disk if all three files exist; generate and persist otherwise.
    /// The cert identity must be stable across restarts — Moonlight pins the
    /// exact cert bytes seen during pairing.
    pub fn new(host_ip: Option<&str>) -> Self {
        if let Some(crypto) = Self::try_load_from_disk() {
            println!("🔒 Cert SHA-256: {}", sha256_fingerprint(&crypto.cert_der));
            return crypto;
        }

        println!("🔑 Generating Nova RSA-2048 certificate (v{})...", CERT_VERSION);

        let mut rng = rand::thread_rng();
        let rsa_key = RsaPrivateKey::new(&mut rng, 2048).expect("Failed to generate RSA key");

        // PKCS#8 DER — no PEM round-trip. Same bytes go to rcgen and the TLS acceptor.
        let pkcs8_der = rsa_key.to_pkcs8_der().expect("Failed to encode RSA key to DER");
        let private_key_der = pkcs8_der.as_bytes().to_vec();

        // Algorithm comes from KeyPair in rcgen 0.14 — there is no params.alg field.
        let key_for_rcgen = PrivateKeyDer::Pkcs8(private_key_der.clone().into());
        let key_pair = KeyPair::from_der_and_sign_algo(&key_for_rcgen, &PKCS_RSA_SHA256)
            .expect("Failed to load RSA-2048 key into rcgen");

        // Cert design: CN + IP SAN, no CA extensions.
        //
        // Sunshine's cert has no SAN and works because Moonlight's handleSslErrors()
        // suppresses all errors when error.certificate() == m_ServerCert (the pinned
        // plaincert DER).  Adding an IP SAN for the host's LAN address means OpenSSL
        // hostname verification PASSES outright — no errors to suppress at all.
        // This is strictly safer and makes us work even if m_ServerCert is null in Qt.
        //
        // CA:TRUE is intentionally absent — it triggers extra OpenSSL chain-validation
        // errors with error.certificate() == null that bypass the suppression check.
        let mut params = CertificateParams::new(Vec::<String>::new())
            .expect("Failed to create certificate params");
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, "NVIDIA GameStream Server");
        params.distinguished_name = dn;
        // LAN IP SAN so OpenSSL hostname check passes without needing error suppression.
        if let Some(ip_str) = host_ip {
            if let Ok(ip) = ip_str.parse::<std::net::IpAddr>() {
                params.subject_alt_names.push(SanType::IpAddress(ip));
                println!("   🌐 SAN IP: {}", ip);
            }
        }
        // Backdate 1 day to absorb clock skew; 20-year validity matches Sunshine.
        let now = time::OffsetDateTime::now_utc();
        params.not_before = now - time::Duration::days(1);
        params.not_after  = now + time::Duration::days(365 * 20);
        let cert     = params.self_signed(&key_pair).expect("Failed to self-sign certificate");
        let cert_der = cert.der().to_vec();
        let cert_pem = der_to_pem(&cert_der);

        if let Err(e) = std::fs::write(data_file(CERT_PATH), &cert_der) {
            eprintln!("⚠️ Could not save {}: {}", CERT_PATH, e);
        }
        if let Err(e) = std::fs::write(data_file(CERT_PEM_PATH), &cert_pem) {
            eprintln!("⚠️ Could not save {}: {}", CERT_PEM_PATH, e);
        }
        if let Err(e) = std::fs::write(data_file(KEY_PATH), &private_key_der) {
            eprintln!("⚠️ Could not save {}: {}", KEY_PATH, e);
        }
        if let Err(e) = std::fs::write(data_file(CERT_VERSION_PATH), &[CERT_VERSION]) {
            eprintln!("⚠️ Could not save {}: {}", CERT_VERSION_PATH, e);
        }

        // plaincert must be hex-encoded PEM — Moonlight's verifySignature calls
        // PEM_read_bio_X509, which requires PEM text, not binary DER bytes.
        let cert_hex = hex::encode_upper(cert_pem.as_bytes());
        let sig_len  = 256;
        let cert_sig = cert_der[cert_der.len() - sig_len..].to_vec();

        let fp = sha256_fingerprint(&cert_der);
        println!("✅ RSA certificate generated — SHA-256: {}", fp);
        println!("   DER size: {} bytes  |  plaincert is hex-PEM (uppercase)", cert_der.len());
        Self { cert_hex, cert_sig, private_key_der, cert_der }
    }

    fn try_load_from_disk() -> Option<Self> {
        // Version gate: certs generated before CERT_VERSION lack KeyCertSign/EKU/DN.
        // Delete stale files so new() regenerates with the correct extensions.
        let stored_version = std::fs::read(data_file(CERT_VERSION_PATH))
            .ok()
            .and_then(|b| b.first().copied());
        if stored_version.unwrap_or(0) < CERT_VERSION {
            println!("🔄 Nova cert is outdated (v{} < v{}) — deleting to regenerate.",
                stored_version.unwrap_or(0), CERT_VERSION);
            println!("   ⚠️  Delete nova_paired.json and re-pair Moonlight after restart.");
            let _ = std::fs::remove_file(data_file(CERT_PATH));
            let _ = std::fs::remove_file(data_file(CERT_PEM_PATH));
            let _ = std::fs::remove_file(data_file(KEY_PATH));
            let _ = std::fs::remove_file(data_file(CERT_VERSION_PATH));
            return None;
        }

        let cert_der        = std::fs::read(data_file(CERT_PATH)).ok()?;
        let private_key_der = std::fs::read(data_file(KEY_PATH)).ok()?;
        if cert_der.is_empty() || private_key_der.is_empty() {
            return None;
        }
        // PEM file might not exist on older installs — derive it from DER.
        let cert_pem = std::fs::read_to_string(data_file(CERT_PEM_PATH))
            .unwrap_or_else(|_| {
                let pem = der_to_pem(&cert_der);
                let _ = std::fs::write(data_file(CERT_PEM_PATH), &pem);
                pem
            });
        let cert_hex = hex::encode_upper(cert_pem.as_bytes());
        let sig_len  = 256;
        let cert_sig = cert_der[cert_der.len() - sig_len..].to_vec();
        let fp = sha256_fingerprint(&cert_der);
        println!("🔑 Loaded existing Nova certificate from disk — SHA-256: {}", fp);
        Some(Self { cert_hex, cert_sig, private_key_der, cert_der })
    }
}

fn derive_aes_key(salt: &str, pin: &str) -> [u8; 16] {
    let mut hasher = Sha256::new();
    let salt_bytes = hex::decode(salt).unwrap_or_default();
    hasher.update(&salt_bytes);
    hasher.update(pin.as_bytes());
    let result = hasher.finalize();
    let mut key = [0u8; 16];
    key.copy_from_slice(&result[..16]);
    key
}

fn aes_ecb_decrypt(key: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let cipher = Aes128::new_from_slice(key).unwrap();
    let mut output = Vec::new();
    for chunk in data.chunks(16) {
        let mut block = [0u8; 16];
        block[..chunk.len()].copy_from_slice(chunk);
        let mut generic_block = GenericArray::from(block);
        cipher.decrypt_block(&mut generic_block);
        output.extend_from_slice(&generic_block);
    }
    output
}

fn aes_ecb_encrypt(key: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let cipher = Aes128::new_from_slice(key).unwrap();
    let mut output = Vec::new();
    for chunk in data.chunks(16) {
        let mut block = [0u8; 16];
        block[..chunk.len()].copy_from_slice(chunk);
        let mut generic_block = GenericArray::from(block);
        cipher.encrypt_block(&mut generic_block);
        output.extend_from_slice(&generic_block);
    }
    output
}

pub async fn start_pairing_server(
    port: u16,
    host_ip: String,
    server_id: String,
    server_mac: String,
    client_info: Arc<Mutex<Option<rtsp::ClientInfo>>>,
    codec_mode_support: u32,
    tray_tx: Arc<std::sync::mpsc::SyncSender<crate::tray::TrayCmd>>,
    global_pin: Arc<Mutex<(String, String)>>,
) {
    // ── Phase 1: Crypto ───────────────────────────────────────────────────────
    // Must run first. try_load_from_disk() may delete stale cert files and
    // regenerate. No port is open yet — Moonlight cannot connect to a cert that
    // isn't built yet.
    let crypto = Arc::new(ServerCrypto::new(Some(host_ip.as_str())));
    let sessions: PairSessions = Arc::new(Mutex::new(HashMap::new()));
    load_paired_clients(&sessions);

    // ── Phase 2: TLS config ───────────────────────────────────────────────────
    // Build from the cert that was just loaded/generated above. plaincert (sent
    // during HTTP pairing) and the TLS identity are guaranteed to be the same
    // bytes because both come from the same crypto.cert_der.
    let cert = CertificateDer::from(crypto.cert_der.clone());
    let key = PrivateKeyDer::Pkcs8(crypto.private_key_der.clone().into());
    // Restrict to TLS 1.2: Sunshine negotiates TLS 1.2 via OpenSSL.  Some Qt/OpenSSL
    // Moonlight builds emit a spurious certificate_unknown fatal alert on TLS 1.3 even
    // when cert bytes are byte-perfect.  TLS 1.2 bypasses that code path entirely.
    // ALPN http/1.1: Qt 5.x sends ALPN ["h2","http/1.1"] by default; without an explicit
    // server ALPN response the HTTP/2 negotiation can silently abort the connection before
    // the cert is evaluated.
    let mut tls_config = ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS12])
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .expect("Failed to build TLS config");
    tls_config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let tls_acceptor = TlsAcceptor::from(Arc::new(tls_config));

    // ── Phase 3: Bind listeners ───────────────────────────────────────────────
    // Ports open only after TLS is fully configured — no window where a
    // connection could arrive before the correct cert is loaded.
    let http_addr  = SocketAddr::from(([0, 0, 0, 0], port));
    let https_addr = SocketAddr::from(([0, 0, 0, 0], 47984));

    let http_listener  = TcpListener::bind(http_addr).await.expect("Failed to bind HTTP port");
    let https_listener = TcpListener::bind(https_addr).await.expect("Failed to bind HTTPS port");

    println!("🔐 Nova HTTP  server listening on port {}", port);
    println!("🔒 Nova HTTPS server listening on port 47984");

    // --- SPAWN HTTPS LOOP ---
    let crypto_https = crypto.clone();
    let sessions_https = sessions.clone();
    let ip_https = host_ip.clone();
    let id_https = server_id.clone();
    let mac_https = server_mac.clone();
    let ci_https = client_info.clone();
    let tray_https = tray_tx.clone();
    let pin_https = global_pin.clone();
    let tls_acceptor_clone = tls_acceptor.clone();

    tokio::task::spawn(async move {
        loop {
            let (stream, peer) = https_listener.accept().await.unwrap();
            let tls_acceptor_inner = tls_acceptor_clone.clone();
            let ip_clone = ip_https.clone();
            let id_clone = id_https.clone();
            let mac_clone = mac_https.clone();
            let crypt = crypto_https.clone();
            let sess = sessions_https.clone();
            let ci = ci_https.clone();
            let tray = tray_https.clone();
            let gpin = pin_https.clone();
            let cms = codec_mode_support;

            tokio::task::spawn(async move {
                println!("🔒 [47984] TLS attempt from {}", peer);
                let tls_stream = match tls_acceptor_inner.accept(stream).await {
                    Ok(s) => {
                        println!("✅ [47984] TLS handshake OK from {}", peer);
                        s
                    },
                    Err(e) => {
                        eprintln!("⚠️ [47984] TLS FAILED from {}: {}", peer, e);
                        return;
                    },
                };
                let io = TokioIo::new(tls_stream);

                let _ = http1::Builder::new()
                    .serve_connection(io, service_fn(move |req| {
                        let ip = ip_clone.clone();
                        let id = id_clone.clone();
                        let mac = mac_clone.clone();
                        let crypt = crypt.clone();
                        let sess = sess.clone();
                        let ci = ci.clone();
                        let tray = tray.clone();
                        let gpin = gpin.clone();
                        async move { handle_request(req, "[HTTPS]", ip, id, mac, crypt, sess, ci, cms, tray, gpin).await }
                    }))
                    .await;
            });
        }
    });

    // --- RUN HTTP LOOP ---
    loop {
        let (stream, peer) = http_listener.accept().await.unwrap();
        let peer_str = peer.to_string();
        let io = TokioIo::new(stream);
        let ip_clone = host_ip.clone();
        let id_clone = server_id.clone();
        let mac_clone = server_mac.clone();
        let crypto_clone = crypto.clone();
        let sessions_clone = sessions.clone();
        let ci_clone = client_info.clone();
        let tray_clone = tray_tx.clone();
        let pin_clone = global_pin.clone();
        let cms = codec_mode_support;

        tokio::task::spawn(async move {
            println!("🌐 [47989] HTTP from {}", peer_str);
            let _ = http1::Builder::new()
                .serve_connection(io, service_fn(move |req| {
                    let ip = ip_clone.clone();
                    let id = id_clone.clone();
                    let mac = mac_clone.clone();
                    let crypt = crypto_clone.clone();
                    let sess = sessions_clone.clone();
                    let ci = ci_clone.clone();
                    let tray = tray_clone.clone();
                    let gpin = pin_clone.clone();
                    async move { handle_request(req, "[HTTP]", ip, id, mac, crypt, sess, ci, cms, tray, gpin).await }
                }))
                .await;
        });
    }
}

async fn handle_request(
    req: Request<Incoming>,
    port_tag: &'static str,
    _host_ip: String,
    server_id: String,
    _server_mac: String,
    crypto: Arc<ServerCrypto>,
    sessions: PairSessions,
    client_info: Arc<Mutex<Option<rtsp::ClientInfo>>>,
    codec_mode_support: u32,
    tray_tx: Arc<std::sync::mpsc::SyncSender<crate::tray::TrayCmd>>,
    global_pin: Arc<Mutex<(String, String)>>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let full_uri = req.uri().to_string();
    if !full_uri.contains("/serverinfo") {
        println!("📥 {} Moonlight Request: {}", port_tag, full_uri);
    }

    let parsed_url = Url::parse(&format!("http://localhost{}", full_uri)).unwrap();
    let path = parsed_url.path();
    let params: HashMap<String, String> = parsed_url.query_pairs().into_owned().collect();

    let client_id = params.get("uniqueid").cloned().unwrap_or_default();
    let phrase = params.get("phrase").cloned().unwrap_or_default();

    let is_paired = {
        let lock = sessions.lock().unwrap();
        if let Some(sess) = lock.get(&client_id) { sess.last_phase == "CLIENTPAIRINGSECRET" } else { false }
    };
    let pair_status = if is_paired { 1 } else { 0 };

    match path {
        "/serverinfo" => {
            let (current_game, server_state) = {
                let guard = client_info.lock().unwrap();
                if let Some(ref info) = *guard {
                    if info.app_id != 0 {
                        (info.app_id, "SUNSHINE_SERVER_BUSY")
                    } else {
                        (0u32, "SUNSHINE_SERVER_FREE")
                    }
                } else {
                    (0u32, "SUNSHINE_SERVER_FREE")
                }
            };
            println!("📊 {} /serverinfo — id={} pair={} game={}", port_tag,
                if client_id.is_empty() { "anon" } else { &client_id },
                pair_status, current_game);
            let body = format!(
                concat!(
                    r#"<?xml version="1.0" encoding="utf-8"?>"#,
                    r#"<root status_code="200">"#,
                    r#"<hostname>Nova</hostname>"#,
                    r#"<appversion>7.1.431.0</appversion>"#,
                    r#"<GfeVersion>3.23.0.74</GfeVersion>"#,
                    r#"<gputype>NVIDIA GeForce</gputype>"#,
                    r#"<GsVersion>7.1.431.0</GsVersion>"#,
                    r#"<uniqueid>{}</uniqueid>"#,
                    r#"<HttpsPort>47984</HttpsPort>"#,
                    r#"<ExternalPort>47989</ExternalPort>"#,
                    r#"<mac>00:11:22:33:44:55</mac>"#,
                    r#"<LocalIP>{}</LocalIP>"#,
                    r#"<ExternalIP>{}</ExternalIP>"#,
                    // H264 (1) + HEVC Main8 (2) + HEVC Main10 (256) = 259.
                    // Bit 256 = SCM_HEVC_MAIN10; without it moonlight-common-c
                    // never sets dynamicRangeMode:1 in ANNOUNCE, blocking HDR10.
                    // The encoder uses dynamicRangeMode from ANNOUNCE (not /launch
                    // hdrMode) as the authoritative HDR gate to avoid encoding HDR
                    // when the display is still in SDR mode.
                    r#"<ServerCodecModeSupport>{}</ServerCodecModeSupport>"#,
                    r#"<PairStatus>{}</PairStatus>"#,
                    r#"<currentgame>{}</currentgame>"#,
                    r#"<state>{}</state>"#,
                    r#"<MaxLumaPixelsH264>1869449984</MaxLumaPixelsH264>"#,
                    r#"<MaxLumaPixelsHEVC>1869449984</MaxLumaPixelsHEVC>"#,
                    // Global server-level HDR/HEVC readiness flags. Some
                    // moonlight-common-c versions gate HEVC negotiation on
                    // IsHdrSupported rather than (or in addition to) the
                    // ServerCodecModeSupport bitmask. Setting both is required
                    // for older compiled clients such as Xbox Moonlight 1.18.0.
                    r#"<IsHdrSupported>1</IsHdrSupported>"#,
                    r#"<SupportedDisplayModeList>"#,
                    r#"<DisplayMode><Width>1280</Width><Height>720</Height><RefreshRate>30</RefreshRate></DisplayMode>"#,
                    r#"<DisplayMode><Width>1280</Width><Height>720</Height><RefreshRate>60</RefreshRate></DisplayMode>"#,
                    r#"<DisplayMode><Width>1920</Width><Height>1080</Height><RefreshRate>30</RefreshRate></DisplayMode>"#,
                    r#"<DisplayMode><Width>1920</Width><Height>1080</Height><RefreshRate>60</RefreshRate></DisplayMode>"#,
                    r#"<DisplayMode><Width>1920</Width><Height>1080</Height><RefreshRate>120</RefreshRate></DisplayMode>"#,
                    r#"<DisplayMode><Width>2560</Width><Height>1440</Height><RefreshRate>60</RefreshRate></DisplayMode>"#,
                    r#"<DisplayMode><Width>2560</Width><Height>1440</Height><RefreshRate>120</RefreshRate></DisplayMode>"#,
                    r#"<DisplayMode><Width>3840</Width><Height>2160</Height><RefreshRate>30</RefreshRate></DisplayMode>"#,
                    r#"<DisplayMode><Width>3840</Width><Height>2160</Height><RefreshRate>60</RefreshRate></DisplayMode>"#,
                    r#"<DisplayMode><Width>3840</Width><Height>2160</Height><RefreshRate>120</RefreshRate></DisplayMode>"#,
                    r#"</SupportedDisplayModeList>"#,
                    r#"</root>"#,
                ),
                server_id, _host_ip, _host_ip, codec_mode_support, pair_status, current_game, server_state,
            );
            Ok(make_xml_response(&body))
        }

        "/pair" => {
            let client_challenge = params.get("clientchallenge").cloned();
            let server_challenge_resp = params.get("serverchallengeresp").cloned();
            let client_pairing_secret = params.get("clientpairingsecret").cloned();

            if phrase == "getservercert" || params.contains_key("getservercert") {
                let salt = params.get("salt").cloned().unwrap_or_default();
                {
                    let mut lock = sessions.lock().unwrap();
                    let session = lock.entry(client_id.clone()).or_default();
                    session.last_phase = "GETSERVERCERT".to_string();
                    session.client_cert = params.get("clientcert").cloned();
                    session.salt = Some(salt.clone());
                    session.aes_key = None;
                }

                // ── PIN collection happens HERE, during getservercert ─────────
                //
                // Android Moonlight (NvHTTP.java) calls getservercert with
                // enableReadTimeout=false (unlimited wait) and clientchallenge
                // with enableReadTimeout=true (READ_TIMEOUT = 7 seconds).
                //
                // GFE's model: server generates PIN, shows it on the host screen,
                // responds to getservercert immediately.  By the time clientchallenge
                // arrives GFE already has the PIN in memory → responds in < 1 ms.
                //
                // Nova's model is the reverse: Moonlight shows the PIN on the
                // client device and the user must type it on the Nova host.
                // Collecting the PIN during clientchallenge was burning through
                // the 7-second budget (PowerShell Add-Type JIT alone takes 3-5 s),
                // and the client timed out before the user could finish typing.
                //
                // Fix: collect the PIN NOW while Android waits without a timeout.
                // clientchallenge then reads session.pin and responds in < 50 ms.
                println!("🤝 Phase 1: Pairing initiated — collecting PIN during getservercert (no client timeout here)");
                let _ = tray_tx.try_send(crate::tray::TrayCmd::OpenPairDialog);

                println!("⏳ Phase 1: waiting for PIN + device name (no timeout)…");
                let (pin, device_name) = loop {
                    let ready = {
                        let mut p = global_pin.lock().unwrap();
                        if !p.0.is_empty() {
                            let pair = p.clone();
                            *p = (String::new(), String::new());
                            Some(pair)
                        } else {
                            None
                        }
                    };
                    if let Some(pair) = ready { break pair; }
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                };

                if pin.is_empty() {
                    println!("⚠️  PIN entry cancelled during getservercert — aborting");
                    return Ok(make_error_response("PIN entry cancelled"));
                }
                let device_name = if device_name.is_empty() {
                    format!("Device-{}", &client_id[..4.min(client_id.len())])
                } else {
                    device_name
                };
                println!("🔑 Phase 1: PIN received (device: \"{}\") — responding to getservercert", device_name);

                // Stash PIN + name in the session so clientchallenge can derive
                // the AES key instantly without any dialog or polling.
                {
                    let mut lock = sessions.lock().unwrap();
                    let session = lock.entry(client_id.clone()).or_default();
                    session.pin  = Some(pin);
                    session.name = Some(device_name);
                }

                let body = format!(r#"<?xml version="1.0" encoding="utf-8"?><root status_code="200"><paired>1</paired><plaincert>{}</plaincert></root>"#, crypto.cert_hex);
                Ok(make_xml_response(&body))

            } else if phrase == "clientchallenge" || client_challenge.is_some() {
                // PIN was collected during getservercert (no read timeout there).
                // Read it instantly from the session — must respond within Android's
                // 7-second READ_TIMEOUT or the client will abort the pairing.
                let (salt, pin) = {
                    let mut lock = sessions.lock().unwrap();
                    let session = lock.entry(client_id.clone()).or_default();
                    session.last_phase = "CLIENTCHALLENGE".to_string();
                    (session.salt.clone().unwrap_or_default(),
                     session.pin.clone().unwrap_or_default())
                };

                if pin.is_empty() {
                    println!("⚠️  clientchallenge arrived but no PIN in session — pairing aborted.");
                    return Ok(make_error_response("PIN not available"));
                }
                println!("🔑 Phase 2: PIN from session — completing challenge instantly");

                let aes_key = derive_aes_key(&salt, &pin);
                let challenge_hex = client_challenge.unwrap_or_default();
                let mut decrypted = Vec::new();
                if let Ok(challenge_bytes) = hex::decode(&challenge_hex) {
                    decrypted = aes_ecb_decrypt(&aes_key, &challenge_bytes);
                }

                let mut rng = rand::thread_rng();
                let mut server_secret = vec![0u8; 16];
                rng.fill_bytes(&mut server_secret);
                let mut server_challenge = vec![0u8; 16];
                rng.fill_bytes(&mut server_challenge);

                let mut to_hash = Vec::new();
                to_hash.extend_from_slice(&decrypted);
                to_hash.extend_from_slice(&crypto.cert_sig); 
                to_hash.extend_from_slice(&server_secret);

                let mut hasher = Sha256::new();
                hasher.update(&to_hash);
                let hash = hasher.finalize().to_vec();

                let mut plaintext = Vec::new();
                plaintext.extend_from_slice(&hash);
                plaintext.extend_from_slice(&server_challenge);

                let encrypted_response = aes_ecb_encrypt(&aes_key, &plaintext);

                {
                    let mut lock = sessions.lock().unwrap();
                    let session = lock.entry(client_id.clone()).or_default();
                    session.aes_key          = Some(aes_key);
                    session.client_hash      = Some(decrypted);
                    session.server_secret    = Some(server_secret);
                    session.server_challenge = Some(server_challenge);
                    // session.name already set during getservercert — do not overwrite
                }

                let body = format!(r#"<?xml version="1.0" encoding="utf-8"?><root status_code="200"><paired>1</paired><challengeresponse>{}</challengeresponse></root>"#, hex::encode_upper(encrypted_response));
                Ok(make_xml_response(&body))

            } else if phrase == "serverchallengeresp" || server_challenge_resp.is_some() {
                let server_secret = {
                    let mut lock = sessions.lock().unwrap();
                    let session = lock.entry(client_id.clone()).or_default();
                    session.last_phase = "SERVERCHALLENGERESP".to_string();
                    session.server_secret.clone().unwrap_or_default()
                };

                let key_pair = RsaKeyPair::from_pkcs8(&crypto.private_key_der).unwrap();
                let mut sig_buf = vec![0u8; key_pair.public().modulus_len()];
                let rng = ring::rand::SystemRandom::new();
                key_pair.sign(&RSA_PKCS1_SHA256, &rng, &server_secret, &mut sig_buf).unwrap();

                let mut pairing_secret = server_secret.clone();
                pairing_secret.extend_from_slice(&sig_buf);
                
                let body = format!(r#"<?xml version="1.0" encoding="utf-8"?><root status_code="200"><paired>1</paired><pairingsecret>{}</pairingsecret></root>"#, hex::encode_upper(pairing_secret));
                Ok(make_xml_response(&body))

            } else if phrase == "clientpairingsecret" || client_pairing_secret.is_some() {
                let device_name = {
                    let mut lock = sessions.lock().unwrap();
                    let session = lock.entry(client_id.clone()).or_default();
                    session.last_phase = "CLIENTPAIRINGSECRET".to_string();
                    session.name.clone()
                        .unwrap_or_else(|| format!("Device-{}", &client_id[..4.min(client_id.len())]))
                };
                persist_paired_client(&client_id, &device_name);
                println!("🎉 Phase 4: Handshake Complete! \"{}\" is paired (saved to {}).",
                    device_name, PAIRED_PATH);
                let body = r#"<?xml version="1.0" encoding="utf-8"?><root status_code="200"><paired>1</paired></root>"#;
                Ok(make_xml_response(body))

            } else if phrase == "pairchallenge" {
                println!("🔄 Phase 5: pairchallenge | Moonlight verifying pairing state...");
                let body = r#"<?xml version="1.0" encoding="utf-8"?><root status_code="200"><paired>1</paired></root>"#;
                Ok(make_xml_response(body))

            } else {
                Ok(make_error_response("Unknown pairing request"))
            }
        }

        "/unpair" => {
            println!("🧹 Moonlight requested /unpair for {}", client_id);
            sessions.lock().unwrap().remove(&client_id);
            remove_paired_client(&client_id);
            let body = r#"<?xml version="1.0" encoding="utf-8"?><root status_code="200"><paired>1</paired></root>"#;
            Ok(make_xml_response(body))
        }

        "/applist" => {
            println!("📋 Moonlight requested /applist");
            let body = concat!(
                r#"<?xml version="1.0" encoding="utf-8"?>"#,
                r#"<root status_code="200">"#,
                // Titles are prefixed with "N. " so Moonlight's client-side
                // alphabetical sort of the app grid lands in this fixed order
                // (the GameStream /applist XML order itself is not honored
                // by Moonlight's UI).
                // All apps route through the VDD (universal capture source),
                // so all apps can do HDR when the client negotiates HEVC Main10.
                r#"<App><AppTitle>1. Desktop</AppTitle><ID>1</ID><IsHdrSupported>1</IsHdrSupported></App>"#,
                r#"<App><AppTitle>2. Steam</AppTitle><ID>2</ID><IsHdrSupported>1</IsHdrSupported></App>"#,
                r#"<App><AppTitle>3. Xbox App</AppTitle><ID>3</ID><IsHdrSupported>1</IsHdrSupported></App>"#,
                r#"<App><AppTitle>4. RetroArch</AppTitle><ID>4</ID><IsHdrSupported>1</IsHdrSupported></App>"#,
                r#"<App><AppTitle>5. Virtual Desktop</AppTitle><ID>5</ID><IsHdrSupported>1</IsHdrSupported></App>"#,
                r#"</root>"#,
            );
            Ok(make_xml_response(body))
        }

        "/appasset" => {
            let app_id = params.get("appid")
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(crate::app_launcher::APP_ID_DESKTOP);
            println!("🖼️  Box art requested — appid={}", app_id);
            // no-store so Android's Glide never caches a stale tile.
            let jpeg = crate::app_launcher::get_box_art(app_id);
            let len = jpeg.len();
            let mut res = Response::new(Full::new(Bytes::from(jpeg)));
            *res.status_mut() = StatusCode::OK;
            res.headers_mut().insert(header::CONTENT_TYPE, "image/jpeg".parse().unwrap());
            res.headers_mut().insert(header::CONTENT_LENGTH, len.to_string().parse().unwrap());
            res.headers_mut().insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
            Ok(res)
        }

        "/cancel" => {
            println!("🛑 Moonlight requested /cancel — quitting app, full VDD teardown pending");
            if let Ok(mut guard) = client_info.lock() {
                if let Some(ref mut info) = *guard {
                    info.streaming_active = false;
                    info.app_id = 0;
                    info.activated = false;
                    // Signal the capture loop to do a full VDD teardown rather
                    // than the normal "suspend and wait for /resume" path.
                    info.cancelled = true;
                }
            }
            let body = r#"<?xml version="1.0" encoding="utf-8"?><root status_code="200"><cancel>1</cancel></root>"#;
            Ok(make_xml_response(body))
        }

        "/launch" | "/resume" => {
            // Dump every parameter Moonlight sends so we can identify undocumented
            // flags (e.g. headless toggles, client type hints, display selections).
            // The rikey is redacted — it's a per-session AES key and must not appear
            // in plain-text logs.
            {
                let mut sorted: Vec<(&String, &String)> = params.iter().collect();
                sorted.sort_by_key(|(k, _)| k.as_str());
                println!("📋 {} params ({} total):", path, sorted.len());
                for (k, v) in &sorted {
                    let display = if *k == "rikey" { "<redacted>" } else { v.as_str() };
                    println!("   {:<30} = {}", k, display);
                }
            }

            // Parse mode string "WxHxFPS" (e.g. "1920x1080x60")
            let mode_str = params.get("mode").map(|s| s.as_str()).unwrap_or("1280x720x60");
            let mut mode_parts = mode_str.split('x');
            let width  = mode_parts.next().and_then(|s| s.parse::<u32>().ok()).unwrap_or(1280);
            let height = mode_parts.next().and_then(|s| s.parse::<u32>().ok()).unwrap_or(720);
            let fps    = mode_parts.next().and_then(|s| s.parse::<u32>().ok()).unwrap_or(60);

            let rikey_hex_str = params.get("rikey").cloned().unwrap_or_default();
            let rikey: [u8; 16] = {
                let bytes = hex::decode(&rikey_hex_str).unwrap_or_else(|_| vec![0u8; 16]);
                let mut key = [0u8; 16];
                let n = bytes.len().min(16);
                key[..n].copy_from_slice(&bytes[..n]);
                key
            };
            // rikeyid is signed on the wire (can be negative) — parse as i32, store as u32 bits.
            let rikeyid = params.get("rikeyid")
                .and_then(|s| s.parse::<i32>().ok())
                .map(|v| v as u32)
                .unwrap_or(0);
            // DEBUG: trace the rikey hex-decode end to end — compare this
            // "decoded" value against control.rs's "🔑 Control session rikey"
            // line to confirm the same key reaches the UDP control socket.
            println!("🔑 /launch rikey: raw=\"{}\" ({} chars) decoded={} rikeyid={}",
                rikey_hex_str, rikey_hex_str.len(), hex::encode(rikey), rikeyid);

            let app_id_str = params.get("appid").map(|s| s.as_str()).unwrap_or("1");
            let app_id_num = app_id_str.parse::<u32>().unwrap_or(1);
            // Moonlight's "Play audio on PC" setting → localAudioPlayMode=1.
            // Default 0 = audio goes to the client only (routed via virtual sink).
            let host_audio = params.get("localAudioPlayMode")
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(0) != 0;
            // videoFormat bitmask: 1=H264, 2=HEVC Main, 0x102=HEVC Main10.
            // This is the codec Limelight selected from (client caps ∩ ServerCodecModeSupport).
            // It MUST match the running encoder codec or the client will decode garbage.
            let video_format = params.get("videoFormat")
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(0);
            // hdrMode=1 means the client wants an HDR stream (requires HEVC Main10).
            let hdr_requested = params.get("hdrMode")
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(0) != 0;
            let vf_name = if video_format & 0x100 != 0 { "HEVC Main10" }
                else if video_format & 0x002 != 0 { "HEVC Main" }
                else { "H264" };
            println!("🚀 {} app={} mode={}x{}@{}fps rikeyid={} hostAudio={}",
                path, app_id_str, width, height, fps, rikeyid, host_audio);
            println!("   ↳ videoFormat={:#x} ({})  hdrMode={}  ServerCodecModeSupport={}",
                video_format, vf_name, hdr_requested as u8, codec_mode_support);
            // Codec is selected dynamically: the encoder will be (re)initialized
            // at session start using this videoFormat via Codec::from_video_format().
            // No mismatch is possible — the server advertises all codecs and the
            // client picks from that intersection.
            if video_format == 0 {
                println!("⚠️  /launch videoFormat=0 — client did not send a codec selection. \
                    Encoder will stay at its current codec until ANNOUNCE is parsed.");
            }

            // Resolve the paired device's friendly name from nova_paired.json using
            // the `uniqueid` Moonlight includes in every authenticated request.
            // Falls back to a short hex prefix of the uniqueid so the rename always
            // produces a non-empty, human-readable label.
            let client_uniqueid = params.get("uniqueid").map(|s| s.as_str()).unwrap_or("");
            let session_device_name = if client_uniqueid.is_empty() {
                String::new()
            } else {
                load_paired_json()
                    .get(client_uniqueid)
                    .cloned()
                    .unwrap_or_else(|| {
                        let prefix = &client_uniqueid[..client_uniqueid.len().min(8)];
                        format!("Client-{}", prefix)
                    })
            };
            if !session_device_name.is_empty() {
                println!("🏷️  Device: \"{}\" (uniqueid={})", session_device_name, client_uniqueid);
            }

            // Store session info — RTSP DESCRIBE reads width/height/fps from here.
            // Setting app_id causes /serverinfo to return currentgame=N (BUSY state),
            // which is what Moonlight checks before proceeding with the RTSP handshake.
            if let Ok(mut guard) = client_info.lock() {
                let mut info = guard.take().unwrap_or_default();
                info.rikey         = rikey;
                info.rikeyid       = rikeyid;
                info.width         = width;
                info.height        = height;
                info.fps           = fps;
                info.app_id        = app_id_num;
                info.host_audio    = host_audio;
                info.video_format  = video_format;
                info.hdr_requested = hdr_requested;
                info.device_name   = session_device_name;
                // /launch starts a fresh session — reset activation state so the
                // capture loop's pre-activation pass runs the VDD/CCD switch
                // during the handshake gap. /resume reattaches to an already-
                // active VDD: leaving activated=true skips re-activation and
                // avoids a topology flicker on reconnect.
                if path == "/launch" {
                    info.activated     = false;
                    info.cancelled     = false; // clear any leftover cancel from the previous session
                    // CRITICAL: reset so the control thread re-sends the 0x010e HDR mode
                    // packet on the first PT_PERIODIC_PING of the NEW session.  Without this
                    // reset the flag is carried over from the previous ClientInfo via take(),
                    // the Xbox never receives the packet, and the TV stays in SDR mode →
                    // "whitewash" on every reconnect.
                    info.hdr_mode_sent    = false;
                    // Reset ANNOUNCE-sourced fields so stale values from a previous
                    // session cannot leak into the new one's codec/HDR decisions.
                    // The authoritative values arrive in the client's ANNOUNCE SDP.
                    info.dynamic_range_mode = 0;
                    info.bit_stream_format  = 0;
                }
                *guard = Some(info);
            }

            // /resume reattaches to an already-running session — only /launch
            // should (re)start the app's process.
            if path == "/launch" {
                crate::app_launcher::launch_app(app_id_num);
            }

            let launch_body;
            let body: &str = if path == "/resume" {
                launch_body = format!(
                    r#"<?xml version="1.0" encoding="utf-8"?><root status_code="200"><resume>1</resume><sessionUrl0>rtsp://{}:48010</sessionUrl0></root>"#,
                    _host_ip
                );
                &launch_body
            } else {
                launch_body = format!(
                    r#"<?xml version="1.0" encoding="utf-8"?><root status_code="200"><gamesession>1</gamesession><sessionUrl0>rtsp://{}:48010</sessionUrl0></root>"#,
                    _host_ip
                );
                &launch_body
            };
            Ok(make_xml_response(body))
        }

        "/ping" => {
            Ok(Response::new(Full::new(Bytes::from("Nova OK"))))
        }

        _ => {
            let mut res = Response::new(Full::new(Bytes::from("Not Found")));
            *res.status_mut() = StatusCode::NOT_FOUND;
            Ok(res)
        }
    }
}

// Helpers
fn make_xml_response(body: &str) -> Response<Full<Bytes>> {
    let mut res = Response::new(Full::new(Bytes::from(body.to_string())));
    *res.status_mut() = StatusCode::OK;
    res.headers_mut().insert(header::CONTENT_TYPE, "application/xml".parse().unwrap());
    res.headers_mut().insert(header::CONTENT_LENGTH, body.len().to_string().parse().unwrap());
    res
}

fn make_error_response(msg: &str) -> Response<Full<Bytes>> {
    let body = format!(r#"<?xml version="1.0" encoding="utf-8"?><root status_code="400" status_message="{}"><paired>0</paired></root>"#, msg);
    let mut res = Response::new(Full::new(Bytes::from(body)));
    *res.status_mut() = StatusCode::BAD_REQUEST;
    res.headers_mut().insert(header::CONTENT_TYPE, "application/xml".parse().unwrap());
    res
}