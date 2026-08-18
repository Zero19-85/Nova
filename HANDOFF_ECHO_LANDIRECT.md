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
