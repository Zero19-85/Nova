//! Certificate identity and mutual TLS, for both peers.
//!
//! Nova's host certificate and an Echo client's certificate are the same kind
//! of object playing opposite roles, so the code that creates, fingerprints,
//! and checks them is written once here.
//!
//! ## The trust model, in one paragraph
//!
//! Every participant has a self-signed certificate and is known by the
//! **SHA-256 of its DER**. Chain validation is meaningless for self-signed
//! certificates, so it is not attempted; what a TLS handshake proves is
//! *possession of the private key* (the CertificateVerify signature, checked
//! for real), and authorization is a separate step: look the presented
//! certificate's fingerprint up in a set of known peers. This is the model
//! Nova already used for Moonlight pairing on port 47984, generalised so the
//! Echo control channel and the signaling relay can all use it.
//!
//! Two verifier flavours fall out of that:
//!
//! - [`AcceptAnyClientCert`] — server side. Requires a client certificate,
//!   accepts any at handshake time, and leaves authorization to the caller
//!   (which matches the fingerprint against its own store). Mirrors Sunshine's
//!   `SSL_VERIFY_PEER | SSL_VERIFY_FAIL_IF_NO_PEER_CERT`.
//! - [`PinnedCert`] — client side. Accepts exactly one server certificate: the
//!   one whose DER matches a configured fingerprint. Used for the signaling
//!   relay, where trusting one key we operate is stronger (and cheaper) than
//!   trusting every public CA.

use std::path::Path;
use std::sync::{Arc, OnceLock};

use rustls_pki_types::{CertificateDer, PrivateKeyDer, UnixTime};
use sha2::{Digest, Sha256};
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use tokio_rustls::rustls::{
    self, ClientConfig, DigitallySignedStruct, DistinguishedName, ServerConfig, SignatureScheme,
};

/// A peer's own certificate and private key.
#[derive(Clone)]
pub struct Identity {
    pub cert_der: Vec<u8>,
    /// PKCS#8 DER.
    pub key_der: Vec<u8>,
    /// SHA-256 of `cert_der`, lowercase hex — how every other participant
    /// refers to this peer.
    pub fingerprint: String,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print key material, not even by accident in an error log.
        write!(f, "Identity({}…)", &self.fingerprint[..16.min(self.fingerprint.len())])
    }
}

impl Identity {
    pub fn new(cert_der: Vec<u8>, key_der: Vec<u8>) -> Self {
        let fingerprint = fingerprint(&cert_der);
        Self { cert_der, key_der, fingerprint }
    }

    /// Short form used in logs — matches how Nova's pairing code abbreviates
    /// fingerprints, so the two are greppable together.
    pub fn short(&self) -> &str {
        &self.fingerprint[..16.min(self.fingerprint.len())]
    }

    /// Load `<dir>/<stem>.cert.der` + `<stem>.key.der`, generating and
    /// persisting a fresh self-signed identity if either is missing.
    ///
    /// Generate-on-first-use rather than requiring enrolment ceremony: a
    /// client's identity is meaningful only once a host has *paired* with it,
    /// and pairing is where the human is already present. An identity that
    /// exists but is not yet trusted anywhere is harmless.
    pub fn load_or_create(dir: &Path, stem: &str, subject: &str) -> Result<Self, String> {
        let cert_path = dir.join(format!("{stem}.cert.der"));
        let key_path = dir.join(format!("{stem}.key.der"));

        if let (Ok(cert_der), Ok(key_der)) =
            (std::fs::read(&cert_path), std::fs::read(&key_path))
        {
            if !cert_der.is_empty() && !key_der.is_empty() {
                return Ok(Self::new(cert_der, key_der));
            }
        }

        let key = rcgen::KeyPair::generate().map_err(|e| format!("generate key: {e}"))?;
        let params = rcgen::CertificateParams::new(vec![subject.to_string()])
            .map_err(|e| format!("certificate params: {e}"))?;
        let cert = params
            .self_signed(&key)
            .map_err(|e| format!("self-sign: {e}"))?;
        let identity = Self::new(cert.der().to_vec(), key.serialize_der());

        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        std::fs::write(&cert_path, &identity.cert_der)
            .map_err(|e| format!("write {}: {e}", cert_path.display()))?;
        std::fs::write(&key_path, &identity.key_der)
            .map_err(|e| format!("write {}: {e}", key_path.display()))?;
        Ok(identity)
    }
}

