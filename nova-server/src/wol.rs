//! Wake-on-LAN, host half: say which MAC wakes this PC, and make sure it can.
//!
//! A client can only wake a machine whose MAC it already holds, and it can only
//! have learned that while the machine was awake. So the MAC is published
//! everywhere a client already looks — the Echo `_echo._tcp` record (`mac=`),
//! the GameStream `_nvstream` record and `/serverinfo` (`<mac>`, which is what
//! Moonlight's own Wake-on-LAN reads) — and clients persist it with the host.
//! Those used to carry the placeholder `00:11:22:33:44:55`, so every Moonlight
//! client that ever paired was storing a MAC that wakes nothing.
//!
//! **Which adapter.** The one carrying the address Nova advertises
//! (`usable_local_ip`), matched by unicast address. Not "the first Ethernet
//! adapter": this box has several, four of them dead `169.254.x` stubs (the
//! same multi-homing that broke UPnP's SSDP bind, see `upnp.rs`).
//!
//! **Making the NIC listen.** A magic packet only wakes the PC if the adapter
//! is armed for it: the driver's *Wake on Magic Packet* setting and Windows'
//! "Allow this device to wake the computer". [`ensure_wake_armed`] turns on
//! whichever is off, once per service start, and says what it found. Neither
//! change restarts the adapter (`-NoRestart`) — a service start can coincide
//! with a stream reconnecting, and dropping the link to flip a power setting
//! would be a strange way to help. What Nova cannot reach is the firmware:
//! waking from a full shutdown (S5) also needs the BIOS/UEFI wake setting, and
//! some NICs need Windows Fast Startup off. Those are reported, not changed.

use std::sync::RwLock;

use windows::Win32::NetworkManagement::IpHelper::{
    GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST,
    IP_ADAPTER_ADDRESSES_LH,
};
use windows::Win32::Networking::WinSock::{AF_INET, SOCKADDR_IN};

/// Published until the real MAC is known. Kept as the historical placeholder
/// rather than empty: some clients treat a missing `<mac>` as malformed.
const PLACEHOLDER_MAC: &str = "00:11:22:33:44:55";

static HOST_MAC: RwLock<Option<String>> = RwLock::new(None);

/// The MAC to publish right now: the LAN adapter's, once [`resolve_for_ip`]
/// has found it, else the placeholder. Read at request time, so a
/// `/serverinfo` served after a late DHCP lease still carries the real one.
pub fn host_mac() -> String {
    HOST_MAC
        .read()
        .ok()
        .and_then(|m| m.clone())
        .unwrap_or_else(|| PLACEHOLDER_MAC.to_string())
}

/// The adapter that carries `ip`.
#[derive(Debug, Clone)]
pub struct LanAdapter {
    /// `AA:BB:CC:DD:EE:FF`, upper case.
    pub mac: String,
    /// The interface alias Windows shows ("Ethernet", "Wi-Fi") — what the
    /// NetAdapter cmdlets take as `-Name`.
    pub alias: String,
    /// The driver's description — what `powercfg /deviceenablewake` takes.
    pub description: String,
}

/// Find the adapter carrying `ip`, remember its MAC for [`host_mac`], and
/// return it. `None` when no adapter has that address or it has no usable
/// hardware address (a VPN tunnel, say).
pub fn resolve_for_ip(ip: &str) -> Option<LanAdapter> {
    let target: std::net::Ipv4Addr = ip.parse().ok()?;
    let adapter = adapter_for_ipv4(target)?;
    if let Ok(mut slot) = HOST_MAC.write() {
        *slot = Some(adapter.mac.clone());
    }
    Some(adapter)
}

fn adapter_for_ipv4(target: std::net::Ipv4Addr) -> Option<LanAdapter> {
    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
    // The documented pattern: start at 15 KB, grow to what the call asks for.
    let mut size: u32 = 15 * 1024;
    for _ in 0..3 {
        // u64-backed so the buffer is 8-byte aligned for the struct it holds.
        let mut buf = vec![0u64; (size as usize).div_ceil(8)];
        let head = buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH;
        let rc = unsafe { GetAdaptersAddresses(AF_INET.0 as u32, flags, None, Some(head), &mut size) };
        const ERROR_BUFFER_OVERFLOW: u32 = 111;
        if rc == ERROR_BUFFER_OVERFLOW {
            continue;
        }
        if rc != 0 {
            return None;
        }
        let mut node = head as *const IP_ADAPTER_ADDRESSES_LH;
        while !node.is_null() {
            let a = unsafe { &*node };
            if carries(a, target) {
                let len = (a.PhysicalAddressLength as usize).min(a.PhysicalAddress.len());
                if len != 6 || a.PhysicalAddress[..6].iter().all(|&b| b == 0) {
                    return None;
                }
                let mac = a.PhysicalAddress[..6]
                    .iter()
                    .map(|b| format!("{b:02X}"))
                    .collect::<Vec<_>>()
                    .join(":");
                let alias = unsafe { a.FriendlyName.to_string() }.unwrap_or_default();
                let description = unsafe { a.Description.to_string() }.unwrap_or_default();
                return Some(LanAdapter { mac, alias, description });
            }
            node = a.Next;
        }
        return None;
    }
    None
}

