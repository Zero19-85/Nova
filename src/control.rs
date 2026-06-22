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
const PT_ENCRYPTED:             u16 = 0x0001;
const PT_INVALIDATE_REF_FRAMES: u16 = 0x0301;
const PT_LOSS_STATS:            u16 = 0x0201;
const PT_PERIODIC_PING:         u16 = 0x0200;
const PT_REQUEST_IDR_FRAME:     u16 = 0x0302;
const PT_INPUT_DATA:            u16 = 0x0206;
// Sunshine extension: host→client HDR mode notification (Apollo stream.cpp IDX_HDR_MODE).
// Triggers Moonlight's VideoRenderer::SetHDR() → LiGetHdrMetadata() →
// SetDisplayHDR() → RequestSetCurrentDisplayModeAsync(Eotf2084), which
// physically switches the TV's HDMI port into HDR10 mode.
const PT_HDR_MODE:              u16 = 0x010e;

/// Builds the payload for a 0x010e HDR mode packet: `enabled(u8)` followed by
/// SS_HDR_METADATA (little-endian, #pragma pack(1), Apollo stream.cpp).
///
/// Field order: displayPrimaries[3]×{x:u16,y:u16} (R,G,B) + whitePoint×{x:u16,y:u16}
/// + maxDisplayLuminance(u32 nits) + minDisplayLuminance(u32, 0.0001-nit units)
/// + maxContentLightLevel(u16) + maxFrameAverageLightLevel(u16) + maxFullFrameLuminance(u32).
/// Total payload: 1 + 32 = 33 bytes.
fn build_hdr_mode_payload() -> Vec<u8> {
    let mut p = Vec::with_capacity(33);
    p.push(1u8); // enabled = true

    // SS_HDR_METADATA — BT.2020 D65 primaries, 1000-nit panel (matches our shim SEI values).
    // Primary order in SS_HDR_METADATA: [0]=Red, [1]=Green, [2]=Blue (Apollo display_base.cpp).
    // Units: chromaticity × 50000 (u16), luminance in nits (u32).
    let w = |v: u16| v.to_le_bytes();
    let d = |v: u32| v.to_le_bytes();

    p.extend_from_slice(&w(35400)); // Red x   (0.708 × 50000)
    p.extend_from_slice(&w(14600)); // Red y   (0.292 × 50000)
    p.extend_from_slice(&w(8500));  // Green x (0.170 × 50000)
    p.extend_from_slice(&w(39850)); // Green y (0.797 × 50000)
    p.extend_from_slice(&w(6550));  // Blue x  (0.131 × 50000)
    p.extend_from_slice(&w(2300));  // Blue y  (0.046 × 50000)
    p.extend_from_slice(&w(15635)); // WhitePoint x (D65 0.3127 × 50000)
    p.extend_from_slice(&w(16450)); // WhitePoint y (D65 0.3290 × 50000)
    p.extend_from_slice(&d(1000));  // maxDisplayLuminance: 1000 nits
    p.extend_from_slice(&d(500));   // minDisplayLuminance: 0.05 nit × 10000
    p.extend_from_slice(&w(0));     // maxContentLightLevel: 0 (Apollo: content-specific)
    p.extend_from_slice(&w(0));     // maxFrameAverageLightLevel: 0
    p.extend_from_slice(&d(400));   // maxFullFrameLuminance: 400 nits (typical for 1000-nit panel)
    p
}

/// Decrypt a 0x0001 encrypted control envelope (Sunshine stream.cpp
/// control_encrypted_t + IDX_ENCRYPTED handler):
///   [u16 LE type=0x0001][u16 LE length][u32 LE seq][16B GCM tag][ciphertext]
/// where length = 4 (seq) + 16 (tag) + ciphertext len, key = the /launch
/// rikey, no AAD.
///
/// Moonlight's RTSP ANNOUNCE negotiates SS_ENC_CONTROL_V2 (a 12-byte
/// deterministic IV), but in practice this client falls back to Nvidia's
/// original ("legacy") control encryption: AES-128-GCM with a 16-byte IV
/// (byte 0 = low byte of seq, bytes 1-15 = zero), tag(16) || ciphertext.
/// ring's AES_128_GCM only supports 96-bit (12-byte) nonces and cannot
/// represent a 16-byte IV at all, so this is decrypted via RustCrypto's
/// generic aes-gcm instead. Confirmed working end-to-end (60fps video +
/// low-latency input) — do not reintroduce the SS_ENC_CONTROL_V2 path
/// without first confirming the client actually negotiates it.
fn decrypt_control_message(rikey: &[u8; 16], data: &[u8]) -> Option<Vec<u8>> {
    // 4B outer header + 4B seq + 16B tag is the minimum (empty plaintext).
    if data.len() < 24 {
        return None;
    }
    let length = u16::from_le_bytes([data[2], data[3]]) as usize;
    // Sunshine's "Runt packet" check (stream.cpp IDX_ENCRYPTED): length must
    // cover at least the 4B seq + 16B tag.
    if length < 20 || 4 + length > data.len() {
        return None;
    }
    let seq_wire = &data[4..8];
    let payload  = &data[8..4 + length]; // tag(16) || ciphertext
    let tag = &payload[..16];
    let cipher = &payload[16..];

    legacy_gcm_decrypt(rikey, seq_wire[0], tag, cipher)
}

