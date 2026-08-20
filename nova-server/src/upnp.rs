//! Zero-config WAN reachability: ask the router to forward Nova's ports.
//!
//! The problem this solves is narrow and concrete. Echo's transport cascade
//! reaches a host on the LAN by itself, and reaches it from anywhere else
//! through a relay — but only if the client can *reach* that relay. On a
//! self-hosted deployment the relay sits on the same private address as Nova,
//! so a phone on cellular has no route to it and the WAN half of the cascade
//! has nowhere to go. Port forwarding fixes that, and asking a human to
//! configure their router is the kind of instruction that ends adoption.
//!
//! ## What is exposed, and what is deliberately not
//!
//! Two mappings, and no more:
//!
//! - **The relay's TCP port** (whatever `[echo.signaling] url` names, normally
//!   8443), so a client off the LAN can trade candidates. It is mutual-TLS and
//!   certificate-pinned in both directions.
//! - **The media UDP port** ([`crate::ECHO_MEDIA_PORT`]), which makes the hole
//!   punch succeed even behind an endpoint-dependent (symmetric) NAT — the one
//!   case the punch cannot solve on its own and which otherwise needs a
//!   full relay-forwarding server nobody wants to pay for.
//!
//! **The Echo control port 48011 is NEVER mapped.** `echo::rpc` drops
//! connections to it from non-private addresses before TLS, deliberately, to
//! keep an internet-facing TCP surface off a LocalSystem service. Forwarding it
//! would hand the internet the very door that fence exists to close, and the
//! fence would then silently refuse every packet arriving through it. If a
//! future change wants WAN control, it belongs on the punched tunnel, which is
//! where it already lives.
//!
//! ## Leases are finite on purpose
//!
//! A permanent mapping outlives the process that asked for it. Nova crashes,
//! the router keeps forwarding a port to a machine with nothing listening, and
//! the rule sits in the router's table until someone notices it — which nobody
//! does. So the mapping is requested with a finite lease and renewed while Nova
//! runs: a Nova that dies stops renewing, and the hole closes by itself within
//! [`LEASE_SECS`]. The explicit release on shutdown is the fast path, not the
//! only one.
//!
//! Some routers refuse finite leases outright (`OnlyPermanentLeasesSupported`).
//! Those fall back to a permanent mapping, where the release on shutdown and
//! the re-add at startup are the whole story.
//!
//! **A router may also grant a different lease than the one asked for**, and
//! the dev-box gateway does exactly that: [`LEASE_SECS`] requests an hour and
//! its table reports 86400. So the self-closing window is "whatever the router
//! decided", not a number this code controls — the renewal keeps it fresh
//! either way, and the explicit release on shutdown is what actually closes it
//! promptly. Worth knowing before trusting the lease as a security boundary:
//! it is a backstop, not a guarantee with a number attached.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Mutex;
use std::time::Duration;

use igd_next::aio::tokio as igd_tokio;
use igd_next::{PortMappingProtocol, SearchOptions};

/// How long a mapping is requested for.
///
/// An hour is long enough that renewal is not chatty and short enough that a
/// hole left by a crashed Nova closes while the user is still in the room.
const LEASE_SECS: u32 = 3600;

/// Renewal interval — comfortably inside [`LEASE_SECS`].
///
/// Half the lease, so a single missed renewal (a router rebooting, a transient
/// network blip) still leaves a full half-lease of headroom to recover in.
const RENEW_AFTER: Duration = Duration::from_secs(LEASE_SECS as u64 / 2);

/// How long to look for a router before giving up.
///
/// Startup is not blocked on this — the search runs in its own task — but the
/// mDNS advertisement waits briefly for the answer, so an unbounded search
/// would hold the record back on every network with no UPnP at all.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(5);

/// What the discovery layer is waiting to hear.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Still looking. Nobody should publish an address yet.
    Pending,
    /// No usable public address — no router, no UPnP, it refused, or the
    /// address it reported is itself private. Advertise the LAN address.
    Unavailable,
    /// Forwarded, and this is the address the world can reach.
    Public(Ipv4Addr),
}

static OUTCOME: Mutex<Outcome> = Mutex::new(Outcome::Pending);

/// The mappings currently held open, for the teardown to remove.
///
/// Kept here rather than passed to the release call because the caller that
/// matters is the SCM stop path, which knows nothing about ports and should not
/// have to. Emptied by the release, so a second stop is a no-op.
static ACTIVE: Mutex<Vec<Mapping>> = Mutex::new(Vec::new());

