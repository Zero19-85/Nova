use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper::body::Incoming;
use http_body_util::Full;
use hyper::body::Bytes;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use hyper_util::rt::TokioIo;

pub async fn start_pairing_server(port: u16) {
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = TcpListener::bind(addr).await.expect("Failed to bind pairing port");

    println!("Pairing server listening on port {}", port);

    loop {
        let (stream, _) = listener.accept().await.unwrap();
        let io = TokioIo::new(stream);

        tokio::task::spawn(async move {
            if let Err(err) = http1::Builder::new()
                .serve_connection(io, service_fn(handle_pairing_request))
                .await
            {
                eprintln!("Pairing connection error: {}", err);
            }
        });
    }
}

async fn handle_pairing_request(
    req: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let path = req.uri().path();

    match path {
        "/serverinfo" => {
            let body = "<root><hostname>ARS</hostname></root>";
            Ok(Response::new(Full::new(Bytes::from(body))))
        }
        "/pair" => {
            println!("Pairing request received (stub - real handshake coming soon)");
            let body = "<root><paired>1</paired></root>";
            Ok(Response::new(Full::new(Bytes::from(body))))
        }
        _ => {
            let mut res = Response::new(Full::new(Bytes::from("Not Found")));
            *res.status_mut() = StatusCode::NOT_FOUND;
            Ok(res)
        }
    }
}