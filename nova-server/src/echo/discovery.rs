//! LAN discovery for Echo clients — the `_echo._tcp` mDNS record.
//!
//! Echo's connection needs four facts a phone cannot guess: the host's address,
//! its certificate fingerprint, and — when WAN signalling is configured — the
//! relay's URL and certificate pin. Before this module all four were typed by
//! hand into the app, two of them 64-character hex strings. This publishes them
//! on the LAN so the app can fill its own fields.
//!
//! ## Why a second record rather than extending `_nvstream`
//!
//! Nova already advertises `_nvstream._tcp`, and every Moonlight client on the
//! network parses it. That record advertises port 47989 and describes a
//! GameStream host; Echo's control plane is a different protocol on a different
//! port. Adding Echo keys there would put unknown properties in front of every
//! Moonlight client to describe a service they cannot speak. Two service types
//! is what mDNS is for — one host, two services, one daemon.
//!
//! ## The security boundary, which is the whole point of this file
//!
//! **Nothing advertised here is trusted, and nothing here can create trust.**
//! mDNS is unauthenticated: anything on the LAN can claim to be `_echo._tcp`
//! and publish whatever fingerprint it likes. So `fp` is a *hint that pre-fills
//! a text field*, nothing more. Trust is still established exactly where it was
//! before — in the PIN handshake, where the client checks the host's committed
//! hash and verifies an RSA signature over the host secret against the
//! certificate it was offered (`echo-client/src/pairing.rs`, phase 3). The
//! fingerprint a client *persists* must always be the one it derived from that
//! handshake, never the one it read here.
//!
//! As a rule for anyone extending this: a value published here must never let a
//! client skip a check it would otherwise perform. If a new key would let the
//! app connect without pairing, it does not belong in a TXT record.

use std::collections::HashMap;
use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceInfo};

use crate::config::EchoConfig;

/// The service type Echo clients browse for.
pub(crate) const SERVICE_TYPE: &str = "_echo._tcp.local.";

/// Instance name within that type. One host advertises one Echo endpoint, and
/// the name only has to be unique within the service type.
const INSTANCE_NAME: &str = "Nova";

/// mDNS hostname, matching the `_nvstream` record: the same machine, so the
/// same host name. Two service records pointing at one host is the normal
/// shape, and it keeps the address in one place.
const HOST_NAME: &str = "nova.local.";

/// TXT format version. Bump only for a change an existing client would
/// misread; adding a new optional key is not that.
const TXT_VERSION: &str = "1";

/// How long to wait for `pairing.rs` to publish the certificate.
///
/// Same budget and same reasoning as `echo::signaling`'s wait: on a fresh
/// install the pairing server generates a key pair before it can publish an
/// identity, and that is measured in seconds. Registering before then would
/// advertise an Echo endpoint carrying a blank fingerprint, which is worse than
/// advertising nothing — the app would helpfully fill its field with the blank.
const IDENTITY_WAIT: Duration = Duration::from_secs(60);

/// How long the advertisement waits for the router's answer.
///
/// Longer than `upnp::SEARCH_TIMEOUT`, so the ordinary outcomes — a router that
/// answers, or one that is not there — are already decided and this returns
/// immediately. The margin covers the slow middle case: a router that answers
/// the search and then takes its time over the mapping itself.
const UPNP_WAIT: Duration = Duration::from_secs(12);

/// Poll interval while waiting. A human-scale event, so this is not hot.
const IDENTITY_POLL: Duration = Duration::from_millis(100);

