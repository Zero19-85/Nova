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
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::client::danger::HandshakeSignatureValid;
// Aliased: rcgen (server cert generation below) exports its own DistinguishedName.
use rustls::DistinguishedName as TlsDistinguishedName;
use rustls::{DigitallySignedStruct, SignatureScheme};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, UnixTime};

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
// Format (one device per line, keyed by the SHA-256 fingerprint of the
// client's TLS certificate, lowercase hex):
//   { "<fingerprint>": { "name": "Xbox", "uniqueid": "0123456789ABCDEF", "cert": "<hex-PEM>" }, ... }
//
// The CERTIFICATE — not the uniqueid — is the device identity (Apollo/Sunshine
// parity, nvhttp.cpp `cert_chain`). moonlight-qt and several derived clients
// hardcode uniqueid to "0123456789ABCDEF", so every such client collides on
// it; the client cert is generated per install and is globally unique. The
// uniqueid is stored for logging only. `cert` is the hex-encoded PEM exactly
// as Moonlight sent it in the `clientcert` pairing parameter.
//
// Written one entry per line for easy manual inspection and diff-friendly
// diffs. No external crate required — the structure is fixed so hand-written
// serialisation/deserialisation is sufficient.

/// One trusted (paired) Moonlight client, authenticated by its TLS cert.
#[derive(Clone)]
pub struct PairedClient {
    pub name: String,
    pub uniqueid: String,
    /// Hex-encoded PEM exactly as received in the `clientcert` pairing param.
    pub cert_pem_hex: String,
}

/// fingerprint (lowercase SHA-256 hex of the cert DER) → device record.
/// Shared between the TLS accept loops (connection authorization) and the
/// request handlers (pair/unpair mutations).
type TrustedClients = Arc<Mutex<HashMap<String, PairedClient>>>;

/// Identity attached to an HTTPS connection whose client certificate matched
/// the trusted store — Nova's equivalent of Apollo's `get_verified_cert()`.
#[derive(Clone)]
struct VerifiedClient {
    fingerprint: String,
    name: String,
}

/// Extract a JSON string field (`"field": "value"`) from a single line,
/// honouring backslash escapes inside the value.
fn json_string_field(line: &str, field: &str) -> Option<String> {
    let marker = format!("\"{}\":", field);
    let idx = line.find(&marker)?;
    let after = line[idx + marker.len()..].trim_start();
    let inner = after.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => {
                match chars.next()? {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    other => out.push(other), // \" \\ and any pass-through
                }
            }
            other => out.push(other),
        }
    }
    None
}

fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn load_paired_json() -> HashMap<String, PairedClient> {
    let path = data_file(PAIRED_PATH);
    let text = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return HashMap::new(),
    };
    parse_paired_json(&text)
}

fn parse_paired_json(text: &str) -> HashMap<String, PairedClient> {
    let mut map = HashMap::new();
    for line in text.lines() {
        let line = line.trim().trim_end_matches(',');
        if !line.starts_with('"') { continue; }
        // Key = first quoted token (the cert fingerprint).
        let inner = &line[1..];
        let key_end = match inner.find('"') { Some(i) => i, None => continue };
        let key = &inner[..key_end];
        let rest = &inner[key_end..];
        let name     = json_string_field(rest, "name").unwrap_or_default();
        let uniqueid = json_string_field(rest, "uniqueid").unwrap_or_default();
        let cert     = json_string_field(rest, "cert").unwrap_or_default();
        if key.is_empty() { continue; }
        if cert.is_empty() {
            // Pre-Phase-14 entry: keyed by uniqueid, no certificate stored.
            // Without a cert the device cannot be authenticated — the whole
            // point of the per-client trust model — so it must re-pair.
            println!("⚠️  nova_paired.json: legacy entry \"{}\" ({}) has no client certificate — dropped; re-pair this device", name, key);
            continue;
        }
        // Re-derive the fingerprint from the stored cert rather than trusting
        // the key on disk — a hand-edited key can otherwise grant the wrong cert.
        let Some(der) = client_cert_der_from_hex_pem(&cert) else {
            println!("⚠️  nova_paired.json: entry \"{}\" has an unparseable certificate — dropped", name);
            continue;
        };
        let fingerprint = cert_fingerprint_hex(&der);
        if fingerprint != key {
            println!("⚠️  nova_paired.json: fingerprint key mismatch for \"{}\" — using recomputed fingerprint", name);
        }
        map.insert(fingerprint, PairedClient { name, uniqueid, cert_pem_hex: cert });
    }
    map
}

