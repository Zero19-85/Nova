//! The client half of GameStream's PIN pairing handshake.
//!
//! Nova implements the server half in `nova-server/src/pairing.rs`; this is its
//! mirror. Everything here must match that file byte for byte, so where a choice
//! looks arbitrary it is because the other side made it first.
//!
//! ## What pairing actually establishes
//!
//! Not a shared secret — a **trusted certificate**. At the end of the handshake
//! Nova stores Echo's certificate in `nova_paired.json`, keyed by its SHA-256
//! fingerprint, and from then on authorises Echo by that certificate on every
//! TLS connection. The PIN is scaffolding: it exists only to authenticate this
//! one exchange, and is worthless afterwards.
//!
//! That is why Echo must pair and connect with the **same** identity. See
//! [`nova_core::identity::Identity::load_or_create_rsa2048`].
//!
//! ## The four phases
//!
//! Each is one HTTP GET to port 47989 with query parameters. The host tracks
//! phase order per `uniqueid` and aborts the whole attempt on any out-of-order
//! call, so these cannot be retried individually — a failure means starting over.
//!
//! | Phase | Client sends | Host returns |
//! |---|---|---|
//! | 1 `getservercert` | `salt`, `clientcert` | `plaincert` |
//! | 2 `clientchallenge` | AES(client challenge) | AES(server hash ‖ server challenge) |
//! | 3 `serverchallengeresp` | AES(client hash) | server secret ‖ RSA signature |
//! | 4 `clientpairingsecret` | client secret ‖ RSA signature | `paired=1` |
//!
//! **Phase 1 blocks for as long as the human takes.** Nova collects the PIN
//! during `getservercert` precisely because that is the one phase Moonlight
//! issues without a read timeout, so the client must be equally patient — hence
//! [`PairOptions::consent_timeout`] being minutes rather than seconds.
//!
//! ## Why AES-ECB is not a bug here
//!
//! ECB leaks whether two plaintext blocks are equal. This handshake encrypts
//! random 16-byte challenges and a 32-byte hash under a key derived from a PIN
//! that is discarded immediately afterwards. There are no repeated blocks to
//! correlate and no long-lived key, which is the narrow case where ECB's
//! weakness has nothing to bite on. It is also simply what the protocol
//! specifies — Nova, Sunshine, Apollo, and GeForce Experience all do this, and a
//! "better" mode would fail to interoperate.
//!
//! ## What the client verifies
//!
//! Both checks matter, and neither is optional:
//!
//! 1. **The host's hash** (phase 3) — `SHA-256(client challenge ‖ host cert
//!    signature ‖ host secret)` must equal the hash the host committed to in
//!    phase 2. This proves the peer knew the PIN *and* had already committed to
//!    the certificate it sent us.
//! 2. **The host's signature** (phase 3) — the host secret must be signed by the
//!    private key of that certificate. Without this, somebody who learned the
//!    PIN (a glance at the screen) could pair as Nova using a certificate they
//!    control; the hash alone would not catch it, because they would simply hash
//!    their own certificate's signature.

use std::time::Duration;

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes::Aes128;
use nova_core::identity::{self, Identity, RSA2048_SIGNATURE_LEN};
use rand::RngCore;
use sha2::{Digest, Sha256};

/// Nova's unauthenticated HTTP port. Pairing happens here because it must work
/// before any certificate is trusted — which is exactly why the handshake
/// carries its own mutual proof rather than relying on the transport.
pub const PAIRING_HTTP_PORT: u16 = 47989;

/// Nova's client-authenticated HTTPS port, used to confirm the pairing landed.
pub const PAIRING_HTTPS_PORT: u16 = 47984;

#[derive(Debug, Clone)]
pub struct PairOptions {
    /// Host address — an IP or name, without a port.
    pub host: String,
    /// The 4-digit PIN the user will type into Nova's dialog.
    pub pin: String,
    /// Name Echo suggests for itself. Nova uses the name typed into its own
    /// dialog, so this is advisory.
    pub device_name: String,
    /// How long to wait for a human to answer Nova's PIN dialog. Minutes, not
    /// seconds — Nova's `getservercert` handler waits without any timeout.
    pub consent_timeout: Duration,
    /// Timeout for the phases that follow, which are machine-to-machine. Nova's
    /// own comments note Moonlight aborts if `clientchallenge` takes more than
    /// 7 seconds, so a host that is healthy answers these promptly.
    pub step_timeout: Duration,
    /// Nova's HTTP port. Configurable only so the handshake can be exercised
    /// against a mock host in tests; there is no reason to change it in the
    /// field.
    pub port: u16,
}