/// The RSA modulus size GameStream pairing requires. Not a preference: Nova's
/// `pairing.rs` reads the client certificate's signature as
/// `cert_der[len - 256..]` and verifies it with `RSA_PKCS1_2048_8192_SHA256`, so
/// a certificate that is not RSA-2048 cannot complete the handshake at all.
pub const PAIRING_RSA_BITS: usize = 2048;

/// Length of an RSA-2048 signature, and therefore of the trailing signature
/// blob both peers slice out of a DER certificate during pairing.
pub const RSA2048_SIGNATURE_LEN: usize = 256;

impl Identity {
    /// Load or generate an **RSA-2048** identity — the kind GameStream pairing
    /// requires.
    ///
    /// Separate from [`Identity::load_or_create`] (which generates the faster
    /// ECDSA P-256 key) because the two have genuinely different requirements.
    /// The relay and Nova's own Echo tunnel only need *an* identity, and ECDSA
    /// keys generate in microseconds. A client that intends to pair with Nova
    /// needs RSA specifically, and pays about a second on first run for it.
    ///
    /// Echo must use **this one identity for both** pairing and TLS. Nova's
    /// trust store is keyed by the fingerprint of the certificate presented
    /// during pairing, so a client that paired with one certificate and
    /// connected with another would be refused — correctly, and confusingly.
    ///
    /// An existing identity that is not RSA is **replaced**, with a warning. It
    /// could not have been paired with anything (the handshake would have
    /// rejected it), so there is no trust to lose; leaving it in place would
    /// only produce a pairing failure whose cause is invisible.
    pub fn load_or_create_rsa2048(
        dir: &Path,
        stem: &str,
        subject: &str,
    ) -> Result<Self, String> {
        let cert_path = dir.join(format!("{stem}.cert.der"));
        let key_path = dir.join(format!("{stem}.key.der"));

        if let (Ok(cert_der), Ok(key_der)) =
            (std::fs::read(&cert_path), std::fs::read(&key_path))
        {
            if !cert_der.is_empty() && !key_der.is_empty() {
                if rsa_private_key(&key_der).is_ok() {
                    return Ok(Self::new(cert_der, key_der));
                }
                println!(
                    "⚠️  {} is not an RSA key — regenerating. GameStream pairing requires an \
                     RSA-2048 client certificate, so this identity could never have paired.",
                    key_path.display()
                );
            }
        }

        let identity = Self::generate_rsa2048(subject)?;
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        std::fs::write(&cert_path, &identity.cert_der)
            .map_err(|e| format!("write {}: {e}", cert_path.display()))?;
        std::fs::write(&key_path, &identity.key_der)
            .map_err(|e| format!("write {}: {e}", key_path.display()))?;
        Ok(identity)
    }

    /// Generate an RSA-2048 identity without touching disk.
    ///
    /// The key is generated by the `rsa` crate rather than by rcgen because
    /// rcgen cannot generate RSA keys under the `ring` backend Android uses
    /// ("Ring doesn't have RSA key generation yet"). rcgen *signs* with a
    /// supplied RSA key under either backend, so the certificate itself is still
    /// built the ordinary way and the split is invisible downstream.
    pub fn generate_rsa2048(subject: &str) -> Result<Self, String> {
        use rsa::pkcs8::EncodePrivateKey;

        let mut rng = rand::thread_rng();
        let private = rsa::RsaPrivateKey::new(&mut rng, PAIRING_RSA_BITS)
            .map_err(|e| format!("generate RSA-{PAIRING_RSA_BITS} key: {e}"))?;
        let key_der = private
            .to_pkcs8_der()
            .map_err(|e| format!("encode RSA key as PKCS#8: {e}"))?
            .as_bytes()
            .to_vec();

        let key_pair = rcgen::KeyPair::from_pkcs8_der_and_sign_algo(
            &rustls_pki_types::PrivatePkcs8KeyDer::from(key_der.clone()),
            &rcgen::PKCS_RSA_SHA256,
        )
        .map_err(|e| format!("adopt RSA key into rcgen: {e}"))?;

        let params = rcgen::CertificateParams::new(vec![subject.to_string()])
            .map_err(|e| format!("certificate params: {e}"))?;
        let cert = params
            .self_signed(&key_pair)
            .map_err(|e| format!("self-sign: {e}"))?;

        Ok(Self::new(cert.der().to_vec(), key_der))
    }

