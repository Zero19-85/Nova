//! Echo — Nova's own native client, and the host-side surfaces built for it.
//!
//! Echo exists alongside the legacy Moonlight/GameStream path, not instead of
//! it. Everything in here is additive: no module below may change what a
//! Moonlight client observes.
//!
//! - [`discovery`] — the `_echo._tcp` mDNS record that lets a client find this
//!   host on the LAN without anybody typing an address or a fingerprint.
//! - [`rpc`] — the control/telemetry side-channel on port 48011 (mutual TLS,
//!   authenticated by the client's *pairing* certificate).
//! - [`wan`] — NAT traversal primitives: discovering the host's
//!   server-reflexive address so an Echo client can reach Nova across the
//!   internet without router configuration.
//! - [`signaling`] — the outbound long-poll client that trades those
//!   addresses with a peer through a relay, authenticated with Nova's pairing
//!   identity.
//! - [`session`] — the lifecycle that turns a punched path into a live media
//!   session, and the gate that refuses to do so while a Moonlight client is
//!   streaming. The point where "additive" stops being free and becomes a rule
//!   the code enforces.

//! - [`transport`] — the same command surface as [`rpc`], reached over the
//!   punched UDP path with mutual TLS layered on a reliable-delivery channel.
//!   [`rpc`]'s TCP listener is now LAN-only convenience.

pub mod discovery;
pub mod rpc;
pub mod session;
pub mod signaling;
pub mod transport;
pub mod wan;