/// Nvidia's legacy control-stream encryption: AES-128-GCM with a 16-byte IV
/// (byte 0 = low byte of seq, bytes 1-15 = 0), tag(16) || ciphertext.
fn legacy_gcm_decrypt(rikey: &[u8; 16], seq_lo: u8, tag: &[u8], cipher: &[u8]) -> Option<Vec<u8>> {
    use aes_gcm::aead::{AeadInPlace, KeyInit, generic_array::GenericArray};
    use aes_gcm::AesGcm;
    use cipher::consts::U16;

    type Aes128Gcm16 = AesGcm<aes::Aes128, U16>;

    let mut iv = [0u8; 16];
    iv[0] = seq_lo;

    let key = Aes128Gcm16::new(GenericArray::from_slice(rikey));
    let nonce = GenericArray::from_slice(&iv);
    let tag = GenericArray::from_slice(tag);

    let mut buf = cipher.to_vec();
    key.decrypt_in_place_detached(nonce, &[], &mut buf, tag).ok()?;
    Some(buf)
}

/// Encrypt a plaintext inner control message with the same legacy 16-byte-IV
/// AES-128-GCM scheme as [`legacy_gcm_decrypt`], using our own outgoing
/// sequence counter (Sunshine: session->control.outgoing_iv / control.seq).
/// Returns (tag, ciphertext).
fn legacy_gcm_encrypt(rikey: &[u8; 16], seq_lo: u8, plaintext: &[u8]) -> ([u8; 16], Vec<u8>) {
    use aes_gcm::aead::{AeadInPlace, KeyInit, generic_array::GenericArray};
    use aes_gcm::AesGcm;
    use cipher::consts::U16;

    type Aes128Gcm16 = AesGcm<aes::Aes128, U16>;

    let mut iv = [0u8; 16];
    iv[0] = seq_lo;

    let key = Aes128Gcm16::new(GenericArray::from_slice(rikey));
    let nonce = GenericArray::from_slice(&iv);

    let mut buf = plaintext.to_vec();
    // AES-GCM encryption of a tiny control payload cannot fail (the only
    // failure mode is a plaintext exceeding ~64GiB).
    let tag = key.encrypt_in_place_detached(nonce, &[], &mut buf)
        .expect("AES-128-GCM encrypt of control reply");
    let mut tag_arr = [0u8; 16];
    tag_arr.copy_from_slice(&tag);
    (tag_arr, buf)
}

/// Build and send an encrypted 0x0001 control envelope back to the client:
/// encrypts `[u16 LE msg_type][u16 LE payload.len()][payload]` with
/// [`legacy_gcm_encrypt`] under `seq`, then wraps it as
/// `[u16 LE 0x0001][u16 LE length][u32 LE seq][tag(16)][ciphertext]`
/// (Sunshine stream.cpp encode_control). `seq` is the host's own outgoing
/// control sequence counter — distinct from the client's incoming `seq`.
fn send_control_reply(
    peer: &mut enet::Peer<UdpSocket>,
    channel_id: u8,
    rikey: &[u8; 16],
    seq: u32,
    msg_type: u16,
    payload: &[u8],
) {
    let mut plain = Vec::with_capacity(4 + payload.len());
    plain.extend_from_slice(&msg_type.to_le_bytes());
    plain.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    plain.extend_from_slice(payload);

    let (tag, cipher) = legacy_gcm_encrypt(rikey, seq.to_le_bytes()[0], &plain);

    let length = (cipher.len() + 20) as u16; // seq(4) + tag(16) + cipher
    let mut envelope = Vec::with_capacity(4 + length as usize);
    envelope.extend_from_slice(&PT_ENCRYPTED.to_le_bytes());
    envelope.extend_from_slice(&length.to_le_bytes());
    envelope.extend_from_slice(&seq.to_le_bytes());
    envelope.extend_from_slice(&tag);
    envelope.extend_from_slice(&cipher);

    if let Err(e) = peer.send(channel_id, &enet::Packet::reliable(envelope)) {
        println!("🎮 Control: failed to send reply: {:?}", e);
    }
}