/// Hard ceiling on the whole teardown, search included.
const RELEASE_BUDGET: Duration = Duration::from_secs(6);

/// The address the SSDP search must leave from. See [`search_options`].
static BIND_IP: Mutex<Option<Ipv4Addr>> = Mutex::new(None);

/// Search options that send the discovery packet out of the RIGHT interface.
///
/// **This is the whole reason UPnP appeared not to work on the dev box, and it
/// is worth understanding before anyone "simplifies" it back to
/// `SearchOptions::default()`.**
///
/// SSDP discovery is a UDP datagram to the multicast group 239.255.255.250. A
/// socket bound to `0.0.0.0` leaves the choice of *outgoing interface* to the
/// routing table, and a Windows machine has far more interfaces than its owner
/// thinks: this host has five IPv4 addresses, of which four are `169.254.x`
/// link-local stubs belonging to disconnected Wi-Fi and Bluetooth adapters. The
/// multicast route picked one of those, the search went out an interface with
/// no router on it, and nothing answered — which is indistinguishable, from the
/// caller's side, from a router with UPnP switched off.
///
/// Measured, on this box, with everything else held equal:
///
/// ```text
/// bind 0.0.0.0     -> 0 responders
/// bind 10.0.0.205  -> 1 responder: http://10.0.0.1:49153/IGDdevicedesc_brlan0.xml
/// ```
///
/// So the bind address is not a tuning knob; on any host with more than one
/// interface it decides whether discovery works at all.
fn search_options(bind: Option<Ipv4Addr>) -> SearchOptions {
    let mut options = SearchOptions { timeout: Some(SEARCH_TIMEOUT), ..Default::default() };
    if let Some(ip) = bind {
        options.bind_addr = SocketAddr::new(IpAddr::V4(ip), 0);
    }
    options
}

fn set_outcome(next: Outcome) {
    *OUTCOME.lock().unwrap_or_else(|e| e.into_inner()) = next;
}

/// The current result, without waiting.
pub fn outcome() -> Outcome {
    *OUTCOME.lock().unwrap_or_else(|e| e.into_inner())
}

/// The public address, if one was mapped.
pub fn public_ip() -> Option<Ipv4Addr> {
    match outcome() {
        Outcome::Public(ip) => Some(ip),
        _ => None,
    }
}

