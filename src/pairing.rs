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
use url::Url;

pub struct ServerCrypto {
    pub cert_hex: String,
}

impl ServerCrypto {
    pub fn new() -> Self {
        println!("🔑 Generating Nova Security Certificate...");
        let certified_key = rcgen::generate_simple_self_signed(vec!["NovaServer".into()]).unwrap();
        let cert_der = certified_key.cert.der();
        let cert_hex = hex::encode(cert_der);
        println!("✅ Certificate Generated and Ready!");
        Self { cert_hex }
    }
}

pub async fn start_pairing_server(port: u16, host_ip: String, server_id: String, server_mac: String) {
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = TcpListener::bind(addr).await.expect("Failed to bind pairing port");

    println!("🔐 NVIDIA Pairing server listening on port {}", port);

    let crypto = tokio::task::spawn_blocking(|| {
        Arc::new(ServerCrypto::new())
    }).await.unwrap();

    loop {
        let (stream, _) = listener.accept().await.unwrap();
        let io = TokioIo::new(stream);
        let ip_clone = host_ip.clone();
        let id_clone = server_id.clone();
        let mac_clone = server_mac.clone();
        let crypto_clone = crypto.clone();

        tokio::task::spawn(async move {
            if let Err(err) = http1::Builder::new()
                .serve_connection(io, service_fn(move |req| {
                    let ip = ip_clone.clone();
                    let id = id_clone.clone();
                    let mac = mac_clone.clone();
                    let crypt = crypto_clone.clone();
                    async move { handle_request(req, ip, id, mac, crypt).await }
                }))
                .await
            {
                eprintln!("Pairing error: {}", err);
            }
        });
    }
}

async fn handle_request(req: Request<Incoming>, host_ip: String, server_id: String, server_mac: String, crypto: Arc<ServerCrypto>) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let full_uri = req.uri().to_string();
    println!("📥 Moonlight Request: {}", full_uri);
    
    let parsed_url = Url::parse(&format!("http://localhost{}", full_uri)).unwrap();
    let path = parsed_url.path();

    match path {
    "/serverinfo" => {
            // THE GOLDEN TICKET: A 100% exact replica of Sunshine's ServerInfo block.
            let body = format!(r#"<?xml version="1.0" encoding="utf-8"?>
<root status_code="200">
    <hostname>Nova</hostname>
    <appversion>7.1.431.0</appversion>
    <GfeVersion>3.23.0.74</GfeVersion>
    <uniqueid>{}</uniqueid>
    <NumApps>1</NumApps>
    <State>2</State>
    <BwmSupport>1</BwmSupport>
    <PlayamSupport>1</PlayamSupport>
    <AudioHapticSupport>1</AudioHapticSupport>
    <ServerCodecModeSupport>259</ServerCodecModeSupport>
    <httpsPort>47984</httpsPort>
    <mac>{}</mac>
    <LocalIP>{}</LocalIP>
    <ServerLocalIp>{}</ServerLocalIp>
    <ExternalIP>{}</ExternalIP>
    <ExternalPort>47989</ExternalPort>
    <displayMode>0</displayMode>
    <MaxLumaPixelsH264>8294400</MaxLumaPixelsH264>
    <MaxLumaPixelsHEVC>8294400</MaxLumaPixelsHEVC>
    <ServerLogGroupUId>0</ServerLogGroupUId>
</root>"#, server_id, server_mac, host_ip, host_ip, host_ip);

            let mut res = Response::new(Full::new(Bytes::from(body.clone())));
            *res.status_mut() = StatusCode::OK;
            res.headers_mut().insert(header::CONTENT_TYPE, "application/xml".parse().unwrap());
            res.headers_mut().insert(header::CONTENT_LENGTH, body.len().to_string().parse().unwrap());
            Ok(res)
        }
        "/pair" => {
            let phrase = parsed_url.query_pairs().find(|(k, _)| k == "phrase").map(|(_, v)| v.into_owned()).unwrap_or_default();
            
            let body = if phrase == "getservercert" {
                println!("🤝 Pairing Step 1: Sending Nova Security Certificate to Moonlight...");
                format!(r#"<?xml version="1.0" encoding="utf-8"?>
<root status_code="200">
    <paired>0</paired>
    <plaincert>{}</plaincert>
</root>"#, crypto.cert_hex)
            } else {
                r#"<?xml version="1.0" encoding="utf-8"?>
<root status_code="200">
    <paired>1</paired>
</root>"#.to_string()
            };

            let mut res = Response::new(Full::new(Bytes::from(body.clone())));
            *res.status_mut() = StatusCode::OK;
            res.headers_mut().insert(header::CONTENT_TYPE, "application/xml".parse().unwrap());
            res.headers_mut().insert(header::CONTENT_LENGTH, body.len().to_string().parse().unwrap());
            Ok(res)
        }
        _ => {
            let mut res = Response::new(Full::new(Bytes::from("Not Found")));
            *res.status_mut() = StatusCode::NOT_FOUND;
            res.headers_mut().insert(header::CONTENT_LENGTH, "9".parse().unwrap());
            Ok(res)
        }
    }
}