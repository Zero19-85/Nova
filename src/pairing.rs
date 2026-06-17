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
use std::io::Write as IoWrite;
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

const PAIRED_PATH:        &str = "nova_paired.txt";
const CERT_VERSION_PATH: &str = "nova_cert.version";
const CERT_VERSION:       u8  = 8;

/// Append a client ID to the persist file (one ID per line).
fn persist_paired_client(client_id: &str) {
    use std::fs::OpenOptions;
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(PAIRED_PATH) {
        let _ = writeln!(f, "{}", client_id);
    }
}

/// Remove a client ID from the persist file.
fn remove_paired_client(client_id: &str) {
    if let Ok(contents) = std::fs::read_to_string(PAIRED_PATH) {
        let updated: String = contents
            .lines()
            .filter(|l| l.trim() != client_id)
            .map(|l| format!("{}\n", l))
            .collect();
        let _ = std::fs::write(PAIRED_PATH, updated);
    }
}

/// Load all persisted client IDs and mark them as paired in the sessions map.
fn load_paired_clients(sessions: &PairSessions) {
    let contents = match std::fs::read_to_string(PAIRED_PATH) {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut lock = sessions.lock().unwrap();
    for id in contents.lines().map(str::trim).filter(|s| !s.is_empty()) {
        let entry = lock.entry(id.to_string()).or_default();
        entry.last_phase = "CLIENTPAIRINGSECRET".to_string();
    }
    println!("📂 Loaded {} paired client(s) from disk", lock.len());
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

        if let Err(e) = std::fs::write(CERT_PATH, &cert_der) {
            eprintln!("⚠️ Could not save {}: {}", CERT_PATH, e);
        }
        if let Err(e) = std::fs::write(CERT_PEM_PATH, &cert_pem) {
            eprintln!("⚠️ Could not save {}: {}", CERT_PEM_PATH, e);
        }
        if let Err(e) = std::fs::write(KEY_PATH, &private_key_der) {
            eprintln!("⚠️ Could not save {}: {}", KEY_PATH, e);
        }
        if let Err(e) = std::fs::write(CERT_VERSION_PATH, &[CERT_VERSION]) {
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
        let stored_version = std::fs::read(CERT_VERSION_PATH)
            .ok()
            .and_then(|b| b.first().copied());
        if stored_version.unwrap_or(0) < CERT_VERSION {
            println!("🔄 Nova cert is outdated (v{} < v{}) — deleting to regenerate.",
                stored_version.unwrap_or(0), CERT_VERSION);
            println!("   ⚠️  Delete nova_paired.txt and re-pair Moonlight after restart.");
            let _ = std::fs::remove_file(CERT_PATH);
            let _ = std::fs::remove_file(CERT_PEM_PATH);
            let _ = std::fs::remove_file(KEY_PATH);
            let _ = std::fs::remove_file(CERT_VERSION_PATH);
            return None;
        }

        let cert_der        = std::fs::read(CERT_PATH).ok()?;
        let private_key_der = std::fs::read(KEY_PATH).ok()?;
        if cert_der.is_empty() || private_key_der.is_empty() {
            return None;
        }
        // PEM file might not exist on older installs — derive it from DER.
        let cert_pem = std::fs::read_to_string(CERT_PEM_PATH)
            .unwrap_or_else(|_| {
                let pem = der_to_pem(&cert_der);
                let _ = std::fs::write(CERT_PEM_PATH, &pem);
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

    // --- PIN CHANNEL ---
    let global_pin = Arc::new(Mutex::new(String::new()));
    let pin_thread_ref = global_pin.clone();
    
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        loop {
            let mut input = String::new();
            if stdin.read_line(&mut input).is_ok() {
                let trimmed = input.trim().to_string();
                if trimmed.len() == 4 {
                    *pin_thread_ref.lock().unwrap() = trimmed;
                }
            }
        }
    });

    // --- SPAWN HTTPS LOOP ---
    let crypto_https = crypto.clone();
    let sessions_https = sessions.clone();
    let ip_https = host_ip.clone();
    let id_https = server_id.clone();
    let mac_https = server_mac.clone();
    let pin_https = global_pin.clone();
    let ci_https = client_info.clone();
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
            let pin = pin_https.clone();
            let ci = ci_https.clone();
            let cms = codec_mode_support; // Copy

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
                        let pin = pin.clone();
                        let ci = ci.clone();
                        async move { handle_request(req, "[HTTPS]", ip, id, mac, crypt, sess, pin, ci, cms).await }
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
        let pin_clone = global_pin.clone();
        let ci_clone = client_info.clone();
        let cms = codec_mode_support; // Copy

        tokio::task::spawn(async move {
            println!("🌐 [47989] HTTP from {}", peer_str);
            let _ = http1::Builder::new()
                .serve_connection(io, service_fn(move |req| {
                    let ip = ip_clone.clone();
                    let id = id_clone.clone();
                    let mac = mac_clone.clone();
                    let crypt = crypto_clone.clone();
                    let sess = sessions_clone.clone();
                    let pin = pin_clone.clone();
                    let ci = ci_clone.clone();
                    async move { handle_request(req, "[HTTP]", ip, id, mac, crypt, sess, pin, ci, cms).await }
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
    global_pin: Arc<Mutex<String>>,
    client_info: Arc<Mutex<Option<rtsp::ClientInfo>>>,
    codec_mode_support: u32,
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
                    // Advertise only the codec the encoder is actually running.
                    // Clients pick the highest-quality format from (client caps ∩ server caps);
                    // advertising HEVC here when the encoder is H264 causes a codec mismatch
                    // and a black screen on strict clients such as Xbox Moonlight UWP.
                    r#"<ServerCodecModeSupport>{}</ServerCodecModeSupport>"#,
                    r#"<PairStatus>{}</PairStatus>"#,
                    r#"<currentgame>{}</currentgame>"#,
                    r#"<state>{}</state>"#,
                    r#"<MaxLumaPixelsH264>1869449984</MaxLumaPixelsH264>"#,
                    r#"<MaxLumaPixelsHEVC>1869449984</MaxLumaPixelsHEVC>"#,
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

                println!("🤝 Phase 1: Awaiting PIN...");
                let body = format!(r#"<?xml version="1.0" encoding="utf-8"?><root status_code="200"><paired>1</paired><plaincert>{}</plaincert></root>"#, crypto.cert_hex);
                Ok(make_xml_response(&body))

            } else if phrase == "clientchallenge" || client_challenge.is_some() {
                let salt = {
                    let mut lock = sessions.lock().unwrap();
                    let session = lock.entry(client_id.clone()).or_default();
                    session.last_phase = "CLIENTCHALLENGE".to_string();
                    session.salt.clone().unwrap_or_default()
                };

                println!("⏳ Please type the 4-digit PIN from Moonlight into this console and press Enter...");
                let mut attempts = 0;
                let mut pin = String::new();
                
                while attempts < 600 { 
                    {
                        let mut p_guard = global_pin.lock().unwrap();
                        if p_guard.len() == 4 {
                            pin = p_guard.clone();
                            *p_guard = String::new();
                            break;
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    attempts += 1;
                }

                if pin.is_empty() {
                    println!("⏳ PIN input timed out. Please restart the pairing process.");
                    return Ok(make_error_response("Timeout waiting for PIN"));
                }

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
                    session.aes_key = Some(aes_key);
                    session.client_hash = Some(decrypted);
                    session.server_secret = Some(server_secret);
                    session.server_challenge = Some(server_challenge);
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
                {
                    let mut lock = sessions.lock().unwrap();
                    let session = lock.entry(client_id.clone()).or_default();
                    session.last_phase = "CLIENTPAIRINGSECRET".to_string();
                }
                persist_paired_client(&client_id);
                println!("🎉 Phase 4: Handshake Complete! Device is officially paired (saved to disk).");
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
                r#"<App><AppTitle>1. Desktop</AppTitle><ID>1</ID><IsHdrSupported>0</IsHdrSupported></App>"#,
                r#"<App><AppTitle>2. Steam</AppTitle><ID>2</ID><IsHdrSupported>0</IsHdrSupported></App>"#,
                r#"<App><AppTitle>3. Xbox App</AppTitle><ID>3</ID><IsHdrSupported>0</IsHdrSupported></App>"#,
                r#"<App><AppTitle>4. RetroArch</AppTitle><ID>4</ID><IsHdrSupported>0</IsHdrSupported></App>"#,
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
            println!("🛑 Moonlight requested /cancel — stopping stream");
            if let Ok(mut guard) = client_info.lock() {
                if let Some(ref mut info) = *guard {
                    info.streaming_active = false;
                    info.app_id = 0;
                }
            }
            let body = r#"<?xml version="1.0" encoding="utf-8"?><root status_code="200"><cancel>1</cancel></root>"#;
            Ok(make_xml_response(body))
        }

        "/launch" | "/resume" => {
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
            // Warn immediately if there's a mismatch so the user knows before trying to stream.
            let encoder_mode_bit = codec_mode_support;
            let client_base_codec = video_format & 0x00FF; // strip 10-bit flag for codec ID
            if video_format != 0 && client_base_codec != encoder_mode_bit {
                println!("⚠️  CODEC MISMATCH: client selected {} (videoFormat={:#x}) but \
                    encoder is running codec with ServerCodecModeSupport={}. \
                    Restart nova-server with the matching --codec flag.",
                    vf_name, video_format, encoder_mode_bit);
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
                // Trigger the capture loop's pre-activation pass (lib.rs) for
                // this new launch/resume — runs the VDD/CCD switch during the
                // handshake gap instead of after the control stream connects.
                info.activated  = false;
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