# ECHO LAN-DIRECT: Architectural Blueprint

## The Goal
Eliminate the WAN relay dependency when the host and client are on the same local subnet. 
Current state: `session::open_path` unconditionally requires a WAN relay offer to latch the media socket, even if the devices are 6 feet apart.

## The Design: LAN Rendezvous (NOT LAN Transport)
We will use the existing mTLS TCP 48011 control port **strictly** for candidate exchange. We will NOT port the `transport.rs` detach/reaper/sweep session logic over to TCP. 

**The Pipeline:**
`bind → TCP+mTLS to host:48011 → lan_rendezvous (RPC) → punch UDP (1.5s timeout) → RUDP tunnel → start_session`

Everything downstream of `open_path` returning an `OpenPath` remains byte-for-byte identical. 

## The Staged Fallback
We do not race the LAN attempt against the WAN relay, as that creates concurrent writers to the `latched` cell.
1. Connect via TCP 48011 (using cached IP or mDNS resolve).
2. Execute `lan_rendezvous` RPC.
3. If failure at any point (TCP refused, mTLS fails, no candidates, or punch times out), gracefully abandon the LAN attempt and fall straight through to the existing WAN relay path.

## Security Boundaries & Validation
The Host must validate any offered LAN candidate before punching:
* **Must be the same IP as the TCP connection source:** Prevents authenticated clients from acting as redirectors/amplifiers against third parties.
* **Must be in Private Address Space.**
* **Must have a non-zero port.**

---

## IMPLEMENTED — the client-side staged selector (2026-08-20)

The host half landed 2026-08-19 (`8837dfa`) with no caller. This is the caller.
`session::open_path` is no longer unconditionally relay-mediated. 338 workspace
tests pass. **Not yet exercised against a live host** — see the bottom of this
section for what to watch.

### What runs, in order

```
bind media socket
  ├─ stage 1  LAN      lan_endpoint set?  TCP+mTLS 48011 → lan_rendezvous → punch (1.5 s)
  ├─ stage 2  relay    STUN gather → lookup → offer → punch (8 s)
  └─ stage 3  direct   wan_endpoint offered as an EXTRA CANDIDATE inside stage 2
```

Stage 1 is staged, never raced — the blueprint's reasoning holds: racing puts two
concurrent writers on the host's latch cell and leaves the winner to timing,
while the loser keeps blasting at a host that has moved on.

**One socket for all of it.** The LAN attempt punches on the same socket the
relay path would use, because the socket *is* the path — anything that rebinds
has thrown away the mapping being negotiated. A failed LAN punch leaves it
perfectly usable: unconnected UDP, no state to reset.

### Stage 3 does NOT do what the phase brief asked, and cannot today

The brief asked for a standalone direct dial to a manual WAN endpoint when the
relay fails. Two host-side facts make that unimplementable from the client:

1. **TCP 48011 drops non-private sources before TLS** (`rpc.rs::is_lan_peer`,
   deliberate — it removes an internet-facing TCP surface from a LocalSystem
   service). So the rendezvous RPC cannot be reached over the internet.
2. **The host answers an unsolicited punch probe but never latches it.**
   `wan.rs` sets the latch cell only in the `punch_rx` arm — the one that runs
   when the host has been *told* to punch, by a relay offer or a LAN rendezvous.
   Answering a stray probe is a cooperative obligation, not an admission.
   `start_session` refuses with `NoPathLatched` when nothing is latched.

So a standalone direct dial would punch successfully, report a path open, and
then be refused a session a moment later — a failure that looks like a success,
which is the worst shape available.

What stage 3 does instead is real: the manual endpoint is added to the punch
candidate list inside the relay-mediated attempt, where the relay offer has
already authorised the host to blast. If that address is the one that answers,
media flows straight to it and never touches a relay-discovered candidate —
worth having for a host behind a port forward whose reflexive candidate is
wrong. A bare address borrows the port the relay reports for the host; without a
port to borrow it is ignored with a warning rather than guessed at.

**Making it standalone needs a host-side decision, not a client change:** the
host would have to accept an authenticated rendezvous from a public source and
latch on it. That means re-opening the internet-facing TCP surface, or building
an authenticated UDP rendezvous. Neither is a thing to slip in quietly.

### The transport is classified from the peer, never from the branch

`Transport::{Lan, WanPunch, DirectWan}` is decided by looking at the address the
punch latched:

- private address → `Lan`, **even when the relay did the signalling**. The relay
  is a signalling channel; a session that traded candidates through it and then
  punched to `192.168.x.x` is carrying media on the local segment. Reporting that
  as WAN would put a cyan badge on the fastest path Echo has and send anyone
  debugging latency out to the internet.
- the manual endpoint → `DirectWan`.
- anything else → `WanPunch`.

`is_private_addr` in `session.rs` must stay identical to `rpc.rs::is_lan_peer`.
The two ends classifying one path differently is how a badge ends up disagreeing
with the host log about what just happened.

### A LAN-only host no longer needs a relay at all

Stage 1 uses no STUN, no relay and no internet, and the host's session manager
has been unconditional since `8837dfa`. So `KnownHost.streamable` now accepts
"paired + a LAN address" as a route, and `open_path` checks for a blank relay
URL *before* STUN so that install fails with a sentence a user can act on rather
than a relay-URL parse error thirty lines later.

### Wire surface added

- `ConnectOptions`: `lan_endpoint`, `wan_endpoint`, `lan_timeout`
  (`DEFAULT_LAN_TIMEOUT` = 500 ms, covering TCP connect *and* TLS handshake as
  one budget — splitting them lets a host that accepts TCP then stalls in TLS
  hold the whole cascade open).
- JNI config keys: `lan_endpoint`, `wan_endpoint`, `lan_timeout_ms`, all
  optional. Omitting `lan_endpoint` skips stage 1 outright.
- Events: `lan_attempt`, `lan_rendezvous`, `lan_abandoned`, and `transport` on
  `path_open`. The Kotlin badge branches on that string, so it is API.
- CLI: `echo-client connect|stream --lan <addr> --wan <addr>`.

### Live test — what to watch

The decisive evidence is on the HOST, in `nova-service.log`:

```
🤝 Echo LAN rendezvous: "<device>" at 10.0.0.x — punching toward 10.0.0.x:<port>
✅ Punch succeeded: path open to 10.0.0.x — <describe_path>
```

A `⛔ Echo LAN rendezvous with "<device>": …` line means a candidate was refused,
and names which rule. On the client the same story appears in the event log
(telemetry toggle on): `LAN first:` → `LAN rendezvous accepted` → `path open …
via lan`.

Wi-Fi → cellular: the same tap should log `LAN abandoned — …` within ~500 ms and
then go through the relay, landing on `wan_punch`. If a LAN attempt on cellular
takes visibly longer than half a second, `lan_timeout` is the knob.

---

## LIVE-CONFIRMED, and the WAN blocker is the relay's address (2026-08-20)

### Stage 1 works on the real host

From `nova-service.log`, unprompted, during ordinary use:

```
🤝 Echo LAN rendezvous: "My Device" at 10.0.0.188 — punching toward 10.0.0.188:41870
✅ Punch succeeded: path open to 10.0.0.188:41870 after 1 round(s) (PeerProbe) — LAN (direct)
🥊 Offer received — blasting at 1 candidate(s) for 1.5s: 10.0.0.188:41870
```

One candidate, 1.5 s budget, one round, `PeerProbe`. That is the staged selector
end to end against the deployed host, with no relay involved.

### The Wi-Fi → cellular failure was NOT the cascade

Reported as "Stage 2 did not fire". It did. The relay is the blocker:

```
nova.toml:  [echo.signaling] url = "https://127.0.0.1:8443/v1/signal"
relay log:  📡 Relay listening on 0.0.0.0:8443 (mutual TLS)
```

`nova-relay` runs on the host itself and its only address is **10.0.0.205** —
RFC1918. The host rewrites the *advertised* copy to `https://10.0.0.205:8443/...`
(the loopback-rewrite from 2026-08-18, working as designed), so the phone's
stored relay URL is a private address. From cellular there is no route to it, so
stage 2 reached `RelayConnection::connect` and could not open a socket.

The relay log confirms it from the other side: every client connection in it
comes from `10.0.0.188` (the LAN). **Not one connection from a carrier address
exists**, because none could arrive.

**There is therefore no WAN path in this deployment at all**, and no client
change can create one. What is needed is a relay with an address reachable from
the internet — a forwarded 8443, or the relay hosted elsewhere — and
`[echo.signaling] url` pointing at it. Everything above the relay is already
built and proven: the client offers both its reflexive and local candidates
(`73.213.125.252:33235, 10.0.0.188:33235` in the log), and the host publishes its
own STUN-discovered candidate to the relay automatically.