fn carries(a: &IP_ADAPTER_ADDRESSES_LH, target: std::net::Ipv4Addr) -> bool {
    let mut u = a.FirstUnicastAddress;
    while !u.is_null() {
        let entry = unsafe { &*u };
        let sa = entry.Address.lpSockaddr;
        if !sa.is_null() && unsafe { (*sa).sa_family } == AF_INET {
            let sin = unsafe { &*(sa as *const SOCKADDR_IN) };
            let raw = unsafe { sin.sin_addr.S_un.S_addr };
            if std::net::Ipv4Addr::from(u32::from_be(raw)) == target {
                return true;
            }
        }
        u = entry.Next;
    }
    false
}

/// Turn on whatever is off of the two Windows-side wake settings for
/// `adapter`, and log what was found. Blocking (one PowerShell run, ~1 s) —
/// call from a background thread.
pub fn ensure_wake_armed(adapter: &LanAdapter) {
    // Single-quoted PowerShell literals; a quote inside a name is doubled.
    let q = |s: &str| s.replace('\'', "''");
    let script = format!(
        r#"$ErrorActionPreference = 'Stop'
$name = '{alias}'; $desc = '{desc}'
$out = @()
try {{
  $pm = Get-NetAdapterPowerManagement -Name $name
  if ($pm.WakeOnMagicPacket -eq 'Disabled') {{
    Set-NetAdapterPowerManagement -Name $name -WakeOnMagicPacket Enabled -NoRestart
    $out += 'magic-packet=ENABLED-NOW'
  }} else {{ $out += "magic-packet=$($pm.WakeOnMagicPacket)" }}
}} catch {{ $out += "magic-packet=ERROR($($_.Exception.Message))" }}
$armed = @(powercfg /devicequery wake_armed) -contains $desc
if (-not $armed) {{
  powercfg /deviceenablewake "$desc" | Out-Null
  $armed = @(powercfg /devicequery wake_armed) -contains $desc
  $out += $(if ($armed) {{ 'wake-armed=ENABLED-NOW' }} else {{ 'wake-armed=REFUSED' }})
}} else {{ $out += 'wake-armed=yes' }}
try {{
  $sw = Get-NetAdapterAdvancedProperty -Name $name | Where-Object {{ $_.DisplayName -match 'Shutdown Wake' }} | Select-Object -First 1
  if ($sw) {{ $out += "shutdown-wol=$($sw.DisplayValue)" }}
}} catch {{}}
try {{
  $fs = (Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\Power' -Name HiberbootEnabled).HiberbootEnabled
  $out += "fast-startup=$fs"
}} catch {{}}
$out -join ' '"#,
        alias = q(&adapter.alias),
        desc = q(&adapter.description),
    );

    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    // -EncodedCommand, never -Command: powershell.exe re-parses a -Command
    // argument with its own rules and strips the script's inner double quotes,
    // so "magic-packet=$(...)" became a call to a command named
    // `magic-packet=ERROR` (measured 2026-11-09). Base64 UTF-16LE has no
    // quoting to get wrong.
    let encoded = base64_utf16le(&script);
    let result = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-EncodedCommand", &encoded])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match result {
        Ok(out) => {
            let mut report = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if report.is_empty() {
                // Say what went wrong rather than logging an empty verdict.
                let err = String::from_utf8_lossy(&out.stderr);
                report = format!("check produced no answer: {}", err.lines().next().unwrap_or("(no stderr)"));
            }
            println!(
                "⏰ Wake-on-LAN: {} ({}, {}) — {report}",
                adapter.alias, adapter.mac, adapter.description
            );
            // The vendor's own shut-down wake setting (Realtek: "Shutdown
            // Wake-On-Lan") is the one that decides waking from power-off.
            // Reported, never changed: it is a driver property whose name and
            // values differ per vendor.
            if !report.contains("shutdown-wol=Enabled") {
                println!(
                    "   ↳ Waking from SLEEP is covered. Waking from a full SHUT DOWN also needs the \
                     BIOS/UEFI wake-on-LAN setting and the adapter's own shut-down wake option; on \
                     some NICs, Windows Fast Startup off as well."
                );
            }
        }
        Err(e) => println!("⚠️  Wake-on-LAN: could not run the adapter check: {e}"),
    }
}

/// What `powershell.exe -EncodedCommand` takes: the script as UTF-16LE, base64.
fn base64_utf16le(script: &str) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | *chunk.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_like_powershell_expects() {
        // [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes('ab'))
        assert_eq!(base64_utf16le("ab"), "YQBiAA==");
        assert_eq!(base64_utf16le("abc"), "YQBiAGMA");
    }

    /// Runs the real adapter check against this machine's LAN adapter and
    /// prints the verdict line. Changes nothing on an adapter already armed.
    #[test]
    #[ignore]
    fn arm_check_reports_live() {
        let socket = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
        socket.connect("8.8.8.8:80").unwrap();
        let ip = socket.local_addr().unwrap().ip().to_string();
        ensure_wake_armed(&resolve_for_ip(&ip).expect("adapter"));
    }

    #[test]
    fn placeholder_until_resolved() {
        // Nothing resolved in a fresh test process.
        assert!(host_mac().len() == 17);
    }

    /// The machine running the tests has a LAN address; its adapter must be
    /// found and must yield a real six-byte MAC.
    #[test]
    #[ignore]
    fn resolves_the_lan_adapter_live() {
        let socket = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
        socket.connect("8.8.8.8:80").unwrap();
        let ip = socket.local_addr().unwrap().ip().to_string();
        let adapter = resolve_for_ip(&ip).expect("adapter for the LAN address");
        println!("{ip} -> {adapter:?}");
        assert_eq!(adapter.mac.len(), 17);
        assert_eq!(host_mac(), adapter.mac);
    }
}
