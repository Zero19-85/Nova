//! Video receive path — the client half of Nova's media wire format.
//!
//! Nova sends video as GameStream `NV_VIDEO_PACKET` datagrams (the format
//! `nova-server/src/rtp.rs` builds, itself matching Sunshine's
//! `video_packet_raw_t`). Echo keeps that framing rather than inventing one:
//! it is already proven against a real encoder at 120 fps, it carries the
//! frame index in the clear where loss recovery needs it, and keeping one
//! packetiser on the host means an Echo bug cannot be a Moonlight regression.
//!
//! What Echo adds on top is authenticated encryption of the reassembled frame
//! — see [`nova_core::media_crypto`] — because GameStream sends video in the
//! clear and Echo's whole point is crossing the internet.
//!
//! ## Wire layout, per datagram
//!
//! ```text
//!   [ 0..12]  RTP header      (byte 0 = 0x90: V=2, X=1)
//!   [12..16]  reserved
//!   [16..20]  streamPacketIndex   (LE)
//!   [20..24]  frameIndex          (LE)  ← the decryption counter
//!   [24]      flags: 0x01 pic data, 0x02 EOF, 0x04 SOF
//!   [25..28]  extra/multi-FEC flags
//!   [28..32]  fecInfo (LE): bits 4-11 FEC %, 12-21 shard index, 22-31 data shards
//!   [32..  ]  payload — every datagram in a frame is the same size, zero-padded
//! ```
//!
//! The first data shard's payload begins with an 8-byte frame header: byte 0 is
//! `0x01` (header size 8), byte 3 is the frame type (1 = P, 2 = IDR), bytes 4-5
//! are `lastPayloadLen` — the real length of the final shard, without which the
//! zero padding would be fed to the decoder as if it were data.
//!
//! ## Forward error correction, and the ordering that makes it work
//!
//! The host sends Reed-Solomon parity shards alongside the data shards, and
//! this module reconstructs from them. The layering is what matters:
//!
//! ```text
//!   encode → SEAL (AES-GCM, whole frame) → shard → FEC parity → wire
//!   wire → FEC reconstruct → reassemble → OPEN (AES-GCM) → decoder
//! ```
//!
//! Because the host seals *before* sharding, parity is computed over
//! ciphertext, and reconstruction is pure arithmetic over bytes — it needs no
//! key and reveals nothing. The frame is authenticated once, after it is whole.
//! Had the host sealed each shard instead, every recovered shard would have to
//! be decrypted separately and a recovered *parity* shard could not be
//! decrypted at all.
//!
//! Two details are load-bearing and easy to get wrong:
//!
//! - **Parity covers only part of each datagram.** `rtp.rs` computes parity
//!   with bytes `0..16` (the RTP header) and `28..32` (`fecInfo`) zeroed, then
//!   fills them in afterwards. So this code must re-zero those ranges on every
//!   received shard before reconstructing, or the parity will not verify.
//!   Sunshine and moonlight-common-c do exactly the same dance.
//! - **The parity count is derived, not transmitted.** `fecInfo` carries the
//!   FEC percentage and the data-shard count; the parity count is
//!   `ceil(data × pct / 100)`, which reproduces the host's arithmetic including
//!   the minimum-parity adjustment (the host recomputes the percentage when it
//!   raises parity to the floor, precisely so this stays invertible).

use std::collections::HashMap;

use nova_core::demux::{self, Class, ECHO_MEDIA};
use nova_core::media_crypto::{CryptoError, SessionKeys, STREAM_VIDEO};
use reed_solomon_erasure::galois_8::ReedSolomon;

/// Bytes of headers before the payload in every video datagram.
pub const HEADERS_SIZE: usize = 32;
/// Frame header carried at the start of the first data shard.
pub const FRAME_HEADER_SIZE: usize = 8;

const FLAG_CONTAINS_PIC_DATA: u8 = 0x01;
const FLAG_EOF: u8 = 0x02;
const FLAG_SOF: u8 = 0x04;

/// How many frames may be partially assembled at once.
///
/// Small on purpose. Datagrams for one frame arrive within a few hundred
/// microseconds of each other, so anything older than a couple of frames is
/// never going to complete — and an unbounded map here is a memory-exhaustion
/// primitive for anyone who can reach the media port, which on a punched WAN
/// path is anyone at all.
const MAX_PENDING_FRAMES: usize = 4;

/// A fully reassembled, decrypted frame ready for a decoder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedFrame {
    /// Wire frame index — monotonic per session, and the AES-GCM counter.
    pub index: u32,
    /// 2 = IDR/keyframe, 1 = P-frame.
    pub frame_type: u8,
    /// Annex-B NAL units (H.264/HEVC) or OBUs (AV1).
    pub data: Vec<u8>,
    /// When this frame's **first shard** reached the client.
    ///
    /// Carried so the pipeline can report its own latency. Everything after
    /// this point — waiting for the remaining shards, FEC repair, decryption,
    /// the queue ahead of the decoder — is time the client added, measured on
    /// one clock with no host synchronisation needed. That distinction is the
    /// whole point: a pointer that trails the hand looks identical whether the
    /// delay is in input or in video, and this measures the video half
    /// directly instead of inferring it.
    pub first_shard_at: std::time::Instant,
}

impl DecodedFrame {
    pub fn is_keyframe(&self) -> bool {
        self.frame_type == 2
    }

    /// Whether this frame is an **RFI recovery frame** (wire type 5).
    ///
    /// The host sends this after honouring an `invalidate_ref_frames`
    /// request: it is a P-frame, but one the encoder coded against a
    /// reference it proved survives the invalidated range. So it is a valid
    /// re-entry point into the stream without being a keyframe, which is the
    /// entire point -- it costs a fraction of an IDR's bits.
    ///
    /// Treating it as an ordinary P-frame would be the worst possible bug
    /// here: a gate that re-armed on loss would refuse the very frame sent to
    /// repair it, and the picture would stay frozen while the host believed
    /// it had already answered.
    pub fn is_recovery(&self) -> bool {
        self.frame_type == 5
    }
}