impl PairOptions {
    pub fn new(host: impl Into<String>, pin: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            pin: pin.into(),
            device_name: "Echo".into(),
            consent_timeout: Duration::from_secs(180),
            step_timeout: Duration::from_secs(15),
            port: PAIRING_HTTP_PORT,
        }
    }
}

/// What a successful pairing established.
#[derive(Debug, Clone)]
pub struct PairedHost {
    /// SHA-256 of the host's certificate — the value `echo-client stream
    /// --host` wants, and the same value the signaling relay keys Nova by.
    pub fingerprint: String,
    pub cert_der: Vec<u8>,
    /// The `uniqueid` used for this handshake, for correlating with nova.log.
    pub unique_id: String,
}

/// Progress through the handshake, for a UI or a console.
#[derive(Debug, Clone)]
pub enum PairEvent {
    /// Show this PIN to the user — they must type it into Nova's dialog.
    AwaitingConsent { pin: String },
    /// The host answered phase 1; the PIN was accepted at the dialog.
    HostCertificate { fingerprint: String },
    ChallengeAccepted,
    /// Both of the client's verifications passed.
    HostVerified,
    Paired { fingerprint: String },
}

/// Generate a PIN of the shape Nova's dialog expects.
///
/// Uniform over 0000–9999 rather than assembled from digits, so every PIN is
/// equally likely — and drawn from the OS CSPRNG, because a predictable PIN
/// would let an attacker who can reach port 47989 pair itself.
pub fn generate_pin() -> String {
    let mut bytes = [0u8; 4];
    rand::thread_rng().fill_bytes(&mut bytes);
    format!("{:04}", u32::from_be_bytes(bytes) % 10_000)
}

/// Run the handshake. On success Nova trusts `identity`'s certificate.
pub async fn pair(
    identity: &Identity,
    opts: &PairOptions,
    progress: &mut impl FnMut(PairEvent),
) -> Result<PairedHost, String> {
    // Fail before involving the host or the human if this identity could never
    // complete the handshake — the alternative is a hash mismatch at phase 4
    // that reads like a wrong PIN.
    let client_cert_sig = identity.cert_signature()?.to_vec();

    let unique_id = unique_id_for(identity);
    let base = QueryBase {
        host: opts.host.clone(),
        port: opts.port,
        unique_id: unique_id.clone(),
        device_name: opts.device_name.clone(),
    };

    // ── Phase 1: getservercert ──────────────────────────────────────────────
    let mut salt = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut salt);
    let salt_hex = hex::encode_upper(salt);

    progress(PairEvent::AwaitingConsent { pin: opts.pin.clone() });

    let body = base
        .get(
            &[
                ("phrase", "getservercert"),
                ("salt", &salt_hex),
                ("clientcert", &identity.cert_hex_pem()),
            ],
            opts.consent_timeout,
        )
        .await?;
    expect_paired(&body, "getservercert")?;

    let plaincert = xml_field(&body, "plaincert")
        .ok_or("the host's getservercert reply carried no <plaincert>")?;
    let host_cert_der = hex_pem_to_der(&plaincert)
        .ok_or("the host's certificate could not be decoded from hex-PEM")?;
    let host_cert_sig = host_cert_der
        .len()
        .checked_sub(RSA2048_SIGNATURE_LEN)
        .map(|s| host_cert_der[s..].to_vec())
        .ok_or("the host's certificate is too small to be RSA-2048")?;
    let fingerprint = identity::fingerprint(&host_cert_der);
    progress(PairEvent::HostCertificate { fingerprint: fingerprint.clone() });

    // The key both sides derive independently. Everything after this point
    // fails if the human typed a different PIN — which is the entire design.
    let aes_key = derive_aes_key(&salt_hex, &opts.pin);

    // ── Phase 2: clientchallenge ────────────────────────────────────────────
    let mut client_challenge = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut client_challenge);
    let body = base
        .get(
            &[("clientchallenge", &hex::encode_upper(aes_ecb_encrypt(&aes_key, &client_challenge)))],
            opts.step_timeout,
        )
        .await?;
    expect_paired(&body, "clientchallenge")?;

    let response = xml_field(&body, "challengeresponse")
        .ok_or("the host's clientchallenge reply carried no <challengeresponse>")?;
    let decrypted = aes_ecb_decrypt(
        &aes_key,
        &hex::decode(response.trim()).map_err(|e| format!("challengeresponse is not hex: {e}"))?,
    );
    if decrypted.len() < 48 {
        // 32-byte hash + 16-byte challenge. A short answer here almost always
        // means the PIN was wrong: the host encrypted 48 bytes under a
        // different key, and we are looking at noise.
        return Err(format!(
            "the host's challenge response was {} bytes, expected at least 48 — check the PIN \
             typed into Nova matches the one shown here",
            decrypted.len()
        ));
    }
    let host_hash = decrypted[..32].to_vec();
    let host_challenge = &decrypted[32..48];
    progress(PairEvent::ChallengeAccepted);

    // ── Phase 3: serverchallengeresp ────────────────────────────────────────
    let mut client_secret = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut client_secret);
    let client_hash = sha256_concat(&[host_challenge, &client_cert_sig, &client_secret]);

    let body = base
        .get(
            &[(
                "serverchallengeresp",
                &hex::encode_upper(aes_ecb_encrypt(&aes_key, &client_hash)),
            )],
            opts.step_timeout,
        )
        .await?;
    expect_paired(&body, "serverchallengeresp")?;

    let secret_blob = xml_field(&body, "pairingsecret")
        .ok_or("the host's serverchallengeresp reply carried no <pairingsecret>")?;
    let secret_blob =
        hex::decode(secret_blob.trim()).map_err(|e| format!("pairingsecret is not hex: {e}"))?;
    if secret_blob.len() <= 16 {
        return Err("the host's pairing secret is too short to carry a signature".into());
    }
    let (host_secret, host_signature) = secret_blob.split_at(16);

    // Verification 1 — the host knew the PIN and had committed to this exact
    // certificate before we ever saw its secret.
    let expected = sha256_concat(&[&client_challenge, &host_cert_sig, host_secret]);
    if !identity::constant_time_eq(&expected, &host_hash) {
        return Err(
            "the host's challenge hash does not match: either the PIN typed into Nova differs \
             from the one shown here, or something is impersonating the host"
                .into(),
        );
    }
    // Verification 2 — and it actually holds that certificate's private key.
    verify_host_signature(&host_cert_der, host_secret, host_signature)?;
    progress(PairEvent::HostVerified);

    // ── Phase 4: clientpairingsecret ────────────────────────────────────────
    let mut pairing_secret = client_secret.to_vec();
    pairing_secret.extend_from_slice(&identity.sign_pkcs1_sha256(&client_secret)?);

    let body = base
        .get(
            &[("clientpairingsecret", &hex::encode_upper(&pairing_secret))],
            opts.step_timeout,
        )
        .await?;
    // The host answers `paired=0` here when *its* two checks fail, which is the
    // wrong-PIN case seen from the other side.
    expect_paired(&body, "clientpairingsecret").map_err(|e| {
        format!("{e} — the host rejected the pairing; the usual cause is a mistyped PIN")
    })?;

    progress(PairEvent::Paired { fingerprint: fingerprint.clone() });
    Ok(PairedHost { fingerprint, cert_der: host_cert_der, unique_id })
}