/// Build the TXT properties for the record.
///
/// Split out from registration so the contents can be asserted without a
/// network: this is the part with rules in it, and the part a later edit is
/// most likely to get subtly wrong.
///
/// `relay`/`relaypin` are emitted **only when both are configured**. A
/// half-configured relay is not usable — the pin is what authenticates it — and
/// publishing a URL without its pin would invite the app to fill one field and
/// leave the user hunting for the other. Absent keys let the app say "LAN only"
/// honestly instead of showing blanks.
fn txt_records(
    fingerprint: &str,
    host_label: &str,
    relay_url: &str,
    relay_pin: &str,
    local_ip: &str,
    public_ip: Option<&str>,
    advertise_override: &str,
) -> Vec<(String, String)> {
    let mut txt = vec![
        ("txtvers".to_string(), TXT_VERSION.to_string()),
        ("fp".to_string(), fingerprint.to_string()),
        ("name".to_string(), host_label.to_string()),
    ];

    let url = relay_url.trim();
    let pin = relay_pin.trim();
    if !url.is_empty() && !pin.is_empty() {
        // The address a client should dial, in preference order: the public one
        // if the router forwarded a port for us, otherwise this host's LAN
        // address.
        //
        // The public address wins because of WHEN a client uses the relay at
        // all. Echo's cascade tries the LAN directly first and only falls
        // through to the relay when that fails — which, in practice, means the
        // client is not on this network. Advertising the LAN address to a phone
        // that will next open the app on cellular gives it a URL guaranteed to
        // fail there, and the phone has no way to learn a better one, because
        // learning it would require the mDNS it can no longer hear.
        //
        // The trade, stated plainly: a client that IS on this LAN and whose
        // direct attempt failed now reaches the relay by its public address,
        // which needs NAT hairpinning — and not every router does it. That case
        // is narrow (stage 1 covers the LAN, and it is live-confirmed) and the
        // failure is recoverable by hand; the cellular case is the common one
        // and had no recovery at all.
        let advertised = match public_ip {
            Some(public) => relay_for_wan(url, public),
            None => relay_reachable_from(url, local_ip),
        };
        // `[echo.signaling] advertise_url` overrides everything above. An
        // operator who typed an address has answered this question themselves,
        // and it is the only source here that can know about a forwarded port
        // or a DNS name Nova has no way to discover.
        let advertised = if advertise_override.trim().is_empty() {
            advertised
        } else {
            advertise_override.trim().to_string()
        };
        txt.push(("relay".to_string(), advertised));
        txt.push(("relaypin".to_string(), pin.to_string()));
    }

    txt
}

/// Rewrite a loopback relay URL to this host's LAN address.
///
/// `[echo.signaling] url` is written from the host's point of view, and
/// `https://127.0.0.1:8443/...` is perfectly correct there — the relay usually
/// runs on the same machine, and loopback is the fastest way for Nova itself to
/// reach it. It is nonsense in a *broadcast*, though: a phone that reads
/// `127.0.0.1` resolves it to its own loopback and gets `Connection refused`,
/// which is exactly what happened on the first live discovery (2026-08-18).
///
/// A loopback address is never meaningful to a recipient elsewhere on the
/// network, so there is no case where forwarding one unchanged is the right
/// answer — the choice is only between rewriting it and advertising nothing.
///
/// This changes only the advertised copy. Nova's own signalling client keeps
/// using the configured URL verbatim, so the host still takes the loopback path
/// to its own relay.
///
/// Two facts make the rewrite sound rather than a guess, both checked on the
/// dev box: the relay listens on `0.0.0.0`, so it genuinely answers on the LAN
/// address; and the relay's TLS is pinned by certificate fingerprint with a
/// custom verifier (`identity::client_config_pinned`), never by hostname, so
/// changing the authority cannot break the handshake.
fn relay_reachable_from(relay_url: &str, local_ip: &str) -> String {
    let trimmed = relay_url.trim();
    let Ok(mut parsed) = url::Url::parse(trimmed) else {
        // Not parseable: pass it through untouched. Rewriting something we do
        // not understand is how a working configuration gets mangled, and the
        // client reports a bad URL far more clearly than a corrupted one.
        return trimmed.to_string();
    };

    let loopback = match parsed.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        None => false,
    };
    if !loopback {
        return trimmed.to_string();
    }

    // An empty or unusable `local_ip` leaves the URL alone: an honestly wrong
    // address a human can recognise beats a silently blank one.
    if local_ip.trim().is_empty() || parsed.set_host(Some(local_ip.trim())).is_err() {
        return trimmed.to_string();
    }
    parsed.to_string()
}