/// Wait up to `limit` for the port-mapping attempt to resolve either way.
///
/// Polled rather than signalled, matching `wait_for_local_ip`: the wait happens
/// once per process, off the hot path, and a poll needs no channel to be wired
/// through three layers of startup. Returns as soon as the answer is known, so
/// a machine with no UPnP router does not pay the full timeout — the search
/// itself gives up at [`SEARCH_TIMEOUT`] and records `Unavailable`.
pub async fn await_outcome(limit: Duration) -> Outcome {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        let now = outcome();
        if now != Outcome::Pending {
            return now;
        }
        if tokio::time::Instant::now() >= deadline {
            return Outcome::Pending;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Record that no mapping will be attempted, so waiters stop waiting.
///
/// For the callers that decide against even searching — UPnP switched off, no
/// local address, an address that is not IPv4. Without it those paths would
/// leave the outcome `Pending` and every waiter would burn its full timeout to
/// learn something already known.
pub fn give_up() {
    set_outcome(Outcome::Unavailable);
}

/// One port Nova wants forwarded.
#[derive(Debug, Clone, Copy)]
pub struct Mapping {
    pub protocol: PortMappingProtocol,
    pub port: u16,
    /// Shown in the router's UI, so a curious owner can see who asked.
    pub label: &'static str,
}

/// Discover the router, publish the external address, and hold the mappings
/// open for as long as this process lives.
///
/// Returns immediately; everything happens in a background task. Failure at any
/// step is recorded as [`Outcome::Unavailable`] and logged once — a host with no
/// UPnP router is an ordinary configuration, not a fault, and it still works
/// perfectly on the LAN.
pub fn spawn(local_ip: Ipv4Addr, mappings: Vec<Mapping>) {
    tokio::spawn(async move {
        *BIND_IP.lock().unwrap_or_else(|e| e.into_inner()) = Some(local_ip);
        let gateway = match tokio::time::timeout(
            SEARCH_TIMEOUT,
            igd_tokio::search_gateway(search_options(Some(local_ip))),
        )
        .await
        {
            Ok(Ok(gateway)) => gateway,
            Ok(Err(e)) => {
                println!(
                    "🌐 UPnP: no router answered ({e}) — Echo will work on this LAN, and from \
                     outside only through a relay that is already reachable"
                );
                set_outcome(Outcome::Unavailable);
                return;
            }
            Err(_) => {
                println!("🌐 UPnP: no router answered within {SEARCH_TIMEOUT:?} — LAN only");
                set_outcome(Outcome::Unavailable);
                return;
            }
        };
        println!("🌐 UPnP: router found at {}", gateway.addr);

        let external = match gateway.get_external_ip().await {
            Ok(IpAddr::V4(ip)) => ip,
            Ok(IpAddr::V6(ip)) => {
                println!("🌐 UPnP: router reports IPv6 {ip} — Nova advertises IPv4, not mapping");
                set_outcome(Outcome::Unavailable);
                return;
            }
            Err(e) => {
                println!("🌐 UPnP: router would not report its external address ({e})");
                set_outcome(Outcome::Unavailable);
                return;
            }
        };

        // A private "external" address means the ISP is doing NAT above this
        // router — carrier-grade NAT, or a modem in front of the router. Opening
        // a port here forwards traffic that will never arrive, and publishing
        // the address would be worse than publishing nothing: the client would
        // spend its whole WAN attempt dialling somewhere unreachable instead of
        // failing fast and saying why.
        if is_private_v4(external) {
            println!(
                "🌐 UPnP: the router's external address is {external}, which is private — this \
                 connection is behind carrier-grade NAT, so forwarding a port cannot make this \
                 host reachable. Echo needs a relay with a public address for WAN sessions."
            );
            set_outcome(Outcome::Unavailable);
            return;
        }

        // Ask for every mapping before publishing anything. A half-mapped host
        // advertises a public address that answers signalling and drops media,
        // which presents as "it connects and then nothing happens" — much
        // harder to diagnose than a clean refusal.
        let mut mapped = Vec::new();
        for m in &mappings {
            match add_mapping(&gateway, local_ip, m).await {
                Ok(()) => mapped.push(*m),
                Err(e) => {
                    println!("🌐 UPnP: could not forward {} {} ({e})", proto_name(m.protocol), m.port);
                    // Roll back whatever succeeded, so a failed attempt leaves
                    // the router's table exactly as it was found.
                    for done in &mapped {
                        let _ = gateway.remove_port(done.protocol, done.port).await;
                    }
                    set_outcome(Outcome::Unavailable);
                    return;
                }
            }
        }

        println!(
            "🌐 UPnP: forwarded {} to {local_ip} — this host is reachable at {external}",
            mapped
                .iter()
                .map(|m| format!("{} {}", proto_name(m.protocol), m.port))
                .collect::<Vec<_>>()
                .join(" + "),
        );
        // Recorded before the outcome is published, so a teardown racing a very
        // fast startup can never find a public address with no mappings to
        // remove behind it.
        *ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = mapped.clone();
        set_outcome(Outcome::Public(external));

        // Renew for as long as this process lives. The task ending — because
        // the runtime is shutting down — is what lets the lease lapse, which is
        // the backstop for every teardown path that never gets to run code.
        let mut ticker = tokio::time::interval(RENEW_AFTER);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await; // the immediate first tick
        loop {
            ticker.tick().await;
            for m in &mapped {
                if let Err(e) = add_mapping(&gateway, local_ip, m).await {
                    println!("🌐 UPnP: renewing {} {} failed ({e})", proto_name(m.protocol), m.port);
                }
            }
            // The address can change under us — most consumer connections get a
            // new one on reconnect — and an advertisement naming the old one is
            // worse than none. Re-read it every renewal.
            if let Ok(IpAddr::V4(now)) = gateway.get_external_ip().await {
                if !is_private_v4(now) && Some(now) != public_ip() {
                    println!("🌐 UPnP: external address changed to {now}");
                    set_outcome(Outcome::Public(now));
                }
            }
        }
    });
}

/// Add one mapping, tolerating routers that refuse finite leases.
async fn add_mapping(
    gateway: &igd_next::aio::Gateway<igd_tokio::Tokio>,
    local_ip: Ipv4Addr,
    m: &Mapping,
) -> Result<(), String> {
    let local = SocketAddr::new(IpAddr::V4(local_ip), m.port);
    match gateway.add_port(m.protocol, m.port, local, LEASE_SECS, m.label).await {
        Ok(()) => Ok(()),
        Err(first) => {
            // `OnlyPermanentLeasesSupported` is common on consumer hardware.
            // A permanent mapping is worse (it outlives a crash) but it is what
            // the router will accept, and the shutdown release still removes it.
            match gateway.add_port(m.protocol, m.port, local, 0, m.label).await {
                Ok(()) => {
                    println!(
                        "🌐 UPnP: {} {} mapped without a lease — this router only accepts \
                         permanent rules, so an unclean exit will leave it behind until the \
                         next start removes it",
                        proto_name(m.protocol),
                        m.port
                    );
                    Ok(())
                }
                Err(second) => Err(format!("{first}; permanent retry: {second}")),
            }
        }
    }
}

/// Remove every mapping this process created.
///
/// **Synchronous**, because the one caller that matters is the service's stop
/// path, which runs on an SCM thread with no runtime under it — and which must
/// finish before the SCM is told the service has stopped. Builds a small
/// runtime of its own rather than requiring one.
///
/// Best-effort and hard time-boxed. Shutdown must not hang on a router that has
/// stopped answering, and it does not need to: the finite lease means a mapping
/// this fails to remove closes by itself. This is the fast path, not the only
/// one.
pub fn release_port_mappings() {
    let mappings: Vec<Mapping> = {
        let mut held = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *held)
    };
    if mappings.is_empty() {
        return;
    }
    set_outcome(Outcome::Unavailable);

    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(e) => {
            println!("🌐 UPnP: no runtime to remove port mappings ({e}) — they will expire");
            return;
        }
    };
    runtime.block_on(async move {
        match tokio::time::timeout(RELEASE_BUDGET, remove_all(&mappings)).await {
            Ok(()) => {}
            Err(_) => println!(
                "🌐 UPnP: the router did not answer within {RELEASE_BUDGET:?} — leaving the \
                 mappings to expire with their lease"
            ),
        }
    });
}