/// Confirm the host now authorises this identity, over the port that actually
/// enforces it.
///
/// Worth doing separately: phase 4 returning `paired=1` says the handshake
/// completed, not that the trust store entry works. This opens a mutually
/// authenticated TLS connection to 47984 — the same check every later
/// connection performs — so a failure here means "paired but not usable", which
/// is a different problem with a different fix.
pub async fn verify_paired(identity: &Identity, host: &str, cert_der: &[u8]) -> Result<(), String> {
    use tokio_rustls::rustls::pki_types::ServerName;

    let pin: [u8; 32] = Sha256::digest(cert_der).into();
    let config = identity::client_config_pinned(identity, pin)?;
    let connector = tokio_rustls::TlsConnector::from(std::sync::Arc::new(config));

    let tcp = tokio::net::TcpStream::connect((host, PAIRING_HTTPS_PORT))
        .await
        .map_err(|e| format!("connect to {host}:{PAIRING_HTTPS_PORT}: {e}"))?;
    // The certificate is pinned by fingerprint, so the name is a formality the
    // API requires rather than something being validated.
    let server_name = ServerName::try_from("nova").map_err(|e| format!("server name: {e}"))?;
    let stream = connector
        .connect(server_name, tcp)
        .await
        .map_err(|e| format!("TLS handshake with the host: {e}"))?;

    let unique_id = unique_id_for(identity);
    let path = format!("/pair?uniqueid={unique_id}&phrase=pairchallenge");
    let body = http_get_over(stream, host, &path, Duration::from_secs(10)).await?;
    expect_paired(&body, "pairchallenge").map_err(|e| {
        format!("{e} — the handshake completed but the host does not authorise this certificate")
    })
}

// ── Protocol primitives (mirrors of nova-server/src/pairing.rs) ─────────────