/// Parse a control-stream message: [u16 LE type][u16 LE payload length][payload].
/// Modern Moonlight negotiates control encryption (SS_ENC_CONTROL_V2) and
/// wraps EVERYTHING — including IDR requests — in 0x0001 envelopes. Ignoring
/// those means the client can never request recovery, which is fatal with an
/// infinite GOP (one missed IDR = black screen forever).
fn handle_control_message(
    channel_id: u8,
    data: &[u8],
    client_info: &Arc<Mutex<Option<ClientInfo>>>,
    peer: &mut enet::Peer<UdpSocket>,
) {
    if data.len() < 4 {
        return;
    }
    let msg_type = u16::from_le_bytes([data[0], data[1]]);
    match msg_type {
        PT_ENCRYPTED => {
            let rikey = client_info.lock().ok()
                .and_then(|g| g.as_ref().map(|c| c.rikey));
            let Some(rikey) = rikey else {
                println!("🎮 Control: encrypted message but no session rikey — dropping");
                return;
            };
            match decrypt_control_message(&rikey, data) {
                // Inner message can't be another 0x0001 (would loop) — anything
                // else dispatches through the same handler.
                Some(inner) if inner.len() >= 2
                    && u16::from_le_bytes([inner[0], inner[1]]) != PT_ENCRYPTED =>
                {
                    handle_control_message(channel_id, &inner, client_info, peer);
                }
                Some(_) => {}
                None => {
                    let addr = peer.address().map(|a| a.to_string()).unwrap_or_else(|| "?".to_string());
                    println!("🎮 Control: failed to decrypt 0x0001 envelope ({} bytes) from {} — bad tag/format", data.len(), addr);
                    println!("    rikey={}", hex::encode(rikey));
                    println!("    raw  ={}", hex::encode(data));
                }
            }
        }
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
        // Echo the ping payload straight back as a basic keepalive ACK —
        // without any reply on the encrypted control channel, long-running
        // sessions look idle/dead to the client and it tears down the stream.
        // On the very first ping, also send the 0x010e HDR mode packet if
        // the session is HDR — this is what makes the Xbox call
        // RequestSetCurrentDisplayModeAsync(Eotf2084) and switch the TV to HDR.
        PT_PERIODIC_PING => {
            let session = client_info.lock().ok().and_then(|mut g| {
                g.as_mut().map(|c| {
                    let seq = c.control_out_seq;
                    c.control_out_seq = c.control_out_seq.wrapping_add(1);
                    let hdr_pkt = if c.hdr_requested && !c.hdr_mode_sent {
                        c.hdr_mode_sent = true;
                        let hdr_seq = c.control_out_seq;
                        c.control_out_seq = c.control_out_seq.wrapping_add(1);
                        Some((c.rikey, hdr_seq))
                    } else {
                        None
                    };
                    (c.rikey, seq, hdr_pkt)
                })
            });
            if let Some((rikey, seq, hdr_pkt)) = session {
                send_control_reply(peer, channel_id, &rikey, seq, PT_PERIODIC_PING, &data[4..]);
                if let Some((hdr_rikey, hdr_seq)) = hdr_pkt {
                    let payload = build_hdr_mode_payload();
                    send_control_reply(peer, channel_id, &hdr_rikey, hdr_seq, PT_HDR_MODE, &payload);
                    println!("🎨 Control: HDR mode packet sent (0x010e, BT.2020 1000-nit metadata)");
                }
            }
        }
        // Gamepad, mouse, and keyboard input (see input.rs): controller
        // packets are mirrored onto a virtual Xbox 360 pad via ViGEmBus
        // (split-seat passthrough), while mouse/keyboard packets are
        // injected directly into the host session via SendInput.
        PT_INPUT_DATA => {
            crate::input::handle_input_packet(&data[4..]);
        }
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
        enet::Event::Receive { peer, channel_id, packet, .. } => {
            handle_control_message(channel_id, packet.data(), client_info, peer);
        }
    }
}
