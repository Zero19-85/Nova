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
use std::io::Write; 

// TLS/HTTPS Imports
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
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
use rcgen::{CertificateParams, KeyPair};

type PairSessions = Arc<Mutex<HashMap<String, PairSession>>>;

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
    pub cert_hex: String,
    pub cert_sig: Vec<u8>,
    pub private_key_der: Vec<u8>,
    pub cert_der: Vec<u8>,
}

impl ServerCrypto {
    pub fn new() -> Self {
        println!("🔑 Generating Nova RSA Security Certificate...");
        
        let mut rng = rand::thread_rng();
        let rsa_key = RsaPrivateKey::new(&mut rng, 2048).expect("Failed to generate RSA key");
            
        let pkcs8_pem = rsa_key.to_pkcs8_pem(rsa::pkcs8::LineEnding::LF).expect("Failed to encode RSA key to PEM");
        let key_pair = KeyPair::from_pem(&pkcs8_pem.to_string()).expect("Failed to load KeyPair into rcgen");

        let pkcs8_der = rsa_key.to_pkcs8_der().expect("Failed to encode RSA key to DER");
        let private_key_der = pkcs8_der.as_bytes().to_vec();

        let params = CertificateParams::new(vec!["NovaServer".into()]).expect("Failed to create certificate params");
        let cert = params.self_signed(&key_pair).expect("Failed to generate Certificate");
            
        let cert_der = cert.der().to_vec();
        let cert_hex = hex::encode(&cert_der);

        let sig_len = 256;
        let cert_sig = cert_der[cert_der.len() - sig_len..].to_vec();

        println!("✅ RSA Certificate Generated and Ready!");
        Self { cert_hex, cert_sig, private_key_der, cert_der }
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

pub async fn start_pairing_server(port: u16, host_ip: String, server_id: String, server_mac: String) {
    let http_addr = SocketAddr::from(([0, 0, 0, 0], port)); // 47989
    let https_addr = SocketAddr::from(([0, 0, 0, 0], 47984)); // HTTPS Port

    let http_listener = TcpListener::bind(http_addr).await.expect("Failed to bind HTTP port");
    let https_listener = TcpListener::bind(https_addr).await.expect("Failed to bind HTTPS port");

    println!("🔐 NVIDIA HTTP server listening on port {}", port);
    println!("🔒 NVIDIA HTTPS server listening on port 47984");

    let crypto = Arc::new(ServerCrypto::new());
    let sessions: PairSessions = Arc::new(Mutex::new(HashMap::new()));

    // --- SETUP TLS ---
    let cert = CertificateDer::from(crypto.cert_der.clone());
    let key = PrivateKeyDer::Pkcs8(crypto.private_key_der.clone().into());
    let mut tls_config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .expect("Failed to build TLS config");
    tls_config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let tls_acceptor = TlsAcceptor::from(Arc::new(tls_config));

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
    let tls_acceptor_clone = tls_acceptor.clone();

    tokio::task::spawn(async move {
        loop {
            let (stream, _) = https_listener.accept().await.unwrap();
            let tls_acceptor_inner = tls_acceptor_clone.clone();
            let ip_clone = ip_https.clone();
            let id_clone = id_https.clone();
            let mac_clone = mac_https.clone();
            let crypt = crypto_https.clone();
            let sess = sessions_https.clone();
            let pin = pin_https.clone();

            tokio::task::spawn(async move {
                // Wrap the TCP stream in TLS
                let tls_stream = match tls_acceptor_inner.accept(stream).await {
                    Ok(s) => s,
                    Err(_) => return, // Ignore random network scans
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
                        async move { handle_request(req, ip, id, mac, crypt, sess, pin).await }
                    }))
                    .await;
            });
        }
    });

    // --- RUN HTTP LOOP ---
    loop {
        let (stream, _) = http_listener.accept().await.unwrap();
        let io = TokioIo::new(stream);
        let ip_clone = host_ip.clone();
        let id_clone = server_id.clone();
        let mac_clone = server_mac.clone();
        let crypto_clone = crypto.clone();
        let sessions_clone = sessions.clone();
        let pin_clone = global_pin.clone();

        tokio::task::spawn(async move {
            let _ = http1::Builder::new()
                .serve_connection(io, service_fn(move |req| {
                    let ip = ip_clone.clone();
                    let id = id_clone.clone();
                    let mac = mac_clone.clone();
                    let crypt = crypto_clone.clone();
                    let sess = sessions_clone.clone();
                    let pin = pin_clone.clone();
                    async move { handle_request(req, ip, id, mac, crypt, sess, pin).await }
                }))
                .await;
        });
    }
}