/// `SHA-256(salt ‖ PIN)`, truncated to 128 bits.
///
/// `salt` is the **hex string** as it travelled on the wire, decoded here — the
/// host does the same to its copy, so both hash the same 16 raw bytes.
fn derive_aes_key(salt_hex: &str, pin: &str) -> [u8; 16] {
    let mut hasher = Sha256::new();
    hasher.update(hex::decode(salt_hex).unwrap_or_default());
    hasher.update(pin.as_bytes());
    let mut key = [0u8; 16];
    key.copy_from_slice(&hasher.finalize()[..16]);
    key
}

/// AES-128-ECB over whole blocks, zero-padding a short final block.
///
/// The padding matches the host's implementation exactly, including the part
/// that is not conventional: a trailing partial block is zero-extended rather
/// than PKCS#7-padded. Every payload this protocol encrypts is already a
/// multiple of 16, so the branch never fires in practice — but if the two sides
/// disagreed about it, the failure would appear as a corrupted final block.
fn aes_ecb_encrypt(key: &[u8; 16], data: &[u8]) -> Vec<u8> {
    ecb(key, data, true)
}

fn aes_ecb_decrypt(key: &[u8; 16], data: &[u8]) -> Vec<u8> {
    ecb(key, data, false)
}

fn ecb(key: &[u8; 16], data: &[u8], encrypt: bool) -> Vec<u8> {
    let cipher = Aes128::new_from_slice(key).expect("a 16-byte key is always valid for AES-128");
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks(16) {
        let mut block = [0u8; 16];
        block[..chunk.len()].copy_from_slice(chunk);
        let mut block = GenericArray::from(block);
        if encrypt {
            cipher.encrypt_block(&mut block);
        } else {
            cipher.decrypt_block(&mut block);
        }
        out.extend_from_slice(&block);
    }
    out
}

fn sha256_concat(parts: &[&[u8]]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().to_vec()
}

/// The RSA-PKCS#1-v1.5 / SHA-256 verifier for the configured provider.
///
/// Selected by feature rather than hard-coded so the Android build (`ring`) and
/// the host build (`aws-lc-rs`) each use the backend they already link, instead
/// of dragging in a second one for this single check.
fn rsa_pkcs1_sha256() -> &'static dyn rustls_pki_types::SignatureVerificationAlgorithm {
    #[cfg(feature = "aws-lc-rs")]
    {
        webpki::aws_lc_rs::RSA_PKCS1_2048_8192_SHA256
    }
    #[cfg(all(feature = "ring", not(feature = "aws-lc-rs")))]
    {
        webpki::ring::RSA_PKCS1_2048_8192_SHA256
    }
}

fn verify_host_signature(cert_der: &[u8], message: &[u8], signature: &[u8]) -> Result<(), String> {
    let cert = rustls_pki_types::CertificateDer::from(cert_der.to_vec());
    let ee = webpki::EndEntityCert::try_from(&cert)
        .map_err(|e| format!("the host's certificate could not be parsed: {e}"))?;
    ee.verify_signature(rsa_pkcs1_sha256(), message, signature).map_err(|_| {
        "the host did not sign its pairing secret with the certificate it presented — something \
         is impersonating the host"
            .to_string()
    })
}

/// `uniqueid` for this identity.
///
/// Derived from the certificate fingerprint rather than hard-coded. Moonlight
/// ships the constant `0123456789ABCDEF`, which is why Nova stopped keying trust
/// by this value (Phase 14.1) — but it still keys the *handshake session*, so two
/// Echo clients pairing at once would otherwise collide and abort each other on
/// the host's phase-order check.
fn unique_id_for(identity: &Identity) -> String {
    identity.fingerprint[..16].to_uppercase()
}

// ── Minimal HTTP ────────────────────────────────────────────────────────────

struct QueryBase {
    host: String,
    port: u16,
    unique_id: String,
    device_name: String,
}

impl QueryBase {
    async fn get(&self, params: &[(&str, &str)], timeout: Duration) -> Result<String, String> {
        let mut query = format!(
            "/pair?uniqueid={}&uuid={}&devicename={}&updateState=1",
            self.unique_id,
            self.unique_id,
            percent_encode(&self.device_name),
        );
        for (key, value) in params {
            query.push('&');
            query.push_str(key);
            query.push('=');
            query.push_str(&percent_encode(value));
        }
        // The connect is inside the timeout, not before it. An address that is
        // routed but silent — a firewall dropping rather than rejecting, which
        // is the common LAN misconfiguration — otherwise blocks for the OS TCP
        // timeout (about 21 s on Windows) no matter what the caller asked for.
        let connect = tokio::net::TcpStream::connect((self.host.as_str(), self.port));
        let tcp = tokio::time::timeout(timeout, connect)
            .await
            .map_err(|_| {
                format!(
                    "could not reach {}:{} within {:?} — the address is routed but nothing \
                     answered. Check Nova is running and that a firewall is not dropping the port.",
                    self.host, self.port, timeout
                )
            })?
            .map_err(|e| {
                format!(
                    "connect to {}:{}: {e} — is Nova running and reachable on the LAN?",
                    self.host, self.port
                )
            })?;
        http_get_over(tcp, &self.host, &query, timeout).await
    }
}