/// Why a datagram or frame was discarded. Counted rather than logged per
/// occurrence: at 120 fps a per-packet log line is its own denial of service.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReceiveStats {
    pub frames_completed: u64,
    pub keyframes: u64,
    /// Frames abandoned because too many shards were lost for FEC to repair.
    pub frames_incomplete: u64,
    /// Frames that arrived complete but failed authentication: corrupted in
    /// flight, or forged.
    pub frames_failed_auth: u64,
    /// Datagrams too short, malformed, or from a stale frame index.
    pub packets_rejected: u64,
    /// Frames that were rebuilt from parity — loss the viewer never saw. The
    /// number worth watching on a WAN link: rising with `frames_incomplete`
    /// flat means FEC is doing its job at the current percentage.
    pub frames_recovered_by_fec: u64,
    /// Frames withheld from the sink because no keyframe had arrived yet (see
    /// [`crate::gate`]). Zero on a healthy session, because Nova starts one with
    /// an IDR; a persistent nonzero count means it did not.
    pub frames_dropped_before_keyframe: u64,
}

struct PartialFrame {
    /// One slot per shard, data first then parity, holding the **whole
    /// datagram** rather than just its payload — parity was computed over the
    /// full block, so reconstruction has to see the full block.
    shards: Vec<Option<Vec<u8>>>,
    /// How many of `shards` are data (the rest are parity).
    data_shards: usize,
    received: usize,
    /// Uniform datagram size for this frame.
    block_size: usize,
    /// When the first shard of this frame arrived — see
    /// [`DecodedFrame::first_shard_at`].
    first_shard_at: std::time::Instant,
}

/// Reassembles datagrams into frames, then opens them.
///
/// Stateful and single-threaded by design: it belongs to one session and one
/// socket, and sharing it would mean locking on the hot path for no benefit.
pub struct VideoDepacketizer {
    /// `None` = accept plaintext. Only correct against a host that is not
    /// sealing; a session that negotiated encryption must always construct
    /// this with keys, or a stripped-encryption stream would be accepted
    /// silently — which is the whole attack.
    keys: Option<SessionKeys>,
    pending: HashMap<u32, PartialFrame>,
    /// Highest frame index completed, so a late duplicate of an old frame is
    /// dropped instead of re-emitted out of order to the decoder.
    last_completed: u32,
    /// Index of the last frame actually handed out, so a gap between
    /// consecutive deliveries can be recognised as transit loss.
    ///
    /// Distinct from `last_completed`, which also advances for frames that
    /// were seen but abandoned. This one only moves when a frame really
    /// reached the caller, which is exactly the set the decoder saw.
    last_delivered: u32,
    /// Inclusive range of indices that never completed, widened as further
    /// gaps appear and cleared by `take_transit_loss`.
    transit_loss: Option<(u32, u32)>,
    pub stats: ReceiveStats,
}

impl VideoDepacketizer {
    pub fn new(keys: Option<SessionKeys>) -> Self {
        Self {
            keys,
            pending: HashMap::new(),
            last_completed: 0,
            last_delivered: 0,
            transit_loss: None,
            stats: ReceiveStats::default(),
        }
    }

    /// Feed one datagram. Returns a frame when this datagram completed one —
    /// either because the last data shard arrived, or because enough shards
    /// arrived for FEC to rebuild the missing ones.
    pub fn push(&mut self, datagram: &[u8]) -> Option<DecodedFrame> {
        if datagram.len() <= HEADERS_SIZE || datagram[0] != ECHO_MEDIA {
            self.stats.packets_rejected += 1;
            return None;
        }
        let flags = datagram[24];
        if flags & FLAG_CONTAINS_PIC_DATA == 0 {
            self.stats.packets_rejected += 1;
            return None;
        }
        let frame_index = u32::from_le_bytes(datagram[20..24].try_into().ok()?);
        let fec_info = u32::from_le_bytes(datagram[28..32].try_into().ok()?);
        let shard_index = ((fec_info >> 12) & 0x3FF) as usize;
        let data_shards = ((fec_info >> 22) & 0x3FF) as usize;
        let fec_percentage = ((fec_info >> 4) & 0xFF) as usize;

        if data_shards == 0 {
            self.stats.packets_rejected += 1;
            return None;
        }
        // The host's arithmetic, inverted — see the module docs on why this is
        // derivable rather than transmitted.
        let parity_shards = (data_shards * fec_percentage).div_ceil(100);
        let total_shards = data_shards + parity_shards;
        if shard_index >= total_shards || total_shards > 255 {
            self.stats.packets_rejected += 1;
            return None;
        }
        // A frame we already emitted. Feeding it again would hand the decoder
        // an out-of-order frame.
        if frame_index <= self.last_completed {
            self.stats.packets_rejected += 1;
            return None;
        }

        let block_size = datagram.len();
        let entry = self.pending.entry(frame_index).or_insert_with(|| PartialFrame {
            shards: vec![None; total_shards],
            data_shards,
            received: 0,
            block_size,
            first_shard_at: std::time::Instant::now(),
        });
        // Every shard of a frame is the same size and describes the same shard
        // counts. A mismatch is a stale or forged packet reusing a live index,
        // and letting it into the buffer would corrupt reconstruction.
        if entry.shards.len() != total_shards
            || entry.data_shards != data_shards
            || entry.block_size != block_size
        {
            self.stats.packets_rejected += 1;
            return None;
        }
        if entry.shards[shard_index].is_none() {
            // Store the WHOLE datagram with the parity-excluded ranges zeroed.
            // The host computed parity with bytes 0..16 and 28..32 zero and
            // wrote them afterwards; restoring that state is what makes the
            // arithmetic line up. (It also erases the demux tag in byte 0,
            // which has already done its job by this point.)
            let mut shard = datagram.to_vec();
            shard[0..16].fill(0);
            shard[28..32].fill(0);
            entry.shards[shard_index] = Some(shard);
            entry.received += 1;
        }

        // SOF/EOF carry no information reassembly needs — the shard index and
        // count already say everything — but they are part of the format.
        let _ = (FLAG_SOF, FLAG_EOF);

        let have_all_data = entry.shards[..data_shards].iter().all(Option::is_some);
        let can_reconstruct = entry.received >= data_shards;
        if !have_all_data && !can_reconstruct {
            self.evict_stale(frame_index);
            return None;
        }

        let mut partial = self.pending.remove(&frame_index).expect("just inserted or found");
        self.evict_stale(frame_index);
        // Mark the frame done HERE rather than after a successful open. A frame
        // that fails authentication is just as finished as one that succeeds,
        // and leaving the index open meant its remaining shards each built a
        // fresh partial, re-ran reconstruction, and failed the tag again — one
        // corrupt frame counting as several, with the work repeated per shard.
        // On a lossy link that is the exact moment the client can least afford
        // to be doing extra work.
        self.last_completed = self.last_completed.max(frame_index);

        if !have_all_data {
            // Enough shards, but not the right ones: rebuild the missing data
            // shards from parity. This is the loss the viewer never sees.
            match reconstruct(&mut partial) {
                Ok(()) => self.stats.frames_recovered_by_fec += 1,
                Err(()) => {
                    self.stats.frames_incomplete += 1;
                    return None;
                }
            }
        }
        self.finish(frame_index, partial)
    }

