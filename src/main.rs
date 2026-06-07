// main.rs
mod capture;
mod rtsp;
mod rtp;
mod pairing;
mod debug;

use clap::Parser;
use std::sync::{Arc, Mutex};
use std::collections::HashMap;
use tokio::net::TcpListener;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};

use pairing::{handle_request, ServerCrypto};
use rcgen::{CertificateParams, KeyPair};
use mdns_sd::{ServiceDaemon, ServiceInfo};

#[derive(Parser, Debug)]
#[command(author, version, about = "Nova Server")]
struct Args {
    #[arg(long, default_value_t = 1920)]
    width: i32,
    #[arg(long, default_value_t = 1080)]
    height: i32,
    #[arg(long, default_value_t = 15000)]
    bitrate: i32,
    #[arg(long, default_value = "h264")]
    codec: String,
    #[arg(long, default_value_t = 60)]
    fps: u32,
}

fn generate_self_signed_cert() -> (Vec<u8>, Vec<u8>) {
    let key_pair = KeyPair::generate().unwrap();
    let params = CertificateParams::new(vec!["Nova".to_string()]).unwrap();
    let cert = params.self_signed(&key_pair).unwrap();

    (cert.der().to_vec(), key_pair.serialize_der())
}

#[tokio::main]
async fn main() {
    let _args = Args::parse();

    debug::init_debug_logger();

    println!("=== Nova Server ===");
    println!("🌐 LAN IP: {}", get_local_ip());

    let crypto = Arc::new(ServerCrypto::new());
    let sessions: pairing::PairSessions = Arc::new(Mutex::new(HashMap::new()));
    let global_pin = Arc::new(Mutex::new(String::new()));

    let server_id = "NOVA-LOCAL".to_string();
    let server_mac = "00:00:00:00:00:00".to_string();
    let host_ip = get_local_ip();

    // Generate real self-signed certificate once at startup
    let (cert_der, key_der) = generate_self_signed_cert();
    let cert = CertificateDer::from(cert_der);
    let key = PrivateKeyDer::Pkcs8(key_der.into());

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(config));

    // ====================== mDNS Broadcasting ======================
    let daemon = ServiceDaemon::new().unwrap();

    let service_type = "_nvstream._tcp.local.";
    let instance_name = "Nova";
    let port = 47989;

    let mut properties = std::collections::HashMap::new();
    properties.insert("server_name".to_string(), "Nova".to_string());
    properties.insert("uniqueid".to_string(), server_id.clone());

    let service_info = ServiceInfo::new(
        service_type,
        instance_name,
        &format!("{}.local.", instance_name),
        &host_ip,
        port,
        properties,
    ).unwrap();

    daemon.register(service_info).unwrap();
    println!("📡 mDNS broadcasting as Nova");

    // ====================== HTTPS Server (47984) ======================
    let https_listener = TcpListener::bind("0.0.0.0:47984").await.unwrap();
    println!("🔒 HTTPS listening on 47984");

    let crypto_clone = crypto.clone();
    let sessions_clone = sessions.clone();
    let pin_clone = global_pin.clone();

    let host_ip_https = host_ip.clone();
    let server_id_https = server_id.clone();
    let server_mac_https = server_mac.clone();

    tokio::spawn(async move {
        loop {
            let (stream, _) = https_listener.accept().await.unwrap();

            if let Ok(tls_stream) = acceptor.accept(stream).await {
                let io = TokioIo::new(tls_stream);

                let crypto = crypto_clone.clone();
                let sessions = sessions_clone.clone();
                let pin = pin_clone.clone();
                let host_ip = host_ip_https.clone();
                let server_id = server_id_https.clone();
                let server_mac = server_mac_https.clone();

                tokio::spawn(async move {
                    let _ = http1::Builder::new()
                        .serve_connection(
                            io,
                            service_fn(move |req| {
                                handle_request(
                                    req,
                                    host_ip.clone(),
                                    server_id.clone(),
                                    server_mac.clone(),
                                    crypto.clone(),
                                    sessions.clone(),
                                    pin.clone(),
                                )
                            }),
                        )
                        .await;
                });
            }
        }
    });

    // ====================== HTTP Server (47989) ======================
    let http_listener = TcpListener::bind("0.0.0.0:47989").await.unwrap();
    println!("🔐 HTTP listening on 47989");

    let crypto_clone2 = crypto.clone();
    let sessions_clone2 = sessions.clone();
    let pin_clone2 = global_pin.clone();

    tokio::spawn(async move {
        loop {
            let (stream, _) = http_listener.accept().await.unwrap();
            let io = TokioIo::new(stream);

            let crypto = crypto_clone2.clone();
            let sessions = sessions_clone2.clone();
            let pin = pin_clone2.clone();
            let host_ip = host_ip.clone();
            let server_id = server_id.clone();
            let server_mac = server_mac.clone();

            tokio::spawn(async move {
                let _ = http1::Builder::new()
                    .serve_connection(
                        io,
                        service_fn(move |req| {
                            handle_request(
                                req,
                                host_ip.clone(),
                                server_id.clone(),
                                server_mac.clone(),
                                crypto.clone(),
                                sessions.clone(),
                                pin.clone(),
                            )
                        }),
                    )
                    .await;
            });
        }
    });

    println!("✅ Nova Server running. Should now appear in Moonlight.");
    tokio::signal::ctrl_c().await.unwrap();
}

fn get_local_ip() -> String {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
    socket.connect("8.8.8.8:80").ok();
    socket.local_addr().map(|a| a.ip().to_string()).unwrap_or("127.0.0.1".to_string())
}