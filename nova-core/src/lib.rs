//! Protocol and identity shared by the Nova host and the Echo client.
//!
//! ## What belongs in here
//!
//! Exactly the things **both peers must agree on**, and nothing else:
//!
//! - [`stun`] — RFC 8489 binding codec and NAT mapping classification. Both
//!   sides discover their own public address the same way, and both must
//!   recognise the other's probes during a hole punch.
//! - [`envelope`] — the newline-delimited JSON request/response shape used by
//!   Nova's control channel and by the signaling relay. One codec on both
//!   ends of every conversation.
//! - [`identity`] — certificate identity: fingerprinting, load-or-generate,
//!   and the mutual-TLS configurations. Nova's host certificate and an Echo
//!   client's certificate are the same kind of object playing opposite roles,
//!   so the code that builds and checks them is written once.
//! - [`media_crypto`] — AES-128-GCM sealing of encoded frames. The host seals
//!   and the client opens, so a single implementation is the only way the two
//!   can be guaranteed to agree on nonce construction — the detail whose
//!   mismatch would be silent and catastrophic.
//! - [`demux`] — how one punched UDP socket carries media, control, STUN, and
//!   a legacy Moonlight stream at once without either side guessing.
//! - [`rudp`] — acknowledged, ordered delivery for control messages. Media
//!   tolerates loss; session state does not.
//! - [`input_channel`] — sealed, unreliable, unordered input datagrams. Input
//!   wants neither ordering nor retransmission, and imposing both on it made
//!   the pointer lag seconds behind the hand; it gets redundancy and
//!   deduplication instead.
//! - [`punch`] — UDP hole punching (simultaneous open). Symmetric by nature:
//!   both peers run the identical algorithm, which is the mechanism itself.
//! - [`relay`] — the signaling-relay client. Nova and Echo are *both* clients
//!   of the relay, speaking the same protocol; only the commands they send
//!   differ.
//!
//! ## What deliberately does not
//!
//! Nova's `echo::rpc` is a *server* whose authorization reads the pairing
//! trust store, and `echo::wan`'s gathering loop drives the media socket owned
//! by `rtp.rs`. Both are host-bound by nature, so they stay in `nova-server`
//! and consume this crate rather than living in it. The rule of thumb: if it
//! needs Windows, a capture pipeline, or Nova's on-disk state, it is not core.
//!
//! This crate compiles for the Xbox and Android targets Echo will eventually
//! run on, which is the practical reason the boundary is enforced rather than
//! merely intended.

pub mod demux;
pub mod envelope;
pub mod identity;
pub mod input_channel;
pub mod media_crypto;
pub mod rudp;
pub mod punch;
pub mod relay;
pub mod stun;
