use std::net::UdpSocket;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusty_enet as enet;

use crate::rtsp::ClientInfo;

/// Moonlight's "control stream" is ENet (reliable UDP), not TCP — this is what
/// was failing with "error 11 / check UDP 47999" after the RTSP handshake
/// completed. We host a single-peer ENet server here; the library handles the
/// CONNECT/VERIFY_CONNECT handshake, acks, and channel framing automatically.
pub fn start_control_server(port: u16, client_info: Arc<Mutex<Option<ClientInfo>>>) {
    let socket = match UdpSocket::bind(("0.0.0.0", port)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("⚠️  Failed to bind control port {}: {}", port, e);
            return;
        }
    };
    println!("🎮 Control stream (ENet) listening on UDP {}", port);

    let mut host: enet::Host<UdpSocket> = match enet::Host::new(
        socket,
        enet::HostSettings {
            peer_limit: 1,
            channel_limit: 1,
            ..Default::default()
        },
    ) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("⚠️  Failed to create ENet control host: {:?}", e);
            return;
        }
    };

    loop {
        loop {
            match host.service() {
                Ok(Some(event)) => handle_event(event, &client_info),
                Ok(None) => break,
                Err(e) => {
                    eprintln!("⚠️  Control stream socket error: {:?}", e);
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(4));
    }
}

fn handle_event(event: enet::Event<UdpSocket>, client_info: &Arc<Mutex<Option<ClientInfo>>>) {
    match event {
        enet::Event::Connect { peer, .. } => {
            let addr = peer.address().map(|a| a.to_string()).unwrap_or_else(|| "?".to_string());
            println!("🎮 Control stream: peer connected from {}", addr);
            let _ = client_info;
        }
        enet::Event::Disconnect { peer, .. } => {
            let addr = peer.address().map(|a| a.to_string()).unwrap_or_else(|| "?".to_string());
            println!("🎮 Control stream: peer {} disconnected", addr);
        }
        enet::Event::Receive { channel_id, packet, .. } => {
            println!("🎮 Control rx {} bytes on channel {}", packet.data().len(), channel_id);
        }
    }
}