fn serialize_paired_json(map: &HashMap<String, PairedClient>) -> String {
    let mut out = String::from("{\n");
    let mut entries: Vec<_> = map.iter().collect();
    entries.sort_by_key(|(k, _)| k.as_str());
    for (i, (fp, pc)) in entries.iter().enumerate() {
        let comma = if i + 1 < entries.len() { "," } else { "" };
        out.push_str(&format!(
            "  \"{}\": {{ \"name\": \"{}\", \"uniqueid\": \"{}\", \"cert\": \"{}\" }}{}\n",
            fp, json_escape(&pc.name), json_escape(&pc.uniqueid), pc.cert_pem_hex, comma
        ));
    }
    out.push('}');
    out
}

fn save_paired_json(map: &HashMap<String, PairedClient>) {
    let _ = std::fs::write(data_file(PAIRED_PATH), serialize_paired_json(map));
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

// ── Client-certificate helpers (per-device trust model) ─────────────────────

/// Lowercase SHA-256 hex of a certificate's DER bytes — the trust-store key.
fn cert_fingerprint_hex(der: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(der);
    hex::encode(hasher.finalize())
}

/// Minimal base64 decoder (standard alphabet, ignores whitespace/padding) —
/// counterpart of [`der_to_pem`]'s encoder, so no extra dependency is needed.
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &c in input.as_bytes() {
        if matches!(c, b'\r' | b'\n' | b' ' | b'\t' | b'=') { continue; }
        acc = (acc << 6) | val(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// Moonlight sends its client certificate as hex-encoded PEM (`clientcert`
/// pairing param). Decode hex → PEM text → DER bytes.
fn client_cert_der_from_hex_pem(cert_hex: &str) -> Option<Vec<u8>> {
    let pem_bytes = hex::decode(cert_hex.trim()).ok()?;
    let pem = String::from_utf8(pem_bytes).ok()?;
    let body: String = pem
        .lines()
        .filter(|l| !l.contains("-----"))
        .collect::<Vec<_>>()
        .join("");
    let der = base64_decode(&body)?;
    (!der.is_empty()).then_some(der)
}

/// RSA PKCS#1 v1.5 / SHA-256 verification of `signature` over `message` with
/// the public key from `cert_der` — Apollo's `crypto::verify256`. Used in the
/// final pairing phase to prove the client owns the private key matching the
/// certificate it sent in `getservercert` (MITM protection).
fn verify_client_signature(cert_der: &[u8], message: &[u8], signature: &[u8]) -> bool {
    let cert = CertificateDer::from(cert_der.to_vec());
    let Ok(ee) = webpki::EndEntityCert::try_from(&cert) else {
        return false;
    };
    ee.verify_signature(webpki::aws_lc_rs::RSA_PKCS1_2048_8192_SHA256, message, signature)
        .is_ok()
}

/// TLS client-certificate policy for port 47984 — Apollo/Sunshine parity.
///
/// Mirrors Sunshine's OpenSSL setup (`SSL_VERIFY_PEER |
/// SSL_VERIFY_FAIL_IF_NO_PEER_CERT` with a verify callback that returns 1):
/// a client certificate is REQUIRED, but any cert is accepted at handshake
/// time. Moonlight client certs are self-signed, so chain validation is
/// meaningless — possession of the private key is what the TLS
/// CertificateVerify signature proves (checked for real in
/// `verify_tls12_signature` below; a cert without its key cannot complete
/// the handshake). AUTHORIZATION happens after the handshake: the accept
/// loop matches the peer cert's SHA-256 fingerprint against the trusted
/// store and every request from an unmatched cert is answered 401.
#[derive(Debug)]
struct AcceptAnyClientCert {
    algs: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl AcceptAnyClientCert {
    fn new() -> Self {
        Self {
            algs: rustls::crypto::aws_lc_rs::default_provider().signature_verification_algorithms,
        }
    }
}

impl ClientCertVerifier for AcceptAnyClientCert {
    fn root_hint_subjects(&self) -> &[TlsDistinguishedName] {
        // No CA hints: Moonlight certs are self-signed; an empty list tells
        // the client "send whatever client cert you have".
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }

    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        // Sunshine: SSL_VERIFY_FAIL_IF_NO_PEER_CERT. A connection without a
        // client cert can never be authorized, so fail it at the handshake.
        true
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

    // Per-device trust store: cert fingerprint → paired device record.
    // This — not any global flag — is what authorizes a client connection.
    let trusted: TrustedClients = Arc::new(Mutex::new(load_paired_json()));
    {
        let lock = trusted.lock().unwrap();
        if lock.is_empty() {
            println!("📂 No paired clients in {} — devices must pair before streaming", data_file(PAIRED_PATH).display());
        } else {
            println!("📂 Loaded {} paired client(s) from {}:", lock.len(), data_file(PAIRED_PATH).display());
            for (fp, pc) in lock.iter() {
                println!("   • \"{}\" (uniqueid={}, cert sha256={}…)", pc.name, pc.uniqueid, &fp[..16.min(fp.len())]);
            }
        }
    }

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
    //
    // Client certs are REQUIRED on 47984 (AcceptAnyClientCert, Sunshine's
    // SSL_VERIFY_FAIL_IF_NO_PEER_CERT equivalent) — the accept loop below
    // authorizes each connection against the trusted store.
    let mut tls_config = ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS12])
        .with_client_cert_verifier(Arc::new(AcceptAnyClientCert::new()))
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
    let trusted_https = trusted.clone();
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
            let trust = trusted_https.clone();
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

                // ── Per-connection authorization (Apollo's https_server.verify) ──
                // The handshake proved the peer OWNS the private key for the
                // cert it presented (CertificateVerify). Now check that this
                // exact cert is one we paired with. verified=None ⇒ every
                // request on this connection is answered 401.
                let verified: Option<VerifiedClient> = {
                    let (_, conn) = tls_stream.get_ref();
                    let peer_fp = conn
                        .peer_certificates()
                        .and_then(|certs| certs.first())
                        .map(|ee| cert_fingerprint_hex(ee.as_ref()));
                    match peer_fp {
                        Some(fp) => {
                            let name = trust.lock().unwrap().get(&fp).map(|pc| pc.name.clone());
                            match name {
                                Some(name) => {
                                    println!("🔓 [47984] {} verified — device \"{}\" (cert {}…)", peer, name, &fp[..16.min(fp.len())]);
                                    Some(VerifiedClient { fingerprint: fp, name })
                                }
                                None => {
                                    println!("⛔ [47984] {} presented an UNRECOGNIZED client cert ({}…) — denied", peer, &fp[..16.min(fp.len())]);
                                    None
                                }
                            }
                        }
                        None => {
                            println!("⛔ [47984] {} sent no client cert — denied", peer);
                            None
                        }
                    }
                };

                let io = TokioIo::new(tls_stream);

                let _ = http1::Builder::new()
                    .serve_connection(io, service_fn(move |req| {
                        let ip = ip_clone.clone();
                        let id = id_clone.clone();
                        let mac = mac_clone.clone();
                        let crypt = crypt.clone();
                        let sess = sess.clone();
                        let trust = trust.clone();
                        let ci = ci.clone();
                        let tray = tray.clone();
                        let gpin = gpin.clone();
                        let verified = verified.clone();
                        async move { handle_request(req, "[HTTPS]", ip, id, mac, crypt, sess, trust, verified, ci, cms, tray, gpin).await }
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
        let trusted_clone = trusted.clone();
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
                    let trust = trusted_clone.clone();
                    let ci = ci_clone.clone();
                    let tray = tray_clone.clone();
                    let gpin = pin_clone.clone();
                    // HTTP is never certificate-authenticated: verified=None.
                    async move { handle_request(req, "[HTTP]", ip, id, mac, crypt, sess, trust, None, ci, cms, tray, gpin).await }
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
    trusted: TrustedClients,
    verified: Option<VerifiedClient>,
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

    // ── Access control (Apollo/Sunshine parity) ──────────────────────────────
    // HTTPS (47984): EVERY request requires the connection's client cert to
    // have matched the trusted store — Apollo answers 401 otherwise
    // ("on_verify_failed"). Moonlight reacts by treating the host as unpaired
    // and falling back to HTTP /serverinfo.
    // HTTP (47989): unauthenticated transport — exists only for discovery
    // (/serverinfo, limited fields), the pairing handshake (/pair), and /ping.
    // Session-control endpoints are NOT reachable here.
    let is_https = port_tag == "[HTTPS]";
    if is_https && verified.is_none() {
        println!("⛔ {} {} — client not authorized (cert verification failed) → 401", port_tag, path);
        return Ok(make_unauthorized_response(path));
    }
    if !is_https && !matches!(path, "/serverinfo" | "/pair" | "/ping") {
        println!("⛔ {} {} — endpoint requires the authenticated HTTPS channel → 404", port_tag, path);
        let mut res = Response::new(Full::new(Bytes::from("Not Found")));
        *res.status_mut() = StatusCode::NOT_FOUND;
        return Ok(res);
    }

    // Paired ⇔ this connection's client cert matched the trusted store.
    // (On HTTP this is always 0: Moonlight's HTTPS-serverinfo probe with its
    // client cert is what confirms pairing, exactly as with GFE/Sunshine.)
    let pair_status = if verified.is_some() { 1 } else { 0 };

    match path {
        "/serverinfo" => {
            // Busy/current-game state is only disclosed on the authenticated
            // channel (Apollo: `if constexpr (std::is_same_v<SunshineHTTPS, T>)`).
            let (current_game, server_state) = if verified.is_some() {
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
            } else {
                (0u32, "SUNSHINE_SERVER_FREE")
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
                    // SCM bits (moonlight-common-c Limelight.h): H264=0x1,
                    // HEVC Main8=0x100, HEVC Main10=0x200. Without 0x200
                    // moonlight-common-c never sets dynamicRangeMode:1 in
                    // ANNOUNCE, blocking HDR10.
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
                // The client certificate is the device identity Nova pins at
                // the end of the handshake — without it the device could never
                // be authorized on 47984, so reject up front (Apollo fails the
                // same way at clientpairingsecret with "Invalid client
                // certificate"; failing early gives the user a clearer error).
                let Some(client_cert_hex) = params.get("clientcert").cloned() else {
                    println!("⚠️  getservercert without clientcert param — cannot pair this client");
                    return Ok(make_error_response("Missing client certificate"));
                };
                {
                    // getservercert STARTS a pairing attempt: always begin from
                    // a fresh session so state from an aborted attempt can't
                    // leak into this one.
                    let mut lock = sessions.lock().unwrap();
                    let session = lock.entry(client_id.clone()).or_default();
                    *session = PairSession::default();
                    session.last_phase = "GETSERVERCERT".to_string();
                    session.client_cert = Some(client_cert_hex);
                    session.salt = Some(salt.clone());
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
                    // Phase-order enforcement (Apollo fail_pair): an attacker
                    // must not be able to enter the handshake mid-way.
                    if session.last_phase != "GETSERVERCERT" {
                        lock.remove(&client_id);
                        println!("⚠️  Out-of-order clientchallenge (expected after getservercert) — pairing aborted");
                        return Ok(make_error_response("Out of order call to clientchallenge"));
                    }
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
                    session.server_secret    = Some(server_secret);
                    session.server_challenge = Some(server_challenge);
                    // session.name already set during getservercert — do not overwrite
                    // (client_hash is captured in the serverchallengeresp phase —
                    // it is the decrypted challenge RESPONSE, not this challenge.)
                }

                let body = format!(r#"<?xml version="1.0" encoding="utf-8"?><root status_code="200"><paired>1</paired><challengeresponse>{}</challengeresponse></root>"#, hex::encode_upper(encrypted_response));
                Ok(make_xml_response(&body))

            } else if phrase == "serverchallengeresp" || server_challenge_resp.is_some() {
                let (server_secret, aes_key) = {
                    let mut lock = sessions.lock().unwrap();
                    let session = lock.entry(client_id.clone()).or_default();
                    if session.last_phase != "CLIENTCHALLENGE" {
                        lock.remove(&client_id);
                        println!("⚠️  Out-of-order serverchallengeresp (expected after clientchallenge) — pairing aborted");
                        return Ok(make_error_response("Out of order call to serverchallengeresp"));
                    }
                    session.last_phase = "SERVERCHALLENGERESP".to_string();
                    (session.server_secret.clone().unwrap_or_default(),
                     session.aes_key)
                };

                // The encrypted payload is the client's hash of
                // (serverchallenge ‖ client-cert-signature ‖ client-secret).
                // Store it — the clientpairingsecret phase recomputes the hash
                // from the actual values and compares (Apollo `sess.clienthash`,
                // "if hash not correct, probably MITM").
                let client_hash = aes_key.map(|key| {
                    let resp_hex = server_challenge_resp.clone().unwrap_or_default();
                    let resp_bytes = hex::decode(&resp_hex).unwrap_or_default();
                    aes_ecb_decrypt(&key, &resp_bytes)
                });
                {
                    let mut lock = sessions.lock().unwrap();
                    if let Some(session) = lock.get_mut(&client_id) {
                        session.client_hash = client_hash;
                    }
                }

                let key_pair = RsaKeyPair::from_pkcs8(&crypto.private_key_der).unwrap();
                let mut sig_buf = vec![0u8; key_pair.public().modulus_len()];
                let rng = ring::rand::SystemRandom::new();
                key_pair.sign(&RSA_PKCS1_SHA256, &rng, &server_secret, &mut sig_buf).unwrap();

                let mut pairing_secret = server_secret.clone();
                pairing_secret.extend_from_slice(&sig_buf);
                
                let body = format!(r#"<?xml version="1.0" encoding="utf-8"?><root status_code="200"><paired>1</paired><pairingsecret>{}</pairingsecret></root>"#, hex::encode_upper(pairing_secret));
                Ok(make_xml_response(&body))

            } else if phrase == "clientpairingsecret" || client_pairing_secret.is_some() {
                let (device_name, server_challenge, client_hash, client_cert_hex) = {
                    let mut lock = sessions.lock().unwrap();
                    let session = lock.entry(client_id.clone()).or_default();
                    if session.last_phase != "SERVERCHALLENGERESP" {
                        lock.remove(&client_id);
                        println!("⚠️  Out-of-order clientpairingsecret (expected after serverchallengeresp) — pairing aborted");
                        return Ok(make_error_response("Out of order call to clientpairingsecret"));
                    }
                    session.last_phase = "CLIENTPAIRINGSECRET".to_string();
                    (session.name.clone()
                        .unwrap_or_else(|| format!("Device-{}", &client_id[..4.min(client_id.len())])),
                     session.server_challenge.clone().unwrap_or_default(),
                     session.client_hash.clone().unwrap_or_default(),
                     session.client_cert.clone().unwrap_or_default())
                };
                // The session is finished after this phase whatever the outcome.
                sessions.lock().unwrap().remove(&client_id);

                // clientpairingsecret = 16-byte client secret ‖ RSA signature
                // of that secret by the client's certificate key.
                let blob = hex::decode(client_pairing_secret.unwrap_or_default()).unwrap_or_default();
                if blob.len() <= 16 {
                    println!("⚠️  Client pairing secret too short — pairing aborted");
                    return Ok(make_error_response("Client pairing secret too short"));
                }
                let (secret, signature) = blob.split_at(16);

                let Some(cert_der) = client_cert_der_from_hex_pem(&client_cert_hex) else {
                    println!("⚠️  Invalid client certificate — pairing aborted");
                    return Ok(make_error_response("Invalid client certificate"));
                };
                if cert_der.len() < 256 {
                    println!("⚠️  Client certificate too small for an RSA-2048 signature — pairing aborted");
                    return Ok(make_error_response("Invalid client certificate"));
                }

                // Check 1 (Apollo same_hash): the hash the client committed to
                // in serverchallengeresp must equal SHA-256(serverchallenge ‖
                // client-cert-signature ‖ secret). A PIN-guessing MITM that
                // re-signed with a different cert fails here.
                let cert_signature = &cert_der[cert_der.len() - 256..];
                let mut data = Vec::with_capacity(server_challenge.len() + 256 + 16);
                data.extend_from_slice(&server_challenge);
                data.extend_from_slice(cert_signature);
                data.extend_from_slice(secret);
                let mut hasher = Sha256::new();
                hasher.update(&data);
                let expected_hash = hasher.finalize().to_vec();
                let same_hash = !client_hash.is_empty()
                    && expected_hash.len() == client_hash.len()
                    && expected_hash == client_hash;

                // Check 2 (Apollo verify256): the secret must be signed by the
                // private key of the cert the client sent — proves the pairing
                // peer actually owns the certificate Nova is about to trust.
                let signature_ok = verify_client_signature(&cert_der, secret, signature);

                if !(same_hash && signature_ok) {
                    println!("❌ Pairing REJECTED for \"{}\" (hash match: {}, cert signature: {}) — wrong PIN or MITM",
                        device_name, same_hash, signature_ok);
                    let body = r#"<?xml version="1.0" encoding="utf-8"?><root status_code="200"><paired>0</paired></root>"#;
                    return Ok(make_xml_response(body));
                }

                // Pin THIS certificate as the device's identity. From now on,
                // only a TLS connection presenting (and proving ownership of)
                // this exact cert is authorized as this device.
                let fingerprint = cert_fingerprint_hex(&cert_der);
                {
                    let mut lock = trusted.lock().unwrap();
                    lock.insert(fingerprint.clone(), PairedClient {
                        name: device_name.clone(),
                        uniqueid: client_id.clone(),
                        cert_pem_hex: client_cert_hex,
                    });
                    save_paired_json(&lock);
                }
                println!("🎉 Phase 4: Handshake Complete! \"{}\" is paired (cert {}…, saved to {}).",
                    device_name, &fingerprint[..16], PAIRED_PATH);
                let body = r#"<?xml version="1.0" encoding="utf-8"?><root status_code="200"><paired>1</paired></root>"#;
                Ok(make_xml_response(body))

            } else if phrase == "pairchallenge" {
                // Moonlight's post-pairing probe over HTTPS: succeeds only when
                // the connection's client cert matched the trusted store. Over
                // HTTP (never authenticated) this must NOT claim paired=1.
                if verified.is_some() {
                    println!("🔄 Phase 5: pairchallenge — client cert verified, pairing confirmed");
                    let body = r#"<?xml version="1.0" encoding="utf-8"?><root status_code="200"><paired>1</paired></root>"#;
                    Ok(make_xml_response(body))
                } else {
                    println!("⚠️  pairchallenge over unauthenticated channel — paired=0");
                    let body = r#"<?xml version="1.0" encoding="utf-8"?><root status_code="200"><paired>0</paired></root>"#;
                    Ok(make_xml_response(body))
                }

            } else {
                Ok(make_error_response("Unknown pairing request"))
            }
        }

        "/unpair" => {
            // HTTPS-only (the HTTP gate above 404s it): removes the REQUESTING
            // device's own trust entry, identified by its verified cert — an
            // unauthenticated caller must not be able to unpair other devices
            // by guessing uniqueids (they're shared across moonlight-qt builds).
            let v = verified.as_ref().expect("HTTPS gate guarantees verified");
            println!("🧹 /unpair — removing \"{}\" (cert {}…)", v.name, &v.fingerprint[..16]);
            sessions.lock().unwrap().remove(&client_id);
            {
                let mut lock = trusted.lock().unwrap();
                lock.remove(&v.fingerprint);
                save_paired_json(&lock);
            }
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

            // Device identity comes from the connection's VERIFIED client
            // certificate — never from the uniqueid param. moonlight-qt and
            // derived clients hardcode uniqueid to "0123456789ABCDEF", so a
            // uniqueid lookup returns whichever device paired last (the old
            // "every monitor is named after the first device" bug). The cert
            // is unique per client install, so this name is always the one
            // entered when THIS device paired.
            let v = verified.as_ref().expect("HTTPS gate guarantees verified");
            let session_device_name = v.name.clone();
            let client_uniqueid = params.get("uniqueid").map(|s| s.as_str()).unwrap_or("");
            println!("🏷️  Device: \"{}\" (cert {}…, uniqueid={})",
                session_device_name, &v.fingerprint[..16], client_uniqueid);

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
                // Both /launch and /resume begin a NEW streaming session (fresh
                // rikey, fresh control connection, fresh ANNOUNCE), so bump the
                // session generation — the control thread uses it to tell this
                // session's ENet peer apart from a zombie peer of the previous
                // session (quitting Moonlight on Xbox never sends an ENet
                // disconnect; the zombie lingers until its 10-30 s timeout).
                info.session_generation = info.session_generation.wrapping_add(1);
                // Arm the session: streaming starts at RTSP PLAY. If the previous
                // session is still nominally connected (zombie), clearing this now
                // makes the capture loop suspend it immediately and latch THIS
                // session cleanly at PLAY. Without it /resume never re-runs the
                // session start (new rikey, codec renegotiation, audio restart) —
                // the client waits on a dead session and is kicked back to the
                // app list after its 7-second timeout.
                info.streaming_active = false;
                info.cancelled        = false; // clear any leftover cancel from the previous session
                // CRITICAL: reset so the control thread re-sends the 0x010e HDR mode
                // packet on the first PT_PERIODIC_PING of the NEW session.  Without this
                // reset the flag is carried over from the previous ClientInfo via take(),
                // the Xbox never receives the packet, and the TV stays in SDR mode →
                // "whitewash" on every reconnect (applies to /resume the same as /launch).
                info.hdr_mode_sent    = false;
                // Reset ANNOUNCE-sourced fields so stale values from a previous
                // session cannot leak into the new one's codec/HDR decisions.
                // The authoritative values arrive in the client's ANNOUNCE SDP.
                info.dynamic_range_mode = 0;
                info.bit_stream_format  = 0;
                // /launch starts a fresh session — reset activation state so the
                // capture loop's pre-activation pass runs the VDD/CCD switch
                // during the handshake gap. /resume reattaches to an already-
                // active VDD: leaving activated=true skips re-activation and
                // avoids a topology flicker on reconnect.
                if path == "/launch" {
                    info.activated = false;
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

/// Apollo's `on_verify_failed` body: HTTP 401 + XML status so Moonlight shows
/// "not authorized / re-pair" instead of a protocol error.
fn make_unauthorized_response(path: &str) -> Response<Full<Bytes>> {
    let body = format!(
        r#"<?xml version="1.0" encoding="utf-8"?><root status_code="401" query="{}" status_message="The client is not authorized. Certificate verification failed."><paired>0</paired></root>"#,
        path
    );
    let mut res = Response::new(Full::new(Bytes::from(body)));
    *res.status_mut() = StatusCode::UNAUTHORIZED;
    res.headers_mut().insert(header::CONTENT_TYPE, "application/xml".parse().unwrap());
    res
}

fn make_error_response(msg: &str) -> Response<Full<Bytes>> {
    let body = format!(r#"<?xml version="1.0" encoding="utf-8"?><root status_code="400" status_message="{}"><paired>0</paired></root>"#, msg);
    let mut res = Response::new(Full::new(Bytes::from(body)));
    *res.status_mut() = StatusCode::BAD_REQUEST;
    res.headers_mut().insert(header::CONTENT_TYPE, "application/xml".parse().unwrap());
    res
}
#[cfg(test)]
mod tests {
    use super::*;

    /// base64_decode must invert der_to_pem's encoder for arbitrary binary
    /// (the pair that carries client certs through nova_paired.json).
    #[test]
    fn base64_roundtrip_inverts_pem_encoder() {
        let data: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        let pem = der_to_pem(&data);
        let body: String = pem.lines().filter(|l| !l.contains("-----")).collect();
        assert_eq!(base64_decode(&body).expect("decode"), data);

        // Non-multiple-of-3 lengths exercise both padding branches.
        for len in [0usize, 1, 2, 3, 4, 5] {
            let d = &data[..len];
            let pem = der_to_pem(d);
            let body: String = pem.lines().filter(|l| !l.contains("-----")).collect();
            assert_eq!(base64_decode(&body).as_deref(), Some(d));
        }
    }

    /// Hex-PEM (the wire format of Moonlight's `clientcert` param) must decode
    /// to the exact DER bytes, and the fingerprint must be stable.
    #[test]
    fn client_cert_hex_pem_decodes_to_der() {
        let fake_der: Vec<u8> = (0..300u32).map(|i| (i % 251) as u8).collect();
        let pem = der_to_pem(&fake_der);
        let hex_pem = hex::encode_upper(pem.as_bytes());
        let decoded = client_cert_der_from_hex_pem(&hex_pem).expect("hex-PEM decode");
        assert_eq!(decoded, fake_der);
        assert_eq!(cert_fingerprint_hex(&decoded).len(), 64);
    }

    /// The trust store must round-trip devices keyed by fingerprint, drop
    /// cert-less legacy entries, and heal a tampered fingerprint key.
    #[test]
    fn paired_store_roundtrip_and_legacy_migration() {
        let fake_der: Vec<u8> = (0..300u32).map(|i| (i % 249) as u8).collect();
        let cert_hex = hex::encode_upper(der_to_pem(&fake_der).as_bytes());
        let fp = cert_fingerprint_hex(&fake_der);

        let mut map = HashMap::new();
        map.insert(fp.clone(), PairedClient {
            name: "Living Room \"TV\"".to_string(), // exercises quote escaping
            uniqueid: "0123456789ABCDEF".to_string(),
            cert_pem_hex: cert_hex.clone(),
        });
        let text = serialize_paired_json(&map);
        let loaded = parse_paired_json(&text);
        assert_eq!(loaded.len(), 1);
        let pc = loaded.get(&fp).expect("fingerprint key");
        assert_eq!(pc.name, "Living Room \"TV\"");
        assert_eq!(pc.uniqueid, "0123456789ABCDEF");
        assert_eq!(pc.cert_pem_hex, cert_hex);

        // Legacy (pre-cert) entry: must be dropped, not trusted.
        let legacy = r#"{
  "0123456789ABCDEF": { "name": "Xbox" }
}"#;
        assert!(parse_paired_json(legacy).is_empty());

        // Tampered key: entry is re-keyed by the recomputed fingerprint, so a
        // hand-edited key cannot grant a different cert someone else's trust.
        let tampered = text.replace(&fp, &"0".repeat(64));
        let healed = parse_paired_json(&tampered);
        assert!(healed.contains_key(&fp));
        assert!(!healed.contains_key(&"0".repeat(64)));
    }
}
