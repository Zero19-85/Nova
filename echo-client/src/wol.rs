//! Wake-on-LAN: wake a sleeping Nova host from its last known MAC.
//!
//! One implementation for every client — the Xbox bridge (`echo_wake`) and the
//! Android JNI (`nativeWake`) both land here — because the packet is trivially
//! easy to get subtly wrong (a MAC parsed with the wrong separator, a socket
//! without `SO_BROADCAST` that "sends" into nothing) and a wake that silently
//! does nothing is indistinguishable from a PC with WoL switched off.
//!
//! The MAC comes from the host itself, published in its `_echo._tcp` record
//! (`mac=`) while it was awake and persisted with the host card. See
//! `nova-server/src/wol.rs` for the host half.
//!
//! **Where it goes.** A sleeping PC has no IP stack worth the name; the NIC
//! listens for the pattern on the wire, so the packet only has to reach the
//! segment. It is sent to the limited broadcast 255.255.255.255 AND to the
//! host's subnet broadcast (its last known address with the host part set,
//! assuming a /24 — the common home LAN), because some routers and Wi-Fi
//! access points drop one kind and pass the other. Both on UDP 9 (discard, the
//! convention) and 7 (echo, what some BIOSes expect), several times over a
//! short spread, since a single UDP datagram on Wi-Fi is a coin toss.
//!
//! **Blocking**, for ~0.5 s. Call from a worker thread.

use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::time::Duration;

/// Rounds of the full set of destinations.
const ROUNDS: u32 = 3;
const ROUND_GAP: Duration = Duration::from_millis(200);
const PORTS: [u16; 2] = [9, 7];

/// Parse `AA:BB:CC:DD:EE:FF` (also `-` separated, or bare hex).
pub fn parse_mac(mac: &str) -> Option<[u8; 6]> {
    let hex: String = mac.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() != 12 || mac.chars().any(|c| !(c.is_ascii_hexdigit() || c == ':' || c == '-')) {
        return None;
    }
    let mut out = [0u8; 6];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    // All-zero and the old placeholder wake nothing — refuse them by name so
    // the UI can say why rather than "sent".
    if out == [0; 6] || out == [0x00, 0x11, 0x22, 0x33, 0x44, 0x55] {
        return None;
    }
    Some(out)
}

/// The 102-byte magic packet: six 0xFF, then the MAC sixteen times.
pub fn magic_packet(mac: [u8; 6]) -> [u8; 102] {
    let mut packet = [0xFFu8; 102];
    for i in 0..16 {
        packet[6 + i * 6..12 + i * 6].copy_from_slice(&mac);
    }
    packet
}

/// Every broadcast address worth trying for a host last seen at `last_ip`.
fn destinations(last_ip: Option<Ipv4Addr>) -> Vec<Ipv4Addr> {
    let mut out = vec![Ipv4Addr::BROADCAST];
    if let Some(ip) = last_ip.filter(|ip| ip.is_private()) {
        let [a, b, c, _] = ip.octets();
        out.push(Ipv4Addr::new(a, b, c, 255));
    }
    out
}

/// Send the magic packet for `mac`. `last_ip` is the host's last known LAN
/// address, used only to derive its subnet broadcast. Returns how many
/// datagrams left the socket; `Err` only when nothing could be sent at all.
pub fn wake(mac: &str, last_ip: Option<&str>) -> Result<u32, String> {
    let mac = parse_mac(mac).ok_or_else(|| format!("\"{mac}\" is not a usable MAC address"))?;
    let packet = magic_packet(mac);
    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("bind: {e}"))?;
    socket.set_broadcast(true).map_err(|e| format!("SO_BROADCAST: {e}"))?;

    let targets = destinations(last_ip.and_then(|s| s.trim().parse().ok()));
    let mut sent = 0;
    let mut last_error = None;
    for round in 0..ROUNDS {
        for ip in &targets {
            for port in PORTS {
                match socket.send_to(&packet, SocketAddrV4::new(*ip, port)) {
                    Ok(_) => sent += 1,
                    Err(e) => last_error = Some(format!("{ip}:{port}: {e}")),
                }
            }
        }
        if round + 1 < ROUNDS {
            std::thread::sleep(ROUND_GAP);
        }
    }
    if sent == 0 {
        return Err(last_error.unwrap_or_else(|| "no destination accepted the packet".into()));
    }
    Ok(sent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_formats_hosts_publish() {
        let mac = [0x30, 0x56, 0x0F, 0x44, 0x51, 0xE5];
        assert_eq!(parse_mac("30:56:0F:44:51:E5"), Some(mac));
        assert_eq!(parse_mac("30-56-0f-44-51-e5"), Some(mac));
        assert_eq!(parse_mac("30560F4451E5"), Some(mac));
    }

    #[test]
    fn refuses_what_wakes_nothing() {
        assert_eq!(parse_mac(""), None);
        assert_eq!(parse_mac("30:56:0F:44:51"), None);
        assert_eq!(parse_mac("30:56:0F:44:51:E5:00"), None);
        assert_eq!(parse_mac("zz:56:0F:44:51:E5"), None);
        assert_eq!(parse_mac("00:00:00:00:00:00"), None);
        // The placeholder Nova used to publish before it knew its real MAC.
        assert_eq!(parse_mac("00:11:22:33:44:55"), None);
    }

    #[test]
    fn packet_is_sync_stream_then_sixteen_macs() {
        let mac = [1, 2, 3, 4, 5, 6];
        let p = magic_packet(mac);
        assert_eq!(&p[..6], &[0xFF; 6]);
        for i in 0..16 {
            assert_eq!(&p[6 + i * 6..12 + i * 6], &mac);
        }
    }

    #[test]
    fn subnet_broadcast_only_for_a_private_address() {
        assert_eq!(
            destinations(Some(Ipv4Addr::new(10, 0, 0, 205))),
            vec![Ipv4Addr::BROADCAST, Ipv4Addr::new(10, 0, 0, 255)]
        );
        assert_eq!(destinations(Some(Ipv4Addr::new(8, 8, 8, 8))), vec![Ipv4Addr::BROADCAST]);
        assert_eq!(destinations(None), vec![Ipv4Addr::BROADCAST]);
    }
}