/// Rewrite a relay URL that only works inside this network to the public
/// address the router is forwarding.
///
/// Broader than [`relay_reachable_from`], and deliberately so: that one repairs
/// loopback, because loopback is the only address that is *always* wrong in a
/// broadcast. This one repairs loopback **and private space**, because the
/// question here is different — not "will anyone on this network understand
/// it?" but "will the phone that reads this understand it after it leaves?".
/// `https://10.0.0.205:8443/…` is a perfectly good answer to the first question
/// and a useless one to the second, and the second is the question a record
/// stored for later has to answer.
///
/// A URL naming a public host or a domain is left alone: an operator who put a
/// real address there has already answered this, and overwriting their DNS name
/// with a raw IP would break the one setup that needs no help at all.
fn relay_for_wan(relay_url: &str, public_ip: &str) -> String {
    let trimmed = relay_url.trim();
    let Ok(mut parsed) = url::Url::parse(trimmed) else {
        return trimmed.to_string();
    };

    let local_only = match parsed.host() {
        Some(url::Host::Ipv4(ip)) => {
            ip.is_loopback() || ip.is_private() || ip.is_link_local() || ip.is_unspecified()
        }
        Some(url::Host::Ipv6(ip)) => ip.is_loopback() || ip.is_unspecified(),
        // A name, including "localhost". Anything resolvable is the operator's
        // own choice and is left to them; localhost is the one exception,
        // because it resolves to the reader rather than to us.
        Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        None => false,
    };
    if !local_only {
        return trimmed.to_string();
    }

    if public_ip.trim().is_empty() || parsed.set_host(Some(public_ip.trim())).is_err() {
        return trimmed.to_string();
    }
    parsed.to_string()
}