/// One HTTP/1.1 GET over an established stream, returning the body.
///
/// `Connection: close` turns response framing into "read to EOF", which removes
/// any need to parse `Content-Length` or handle chunked encoding. For four
/// request/response pairs against a known server that is the right trade —
/// pulling in an HTTP client stack to send four GETs would cost more than it
/// buys.
async fn http_get_over<S>(mut stream: S, host: &str, path: &str, timeout: Duration) -> Result<String, String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: Echo\r\nConnection: close\r\n\r\n"
    );
    let exchange = async {
        stream
            .write_all(request.as_bytes())
            .await
            .map_err(|e| format!("send request: {e}"))?;
        stream.flush().await.map_err(|e| format!("flush request: {e}"))?;
        let mut raw = Vec::new();
        stream
            .read_to_end(&mut raw)
            .await
            .map_err(|e| format!("read response: {e}"))?;
        Ok::<Vec<u8>, String>(raw)
    };

    let raw = tokio::time::timeout(timeout, exchange)
        .await
        .map_err(|_| {
            format!(
                "the host did not answer within {:?} — if this was the PIN step, nobody \
                 completed Nova's dialog",
                timeout
            )
        })??;

    let text = String::from_utf8_lossy(&raw).into_owned();
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .ok_or("the host's reply had no HTTP body")?;
    Ok(body.to_string())
}

/// Extract `<tag>…</tag>` from Nova's XML replies.
///
/// A substring scan rather than an XML parser: these documents are generated by
/// one known `format!` in `pairing.rs`, they are flat, and every value is hex or
/// a small integer. A parser would add a dependency to gain robustness against
/// documents this endpoint cannot produce.
fn xml_field(body: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = body.find(&open)? + open.len();
    let end = body[start..].find(&close)? + start;
    Some(body[start..end].trim().to_string())
}

/// Nova signals refusal with `<paired>0</paired>` and HTTP 200, so the status
/// line says nothing useful and this is the only place failure is visible.
fn expect_paired(body: &str, phase: &str) -> Result<(), String> {
    match xml_field(body, "paired").as_deref() {
        Some("1") => Ok(()),
        Some(other) => Err(format!("the host refused at {phase} (paired={other})")),
        None => {
            let detail = xml_field(body, "status_message")
                .or_else(|| body.lines().next().map(str::to_string))
                .unwrap_or_default();
            Err(format!("the host's {phase} reply was not a pairing answer: {detail}"))
        }
    }
}

fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn hex_pem_to_der(hex_pem: &str) -> Option<Vec<u8>> {
    let pem = String::from_utf8(hex::decode(hex_pem.trim()).ok()?).ok()?;
    identity::pem_to_der(&pem)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixed inputs, so these assert the algorithm rather than re-deriving it.
    const SALT_HEX: &str = "000102030405060708090A0B0C0D0E0F";
    const PIN: &str = "1234";

    #[test]
    fn the_aes_key_matches_the_hosts_derivation() {
        // Mirror of nova-server's `derive_aes_key`: SHA-256 over the *decoded*
        // salt followed by the PIN's ASCII bytes, truncated to 16.
        let mut expected = Sha256::new();
        expected.update(hex::decode(SALT_HEX).unwrap());
        expected.update(PIN.as_bytes());
        assert_eq!(derive_aes_key(SALT_HEX, PIN)[..], expected.finalize()[..16]);
    }

    #[test]
    fn a_different_pin_derives_a_different_key() {
        assert_ne!(derive_aes_key(SALT_HEX, "1234"), derive_aes_key(SALT_HEX, "4321"));
        assert_ne!(derive_aes_key(SALT_HEX, PIN), derive_aes_key("0F0E0D0C", PIN));
    }

    #[test]
    fn ecb_round_trips_the_payload_sizes_the_protocol_uses() {
        let key = derive_aes_key(SALT_HEX, PIN);
        for len in [16usize, 32, 48] {
            let data: Vec<u8> = (0..len as u8).collect();
            assert_eq!(aes_ecb_decrypt(&key, &aes_ecb_encrypt(&key, &data)), data, "len {len}");
        }
    }

    #[test]
    fn a_short_block_is_zero_extended_exactly_as_the_host_does() {
        // Not reachable through the protocol, but if the two implementations
        // ever disagreed the symptom would be a corrupt final block rather than
        // an error, so the behaviour is pinned.
        let key = derive_aes_key(SALT_HEX, PIN);
        let mut padded = [0u8; 16];
        padded[..3].copy_from_slice(&[1, 2, 3]);
        assert_eq!(aes_ecb_encrypt(&key, &[1, 2, 3]), aes_ecb_encrypt(&key, &padded));
    }

    #[test]
    fn xml_fields_are_extracted_from_the_hosts_actual_reply_shape() {
        let body = r#"<?xml version="1.0" encoding="utf-8"?><root status_code="200"><paired>1</paired><plaincert>DEADBEEF</plaincert></root>"#;
        assert_eq!(xml_field(body, "paired").as_deref(), Some("1"));
        assert_eq!(xml_field(body, "plaincert").as_deref(), Some("DEADBEEF"));
        assert_eq!(xml_field(body, "challengeresponse"), None);
    }

    #[test]
    fn a_refusal_is_distinguished_from_a_malformed_reply() {
        // These need different messages: one means "wrong PIN", the other means
        // "you are not talking to Nova".
        let refused = r#"<root status_code="200"><paired>0</paired></root>"#;
        assert!(expect_paired(refused, "clientpairingsecret").unwrap_err().contains("paired=0"));

        let wrong_server = "<html>404 Not Found</html>";
        let err = expect_paired(wrong_server, "getservercert").unwrap_err();
        assert!(err.contains("not a pairing answer"), "{err}");
    }

    #[test]
    fn an_error_reply_surfaces_the_hosts_status_message() {
        let body = r#"<root status_code="400" status_message="Out of order call to clientchallenge"></root>"#;
        let err = expect_paired(body, "clientchallenge").unwrap_err();
        assert!(err.contains("Out of order"), "the host's own diagnosis should survive: {err}");
    }

    #[test]
    fn device_names_with_spaces_survive_the_query_string() {
        assert_eq!(percent_encode("Bobby's Pixel 9"), "Bobby%27s%20Pixel%209");
        assert_eq!(percent_encode("ABCdef123-_.~"), "ABCdef123-_.~");
    }

    #[test]
    fn pins_are_always_four_digits() {
        for _ in 0..200 {
            let pin = generate_pin();
            assert_eq!(pin.len(), 4, "{pin}");
            assert!(pin.chars().all(|c| c.is_ascii_digit()), "{pin}");
        }
    }

    #[test]
    fn the_unique_id_is_stable_and_derived_from_the_identity() {
        let id = Identity::generate_rsa2048("echo-test").expect("identity");
        assert_eq!(unique_id_for(&id), unique_id_for(&id));
        assert_eq!(unique_id_for(&id).len(), 16);
        assert_eq!(unique_id_for(&id), id.fingerprint[..16].to_uppercase());
    }

    #[test]
    fn the_hex_pem_the_client_sends_is_what_the_host_decodes() {
        // Mirrors nova-server's `client_cert_der_from_hex_pem`.
        let id = Identity::generate_rsa2048("echo-test").expect("identity");
        let hex_pem = id.cert_hex_pem();
        let pem = String::from_utf8(hex::decode(&hex_pem).unwrap()).unwrap();
        let body: String = pem.lines().filter(|l| !l.contains("-----")).collect();
        assert_eq!(hex_pem_to_der(&hex_pem).unwrap(), id.cert_der);
        assert!(!body.is_empty());
    }

    #[tokio::test]
    async fn the_http_client_frames_a_response_by_connection_close() {
        // Verified against the live host before being written down: Nova's
        // hyper server honours `Connection: close`, so reading to EOF is
        // sufficient framing and no Content-Length parsing is needed.
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (client, mut server) = tokio::io::duplex(4096);
        let server = tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            let n = server.read(&mut buf).await.expect("read request");
            let request = String::from_utf8_lossy(&buf[..n]).into_owned();
            server
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/xml\r\n\
                      content-length: 70\r\nconnection: close\r\n\r\n\
                      <?xml version=\"1.0\"?><root status_code=\"200\"><paired>1</paired></root>",
                )
                .await
                .expect("write response");
            drop(server); // EOF is what ends the read
            request
        });

        let body = http_get_over(client, "nova.local", "/pair?x=1", Duration::from_secs(5))
            .await
            .expect("the response must parse");
        assert_eq!(xml_field(&body, "paired").as_deref(), Some("1"));
        assert!(!body.starts_with("HTTP/"), "headers must be stripped from the body");

        let request = server.await.expect("server task");
        assert!(request.starts_with("GET /pair?x=1 HTTP/1.1\r\n"), "{request}");
        assert!(request.contains("Host: nova.local\r\n"), "{request}");
        assert!(request.contains("Connection: close\r\n"), "{request}");
    }

    #[tokio::test]
    async fn a_host_that_never_answers_times_out_instead_of_hanging() {
        let (client, _server) = tokio::io::duplex(64);
        let err = http_get_over(client, "nova", "/pair", Duration::from_millis(150))
            .await
            .unwrap_err();
        assert!(err.contains("did not answer"), "{err}");
    }

    /// The whole handshake, driven by the real client against a host that
    /// reimplements `nova-server/src/pairing.rs`.
    ///
    /// This is the closest thing to hardware validation available here: it
    /// exercises phase ordering, the query-string shape, hex/PEM encoding, both
    /// AES directions, both hash constructions, and both signatures. What it
    /// cannot catch is the mock having copied a *misreading* of the host — only
    /// a live run against Nova proves that, which is why one is still owed.
    #[tokio::test]
    async fn the_full_handshake_completes_against_a_host_implementing_novas_side() {
        let host_identity = Identity::generate_rsa2048("nova").expect("host identity");
        let client_identity = Identity::generate_rsa2048("echo").expect("client identity");
        let pin = "4821";

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let host = tokio::spawn(mock_nova(listener, host_identity.clone(), pin.to_string()));

        let opts = PairOptions {
            port,
            consent_timeout: Duration::from_secs(10),
            step_timeout: Duration::from_secs(10),
            ..PairOptions::new("127.0.0.1", pin)
        };
        let mut seen = Vec::new();
        let paired = pair(&client_identity, &opts, &mut |e: PairEvent| seen.push(e))
            .await
            .expect("the handshake must complete");

        assert_eq!(paired.fingerprint, host_identity.fingerprint);
        assert!(matches!(seen.first(), Some(PairEvent::AwaitingConsent { .. })));
        assert!(
            seen.iter().any(|e| matches!(e, PairEvent::HostVerified)),
            "the client must report verifying the host before declaring success"
        );
        assert!(matches!(seen.last(), Some(PairEvent::Paired { .. })));

        let trusted = host.await.expect("host task");
        assert_eq!(
            trusted.expect("the host must have trusted a certificate"),
            client_identity.cert_der,
            "the host must trust the exact certificate the client presented"
        );
    }

    #[tokio::test]
    async fn a_wrong_pin_is_refused_rather_than_silently_pairing() {
        let host_identity = Identity::generate_rsa2048("nova").expect("host identity");
        let client_identity = Identity::generate_rsa2048("echo").expect("client identity");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        // The host was told 4821; the user types 1111.
        let host = tokio::spawn(mock_nova(listener, host_identity, "4821".to_string()));

        let opts = PairOptions {
            port,
            consent_timeout: Duration::from_secs(10),
            step_timeout: Duration::from_secs(10),
            ..PairOptions::new("127.0.0.1", "1111")
        };
        let err = pair(&client_identity, &opts, &mut |_| {})
            .await
            .expect_err("a wrong PIN must not pair");
        // Under a wrong key the challenge response decrypts to noise, so the
        // client catches it at its own hash check — before sending anything the
        // host could store.
        assert!(
            err.contains("PIN") || err.contains("hash"),
            "the error should point at the PIN: {err}"
        );
        assert!(host.await.expect("host task").is_none(), "nothing may be trusted");
    }

    /// A minimal Nova, mirroring `nova-server/src/pairing.rs`'s `/pair` handler.
    /// Returns the certificate it ended up trusting, if any.
    async fn mock_nova(
        listener: tokio::net::TcpListener,
        identity: Identity,
        pin: String,
    ) -> Option<Vec<u8>> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let cert_sig = identity.cert_signature().expect("host cert signature").to_vec();
        let mut aes_key = [0u8; 16];
        let mut host_secret = [0u8; 16];
        let mut host_challenge = [0u8; 16];
        let mut client_hash: Vec<u8> = Vec::new();
        let mut client_cert: Vec<u8> = Vec::new();
        let mut trusted = None;

        // Bounded by a timeout rather than by a connection count: a handshake
        // that fails part-way — the wrong-PIN case — makes fewer than four
        // requests, and waiting for the missing one would hang the test rather
        // than fail it.
        for _ in 0..4 {
            let accepted =
                tokio::time::timeout(Duration::from_secs(2), listener.accept()).await;
            let Ok(Ok((mut sock, _))) = accepted else { break };
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]).into_owned();
            let query: std::collections::HashMap<String, String> = request
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|p| p.split_once('?'))
                .map(|(_, q)| {
                    q.split('&')
                        .filter_map(|kv| kv.split_once('='))
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect()
                })
                .unwrap_or_default();

            let body = if let Some(salt) = query.get("salt") {
                // Phase 1. The PIN is whatever was typed at the host's dialog.
                aes_key = derive_aes_key(salt, &pin);
                client_cert = hex_pem_to_der(query.get("clientcert").expect("clientcert"))
                    .expect("client cert decodes");
                format!(
                    "<root status_code=\"200\"><paired>1</paired><plaincert>{}</plaincert></root>",
                    identity.cert_hex_pem()
                )
            } else if let Some(challenge) = query.get("clientchallenge") {
                // Phase 2.
                let decrypted =
                    aes_ecb_decrypt(&aes_key, &hex::decode(challenge).expect("hex"));
                rand::thread_rng().fill_bytes(&mut host_secret);
                rand::thread_rng().fill_bytes(&mut host_challenge);
                let hash = sha256_concat(&[&decrypted, &cert_sig, &host_secret]);
                let mut plaintext = hash;
                plaintext.extend_from_slice(&host_challenge);
                format!(
                    "<root status_code=\"200\"><paired>1</paired><challengeresponse>{}</challengeresponse></root>",
                    hex::encode_upper(aes_ecb_encrypt(&aes_key, &plaintext))
                )
            } else if let Some(resp) = query.get("serverchallengeresp") {
                // Phase 3.
                client_hash = aes_ecb_decrypt(&aes_key, &hex::decode(resp).expect("hex"));
                let mut secret = host_secret.to_vec();
                secret.extend_from_slice(
                    &identity.sign_pkcs1_sha256(&host_secret).expect("host signs"),
                );
                format!(
                    "<root status_code=\"200\"><paired>1</paired><pairingsecret>{}</pairingsecret></root>",
                    hex::encode_upper(secret)
                )
            } else if let Some(secret) = query.get("clientpairingsecret") {
                // Phase 4 — the host's own two checks.
                let blob = hex::decode(secret).expect("hex");
                let (client_secret, signature) = blob.split_at(16);
                let expected = sha256_concat(&[
                    &host_challenge,
                    &client_cert[client_cert.len() - RSA2048_SIGNATURE_LEN..],
                    client_secret,
                ]);
                let same_hash = client_hash.starts_with(&expected);
                let sig_ok =
                    verify_host_signature(&client_cert, client_secret, signature).is_ok();
                if same_hash && sig_ok {
                    trusted = Some(client_cert.clone());
                }
                format!(
                    "<root status_code=\"200\"><paired>{}</paired></root>",
                    if same_hash && sig_ok { 1 } else { 0 }
                )
            } else {
                "<root status_code=\"400\"><paired>0</paired></root>".to_string()
            };

            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/xml\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(response.as_bytes()).await;
            let _ = sock.shutdown().await;
        }
        trusted
    }

    /// The client's two verifications, exercised against a host played by this
    /// test — the check that would otherwise only run against real hardware.
    #[test]
    fn the_client_accepts_a_correct_host_and_rejects_a_forged_one() {
        let host = Identity::generate_rsa2048("nova").expect("host identity");
        let host_sig = host.cert_signature().expect("host cert signature").to_vec();
        let client_challenge = [7u8; 16];
        let host_secret = [9u8; 16];

        // What a correct host commits to in phase 2 and reveals in phase 3.
        let committed = sha256_concat(&[&client_challenge, &host_sig, &host_secret]);
        let recomputed = sha256_concat(&[&client_challenge, &host_sig, &host_secret]);
        assert!(identity::constant_time_eq(&committed, &recomputed));

        let signature = host.sign_pkcs1_sha256(&host_secret).expect("host signs its secret");
        verify_host_signature(&host.cert_der, &host_secret, &signature)
            .expect("a genuine host must be accepted");

        // An impostor that learned the PIN, so its hash checks out, but signs
        // with its own key — the case verification 2 exists for.
        let impostor = Identity::generate_rsa2048("nova").expect("impostor identity");
        let forged = impostor.sign_pkcs1_sha256(&host_secret).expect("impostor signs");
        assert!(
            verify_host_signature(&host.cert_der, &host_secret, &forged).is_err(),
            "a signature from the wrong key must be refused"
        );

        // And a host that reveals a different secret than it committed to.
        let tampered = sha256_concat(&[&client_challenge, &host_sig, &[0u8; 16]]);
        assert!(!identity::constant_time_eq(&tampered, &committed));
    }
}