**What changed here in response:** `open_path` now recognises a private relay
authority and says so instead of reporting a generic connect failure. The
message names the address and what to do about it. That converts the whole
investigation above into one line the user reads on the phone.

### The manual WAN field is not the answer to this

Worth stating because it is the intuitive next move: filling in a manual WAN
endpoint does **not** rescue a private relay. Stage 3 offers that endpoint as an
extra candidate *inside* the relay-mediated punch, and a punch cannot start
without the relay exchange that authorises the host to blast. It is a fix for a
wrong reflexive candidate, not a substitute for signalling. See the previous
section for why a standalone direct dial is refused by the host.

### IP automation: already automatic, now visible

The host's public address reaches the client with nobody typing anything:
`echo::wan::spawn_gatherer` runs STUN, `echo::signaling` announces the result,
and the client's `lookup` reads it back. The new diagnostics panel displays those
candidates (`HOST CANDIDATES: …`) so "do I need to fill in the manual WAN field?"
has a visible answer, which is normally *no*.

---

## Zero-config WAN: UPnP port mapping (2026-08-20) — BUILT, and INERT on the dev network

`nova-server/src/upnp.rs`. Deployed and running; the code path is exercised and
correct. **It does nothing on the dev box, because the router does not answer
UPnP** — see the measurement below before assuming a bug.

### What it does

At Master startup, after the local address is known (read there, not at process
start — the 2026-08-19 rule), Nova searches for an IGD, asks for its external
address, and requests two mappings. The result feeds the mDNS relay
advertisement, so a phone paired on the LAN stores a URL that still works from
cellular.

Ports opened, and only these two:

| Port | Why |
|---|---|
| relay TCP (from `[echo.signaling] url`, normally 8443) | so an off-LAN client can trade candidates |
| `ECHO_MEDIA_PORT` UDP (47998) | makes the punch succeed behind endpoint-dependent (symmetric) NAT — the one case a punch cannot solve alone |

**48011 is never mapped.** `echo::rpc` drops non-private sources before TLS to
keep an internet-facing TCP surface off a LocalSystem service. Forwarding it
would open the door that fence exists to close, and every packet arriving
through it would be refused anyway. Do not "fix" this by adding it.

The relay port is read from the configured URL rather than hardcoded, so an
operator who moved the relay does not silently get a mapping for a dead port.

### Leases are finite on purpose

Requested for 1 hour and renewed at 30 minutes. A permanent mapping outlives the
process: Nova crashes, and the router forwards a port to a machine with nothing
listening until somebody notices, which nobody does. A Nova that dies stops
renewing and the hole closes by itself. The explicit release on service stop
(`service_main`, before reporting STOPPED) is the fast path, not the only one —
which is what lets that release be hard time-boxed at 6 s and never hang a stop.

Routers that reject finite leases (`OnlyPermanentLeasesSupported`) fall back to
permanent, and log that they did.

### Carrier-grade NAT is detected and refused

If the router's "external" address is itself private — including **100.64.0.0/10**,
which a plain `is_private()` misses — the ISP is doing NAT above the router.
Forwarding a port cannot make the host reachable, so Nova publishes nothing
rather than advertising an address that will silently swallow every WAN attempt.
Tested.
### LIVE AND WORKING — and the bug was the SSDP bind address, not the router

**Superseding an earlier conclusion in this document that was wrong.** The first
pass reported "this router does not do UPnP", supported by three PowerShell
probes that each returned 0 responders. Those probes shared the exact bug they
were being used to rule out, so they confirmed a false conclusion.

The real fault: `SearchOptions::default()` binds the SSDP socket to `0.0.0.0`,
which leaves the *outgoing interface* for the 239.255.255.250 multicast to the
routing table. This host has **five IPv4 addresses** — one real Ethernet
(10.0.0.205) and four `169.254.x` link-local stubs belonging to disconnected
Wi-Fi and Bluetooth adapters. The search left through one of the stubs, reached
no router, and timed out. From the caller's side that is indistinguishable from
a gateway with UPnP switched off.

Measured, everything else held equal:

```
bind 0.0.0.0     -> 0 responders
bind 10.0.0.205  -> 1 responder: http://10.0.0.1:49153/IGDdevicedesc_brlan0.xml
```