/// A human-readable name for this machine, for the app's device list.
///
/// Cosmetic only. The app must key its saved records by fingerprint, because a
/// machine name is neither unique nor stable.
fn host_label() -> String {
    std::env::var("COMPUTERNAME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "Nova".to_string())
}

/// Wait for the pairing server to publish Nova's certificate, then return its
/// fingerprint. `None` if it never appears within [`IDENTITY_WAIT`].
async fn wait_for_fingerprint() -> Option<String> {
    let deadline = tokio::time::Instant::now() + IDENTITY_WAIT;
    loop {
        if let Some((cert_der, _key_der)) = crate::pairing::server_identity() {
            return Some(crate::pairing::fingerprint_of_cert(&cert_der));
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(IDENTITY_POLL).await;
    }
}

/// Advertise the Echo control endpoint on the LAN.
///
/// Returns immediately. Registration happens on a spawned task because it has
/// to wait for the pairing identity, and blocking Master startup on a
/// certificate a fresh install has not generated yet would hold up every
/// listener queued behind it.
///
/// Takes the daemon by reference and clones it. `ServiceDaemon` is a handle to
/// a thread that owns its own state and keeps polling after every handle drops,
/// so both records outlive the function that registered them — which is already
/// how the `_nvstream` record survives `start_master_network` returning.
pub(crate) fn spawn(mdns: &ServiceDaemon, cfg: &EchoConfig) {
    if !cfg.enabled {
        println!("📡 Echo mDNS: [echo] enabled = false — not advertising");
        return;
    }

    let mdns = mdns.clone();
    let port = cfg.port;
    let relay_url = cfg.signaling.url.clone();
    let relay_pin = cfg.signaling.relay_cert_sha256.clone();
    let advertise_override = cfg.signaling.advertise_url.clone();

    tokio::spawn(async move {
        // Resolved HERE, not at the call site. The address is a startup fact
        // that arrives on its own schedule, exactly like the certificate below,
        // and a boot-time start used to capture `0.0.0.0` before any NIC had an
        // address — advertising an Echo endpoint no client can open, with the
        // relay URL rewritten to `https://0.0.0.0:8443` for good measure.
        let Some(ip) = crate::wait_for_local_ip().await else {
            println!(
                "⚠️ Echo mDNS: no LAN address available — not advertising (Nova is \
                 still reachable by address, just not discoverable)"
            );
            return;
        };
        let Some(fingerprint) = wait_for_fingerprint().await else {
            println!(
                "⚠️ Echo mDNS: no pairing identity published after {IDENTITY_WAIT:?} — \
                 not advertising (is the pairing server running in this process?)"
            );
            return;
        };

        // Wait for the router, but not for long. The record a client stores now
        // is the one it will dial from cellular later, so publishing before the
        // public address is known would hand it a LAN URL it can never correct
        // — correcting it needs the mDNS it will no longer be able to hear.
        //
        // Bounded because a network with no UPnP router is a perfectly ordinary
        // one and must not delay discovery: the search itself gives up first and
        // reports `Unavailable`, so this returns early in every case except a
        // router that is answering slowly.
        let public = match crate::upnp::await_outcome(UPNP_WAIT).await {
            crate::upnp::Outcome::Public(ip) => Some(ip.to_string()),
            crate::upnp::Outcome::Unavailable => None,
            crate::upnp::Outcome::Pending => {
                println!(
                    "📡 Echo mDNS: the router has not answered after {UPNP_WAIT:?} — advertising \
                     the LAN relay for now (a WAN client will need the address by hand)"
                );
                None
            }
        };

        let label = host_label();
        let records =
            txt_records(&fingerprint, &label, &relay_url, &relay_pin, &ip, public.as_deref(), &advertise_override);
        // Say so when the advertised URL is not the configured one, so the log
        // explains an address the operator never typed.
        if let Some((_, advertised)) = records.iter().find(|(k, _)| k == "relay") {
            if advertised != relay_url.trim() {
                println!(
                    "📡 Echo mDNS: advertising relay {advertised} in place of the configured {} \
                     ({})",
                    relay_url.trim(),
                    if public.is_some() {
                        "reachable from outside this network"
                    } else {
                        "the configured address means nothing to a client"
                    }
                );
            }
        }
        let props: HashMap<String, String> = records.into_iter().collect();
        let advertises_relay = props.contains_key("relay");

        let svc =
            match ServiceInfo::new(SERVICE_TYPE, INSTANCE_NAME, HOST_NAME, ip.as_str(), port, props) {
                Ok(s) => s,
                Err(e) => {
                    println!("⚠️ Echo mDNS: could not build the service record: {e}");
                    return;
                }
            };

        match mdns.register(svc) {
            Ok(()) => println!(
                "📡 Echo mDNS: advertising {SERVICE_TYPE} on {ip}:{port} as \"{label}\" \
                 (fp {}…, relay {})",
                &fingerprint[..16.min(fingerprint.len())],
                if advertises_relay { "advertised" } else { "not configured" },
            ),
            Err(e) => println!("⚠️ Echo mDNS: registration failed: {e}"),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const FP: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";

    /// Stand-in for the host's own LAN address — what a loopback relay URL is
    /// rewritten to before it goes on the wire.
    const LAN: &str = "10.0.0.205";

    fn get<'a>(txt: &'a [(String, String)], key: &str) -> Option<&'a str> {
        txt.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    #[test]
    fn always_publishes_version_fingerprint_and_name() {
        let txt = txt_records(FP, "GAMING-PC", "", "", LAN, None, "");
        assert_eq!(get(&txt, "txtvers"), Some("1"));
        assert_eq!(get(&txt, "fp"), Some(FP));
        assert_eq!(get(&txt, "name"), Some("GAMING-PC"));
    }

    #[test]
    fn relay_keys_appear_only_when_both_are_configured() {
        let both = txt_records(FP, "PC", "https://relay:8443/v1/signal", FP, LAN, None, "");
        assert_eq!(get(&both, "relay"), Some("https://relay:8443/v1/signal"));
        assert_eq!(get(&both, "relaypin"), Some(FP));

        // A URL with no pin is not a usable relay: the pin is what authenticates
        // it. Publishing the URL alone would have the app fill one field and
        // leave the user hunting for the other.
        let url_only = txt_records(FP, "PC", "https://relay:8443/v1/signal", "", LAN, None, "");
        assert_eq!(get(&url_only, "relay"), None);
        assert_eq!(get(&url_only, "relaypin"), None);

        let pin_only = txt_records(FP, "PC", "", FP, LAN, None, "");
        assert_eq!(get(&pin_only, "relay"), None);

        let neither = txt_records(FP, "PC", "", "", LAN, None, "");
        assert_eq!(get(&neither, "relay"), None);
        assert_eq!(get(&neither, "relaypin"), None);
    }

    #[test]
    fn whitespace_only_relay_config_counts_as_unconfigured() {
        // `nova.toml` ships these keys as `""`, and an operator clearing one by
        // hand commonly leaves a space behind.
        let txt = txt_records(FP, "PC", "   ", "  ", LAN, None, "");
        assert_eq!(get(&txt, "relay"), None);
    }

    #[test]
    fn relay_values_are_trimmed() {
        let padded_pin = format!(" {FP} ");
        let txt = txt_records(FP, "PC", "  https://r:8443/v1/signal \n", &padded_pin, LAN, None, "");
        assert_eq!(get(&txt, "relay"), Some("https://r:8443/v1/signal"));
        assert_eq!(get(&txt, "relaypin"), Some(FP));
    }

    #[test]
    fn record_fits_comfortably_in_one_response() {
        // Two 64-hex fingerprints plus a URL is the realistic worst case. DNS
        // caps a single TXT string at 255 bytes, and a record much past ~400
        // stops fitting a clean response — which shows up as discovery that is
        // flaky in exactly the "works on my desk" way.
        let txt = txt_records(
            FP,
            "A-VERY-LONG-WORKSTATION-NAME",
            "https://relay.example.com:8443/v1/signal",
            FP,
            LAN,
            None,
            "",
        );
        for (k, v) in &txt {
            assert!(k.len() + v.len() + 1 <= 255, "TXT string {k} exceeds 255 bytes");
        }
        let total: usize = txt.iter().map(|(k, v)| k.len() + v.len() + 2).sum();
        assert!(total < 400, "TXT record is {total} bytes — too large for one clean response");
    }

    #[test]
    fn host_label_is_never_blank() {
        assert!(!host_label().trim().is_empty());
    }

    // ── The loopback relay rewrite ──────────────────────────────────────────
    // Live 2026-08-18: the phone read `https://127.0.0.1:8443/v1/signal` out of
    // the record, resolved it to its own loopback, and reported
    // `Connection refused (os error 111)`.

    #[test]
    fn a_loopback_relay_is_advertised_at_the_lan_address() {
        assert_eq!(
            relay_reachable_from("https://127.0.0.1:8443/v1/signal", LAN),
            "https://10.0.0.205:8443/v1/signal",
        );
    }

    #[test]
    fn every_spelling_of_loopback_is_rewritten() {
        // All three reach the same interface and all three are equally useless
        // to a phone, so recognising only the common one would leave the bug
        // half-fixed and harder to spot the next time.
        for url in [
            "https://127.0.0.1:8443/v1/signal",
            "https://localhost:8443/v1/signal",
            "https://LOCALHOST:8443/v1/signal",
            // 127.0.0.0/8 is loopback in its entirety, not just .1.
            "https://127.0.0.53:8443/v1/signal",
            "https://[::1]:8443/v1/signal",
        ] {
            let out = relay_reachable_from(url, LAN);
            assert!(out.contains(LAN), "{url} was left pointing at loopback: {out}");
        }
    }

    #[test]
    fn the_port_and_path_survive_the_rewrite() {
        // Only the host is wrong. Losing the port would send the client to 443
        // and losing the path would miss the signalling endpoint — both would
        // read as "the rewrite did nothing" from the far end.
        let out = relay_reachable_from("https://127.0.0.1:8443/v1/signal", LAN);
        assert!(out.starts_with("https://"), "scheme lost: {out}");
        assert!(out.contains(":8443"), "port lost: {out}");
        assert!(out.ends_with("/v1/signal"), "path lost: {out}");
    }

    // ── The WAN rewrite ─────────────────────────────────────────────────────

    const WAN: &str = "73.213.125.252";

    #[test]
    fn the_wan_rewrite_replaces_private_addresses_as_well_as_loopback() {
        // The difference from `relay_reachable_from`, and the reason both
        // exist: a LAN address is a fine answer for a client on this network
        // and a useless one for the same client tomorrow on cellular. The
        // record is stored and reused, so it has to answer the harder question.
        for url in [
            "https://127.0.0.1:8443/v1/signal",
            "https://10.0.0.205:8443/v1/signal",
            "https://192.168.1.10:8443/v1/signal",
            "https://172.16.4.4:8443/v1/signal",
            "https://localhost:8443/v1/signal",
        ] {
            let out = relay_for_wan(url, WAN);
            assert!(out.contains(WAN), "{url} kept a local-only address: {out}");
            assert!(out.contains(":8443"), "port lost: {out}");
            assert!(out.ends_with("/v1/signal"), "path lost: {out}");
        }
    }

    #[test]
    fn the_wan_rewrite_leaves_an_operators_own_address_alone() {
        // Someone who put a real address or a DNS name there has already solved
        // this, and replacing their name with a raw IP would break the one
        // deployment that needed no help — including the certificate-pinned
        // hostname case, where the name is the thing they maintain.
        for url in [
            "https://relay.example.com:8443/v1/signal",
            "https://203.0.113.9:8443/v1/signal",
        ] {
            assert_eq!(relay_for_wan(url, WAN), url, "{url} should pass through");
        }
    }

    #[test]
    fn the_advertised_relay_prefers_the_public_address_when_there_is_one() {
        // The whole point of the UPnP work, asserted at the layer a client
        // actually reads.
        let txt = txt_records(FP, "PC", "https://127.0.0.1:8443/v1/signal", FP, LAN, Some(WAN), "");
        let relay = txt.iter().find(|(k, _)| k == "relay").expect("relay advertised").1.clone();
        assert!(relay.contains(WAN), "expected the public address, got {relay}");

        // And falls back to the LAN address when the router gave us nothing,
        // which must keep working exactly as it did before.
        let txt = txt_records(FP, "PC", "https://127.0.0.1:8443/v1/signal", FP, LAN, None, "");
        let relay = txt.iter().find(|(k, _)| k == "relay").expect("relay advertised").1.clone();
        assert!(relay.contains(LAN), "expected the LAN address, got {relay}");
    }

    #[test]
    fn an_explicit_advertise_url_beats_everything_nova_worked_out() {
        // The escape hatch for a hand-forwarded port or a DNS name, and the one
        // source here that can know something Nova cannot discover. It has to
        // win over the UPnP address too, or an operator's deliberate setting
        // would be silently replaced the moment a router started answering.
        const MANUAL: &str = "https://home.example.net:8443/v1/signal";
        for public in [None, Some(WAN)] {
            let txt =
                txt_records(FP, "PC", "https://127.0.0.1:8443/v1/signal", FP, LAN, public, MANUAL);
            let relay = txt.iter().find(|(k, _)| k == "relay").expect("relay advertised").1.clone();
            assert_eq!(relay, MANUAL, "the operator's address must win (public={public:?})");
        }
    }

    #[test]
    fn a_routable_relay_is_never_touched() {
        // The overwhelmingly common case, and the one where rewriting would be
        // actively destructive: a relay genuinely hosted elsewhere.
        for url in [
            "https://relay.example.com:8443/v1/signal",
            "https://10.0.0.7:8443/v1/signal",
            "https://192.168.1.50:8443/v1/signal",
        ] {
            assert_eq!(relay_reachable_from(url, LAN), url);
        }
    }

    #[test]
    fn an_unusable_local_ip_leaves_the_url_alone() {
        // Better to advertise an address a human recognises as wrong than a
        // blank or malformed one they cannot diagnose.
        let url = "https://127.0.0.1:8443/v1/signal";
        assert_eq!(relay_reachable_from(url, ""), url);
        assert_eq!(relay_reachable_from(url, "   "), url);
    }

    #[test]
    fn an_unparseable_url_passes_through_untouched() {
        // A client reports a bad URL far more clearly than a corrupted one.
        for url in ["not a url", "://missing-scheme", ""] {
            assert_eq!(relay_reachable_from(url, LAN), url);
        }
    }

    #[test]
    fn the_rewrite_reaches_the_advertised_record() {
        // The unit above proves the helper; this proves it is actually wired
        // into what goes on the wire.
        let txt = txt_records(FP, "PC", "https://127.0.0.1:8443/v1/signal", FP, LAN, None, "");
        assert_eq!(get(&txt, "relay"), Some("https://10.0.0.205:8443/v1/signal"));
        assert_eq!(get(&txt, "relaypin"), Some(FP));
    }
}