    /// Abandon partial frames that can no longer complete, so one lost shard
    /// does not leak a buffer for the rest of the session.
    fn evict_stale(&mut self, newest: u32) {
        if self.pending.len() <= MAX_PENDING_FRAMES {
            return;
        }
        let cutoff = newest.saturating_sub(MAX_PENDING_FRAMES as u32);
        let before = self.pending.len();
        self.pending.retain(|&idx, _| idx > cutoff);
        self.stats.frames_incomplete += (before - self.pending.len()) as u64;
    }

    fn finish(&mut self, index: u32, partial: PartialFrame) -> Option<DecodedFrame> {
        let shard_count = partial.data_shards;
        let payload_size = partial.block_size - HEADERS_SIZE;
        let mut stream = Vec::with_capacity(shard_count * payload_size);
        for shard in partial.shards.iter().take(shard_count) {
            let Some(bytes) = shard else {
                // Unreachable: the caller only gets here with every data shard
                // present or reconstructed. Counted rather than panicked,
                // because a panic here would be a remote crash.
                self.stats.frames_incomplete += 1;
                return None;
            };
            stream.extend_from_slice(&bytes[HEADERS_SIZE..]);
        }
        if stream.len() < FRAME_HEADER_SIZE {
            self.stats.packets_rejected += 1;
            return None;
        }

        // Frame header: byte 3 is the type, bytes 4-5 the true length of the
        // final shard. Without trimming to it, the encoder's zero padding is
        // handed to the decoder as trailing garbage.
        let frame_type = stream[3];
        let last_payload_len = u16::from_le_bytes([stream[4], stream[5]]) as usize;
        let total = (shard_count - 1) * payload_size + last_payload_len;
        if total > stream.len() || total < FRAME_HEADER_SIZE {
            self.stats.packets_rejected += 1;
            return None;
        }
        let body = &stream[FRAME_HEADER_SIZE..total];

        let data = match &self.keys {
            None => body.to_vec(),
            Some(keys) => match keys.open(STREAM_VIDEO, index, frame_type, body) {
                Ok(plain) => plain,
                Err(CryptoError::Truncated) | Err(CryptoError::Authentication) => {
                    // Reassembled but not authentic. Never pass it on: a
                    // decoder fed forged input is the softest target in the
                    // whole pipeline.
                    self.stats.frames_failed_auth += 1;
                    return None;
                }
                Err(_) => {
                    self.stats.frames_failed_auth += 1;
                    return None;
                }
            },
        };

        self.stats.frames_completed += 1;
        if frame_type == 2 {
            self.stats.keyframes += 1;
        }

        // Transit loss: a jump in delivered indices means the frames in
        // between never completed -- lost beyond what FEC could repair, or
        // abandoned as stale. They are exactly the frames the decoder will
        // never see, so they are exactly what the encoder must invalidate.
        //
        // This is the loss the queue-overflow path cannot see: nothing was
        // ever dropped locally, so no sink ever noticed. Before this the only
        // repair for it was the host's rolling intra refresh, which is why a
        // clean-looking link still smeared after a burst.
        //
        // The first delivery of a session is not a gap: `last_delivered`
        // starts at 0 and the host's wire indices are 1-based, so a session
        // opening at frame 1 is continuous. A session that opens mid-GOP is
        // handled by the keyframe gate instead -- there is no reference to
        // repair yet, so asking to invalidate one would be meaningless.
        if self.last_delivered != 0 && index > self.last_delivered + 1 {
            let (first, last) = (self.last_delivered + 1, index - 1);
            self.transit_loss = Some(match self.transit_loss {
                Some((have_first, have_last)) => {
                    (have_first.min(first), have_last.max(last))
                }
                None => (first, last),
            });
        }
        self.last_delivered = index;

        Some(DecodedFrame { index, frame_type, data, first_shard_at: partial.first_shard_at })
    }

    /// The range of wire indices that never arrived, clearing it as it
    /// reports it.
    ///
    /// Reported the moment the gap is observed rather than on a timer: the
    /// encoder can only re-point a reference while the lost frames are still
    /// inside its DPB, which holds five. Live 2026-08-22, every one of 24
    /// invalidation requests arrived 30-80 frames late and was refused
    /// (`RFI range 154965-155019 >= DPB 5`) purely because the client batched
    /// them onto a 500 ms tick. Latency here is not a nicety; it decides
    /// whether the cheap repair is possible at all.
    pub fn take_transit_loss(&mut self) -> Option<(u32, u32)> {
        self.transit_loss.take()
    }
}