`upnp::search_options` now binds to the host's LAN address, which
`upnp::spawn` already receives, and the same address is stored for the teardown
search. **On any multi-homed host the bind address decides whether discovery
works at all — it is not a tuning knob.**

Live result:

```
🌐 UPnP: router found at 10.0.0.1:49153
🌐 UPnP: forwarded UDP 47998 + TCP 8443 to 10.0.0.205 — this host is reachable at 73.213.125.252
📡 Echo mDNS: advertising relay https://73.213.125.252:8443/v1/signal in place of the
   configured https://127.0.0.1:8443/v1/signal (reachable from outside this network)
```

Confirmed independently from the router's own mapping table
(`GetSpecificPortMappingEntry` against `WANIPConnection:1`):

| Rule | Result |
|---|---|
| TCP 8443 | PRESENT → 10.0.0.205:8443, desc `Nova Echo relay` |
| UDP 47998 | PRESENT → 10.0.0.205:47998, desc `Nova Echo media` |
| TCP 48011 | **NOT PRESENT** — the deliberate exclusion, verified rather than assumed |

The external address also corroborates independently: 73.213.125.252 is the same
public address the Android client's own STUN gathering reported when it offered
candidates through the relay (`🥊 Offer received — blasting at 73.213.125.252:…`).

### One claim in this document was too strong

The lease section said a hole left by a crashed Nova closes "within
[`LEASE_SECS`]", an hour. The router **granted 86400** against a request for
3600 — routers are free to clamp the lease and this one does. So the self-closing
window is whatever the gateway decided, not a number Nova controls. The renewal
keeps it fresh and the release on service stop is what closes it promptly; the
lease is a backstop, not a guarantee with a number attached.

---

### SUPERSEDED — "this router does not do UPnP" (kept for the diagnostic lesson)

```
🌐 UPnP: no router answered (No response within timeout)
```

Diagnosed rather than assumed, because "the firewall ate it" is the usual cause
on Windows and would have been a Nova bug:

- An **elevated interactive** PowerShell M-SEARCH for
  `InternetGatewayDevice:1` got **0 responders** — so it is not the service's
  Session-0 identity.
- An M-SEARCH for `ssdp:all` also got **0 responders** — not one UPnP device of
  any kind on the LAN.
- The network profile is **Private**, where `Network Discovery (SSDP-In)` and
  `(UPnP-In)` are **enabled** — so Windows Firewall is not dropping the replies.

Conclusion drawn at the time, and **it was wrong**: that the gateway had UPnP
switched off. Every one of those three probes bound its socket to `0.0.0.0` —
the same bug being investigated — so all three produced confirming evidence for
a false conclusion. See the section above for what was actually happening.

**The lesson, which is why this is kept:** a diagnostic that shares an
implementation detail with the thing it is testing is not independent evidence.
Three probes agreeing meant only that the same mistake was made three times. The
check that settled it varied the suspected variable — same probe, two different
bind addresses — instead of repeating the same measurement in different clothes.

### `[echo.signaling] advertise_url` — the answer when UPnP is not available

Added in the same pass, because with UPnP unavailable the manual route is the
one that will actually be used, and it had a trap in it.

`url` and the advertised URL are two different questions. `url` is how **this
host** reaches the relay, and for a self-hosted relay the right answer is
loopback. The advertised one is how **a phone on cellular** reaches it. Setting
`url` to the public address to solve the second breaks the first — the host
would then dial its own public IP and need hairpinning to reach a relay on the
same machine.

So: keep `url` on loopback, put the public address in `advertise_url`. An
explicit value wins over the UPnP address and over the LAN fallback, because an
operator who typed an address means it.

Precedence, highest first: `advertise_url` → UPnP's mapped address → this host's
LAN address.

### One trade worth knowing

When a public address is advertised, a client that is **on this LAN** and whose
stage-1 direct attempt failed will reach the relay by its public address, which
needs NAT hairpinning — and not every router does it. That case is narrow (stage
1 covers the LAN and is live-confirmed) and recoverable by hand; the cellular
case is the common one and previously had no recovery at all.

### Dependency added

`igd-next` 0.17 with `aio_tokio`, which reuses hyper/hyper-util/http-body-util/
tokio/bytes/futures — all already in the tree. Net new crates: `igd-next`,
`xmltree`, `xml-rs`, `tower-service`, plus a second `rand`/`getrandom` lineage
it pulls transitively. Weighed against hand-rolling SSDP + SOAP (~400 lines of
XML handling that touches the router), the library was the better trade.
