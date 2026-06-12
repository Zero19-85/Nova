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

// Control message types (Sunshine stream.cpp packetTypes[]).
const PT_INVALIDATE_REF_FRAMES: u16 = 0x0301;
const PT_LOSS_STATS:            u16 = 0x0201;
const PT_PERIODIC_PING:         u16 = 0x0200;
const PT_REQUEST_IDR_FRAME:     u16 = 0x0302;

/// Parse a control-stream message: [u16 LE type][u16 LE payload length][payload].
/// The periodic 36-byte messages are loss stats (4B header + 32B payload).
fn handle_control_message(channel_id: u8, data: &[u8]) {
    if data.len() < 4 {
        return;
    }
    let msg_type = u16::from_le_bytes([data[0], data[1]]);
    match msg_type {
        PT_REQUEST_IDR_FRAME => {
            println!("🎮 Control: client requested IDR frame");
            crate::encoder::request_idr_global();
        }
        // We don't do reference frame invalidation — recover with an IDR
        // instead (valid per protocol; Sunshine does this when the encoder
        // lacks ref-invalidation support).
        PT_INVALIDATE_REF_FRAMES => {
            println!("🎮 Control: reference frames invalidated → forcing IDR");
            crate::encoder::request_idr_global();
        }
        // Loss stats arrive every ~50ms; payload[0] (i32 LE) is the loss count
        // since the last report. Only log when the client actually lost
        // something — this is the live signal that FEC is being exercised.
        PT_LOSS_STATS => {
            if data.len() >= 8 {
                let lost = i32::from_le_bytes([data[4], data[5], data[6], data[7]]);
                if lost > 0 {
                    println!("🎮 Loss stats: client lost {} packet(s) since last report", lost);
                }
            }
        }
        PT_PERIODIC_PING => {}
        _ => {
            println!("🎮 Control rx type 0x{:04x} ({} bytes) on channel {}",
                msg_type, data.len(), channel_id);
        }
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
            // The control stream dying means the client is gone, whether or not
            // an RTSP TEARDOWN ever arrived (abrupt exit, network drop). End the
            // session so the main loop resets video/audio state — otherwise a
            // reconnect inherits the dead session's target/keys (black screen).
            if let Ok(mut guard) = client_info.lock() {
                if let Some(info) = guard.as_mut() {
                    if info.streaming_active {
                        info.streaming_active = false;
                        println!("🎮 Control stream lost → ending session (resetting stream state)");
                    }
                }
            }
        }
        enet::Event::Receive { channel_id, packet, .. } => {
            handle_control_message(channel_id, packet.data());
        }
    }
}