/// Rebuild missing data shards from parity.
///
/// The generator matrix must match the host's exactly, which is why both sides
/// pin the same `reed-solomon-erasure` major version: a different
/// implementation would not fail, it would reconstruct confidently wrong bytes
/// — caught by the GCM tag afterwards, but as an unexplained authentication
/// failure rather than an obvious one.
fn reconstruct(partial: &mut PartialFrame) -> Result<(), ()> {
    let parity = partial.shards.len() - partial.data_shards;
    if parity == 0 {
        return Err(()); // nothing to rebuild from
    }
    let rs = ReedSolomon::new(partial.data_shards, parity).map_err(|_| ())?;
    rs.reconstruct(&mut partial.shards).map_err(|_| ())?;
    Ok(())
}

/// What a sink wants the host to do about a broken reference chain.
///
/// The two are not interchangeable, and picking the wrong one is a real bug
/// in each direction. An IDR is the only repair that works when the decoder
/// holds no usable reference at all (it was just rebuilt, or the session is
/// starting) -- an invalidation there would re-point at a reference the
/// decoder never had. Conversely, when the client knows exactly which frames
/// it lost, an IDR is a bitrate spike bought for nothing: under CBR every
/// surrounding frame pays for it, which is the smear that follows a drop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairRequest {
    /// Send a full keyframe: nothing here is decodable against anything.
    Keyframe,
    /// These wire indices never reached the decoder. Drop them from the DPB
    /// and code the next frame against an older reference. The host falls
    /// back to a keyframe on its own if it cannot honour the range, so this
    /// is always safe to ask for.
    Invalidate { first: u32, last: u32 },
}

/// Shortest spacing between repair requests.
///
/// A burst of loss produces a gap per frame, and one RPC per frame would be
/// its own denial of service on the control tunnel. Coalescing instead of
/// dropping is what keeps that safe: requests that arrive inside the window
/// widen the pending range rather than being discarded, so nothing is ever
/// lost -- it is only delayed to the next flush.
///
/// Deliberately far below the 500 ms keepalive tick this used to ride on.
/// See `VideoDepacketizer::take_transit_loss` for what that cost.
///
/// **This interval is part of the repair window, not just a politeness
/// limit.** Every millisecond spent holding a request is time the encoder
/// spends coding more frames onto the broken reference, and the host can only
/// re-point while the lost frames are still in a DPB that holds five. At
/// 120 fps that whole budget is 41 ms, so the first value tried here -- 50 ms
/// -- could on its own push a repair out of reach.
///
/// Measured live 2026-08-22 at 4K120, which is what set this number:
///
/// | reporting delay | observed span | outcome         |
/// |-----------------|---------------|-----------------|
/// | 500 ms tick     | 30-80 frames  | 0/24 succeeded  |
/// | 50 ms limiter   | 5-24 frames   | 20 succeeded, most still refused |
///
/// 10 ms is ~1 frame at 120 fps and under one at 60, so the limiter stops
/// being a meaningful term in the span while still collapsing a burst of
/// per-frame gaps into a single request. The cost of asking slightly too
/// often is a few hundred bytes of NDJSON; the cost of asking slightly too
/// late is a full 4K intra frame, which is four orders of magnitude worse.
const REPAIR_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);

/// Accumulates repair requests and releases them at a bounded rate.
#[derive(Default)]
struct RepairOutbox {
    keyframe: bool,
    range: Option<(u32, u32)>,
    last_sent: Option<std::time::Instant>,
}

impl RepairOutbox {
    fn note_keyframe(&mut self) {
        self.keyframe = true;
    }

    fn note_range(&mut self, first: u32, last: u32) {
        self.range = Some(match self.range {
            Some((f, l)) => (f.min(first), l.max(last)),
            None => (first, last),
        });
    }

    /// Send at most one request, oldest-loss-first, if the rate allows.
    ///
    /// A keyframe request outranks an invalidation: it is only raised when
    /// the decoder holds no usable reference at all, and no amount of
    /// re-pointing helps in that state. Taking both clears both, so a stale
    /// invalidation cannot surface after the keyframe that already repaired
    /// it.
    fn flush(&mut self, tx: Option<&tokio::sync::mpsc::UnboundedSender<RepairRequest>>) {
        if !self.keyframe && self.range.is_none() {
            return;
        }
        let due = self
            .last_sent
            .is_none_or(|t| t.elapsed() >= REPAIR_MIN_INTERVAL);
        if !due {
            return; // still pending; the next flush carries the widened range
        }
        let repair = if self.keyframe {
            RepairRequest::Keyframe
        } else {
            let (first, last) = self.range.expect("checked above");
            RepairRequest::Invalidate { first, last }
        };
        self.keyframe = false;
        self.range = None;
        self.last_sent = Some(std::time::Instant::now());
        if let Some(tx) = tx {
            let _ = tx.send(repair);
        }
    }
}

/// Where reassembled frames go. A trait so the headless CLI can count them
/// while a real client hands them to a hardware decoder, without either
/// knowing about the other.
pub trait FrameSink {
    fn on_frame(&mut self, frame: DecodedFrame);

    /// Whether the sink has lost its reference chain and needs a keyframe,
    /// clearing the request as it reports it.
    ///
    /// Nova runs an **infinite GOP**: there is no scheduled IDR, so a sink that
    /// re-arms a keyframe gate after a drop will wait forever unless somebody
    /// asks. Live 2026-08-15: a 3-frame queue overflowed while the decoder was
    /// warming up, closed its gate, and the picture stayed on the first frame
    /// while the host streamed 3,600 more.
    ///
    /// Defaults to `false` for sinks that cannot lose sync — the headless
    /// `LoggingSink` never drops anything, so it never needs to ask.
    fn take_keyframe_request(&mut self) -> bool {
        false
    }

    /// The inclusive range of wire indices the sink lost, clearing it as it
    /// reports it -- the cheap repair, preferred over a keyframe whenever the
    /// sink can name what it missed.
    ///
    /// Defaults to `None` for sinks that never drop frames.
    fn take_invalidation_request(&mut self) -> Option<(u32, u32)> {
        None
    }
}

/// Counts and describes frames instead of decoding them — what the headless
/// build uses to prove media actually flows.
#[derive(Default)]
pub struct LoggingSink {
    pub frames: u64,
    pub bytes: u64,
}