async fn remove_all(mappings: &[Mapping]) {
    let bind = *BIND_IP.lock().unwrap_or_else(|e| e.into_inner());
    let gateway = match igd_tokio::search_gateway(search_options(bind)).await
    {
        Ok(gateway) => gateway,
        Err(e) => {
            println!("🌐 UPnP: could not reach the router to remove port mappings ({e})");
            return;
        }
    };
    for m in mappings {
        match gateway.remove_port(m.protocol, m.port).await {
            Ok(()) => println!("🌐 UPnP: removed {} {}", proto_name(m.protocol), m.port),
            Err(e) => println!("🌐 UPnP: removing {} {} failed ({e})", proto_name(m.protocol), m.port),
        }
    }
}

fn proto_name(p: PortMappingProtocol) -> &'static str {
    match p {
        PortMappingProtocol::TCP => "TCP",
        PortMappingProtocol::UDP => "UDP",
    }
}

/// Private, loopback, link-local or shared-CGNAT (100.64.0.0/10) space.
///
/// The CGNAT range is the one that matters most here and is the one a plain
/// "is_private" check misses: an ISP handing out 100.64.x.x is precisely the
/// case where a forwarded port cannot help.
fn is_private_v4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || (a == 100 && (64..=127).contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carrier_grade_nat_is_not_a_public_address() {
        // The case that makes port forwarding pointless, and the one an
        // `is_private()` check alone would wave through — publishing it would
        // send every WAN client to an address that cannot answer.
        assert!(is_private_v4("100.64.0.1".parse().unwrap()));
        assert!(is_private_v4("100.127.255.255".parse().unwrap()));
        assert!(!is_private_v4("100.128.0.1".parse().unwrap()), "just outside the CGNAT range");
        assert!(!is_private_v4("100.63.255.255".parse().unwrap()), "just below it");
    }

    #[test]
    fn ordinary_private_space_is_refused_too() {
        for ip in ["192.168.1.1", "10.0.0.205", "172.16.0.1", "127.0.0.1", "169.254.1.1"] {
            assert!(is_private_v4(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["73.213.125.252", "8.8.8.8", "172.32.0.1"] {
            assert!(!is_private_v4(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn nothing_is_published_until_the_search_resolves() {
        // The discovery layer branches on this: Pending must never be mistaken
        // for "no public address", or the mDNS record races the router and
        // advertises the LAN relay to a client that will later need the WAN one.
        assert_eq!(outcome(), Outcome::Pending, "a fresh process has not looked yet");
        assert_eq!(public_ip(), None);
    }
}