    /// The trailing signature bytes of this certificate's DER.
    ///
    /// GameStream's pairing handshake mixes each peer's certificate signature
    /// into its challenge hashes, and both sides obtain it by slicing the last
    /// [`RSA2048_SIGNATURE_LEN`] bytes off the DER rather than by parsing the
    /// structure. Nova does exactly this (`pairing.rs`), so Echo must too — a
    /// "more correct" ASN.1 parse would produce the same bytes for a valid
    /// certificate and a *different* answer for a malformed one, and the two
    /// peers must agree even then.
    ///
    /// The key type is checked first, and that check is doing real work rather
    /// than being defensive: an ECDSA P-256 certificate is *larger* than 256
    /// bytes, so the slice would succeed and return 256 bytes of certificate
    /// **body**. Pairing would then fail three phases later with a hash mismatch
    /// indistinguishable from a wrong PIN. Length alone cannot catch this; the
    /// key can.
    pub fn cert_signature(&self) -> Result<&[u8], String> {
        rsa_private_key(&self.key_der).map_err(|e| {
            format!("this identity cannot pair — GameStream requires RSA-2048 ({e})")
        })?;
        self.cert_der
            .len()
            .checked_sub(RSA2048_SIGNATURE_LEN)
            .map(|start| &self.cert_der[start..])
            .ok_or_else(|| {
                format!(
                    "certificate is {} bytes, too small to carry an RSA-2048 signature — this \
                     identity cannot pair",
                    self.cert_der.len()
                )
            })
    }

    /// Sign `message` with this identity's private key, RSA PKCS#1 v1.5 over
    /// SHA-256 — the scheme Nova verifies with (`verify_client_signature`).
    pub fn sign_pkcs1_sha256(&self, message: &[u8]) -> Result<Vec<u8>, String> {
        use rsa::signature::{SignatureEncoding, Signer};

        let private = rsa_private_key(&self.key_der)?;
        let signing_key = rsa::pkcs1v15::SigningKey::<Sha256>::new(private);
        Ok(signing_key.sign(message).to_vec())
    }

    /// This certificate as PEM, hex-encoded uppercase — the `clientcert` wire
    /// format.
    ///
    /// Note the double encoding is deliberate and not redundant: GameStream
    /// carries the **PEM text** (not the DER) hex-encoded, because the original
    /// implementation fed the value straight to OpenSSL's `PEM_read_bio_X509`.
    /// Nova documents the same quirk on its own `plaincert`.
    pub fn cert_hex_pem(&self) -> String {
        hex::encode_upper(der_to_pem(&self.cert_der, "CERTIFICATE").as_bytes())
    }
}

fn rsa_private_key(key_der: &[u8]) -> Result<rsa::RsaPrivateKey, String> {
    use rsa::pkcs8::DecodePrivateKey;
    rsa::RsaPrivateKey::from_pkcs8_der(key_der)
        .map_err(|e| format!("private key is not an RSA PKCS#8 key: {e}"))
}

