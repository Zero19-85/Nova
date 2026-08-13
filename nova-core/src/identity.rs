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