impl FrameSink for LoggingSink {
    fn on_frame(&mut self, frame: DecodedFrame) {
        self.frames += 1;
        self.bytes += frame.data.len() as u64;
        // The first frames tell you the stream is alive and correctly keyed;
        // after that, per-frame logging at 120 fps is noise.
        if self.frames <= 5 || frame.is_keyframe() {
            println!(
                "🎞️  frame {} — {} bytes{}",
                frame.index,
                frame.data.len(),
                if frame.is_keyframe() { " (keyframe)" } else { "" }
            );
        }
    }
}

/// Receive loop: reads the punched socket until cancelled, feeding frames to
/// `sink`.
///
/// `peer` is the address the session was granted for. Datagrams from anywhere
/// else are dropped before parsing — on a WAN-reachable port that is not
/// paranoia, it is the difference between ignoring a scanner and letting one
/// inject into the reassembly buffer. Authentication would catch a forgery
/// anyway; this catches it a step earlier and for free.
///
/// The host also expects periodic "pings" from this socket: `rtp.rs` learns a
/// Moonlight client's address that way. An Echo session's target is pinned
/// instead (the punch already established it), so the pings are here purely to
/// hold the NAT mapping open — without them the pinhole lapses during a quiet
/// moment and the stream stops for reasons that look like packet loss.
///
/// Frames pass a [`KeyframeGate`] before reaching `sink`, so a sink is
/// guaranteed to see a keyframe first and never to receive a P-frame whose
/// references it does not hold. That guarantee belongs here rather than in each
/// sink: it is a property of the stream, and re-deriving it per consumer is how
/// one consumer ends up without it.
pub async fn run_receiver(
    socket: &tokio::net::UdpSocket,
    peer: std::net::SocketAddr,
    mut media_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    keys: Option<SessionKeys>,
    sink: &mut impl FrameSink,
    stop: tokio::sync::watch::Receiver<bool>,
    repair_tx: Option<tokio::sync::mpsc::UnboundedSender<RepairRequest>>,
) -> std::io::Result<ReceiveStats> {
    let mut depack = VideoDepacketizer::new(keys);
    let mut gate = crate::gate::KeyframeGate::new();
    let mut outbox = RepairOutbox::default();
    let mut keepalive = tokio::time::interval(std::time::Duration::from_millis(500));
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut stop = stop;

    loop {
        tokio::select! {
            _ = stop.changed() => {
                if *stop.borrow() { return Ok(finalize(depack.stats, &gate)); }
            }
            _ = keepalive.tick() => {
                // NAT keepalive only. Deliberately not sent before a session
                // is granted: an unsolicited datagram on the host's media port
                // is exactly what `rtp.rs` learns a Moonlight client's address
                // from, so pinging early could redirect somebody else's live
                // stream to this client. Once a session exists the host has
                // pinned its target and ignores these entirely.
                let _ = socket.send_to(b"PING", peer).await;

                // The tick is now only a backstop. Repairs are raised on the
                // frame path the moment loss is observed, because the encoder
                // can only re-point a reference while the lost frames are
                // still in its DPB -- batching them here is what made every
                // one of them arrive too late to honour. This still catches a
                // request raised by a sink after the last frame arrived, and a
                // range held back by the rate limiter when the stream stops.
                if sink.take_keyframe_request() {
                    outbox.note_keyframe();
                }
                if let Some((first, last)) = sink.take_invalidation_request() {
                    outbox.note_range(first, last);
                }
                outbox.flush(repair_tx.as_ref());
            }
            datagram = media_rx.recv() => {
                match datagram {
                    Some(d) => {
                        if let Some(frame) = depack.push(&d) {
                            // Frames the network lost, seen the instant the
                            // gap appears rather than up to 500 ms later.
                            if let Some((first, last)) = depack.take_transit_loss() {
                                outbox.note_range(first, last);
                            }
                            // The gate, not the sink, decides whether a frame is
                            // decodable yet — see this function's docs.
                            if gate.admit(&frame) {
                                sink.on_frame(frame);
                            }
                            // The sink may have just dropped something of its
                            // own on that push, so collect both before
                            // flushing: one request covering everything beats
                            // two the rate limiter would have to merge anyway.
                            if sink.take_keyframe_request() {
                                outbox.note_keyframe();
                            }
                            if let Some((first, last)) = sink.take_invalidation_request() {
                                outbox.note_range(first, last);
                            }
                            outbox.flush(repair_tx.as_ref());
                        }
                    }
                    // demultiplexer stopped
                    None => return Ok(finalize(depack.stats, &gate)),
                }
            }
        }
    }
}

/// The depacketiser counts what it parsed; the gate counts what it withheld.
/// Joining them at the exit keeps the gate's tally out of the hot path.
fn finalize(stats: ReceiveStats, gate: &crate::gate::KeyframeGate) -> ReceiveStats {
    ReceiveStats { frames_dropped_before_keyframe: gate.dropped(), ..stats }
}