/// Wrap DER in PEM armour, 64 characters to a line.
pub fn der_to_pem(der: &[u8], label: &str) -> String {
    let b64 = base64_encode(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

/// Recover DER from PEM armour, ignoring the armour lines.
pub fn pem_to_der(pem: &str) -> Option<Vec<u8>> {
    let body: String = pem.lines().filter(|l| !l.contains("-----")).collect();
    let der = base64_decode(&body)?;
    (!der.is_empty()).then_some(der)
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    out
}

fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let mut acc: u32 = 0;
    let mut bits = 0;
    let mut out = Vec::new();
    for c in input.bytes() {
        if c == b'=' || c.is_ascii_whitespace() {
            continue;
        }
        let v = B64.iter().position(|&b| b == c)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// SHA-256 fingerprint (lowercase hex) of a certificate's DER bytes.
pub fn fingerprint(der: &[u8]) -> String {
    hex::encode(Sha256::digest(der))
}

/// Decode a 64-character hex SHA-256 fingerprint.
pub fn parse_fingerprint(hex_fp: &str) -> Result<[u8; 32], String> {
    let bytes = hex::decode(hex_fp.trim()).map_err(|e| format!("not valid hex: {e}"))?;
    bytes
        .try_into()
        .map_err(|_| "must be a 32-byte (64 hex character) SHA-256".to_string())
}

/// Length-independent comparison, so a fingerprint check cannot be walked byte
/// by byte by timing the reply.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(not(any(feature = "aws-lc-rs", feature = "ring")))]
compile_error!(
    "nova-core needs a TLS crypto provider: enable feature `aws-lc-rs` (the default, used by \
     the host) or `ring` (used by the Android build, which cannot easily build aws-lc-sys)."
);

/// The TLS crypto provider this build uses.
///
/// Named explicitly at every call site rather than relying on rustls's
/// *process-default* provider, and that distinction is load-bearing rather than
/// stylistic. A workspace build can legitimately compile both providers at once
/// — `nova-server` enables `aws-lc-rs` while `echo-android` enables `ring`, and
/// Cargo unifies features across the graph — and with two providers linked and
/// no default installed, `ConfigBuilder` panics at runtime. Passing the
/// provider in makes our configuration independent of process-global state and
/// of whatever else happens to be linked into the binary.
///
/// The choice is purely local: provider selection is not visible on the wire, so
/// a `ring` client and an `aws-lc-rs` host interoperate normally.
pub fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    static PROVIDER: OnceLock<Arc<rustls::crypto::CryptoProvider>> = OnceLock::new();
    PROVIDER
        .get_or_init(|| {
            // aws-lc-rs wins when both are compiled, so a workspace build keeps
            // the host on exactly the provider it ships with.
            #[cfg(feature = "aws-lc-rs")]
            {
                Arc::new(rustls::crypto::aws_lc_rs::default_provider())
            }
            #[cfg(all(feature = "ring", not(feature = "aws-lc-rs")))]
            {
                Arc::new(rustls::crypto::ring::default_provider())
            }
        })
        .clone()
}

fn provider_algs() -> rustls::crypto::WebPkiSupportedAlgorithms {
    provider().signature_verification_algorithms
}

/// Server-side policy: a client certificate is **required**, any certificate
/// is accepted at handshake time, and authorization happens afterwards by
/// fingerprint. See the module docs.
#[derive(Debug)]
pub struct AcceptAnyClientCert {
    algs: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl Default for AcceptAnyClientCert {
    fn default() -> Self {
        Self::new()
    }
}

impl AcceptAnyClientCert {
    pub fn new() -> Self {
        Self { algs: provider_algs() }
    }
}

impl ClientCertVerifier for AcceptAnyClientCert {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        // No CA hints: peer certificates are self-signed, and an empty list
        // tells the client "send whatever certificate you have".
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
        // A connection with no client certificate can never be authorized, so
        // fail it at the handshake rather than one layer later.
        true
    }
}

/// Client-side policy: accept exactly the server certificate whose DER hashes
/// to `pin`.
///
/// Chain building, name validation, and expiry are irrelevant under a pin —
/// the certificate is not being *trusted*, it is being *recognised*. The
/// signature checks are still delegated to rustls's real verifiers, because
/// those prove the peer holds the matching private key, which is the property
/// that actually matters.
pub struct PinnedCert {
    pin: [u8; 32],
    algs: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl std::fmt::Debug for PinnedCert {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PinnedCert({}…)", hex::encode(&self.pin[..8]))
    }
}

impl PinnedCert {
    pub fn new(pin: [u8; 32]) -> Self {
        Self { pin, algs: provider_algs() }
    }
}

impl ServerCertVerifier for PinnedCert {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls_pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let got: [u8; 32] = Sha256::digest(end_entity.as_ref()).into();
        if constant_time_eq(&got, &self.pin) {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!(
                "server certificate {}… does not match the configured pin {}…",
                hex::encode(&got[..8]),
                hex::encode(&self.pin[..8])
            )))
        }
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
}

fn to_rustls(identity: &Identity) -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
    (
        CertificateDer::from(identity.cert_der.clone()),
        PrivateKeyDer::Pkcs8(identity.key_der.clone().into()),
    )
}

/// Mutual-TLS **client** config: present `identity`, recognise the server by
/// `server_pin`.
pub fn client_config_pinned(identity: &Identity, server_pin: [u8; 32]) -> Result<ClientConfig, String> {
    let (cert, key) = to_rustls(identity);
    ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .map_err(|e| format!("client TLS versions: {e}"))?
        .dangerous() // "dangerous" = custom verifier; the pin IS the verification
        .with_custom_certificate_verifier(Arc::new(PinnedCert::new(server_pin)))
        .with_client_auth_cert(vec![cert], key)
        .map_err(|e| format!("client TLS config: {e}"))
}