async fn handle_request(
    req: Request<Incoming>,
    _host_ip: String,
    server_id: String,
    _server_mac: String,
    crypto: Arc<ServerCrypto>,
    sessions: PairSessions,
    global_pin: Arc<Mutex<String>>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let full_uri = req.uri().to_string();
    if !full_uri.contains("/serverinfo") {
        println!("📥 Moonlight Request: {}", full_uri);
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
            let body = format!(r#"<?xml version="1.0" encoding="utf-8"?>
<root status_code="200">
    <hostname>Nova</hostname>
    <appversion>7.1.431.0</appversion>
    <GfeVersion>3.23.0.74</GfeVersion>
    <uniqueid>{}</uniqueid>
    <HttpsPort>47984</HttpsPort>
    <ExternalPort>47989</ExternalPort>
    <mac>00:11:22:33:44:55</mac>
    <LocalIP>{}</LocalIP>
    <ServerCodecModeSupport>259</ServerCodecModeSupport>
    <PairStatus>{}</PairStatus>
    <currentgame>0</currentgame>
    <state>SUNSHINE_SERVER_FREE</state>
    <MaxLumaPixelsHEVC>1869449984</MaxLumaPixelsHEVC>
</root>"#, server_id, _host_ip, pair_status);
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

                println!("\n=======================================================");
                println!("🚨 QUICK! TYPE THE 4-DIGIT PIN AND PRESS ENTER! 🚨");
                println!("=======================================================\n");

                let mut attempts = 0;
                let mut pin = String::new();
                while attempts < 20 { 
                    {
                        let p = global_pin.lock().unwrap().clone();
                        if p.len() == 4 {
                            pin = p;
                            *global_pin.lock().unwrap() = String::new();
                            break;
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    attempts += 1;
                }

                if pin.is_empty() {
                    println!("⏳ Time ran out! Moonlight will drop. Just click your PC to try again!");
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

                let body = format!(r#"<?xml version="1.0" encoding="utf-8"?><root status_code="200"><paired>1</paired><challengeresponse>{}</challengeresponse></root>"#, hex::encode(encrypted_response));
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
                
                let body = format!(r#"<?xml version="1.0" encoding="utf-8"?><root status_code="200"><paired>1</paired><pairingsecret>{}</pairingsecret></root>"#, hex::encode(pairing_secret));
                Ok(make_xml_response(&body))

            } else if phrase == "clientpairingsecret" || client_pairing_secret.is_some() {
                {
                    let mut lock = sessions.lock().unwrap();
                    let session = lock.entry(client_id.clone()).or_default();
                    session.last_phase = "CLIENTPAIRINGSECRET".to_string();
                }
                println!("🎉 Phase 4: Handshake Complete! Device is officially paired.");
                let body = r#"<?xml version="1.0" encoding="utf-8"?><root status_code="200"><paired>1</paired></root>"#;
                Ok(make_xml_response(body))
            } else {
                Ok(make_error_response("Unknown pairing request"))
            }
        }

        "/unpair" => {
            println!("🧹 Moonlight requested /unpair");
            sessions.lock().unwrap().remove(&client_id);
            let body = r#"<?xml version="1.0" encoding="utf-8"?><root status_code="200"><paired>1</paired></root>"#;
            Ok(make_xml_response(body))
        }

        // 🌟 APP LIST IS NOW PROPERLY ROUTED THROUGH SECURE HTTPS!
        "/applist" => {
            println!("📋 Moonlight requested /applist");
            let body = r#"<?xml version="1.0" encoding="utf-8"?>
<root status_code="200">
    <App><IsMediaApp>0</IsMediaApp><AppTitle>Desktop</AppTitle><ID>1</ID></App>
    <App><IsMediaApp>0</IsMediaApp><AppTitle>Steam</AppTitle><ID>2</ID></App>
    <App><IsMediaApp>0</IsMediaApp><AppTitle>Xbox App</AppTitle><ID>3</ID></App>
    <App><IsMediaApp>0</IsMediaApp><AppTitle>Virtual Desktop</AppTitle><ID>4</ID></App>
</root>"#;
            Ok(make_xml_response(body))
        }

        "/launch" => {
            println!("🚀 Moonlight requested /launch for app!");
            let body = r#"<?xml version="1.0" encoding="utf-8"?><root status_code="200"><gamesession>1</gamesession></root>"#;
            Ok(make_xml_response(body))
        }

        "/resume" => {
            println!("▶️ Moonlight requested /resume");
            let body = r#"<?xml version="1.0" encoding="utf-8"?><root status_code="200"><resume>1</resume></root>"#;
            Ok(make_xml_response(body))
        }

        _ => {
            let mut res = Response::new(Full::new(Bytes::from("Not Found")));
            *res.status_mut() = StatusCode::NOT_FOUND;
            Ok(res)
        }
    }
}

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