/// Sole reader of the punched socket, splitting it into the streams that share
/// it.
///
/// There can be exactly one reader — two tasks calling `recv_from` on the same
/// socket would race for each other's datagrams — so demultiplexing happens
/// here and everything else consumes a channel. That is also why this starts
/// before the control channel does: the TLS handshake needs its datagrams
/// delivered, and they arrive on this socket.
pub async fn demultiplex(
    socket: &tokio::net::UdpSocket,
    peer: std::net::SocketAddr,
    media_tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    control_tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    audio_tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) -> std::io::Result<()> {
    // MTU-sized: an undersized buffer makes `recv_from` FAIL on Windows
    // (WSAEMSGSIZE) and discard the datagram rather than truncating it — the
    // same trap `rtp.rs` documents on the host side.
    let mut buf = vec![0u8; 2048];
    loop {
        tokio::select! {
            _ = stop.changed() => {
                if *stop.borrow() { return Ok(()); }
            }
            result = socket.recv_from(&mut buf) => {
                match result {
                    Ok((n, from)) => {
                        // Anything not from our session's host is dropped before
                        // parsing. Authentication would catch a forgery anyway
                        // — TLS on the control side, the GCM tag on the media
                        // side — but on a WAN-reachable port this rejects a
                        // scanner a step earlier and for free.
                        if from != peer {
                            continue;
                        }
                        match demux::classify(&buf[..n]) {
                            Class::EchoMedia => {
                                if media_tx.send(buf[..n].to_vec()).is_err() {
                                    return Ok(());
                                }
                            }
                            Class::EchoControl | Class::EchoControlAck => {
                                if control_tx.send(buf[..n].to_vec()).is_err() {
                                    return Ok(());
                                }
                            }
                            // Input and microphone audio only ever travel
                            // client → host, so one arriving here is our own
                            // datagram reflected by a middlebox or someone
                            // probing. Either way there is no receiver for it
                            // on this side.
                            Class::EchoInput | Class::EchoMic => {}
                            // Downstream game audio goes to its OWN channel,
                            // never the media one. Audio would otherwise be
                            // depacketized, FEC-reconstructed and keyframe-gated
                            // by a pipeline built for video — and, worse, its
                            // decode cost would land inside the video path's
                            // latency budget. That is the coupling that cost a
                            // full round upstream (`learn_ticker`); it is not
                            // being rebuilt here.
                            //
                            // A closed channel is not fatal the way the media
                            // one is: audio is optional in every direction, so a
                            // client running without it drops these for the
                            // price of a byte comparison.
                            Class::EchoAudio => {
                                let _ = audio_tx.send(buf[..n].to_vec());
                            }
                            // The host's STUN keepalives and punch probes, plus
                            // internet noise.
                            Class::Stun | Class::Other => {}
                        }
                    }
                    // An ICMP port-unreachable from an earlier send surfaces
                    // here on Windows as ConnectionReset. It refers to a
                    // datagram already gone, not to this socket dying, so
                    // continuing is correct — the same trap that once aborted
                    // hole punches mid-blast.
                    Err(e) if matches!(
                        e.kind(),
                        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionRefused
                    ) => continue,
                    Err(e) => return Err(e),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Percentage the test host encodes with. Higher than the 5% Nova ships so
    /// small test frames still get parity worth exercising.
    const TEST_FEC_PCT: usize = 20;

    /// Build a frame's datagrams exactly as `nova-server/src/rtp.rs` does —
    /// including Reed-Solomon parity computed over the same zeroed ranges.
    ///
    /// This is deliberately a reimplementation of the host's packetiser rather
    /// than a call into it: these tests exist to catch the two sides drifting
    /// apart, which a shared helper would hide.
    fn packetize(
        frame_index: u32,
        frame_type: u8,
        body: &[u8],
        payload_size: usize,
        fec_pct: usize,
    ) -> Vec<Vec<u8>> {
        let block_size = HEADERS_SIZE + payload_size;

        let mut stream = vec![0u8; FRAME_HEADER_SIZE];
        stream[0] = 0x01;
        stream[3] = frame_type;
        stream.extend_from_slice(body);

        let data_shards = stream.len().div_ceil(payload_size);
        let mut last = (stream.len() % payload_size) as u16;
        if last == 0 {
            last = payload_size as u16;
        }
        stream[4..6].copy_from_slice(&last.to_le_bytes());

        let parity_shards = (data_shards * fec_pct).div_ceil(100);
        let total = data_shards + parity_shards;

        // Data shards first, with only the parity-covered fields written.
        let mut shards: Vec<Vec<u8>> = (0..total).map(|_| vec![0u8; block_size]).collect();
        for (x, shard) in shards.iter_mut().enumerate().take(data_shards) {
            shard[20..24].copy_from_slice(&frame_index.to_le_bytes());
            let mut flags = FLAG_CONTAINS_PIC_DATA;
            if x == 0 {
                flags |= FLAG_SOF;
            }
            if x == data_shards - 1 {
                flags |= FLAG_EOF;
            }
            shard[24] = flags;
            shard[26] = 0x10;
            let start = x * payload_size;
            let end = (start + payload_size).min(stream.len());
            shard[HEADERS_SIZE..HEADERS_SIZE + (end - start)].copy_from_slice(&stream[start..end]);
        }

        if parity_shards > 0 {
            ReedSolomon::new(data_shards, parity_shards)
                .unwrap()
                .encode(&mut shards)
                .unwrap();
        }

        // Post-parity fields, exactly as the host writes them: the demux tag in
        // byte 0, and fecInfo in 28..32.
        for (x, shard) in shards.iter_mut().enumerate() {
            shard[0] = ECHO_MEDIA;
            let fec_info: u32 =
                ((fec_pct as u32) << 4) | ((x as u32) << 12) | ((data_shards as u32) << 22);
            shard[28..32].copy_from_slice(&fec_info.to_le_bytes());
            shard[20..24].copy_from_slice(&frame_index.to_le_bytes());
        }
        shards
    }

    #[test]
    fn a_multi_shard_frame_reassembles_without_the_zero_padding() {
        // 500 bytes across 128-byte shards: the last shard is padded, which is
        // exactly what lastPayloadLen exists to trim.
        let body: Vec<u8> = (0..500u32).map(|i| (i % 251) as u8).collect();
        let packets = packetize(1, 2, &body, 128, TEST_FEC_PCT);
        assert!(packets.len() > 1, "test must exercise multiple shards");

        let mut d = VideoDepacketizer::new(None);
        let mut out = None;
        for p in &packets {
            if let Some(f) = d.push(p) {
                out = Some(f);
            }
        }
        let frame = out.expect("frame completes on the last shard");
        assert_eq!(frame.index, 1);
        assert!(frame.is_keyframe());
        assert_eq!(frame.data, body, "padding must be trimmed, not decoded");
        assert_eq!(d.stats.frames_completed, 1);
    }

    /// Frames the network swallowed must be reported as a range, because that
    /// range is the only thing that lets the host repair them with a P-frame
    /// instead of a keyframe.
    #[test]
    fn a_gap_in_delivered_frames_is_reported_as_transit_loss() {
        let body = vec![7u8; 300];
        let mut d = VideoDepacketizer::new(None);

        for p in packetize(1, 2, &body, 200, TEST_FEC_PCT) {
            d.push(&p);
        }
        assert_eq!(d.take_transit_loss(), None, "the first frame is not a gap");

        for p in packetize(2, 1, &body, 200, TEST_FEC_PCT) {
            d.push(&p);
        }
        assert_eq!(d.take_transit_loss(), None, "consecutive frames are continuous");

        // 3, 4 and 5 never arrive at all -- no shard of them is ever seen, so
        // nothing local drops and only the index jump reveals them.
        for p in packetize(6, 1, &body, 200, TEST_FEC_PCT) {
            d.push(&p);
        }
        assert_eq!(
            d.take_transit_loss(),
            Some((3, 5)),
            "the whole missing run must be named, not just its edges"
        );
        assert_eq!(d.take_transit_loss(), None, "one gap, one report");
    }

    /// A burst of loss must coalesce into one widened range rather than one
    /// request per frame -- and nothing may be silently discarded on the way,
    /// because a dropped range leaves those frames in the DPB forever.
    #[test]
    fn repair_requests_coalesce_instead_of_flooding() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut outbox = RepairOutbox::default();

        outbox.note_range(10, 12);
        outbox.flush(Some(&tx));
        assert_eq!(
            rx.try_recv().ok(),
            Some(RepairRequest::Invalidate { first: 10, last: 12 }),
            "the first loss goes out immediately"
        );

        // Inside the rate-limit window: these must widen the pending range,
        // not vanish.
        outbox.note_range(20, 22);
        outbox.flush(Some(&tx));
        outbox.note_range(30, 31);
        outbox.flush(Some(&tx));
        assert!(rx.try_recv().is_err(), "the rate limiter holds them back");

        std::thread::sleep(REPAIR_MIN_INTERVAL);
        outbox.flush(Some(&tx));
        assert_eq!(
            rx.try_recv().ok(),
            Some(RepairRequest::Invalidate { first: 20, last: 31 }),
            "everything held back must still be covered"
        );
    }

    /// A keyframe request outranks a pending invalidation, and clears it:
    /// re-pointing a reference cannot help a decoder that has none, and the
    /// keyframe repairs the earlier gap anyway.
    #[test]
    fn a_keyframe_request_supersedes_a_pending_invalidation() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut outbox = RepairOutbox::default();

        outbox.note_range(5, 6);
        outbox.note_keyframe();
        outbox.flush(Some(&tx));
        assert_eq!(rx.try_recv().ok(), Some(RepairRequest::Keyframe));

        std::thread::sleep(REPAIR_MIN_INTERVAL);
        outbox.flush(Some(&tx));
        assert!(rx.try_recv().is_err(), "the superseded range must not resurface");
    }

    /// Shards do not arrive in order on a real network, and the frame must not
    /// depend on them doing so.
    #[test]
    fn shards_may_arrive_in_any_order() {
        let body: Vec<u8> = (0..900u32).map(|i| i as u8).collect();
        let mut packets = packetize(4, 1, &body, 200, TEST_FEC_PCT);
        packets.reverse();

        let mut d = VideoDepacketizer::new(None);
        let frame = packets
            .iter()
            .filter_map(|p| d.push(p))
            .next()
            .expect("completes regardless of order");
        assert_eq!(frame.data, body);
    }

    /// The headline of this batch: a data shard lost on the WAN is rebuilt
    /// from parity, and the viewer never sees the loss.
    #[test]
    fn a_lost_data_shard_is_rebuilt_from_parity() {
        let body: Vec<u8> = (0..2000u32).map(|i| (i % 253) as u8).collect();
        let packets = packetize(1, 2, &body, 256, TEST_FEC_PCT);
        let data_shards = (body.len() + FRAME_HEADER_SIZE).div_ceil(256);
        assert!(packets.len() > data_shards, "the test needs real parity shards");

        // Drop a data shard in the middle — the case that is unrecoverable
        // without FEC and invisible with it.
        let mut d = VideoDepacketizer::new(None);
        let mut out = None;
        for (i, p) in packets.iter().enumerate() {
            if i == 2 {
                continue;
            }
            if let Some(f) = d.push(p) {
                out = Some(f);
            }
        }
        let frame = out.expect("FEC must rebuild the frame");
        assert_eq!(frame.data, body, "reconstruction must be byte-exact");
        assert_eq!(d.stats.frames_recovered_by_fec, 1);
        assert_eq!(d.stats.frames_incomplete, 0);
    }

    /// FEC is not magic: losing more shards than there is parity for must be
    /// reported as loss, not papered over with wrong bytes.
    #[test]
    fn losing_more_than_the_parity_budget_is_reported_as_loss() {
        let body: Vec<u8> = (0..2000u32).map(|i| i as u8).collect();
        let packets = packetize(1, 2, &body, 256, TEST_FEC_PCT);
        let data_shards = (body.len() + FRAME_HEADER_SIZE).div_ceil(256);
        let parity = packets.len() - data_shards;

        let mut d = VideoDepacketizer::new(None);
        // Drop one more shard than parity can cover.
        for p in packets.iter().skip(parity + 1) {
            assert!(d.push(p).is_none());
        }
        assert_eq!(d.stats.frames_completed, 0);
        assert_eq!(d.stats.frames_recovered_by_fec, 0);
    }

    /// Reconstruction happens before decryption, so a frame that had to be
    /// repaired must still authenticate — which it only does if parity was
    /// computed over ciphertext and the parity-excluded header ranges were
    /// re-zeroed correctly. This is the test that catches getting either
    /// wrong.
    #[test]
    fn a_repaired_frame_still_authenticates() {
        let keys = SessionKeys::generate();
        let body: Vec<u8> = (0..3000u32).map(|i| (i * 5) as u8).collect();
        let sealed = keys.seal(STREAM_VIDEO, 11, 2, &body);
        let packets = packetize(11, 2, &sealed, 256, TEST_FEC_PCT);

        let mut d = VideoDepacketizer::new(Some(keys));
        let mut out = None;
        for (i, p) in packets.iter().enumerate() {
            if i == 1 {
                continue; // lose a data shard
            }
            if let Some(f) = d.push(p) {
                out = Some(f);
            }
        }
        let frame = out.expect("repaired frame must open");
        assert_eq!(frame.data, body);
        assert_eq!(d.stats.frames_recovered_by_fec, 1);
        assert_eq!(d.stats.frames_failed_auth, 0);
    }

    /// The end-to-end crypto contract: the host seals, the client opens, and
    /// both derive the nonce from the frame index carried in the clear.
    #[test]
    fn a_sealed_frame_is_opened_with_the_session_key() {
        let keys = SessionKeys::generate();
        let body: Vec<u8> = (0..1000u32).map(|i| (i * 7) as u8).collect();
        let sealed = keys.seal(STREAM_VIDEO, 9, 2, &body);
        let packets = packetize(9, 2, &sealed, 256, TEST_FEC_PCT);

        let mut d = VideoDepacketizer::new(Some(keys));
        let frame = packets
            .iter()
            .filter_map(|p| d.push(p))
            .next()
            .expect("frame opens");
        assert_eq!(frame.data, body);
        assert_eq!(d.stats.frames_failed_auth, 0);
    }

    /// A frame corrupted in flight, or forged by whoever found the open port,
    /// must never reach the decoder.
    #[test]
    fn a_forged_frame_is_dropped_rather_than_decoded() {
        let keys = SessionKeys::generate();
        let sealed = keys.seal(STREAM_VIDEO, 2, 1, &b"legitimate".repeat(30));
        // No parity: with FEC available a single corrupted-but-present shard
        // would be indistinguishable from a healthy one and reconstruction
        // would not run, so this isolates the authentication check itself.
        let mut packets = packetize(2, 1, &sealed, 128, 0);
        packets[0][HEADERS_SIZE + 20] ^= 0xFF; // flip a payload byte

        let mut d = VideoDepacketizer::new(Some(keys));
        for p in &packets {
            assert!(d.push(p).is_none(), "nothing may be emitted");
        }
        assert_eq!(d.stats.frames_failed_auth, 1);
        assert_eq!(d.stats.frames_completed, 0);
    }

    /// A session that negotiated encryption must not accept plaintext: an
    /// attacker who can strip the sealing should not be able to downgrade the
    /// stream by simply not encrypting it.
    #[test]
    fn plaintext_is_refused_by_a_keyed_receiver() {
        let keys = SessionKeys::generate();
        let packets = packetize(1, 2, b"plain and unsealed", 128, TEST_FEC_PCT);
        let mut d = VideoDepacketizer::new(Some(keys));
        for p in &packets {
            assert!(d.push(p).is_none());
        }
        assert_eq!(d.stats.frames_completed, 0);
        assert_eq!(
            d.stats.frames_failed_auth, 1,
            "one bad frame must count once, not once per shard"
        );
    }

    /// A datagram carrying Moonlight's RTP version byte instead of Echo's tag
    /// is not ours. The two streams share a socket, so accepting one would
    /// mean decrypting a Moonlight frame with an Echo key and reporting a
    /// forgery that never happened.
    #[test]
    fn a_datagram_without_the_echo_tag_is_refused() {
        let mut packets = packetize(1, 2, &vec![9u8; 200], 256, 0);
        packets[0][0] = 0x90; // Moonlight's RTP version byte
        let mut d = VideoDepacketizer::new(None);
        assert!(d.push(&packets[0]).is_none());
        assert_eq!(d.stats.packets_rejected, 1);
        assert_eq!(d.stats.frames_completed, 0);
    }

    /// One lost shard must not leak its partial frame for the rest of the
    /// session — the media port is reachable from the WAN, so an unbounded
    /// reassembly map is a memory-exhaustion primitive.
    #[test]
    fn incomplete_frames_are_evicted_rather_than_accumulating() {
        let mut d = VideoDepacketizer::new(None);
        for index in 1..50u32 {
            // Send only the first shard of each: every frame stays partial and
            // beyond what parity could rebuild.
            let packets = packetize(index, 1, &vec![0u8; 800], 200, TEST_FEC_PCT);
            d.push(&packets[0]);
        }
        assert!(
            d.pending.len() <= MAX_PENDING_FRAMES + 1,
            "pending frames must stay bounded, found {}",
            d.pending.len()
        );
        assert!(d.stats.frames_incomplete > 0);
    }

    /// Late duplicates of an already-decoded frame must not be re-emitted:
    /// handing a decoder an out-of-order frame is worse than dropping it.
    #[test]
    fn a_duplicate_of_a_completed_frame_is_dropped() {
        let body = vec![7u8; 100];
        let packets = packetize(5, 2, &body, 512, 0);
        let mut d = VideoDepacketizer::new(None);

        assert!(d.push(&packets[0]).is_some(), "single-shard frame completes at once");
        assert!(d.push(&packets[0]).is_none(), "the duplicate must be dropped");
        assert_eq!(d.stats.frames_completed, 1);
    }

    #[test]
    fn malformed_datagrams_are_rejected_without_panicking() {
        let mut d = VideoDepacketizer::new(None);
        for junk in [
            vec![],
            vec![0u8; 10],
            vec![ECHO_MEDIA; HEADERS_SIZE], // headers but no payload
            vec![0xFFu8; HEADERS_SIZE + 8], // nonsense flags and fecInfo
            {
                // Correct tag, absurd shard counts: must not allocate a
                // 1024-shard buffer on a stranger's say-so.
                let mut p = vec![0u8; HEADERS_SIZE + 64];
                p[0] = ECHO_MEDIA;
                p[24] = FLAG_CONTAINS_PIC_DATA;
                p[28..32].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
                p
            },
        ] {
            assert!(d.push(&junk).is_none());
        }
        assert!(d.stats.frames_completed == 0);
    }
}