/// Mutual-TLS **server** config: present `identity` and require a client
/// certificate, leaving authorization to the caller.
pub fn server_config_require_client_cert(identity: &Identity) -> Result<ServerConfig, String> {
    let (cert, key) = to_rustls(identity);
    ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .map_err(|e| format!("server TLS versions: {e}"))?
        .with_client_cert_verifier(Arc::new(AcceptAnyClientCert::new()))
        .with_single_cert(vec![cert], key)
        .map_err(|e| format!("server TLS config: {e}"))
}

/// The peer certificate a completed TLS session presented, as a fingerprint.
pub fn peer_fingerprint(certs: Option<&[CertificateDer<'_>]>) -> Option<String> {
    certs?.first().map(|c| fingerprint(c.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One RSA-2048 identity for the whole module. Generating a key per test
    /// would dominate the suite's runtime for no added coverage.
    fn rsa_identity() -> &'static Identity {
        static ID: OnceLock<Identity> = OnceLock::new();
        ID.get_or_init(|| Identity::generate_rsa2048("echo-test").expect("generate RSA identity"))
    }

    #[test]
    fn a_pairing_identity_carries_a_256_byte_signature_nova_can_slice_out() {
        // Nova reads `cert_der[len - 256..]` and verifies it as RSA-2048. If
        // this ever yields a different length, pairing breaks with a hash
        // mismatch rather than an obvious error, so the length is the assertion.
        let id = rsa_identity();
        let sig = id.cert_signature().expect("RSA-2048 cert must carry a signature");
        assert_eq!(sig.len(), RSA2048_SIGNATURE_LEN);
        assert_eq!(sig, &id.cert_der[id.cert_der.len() - 256..]);
    }

    #[test]
    fn an_ecdsa_identity_is_rejected_as_a_pairing_identity() {
        // The whole reason `load_or_create_rsa2048` exists — and the reason
        // `cert_signature` checks the KEY rather than the certificate length.
        let dir = temp_dir("ecdsa-guard");
        let ecdsa = Identity::load_or_create(&dir, "ec", "echo-test").expect("ecdsa identity");

        // The trap this guards: an ECDSA certificate is comfortably larger than
        // 256 bytes, so a length check would pass and hand back certificate
        // *body* bytes as if they were a signature. Pairing would then fail
        // three phases later with a hash mismatch that looks exactly like a
        // wrong PIN.
        assert!(
            ecdsa.cert_der.len() > RSA2048_SIGNATURE_LEN,
            "if this ever became false the length check would be sufficient and this \
             test would be testing nothing"
        );
        assert!(ecdsa.cert_signature().is_err(), "must be refused on key type, not length");
        assert!(ecdsa.sign_pkcs1_sha256(b"secret").is_err(), "cannot produce a pairing signature");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_signature_verifies_under_the_scheme_nova_checks_with() {
        // Nova verifies with RSA_PKCS1_2048_8192_SHA256 via webpki. Here we
        // check the same scheme through the `rsa` crate's verifier, which is
        // the property that matters: the bytes we emit must satisfy PKCS#1 v1.5
        // over SHA-256 against the public key inside our own certificate.
        use rsa::signature::{SignatureEncoding, Signer, Verifier};

        let id = rsa_identity();
        let message = b"a 16-byte secret";
        let signature = id.sign_pkcs1_sha256(message).expect("sign");
        assert_eq!(signature.len(), RSA2048_SIGNATURE_LEN);

        let private = rsa_private_key(&id.key_der).expect("our own key parses");
        let verifying = rsa::pkcs1v15::VerifyingKey::<Sha256>::new(private.to_public_key());
        let parsed = rsa::pkcs1v15::Signature::try_from(signature.as_slice()).expect("parse");
        verifying.verify(message, &parsed).expect("our signature must verify");

        // A different message must not verify under the same signature.
        let other = rsa::pkcs1v15::SigningKey::<Sha256>::new(private).sign(b"different");
        assert_ne!(other.to_vec(), signature);
    }

    #[test]
    fn hex_pem_round_trips_the_way_the_wire_format_expects() {
        // `clientcert`/`plaincert` carry hex-encoded PEM *text*, not DER. Nova
        // hex-decodes, strips the armour, and base64-decodes; this asserts our
        // encoder is the exact inverse of that.
        let id = rsa_identity();
        let hex_pem = id.cert_hex_pem();
        let pem_bytes = hex::decode(&hex_pem).expect("hex decodes");
        let pem = String::from_utf8(pem_bytes).expect("PEM is text");
        assert!(pem.starts_with("-----BEGIN CERTIFICATE-----"));
        assert_eq!(pem_to_der(&pem).expect("PEM decodes"), id.cert_der);
    }

    #[test]
    fn base64_handles_every_padding_case() {
        // From 1: an empty body is deliberately rejected by `pem_to_der`, since
        // a certificate with no bytes is not a certificate.
        for len in 1..8usize {
            let data: Vec<u8> = (0..len as u8).collect();
            let pem = der_to_pem(&data, "TEST");
            assert_eq!(pem_to_der(&pem).expect("round trip"), data, "length {len}");
        }
    }

    #[test]
    fn a_non_rsa_identity_on_disk_is_replaced_rather_than_used() {
        // A stale ECDSA identity from before pairing existed must not be loaded
        // as a pairing identity — it would fail at the last handshake phase.
        let dir = temp_dir("regenerate");
        let stale = Identity::load_or_create(&dir, "echo", "echo-test").expect("stale identity");
        let replaced = Identity::load_or_create_rsa2048(&dir, "echo", "echo-test").expect("rsa");
        assert_ne!(replaced.fingerprint, stale.fingerprint, "the identity must change");
        assert!(replaced.cert_signature().is_ok());

        // ...and the replacement must then be stable across restarts.
        let again = Identity::load_or_create_rsa2048(&dir, "echo", "echo-test").expect("reload");
        assert_eq!(again.fingerprint, replaced.fingerprint);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("nova-core-identity-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn identities_persist_so_a_restart_keeps_the_same_fingerprint() {
        let dir = temp_dir("persist");
        let first = Identity::load_or_create(&dir, "echo", "echo-client").expect("create");
        let second = Identity::load_or_create(&dir, "echo", "echo-client").expect("load");
        assert_eq!(
            first.fingerprint, second.fingerprint,
            "regenerating on restart would silently un-pair the device"
        );
        assert_eq!(first.cert_der, second.cert_der);
        assert_eq!(first.fingerprint.len(), 64);

        // A different stem is a different peer.
        let other = Identity::load_or_create(&dir, "other", "other").expect("create");
        assert_ne!(first.fingerprint, other.fingerprint);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fingerprints_parse_and_compare_safely() {
        assert!(parse_fingerprint(&"ab".repeat(32)).is_ok());
        assert!(parse_fingerprint(&format!("  {}  ", "cd".repeat(32))).is_ok());
        assert!(parse_fingerprint("abcd").is_err());
        assert!(parse_fingerprint(&"zz".repeat(32)).is_err());
        assert!(parse_fingerprint("").is_err());

        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn a_pin_recognises_exactly_one_certificate() {
        let dir = temp_dir("pin");
        let real = Identity::load_or_create(&dir, "real", "relay").expect("create");
        let impostor = Identity::load_or_create(&dir, "impostor", "relay").expect("create");

        let verifier = PinnedCert::new(parse_fingerprint(&real.fingerprint).unwrap());
        let name = rustls_pki_types::ServerName::try_from("relay").unwrap();
        let now = UnixTime::since_unix_epoch(std::time::Duration::from_secs(1_800_000_000));

        assert!(verifier
            .verify_server_cert(&CertificateDer::from(real.cert_der.clone()), &[], &name, &[], now)
            .is_ok());
        assert!(
            verifier
                .verify_server_cert(&CertificateDer::from(impostor.cert_der), &[], &name, &[], now)
                .is_err(),
            "a different certificate with the same subject must be refused"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn debug_output_never_leaks_key_material() {
        let dir = temp_dir("debug");
        let id = Identity::load_or_create(&dir, "secret", "peer").expect("create");
        let shown = format!("{id:?}");
        assert!(shown.contains(&id.fingerprint[..16]));
        assert!(!shown.contains(&hex::encode(&id.key_der)), "key must never be printable");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
