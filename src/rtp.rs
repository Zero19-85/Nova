use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use reed_solomon_erasure::galois_8::ReedSolomon;

// NV_VIDEO_PACKET flags (from moonlight-common-c VideoDepacketizer.c)
const FLAG_CONTAINS_PIC_DATA: u8 = 0x01;
const FLAG_EOF:                u8 = 0x02;
const FLAG_SOF:                u8 = 0x04;

// GameStream wire layout (matches Sunshine stream.cpp / moonlight-common-c
// RtpVideoQueue): every video datagram is exactly `packet_size + 16` bytes —
// [12B RTP][4B reserved][16B NV_VIDEO_PACKET][payload], zero-padded. Uniform
// size is required for Reed-Solomon FEC — and it must match the packetSize
// the client negotiated in ANNOUNCE (Moonlight: 1392 LAN / 1024 remote),
// because the client reconstructs lost shards at ITS negotiated size.
const DEFAULT_PACKET_SIZE: usize = 1024; // safe fallback if ANNOUNCE omits it
const MAX_RTP_HEADER_SIZE: usize = 16;   // 12B RTP + 4B reserved
const HEADERS_SIZE: usize = 32;          // RTP + reserved + NV_VIDEO_PACKET

// 8-byte frame header for appversion 7.1.415–7.1.445: first byte 0x01 → frameHeaderSize=8
const FRAME_HEADER_SIZE: usize = 8;

// Reed-Solomon FEC defaults: 20% parity (Sunshine's default fecPercentage),
// 2-shard minimum (Moonlight's default minRequiredFecPackets). Runtime-
// configurable via `configure()`; percentage 0 disables FEC entirely
// (A/B test knob for RS matrix compatibility).
const DEFAULT_FEC_PERCENTAGE: usize = 20;
const DEFAULT_MIN_PARITY_SHARDS: usize = 2;

// Packet pacing: a ~45-packet IDR blasted in one sub-millisecond burst can
// overflow the router/AP transmit queue, dropping the tail packets — the
// decoder then renders the top of the frame and corrupts everything below.
// Send in small batches with a short gap, like Sunshine's ratecontrol
// batching (≤64KB batches at 80% of link rate).
//
// PACE_GAP is no longer a constant — see `send_frame`, which computes it as
// 300µs × (60 / fps) so total pacing overhead stays proportional to the
// frame budget (e.g. 150µs at 120fps vs 300µs at 60fps).
const PACE_BATCH_PACKETS: usize = 10;

/// Send one UDP datagram, retrying on `WouldBlock` instead of silently
/// dropping it. Free function (not a method) so callers can pass `&socket`
/// and a shard slice simultaneously without borrow-checker conflicts.
fn send_packet(socket: &UdpSocket, pkt: &[u8], target: SocketAddr) {
    loop {
        match socket.send_to(pkt, target) {
            Ok(_) => return,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::yield_now();
            }
            Err(_) => return,
        }
    }
}

/// Busy-wait for sub-millisecond pacing gaps. `thread::sleep` is unusable
/// here: Windows sleep granularity is 1-15ms, which would stretch a 45-packet
/// IDR over tens of milliseconds and wreck 60fps frame pacing.
fn spin_wait(d: Duration) {
    let end = Instant::now() + d;
    while Instant::now() < end {
        std::hint::spin_loop();
    }
}

pub struct RtpSender {
    socket: UdpSocket,
    sequence_number: u16,
    timestamp: u32,
    /// Client's video address, learned from its first incoming "ping" packet —
    /// the client's source port is ephemeral and cannot be known in advance.
    target: Option<SocketAddr>,
    /// Global packet counter — shifted into NV_VIDEO_PACKET.streamPacketIndex
    packet_counter: u32,
    /// Per-frame counter stored in NV_VIDEO_PACKET.frameIndex. MUST start at 1:
    /// moonlight-common-c initializes `nextFrameNumber = 1` and discards any
    /// packet with `isBefore32(frameIndex, nextFrameNumber)` — a frame 0 (our
    /// forced session-start IDR) is silently dropped, and the client only
    /// re-requests an IDR after it also loses a packet mid-frame
    /// (`waitingForNextSuccessfulFrame`). On a loss-free link that recovery
    /// never fires → eternal black screen → 10 s ML_ERROR_NO_VIDEO_FRAME
    /// ("reduce your bitrate" dialog). Sunshine starts at 1 (video.cpp frame_nr).
    frame_index: u32,
    fps: u32,
    /// True when the active session uses HEVC or AV1. Controls NAL unit
    /// parsing in `detect_frame_type` / `list_nal_types`: HEVC uses a 2-byte
    /// NAL header where `nal_unit_type = (first_byte >> 1) & 0x3F`, versus
    /// H.264's `first_byte & 0x1F`. Incorrect parsing produces all-P-frame
    /// classifications even for IDR/VPS/SPS NALUs → Moonlight never gets a
    /// decodable keyframe → 10-second watchdog timeout.
    is_hevc: bool,
    /// Negotiated packet size (from ANNOUNCE) — wire datagram = this + 16.
    packet_size: usize,
    /// FEC parity percentage; 0 disables FEC (matrix-compat A/B knob).
    fec_percentage: usize,
    min_parity_shards: usize,
    /// Cached RS matrices keyed by (data_shards, parity_shards) — construction
    /// involves a matrix inversion, so don't redo it every frame.
    fec_cache: HashMap<(usize, usize), ReedSolomon>,
    // Per-second send diagnostics
    stat_frames: u32,
    stat_data_pkts: u32,
    stat_parity_pkts: u32,
    stat_bytes: u64,
    stat_window_start: Instant,
    /// Persistent frame payload buffer — reused every frame to eliminate the
    /// per-call Vec heap allocation (frame_header ++ encoded_data).
    stream_buf: Vec<u8>,
    /// Persistent shard pool — grows to the high-watermark shard count (≈ 36
    /// for a typical 4K IDR at 20% FEC) and is reused/zeroed every frame,
    /// eliminating ~36 Vec alloc/dealloc cycles per frame at 60–120 Hz.
    shard_pool: Vec<Vec<u8>>,
}

impl RtpSender {
    pub fn new(bind_port: u16) -> std::io::Result<Self> {
        let socket = UdpSocket::bind(("0.0.0.0", bind_port))?;
        // Give the OS headroom for the ~25-packet bursts sent per frame so a
        // momentary full send buffer blocks (and retries) rather than drops.
        let sock2 = socket2::Socket::from(socket);
        // 8 MB: covers a worst-case 4K IDR burst (~6 MB at uncapped bitrate)
        // plus 2 MB headroom so a full-buffer back-pressure stalls instead of drops.
        sock2.set_send_buffer_size(8 * 1024 * 1024)?;
        // DSCP EF (0xB8 = 101110_00) — Expedited Forwarding: marks video UDP
        // datagrams as low-latency minimum-delay traffic. Best-effort on Windows
        // (honoured by DSCP-aware managed switches and QoS Group Policies).
        // Apollo/Sunshine use qwave.dll QOSAddSocketToFlow for hard guarantees;
        // IP_TOS is the portable fallback that covers most LAN gaming routers.
        let _ = sock2.set_tos(0xB8_u32);
        let socket: UdpSocket = sock2.into();
        socket.set_nonblocking(true)?;
        Ok(Self {
            socket,
            sequence_number: 0,
            timestamp: 0,
            target: None,
            packet_counter: 0,
            frame_index: 1, // Moonlight discards frame 0 — see field doc
            fps: 60,
            packet_size: DEFAULT_PACKET_SIZE,
            fec_percentage: DEFAULT_FEC_PERCENTAGE,
            min_parity_shards: DEFAULT_MIN_PARITY_SHARDS,
            fec_cache: HashMap::new(),
            is_hevc: false,
            stat_frames: 0,
            stat_data_pkts: 0,
            stat_parity_pkts: 0,
            stat_bytes: 0,
            stat_window_start: Instant::now(),
            stream_buf: Vec::new(),
            shard_pool: Vec::new(),
        })
    }

    /// Non-blocking drain of all queued "ping" packets, keeping the most recent
    /// sender. GameStream clients ping the video port every ~500ms for the WHOLE
    /// session (not just after PLAY), so this must be called every loop iteration
    /// — if the socket goes unread after the first learn, stale pings from a
    /// previous session pile up in the receive buffer and the next session
    /// latches onto a dead source port (black screen on reconnect).
    /// Returns the address only when it changes.
    pub fn try_learn_target(&mut self) -> Option<SocketAddr> {
        let mut buf = [0u8; 64];
        let mut latest = None;
        while let Ok((_n, addr)) = self.socket.recv_from(&mut buf) {
            latest = Some(addr);
        }
        let addr = latest?;
        if self.target != Some(addr) {
            self.target = Some(addr);
            return Some(addr);
        }
        None
    }

    pub fn set_fps(&mut self, fps: u32) {
        self.fps = fps.max(1);
    }

    /// Set the codec for NAL unit parsing. Must be called at session start
    /// after the codec is confirmed from the ANNOUNCE SDP so `detect_frame_type`
    /// uses the correct NAL header layout (HEVC 2-byte vs H.264 1-byte).
    pub fn set_codec(&mut self, is_hevc: bool) {
        self.is_hevc = is_hevc;
    }

    /// Apply per-session stream parameters. `packet_size` must be the client's
    /// negotiated x-nv-video[0].packetSize; `fec_percentage` 0 disables FEC.
    pub fn configure(&mut self, packet_size: usize, fec_percentage: usize, min_parity_shards: usize) {
        self.packet_size       = packet_size.clamp(512, 1392);
        self.fec_percentage    = fec_percentage.min(100);
        self.min_parity_shards = min_parity_shards.max(1);
        println!("📐 RTP configured: packetSize={} (datagram={}), fec={}%, minParity={}",
            self.packet_size, self.packet_size + MAX_RTP_HEADER_SIZE,
            self.fec_percentage, self.min_parity_shards);
    }

    /// Drop per-session state so a future PLAY starts a clean stream
    /// (fresh sequence numbers/timestamps, and re-learn the client's
    /// ephemeral video source port via `try_learn_target`).
    pub fn reset(&mut self) {
        // Flush pings buffered from the ending session so the next learn
        // can't latch onto a stale source port.
        let mut buf = [0u8; 64];
        while self.socket.recv_from(&mut buf).is_ok() {}
        self.target          = None;
        self.sequence_number = 0;
        self.timestamp       = 0;
        self.packet_counter  = 0;
        self.frame_index     = 1; // Moonlight discards frame 0 — see field doc
        self.is_hevc         = false;
    }


    /// Send a complete H.264 Annex-B frame using the GameStream NV_VIDEO_PACKET wire format.
    ///
    /// Packet layout per UDP datagram (matches Sunshine's `video_packet_raw_t`):
    ///   [RTP header 12 B, X=1] + [reserved 4 B] + [NV_VIDEO_PACKET 16 B]
    ///   + [frame header 8 B (first pkt only)] + [H.264 data]
    ///
    /// The frame header first byte = 0x01 tells Moonlight frameHeaderSize=8 for
    /// appversion 7.1.415–7.1.445 (which includes our advertised 7.1.431.0).
    ///
    /// Our DESCRIBE response advertises encryptionSupported:0/encryptionRequested:0,
    /// so on real Sunshine session->video.cipher is never set for this session —
    /// video payloads are sent in plaintext (no AES).
    pub fn send_frame(&mut self, data: &[u8]) {
        if data.is_empty() { return; }
        let Some(target) = self.target else { return };

        let frame_type = detect_frame_type(data, self.is_hevc);
        let block_size         = self.packet_size + MAX_RTP_HEADER_SIZE;
        let payload_per_packet = block_size - HEADERS_SIZE;

        // ── Shard accounting (mirrors Sunshine stream.cpp:1328 + fec::encode) ──
        // The payload stream is [8B frame header][H.264 data], split into
        // payload_per_packet slices; the last slice is zero-padded and
        // lastPayloadLen records its real length.
        let stream_len  = FRAME_HEADER_SIZE + data.len();
        let data_shards = (stream_len + payload_per_packet - 1) / payload_per_packet;
        let mut last_payload_len = (stream_len % payload_per_packet) as u16;
        if last_payload_len == 0 { last_payload_len = payload_per_packet as u16; }

        let mut fec_percentage = self.fec_percentage;
        let mut parity_shards  = (data_shards * fec_percentage + 99) / 100;
        if fec_percentage != 0 && parity_shards < self.min_parity_shards {
            parity_shards  = self.min_parity_shards;
            fec_percentage = (100 * parity_shards) / data_shards;
        }
        // GF(2^8) caps data+parity at 255 shards. Sunshine splits oversized
        // frames into up to 4 FEC blocks; our VBV caps frames at ~62 shards,
        // so just degrade to 0% FEC if a freak frame ever exceeds the limit.
        if data_shards + parity_shards > 255 {
            fec_percentage = 0;
            parity_shards  = 0;
        }
        let total_shards = data_shards + parity_shards;

        if self.frame_index < 10 {
            let nal_names: Vec<&str> = list_nal_types(data, self.is_hevc).iter().map(|t| nal_type_name(*t, self.is_hevc)).collect();
            println!("📦 frame {} : {} bytes, {} data + {} parity pkt(s), frame_type={}, NALs={:?}",
                self.frame_index, data.len(), data_shards, parity_shards, frame_type, nal_names);
        }

        // ── Frame header: first 8 bytes of the payload stream ────────────────
        // byte 0 = 0x01 (frameHeaderSize=8), bytes 1-2 = processing latency
        // (unused), byte 3 = frame type (1=P, 2=IDR), bytes 4-5 = lastPayloadLen
        // (LE), bytes 6-7 unused. (Sunshine video_short_frame_header_t.)
        let mut frame_hdr = [0u8; FRAME_HEADER_SIZE];
        frame_hdr[0] = 0x01;
        frame_hdr[3] = frame_type;
        frame_hdr[4..6].copy_from_slice(&last_payload_len.to_le_bytes());

        // Reuse persistent stream buffer — eliminates per-frame Vec heap alloc.
        self.stream_buf.clear();
        self.stream_buf.reserve(stream_len);
        self.stream_buf.extend_from_slice(&frame_hdr);
        self.stream_buf.extend_from_slice(data);

        // ── Build data shards using persistent pool ───────────────────────────
        // The RTP header (bytes 0..16) and fecInfo (28..32) stay ZERO until
        // after parity is computed: Sunshine computes parity with those fields
        // zeroed and writes them afterwards, and moonlight-common-c regenerates
        // them on FEC-recovered packets. The NV_VIDEO_PACKET fields written
        // here ARE covered by parity, so recovery restores them.
        //
        // Grow pool to high watermark; zero-fill all shards in use this frame.
        // Avoids ~36 Vec alloc/dealloc cycles per frame at 60–120 Hz.
        while self.shard_pool.len() < total_shards {
            self.shard_pool.push(vec![0u8; block_size]);
        }
        for shard in self.shard_pool[..total_shards].iter_mut() {
            if shard.len() != block_size { shard.resize(block_size, 0); }
            shard.fill(0);
        }

        // Fill data shards — split borrow: stream_buf and shard_pool are
        // separate struct fields so the compiler allows concurrent access.
        let stream_buf = &self.stream_buf;
        for x in 0..data_shards {
            let shard = &mut self.shard_pool[x];

            // NV_VIDEO_PACKET (bytes 16..32, little-endian)
            let spi = self.packet_counter.wrapping_add(x as u32) << 8;
            shard[16..20].copy_from_slice(&spi.to_le_bytes());
            shard[20..24].copy_from_slice(&self.frame_index.to_le_bytes());
            let mut flags = FLAG_CONTAINS_PIC_DATA;
            if x == 0               { flags |= FLAG_SOF; }
            if x == data_shards - 1 { flags |= FLAG_EOF; }
            shard[24] = flags;
            // shard[25] = extraFlags (0); shard[27] = multiFecBlocks (0)
            shard[26] = 0x10; // multiFecFlags — Sunshine's constant

            let start = x * payload_per_packet;
            let end   = (start + payload_per_packet).min(stream_len);
            shard[HEADERS_SIZE..HEADERS_SIZE + (end - start)]
                .copy_from_slice(&stream_buf[start..end]);
        }

        // ── Reed-Solomon parity shards ───────────────────────────────────────
        if parity_shards > 0 {
            let rs = self.fec_cache
                .entry((data_shards, parity_shards))
                .or_insert_with(|| {
                    ReedSolomon::new(data_shards, parity_shards)
                        .expect("shard counts validated against GF(2^8) limit")
                });
            rs.encode(&mut self.shard_pool[..total_shards]).expect("all shards are block_size");
        }

        // ── Pacing gap — scaled to fps so overhead fits the frame budget ────
        // 300µs × (60 / fps): at 60fps = 300µs, at 120fps = 150µs.
        // A 50-packet IDR at 120fps produces 4 gaps = 600µs (<10% of 8.33ms)
        // vs the old 1200µs (>14%). Clamped 50–300µs so very high fps values
        // don't approach zero and very low fps values don't exceed 300µs.
        let pace_gap_us = (300u64 * 60 / self.fps.max(1) as u64).clamp(50, 300);
        let pace_gap = Duration::from_micros(pace_gap_us);

        // ── Finalize per-packet fields and send (data + parity) ─────────────
        // Split borrow: socket and shard_pool are separate fields.
        let socket = &self.socket;
        for x in 0..total_shards {
            let shard = &mut self.shard_pool[x];
            // RTP header — X bit set (Sunshine's FLAG_EXTENSION) → 4B reserved
            shard[0] = 0x80 | 0x10; // V=2, P=0, X=1, CC=0
            shard[1] = 0;           // no packetType/marker on video
            let seq = self.sequence_number.wrapping_add(x as u16);
            shard[2..4].copy_from_slice(&seq.to_be_bytes());
            shard[4..8].copy_from_slice(&self.timestamp.to_be_bytes());
            // ssrc (8..12) + reserved (12..16) stay 0

            // Post-parity overwrites on ALL shards, exactly like Sunshine's
            // post-encode loop: fecInfo, frameIndex, multiFecBlocks.
            // fecInfo: bits 4-11 = FEC %, 12-21 = shard index, 22-31 = data shards.
            let fec_info: u32 = ((fec_percentage as u32) << 4)
                | ((x as u32) << 12)
                | ((data_shards as u32) << 22);
            shard[28..32].copy_from_slice(&fec_info.to_le_bytes());
            shard[20..24].copy_from_slice(&self.frame_index.to_le_bytes());
            shard[27] = 0; // multiFecBlocks — single FEC block (block 0 of 1)

            send_packet(socket, shard, target);

            // Inter-batch pacing gap.
            if (x + 1) % PACE_BATCH_PACKETS == 0 && x + 1 < total_shards {
                spin_wait(pace_gap);
            }
        }

        self.sequence_number = self.sequence_number.wrapping_add(total_shards as u16);
        self.packet_counter  = self.packet_counter.wrapping_add(total_shards as u32);

        // Advance 90 kHz RTP clock by one frame period
        self.timestamp   = self.timestamp.wrapping_add(90000 / self.fps);
        self.frame_index = self.frame_index.wrapping_add(1);

        // ── Per-second send diagnostics ──────────────────────────────────────
        self.stat_frames      += 1;
        self.stat_data_pkts   += data_shards as u32;
        self.stat_parity_pkts += parity_shards as u32;
        self.stat_bytes       += (total_shards * block_size) as u64;
        if self.stat_window_start.elapsed() >= Duration::from_secs(1) {
            println!("📊 RTP/s: {} frames, {} data + {} parity pkts, fec={}%, {:.1} KB/s, pktsize={}",
                self.stat_frames, self.stat_data_pkts, self.stat_parity_pkts,
                self.fec_percentage, self.stat_bytes as f64 / 1024.0, self.packet_size);
            self.stat_frames       = 0;
            self.stat_data_pkts    = 0;
            self.stat_parity_pkts  = 0;
            self.stat_bytes        = 0;
            self.stat_window_start = Instant::now();
        }
    }
}

/// Human-readable NAL unit type name. `is_hevc` selects the HEVC or H.264 table.
fn nal_type_name(t: u8, is_hevc: bool) -> &'static str {
    if is_hevc {
        // HEVC nal_unit_type (ITU-T H.265 Table 7-1)
        match t {
            0 | 1  => "TRAIL",
            19     => "IDR_W_RADL",
            20     => "IDR_N_LP",
            21     => "CRA",
            32     => "VPS",
            33     => "SPS",
            34     => "PPS",
            35     => "AUD",
            39     => "SEI_PREFIX",
            40     => "SEI_SUFFIX",
            _      => "OTHER",
        }
    } else {
        // H.264 nal_unit_type (ITU-T H.264 Table 7-1)
        match t {
            1 => "P",
            5 => "IDR",
            6 => "SEI",
            7 => "SPS",
            8 => "PPS",
            9 => "AUD",
            _ => "OTHER",
        }
    }
}

/// List the NAL unit type for every Annex-B NAL unit in `data`, in order.
/// For H.264: `data[i+3] & 0x1F` (low 5 bits of the 1-byte NAL header).
/// For HEVC: `(data[i+3] >> 1) & 0x3F` (bits [6:1] of the first byte of the
/// 2-byte NAL header — `forbidden_zero_bit | nal_unit_type[5:0]`).
fn list_nal_types(data: &[u8], is_hevc: bool) -> Vec<u8> {
    let mut types = Vec::new();
    let mut i = 0;
    while i + 3 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let t = if is_hevc {
                (data[i + 3] >> 1) & 0x3F
            } else {
                data[i + 3] & 0x1F
            };
            types.push(t);
            i += 4;
        } else {
            i += 1;
        }
    }
    types
}

/// Scan every Annex-B NAL unit and return 2 (IDR/keyframe) or 1 (P-frame).
///
/// H.264: SPS (type 7) or IDR slice (type 5) → keyframe.
/// HEVC:  VPS (32), IDR_W_RADL (19), IDR_N_LP (20), or CRA (21) → keyframe.
///
/// NVENC with `NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS`
/// prefixes an IDR with AUD + VPS + SPS + PPS (HEVC) or AUD + SPS + PPS (H.264)
/// before the slice NAL, so we must scan all NALs rather than just the first.
/// The returned value goes in byte 3 of the NV_VIDEO_PACKET frame header.
pub(crate) fn detect_frame_type(data: &[u8], is_hevc: bool) -> u8 {
    let mut i = 0;
    while i + 3 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if is_hevc {
                let nal_type = (data[i + 3] >> 1) & 0x3F;
                // VPS(32), IDR_W_RADL(19), IDR_N_LP(20), CRA(21)
                if nal_type == 32 || nal_type == 19 || nal_type == 20 || nal_type == 21 {
                    return 2;
                }
            } else {
                let nal_type = data[i + 3] & 0x1F;
                // SPS(7) or IDR slice(5)
                if nal_type == 7 || nal_type == 5 {
                    return 2;
                }
            }
            i += 4;
        } else {
            i += 1;
        }
    }
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ping(sock: &UdpSocket, dst: std::net::SocketAddr, n: usize) {
        for _ in 0..n {
            sock.send_to(b"PING", dst).unwrap();
        }
        // Let loopback delivery land in the receiver buffer.
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    /// Reproduces the reconnect black-screen: session 1 leaves a backlog of
    /// pings in the receive buffer; after reset(), session 2 (new source port)
    /// must be learned — not a stale session-1 ping.
    fn loopback_addr(sender: &RtpSender) -> std::net::SocketAddr {
        // The sender binds 0.0.0.0 — rewrite to a sendable loopback address.
        let port = sender.socket.local_addr().unwrap().port();
        format!("127.0.0.1:{}", port).parse().unwrap()
    }

    #[test]
    fn reconnect_learns_new_port_not_stale_backlog() {
        let mut sender = RtpSender::new(0).expect("bind ephemeral");
        let dst = loopback_addr(&sender);
        let old = UdpSocket::bind("127.0.0.1:0").unwrap();
        let new = UdpSocket::bind("127.0.0.1:0").unwrap();

        ping(&old, dst, 20);
        assert_eq!(
            sender.try_learn_target().expect("learn session 1").port(),
            old.local_addr().unwrap().port()
        );

        // Pings keep arriving during the stream (these went unread pre-fix).
        ping(&old, dst, 20);

        sender.reset();
        assert!(sender.target.is_none());

        ping(&new, dst, 3);
        assert_eq!(
            sender.try_learn_target().expect("learn session 2").port(),
            new.local_addr().unwrap().port(),
            "reconnect must latch the NEW client port, not a stale buffered ping"
        );
    }

    /// Receive every datagram of one sent frame (data + parity shards).
    /// send_frame is synchronous, so after it returns everything is in the
    /// loopback receive buffer within the read timeout.
    fn recv_frame_datagrams(sock: &UdpSocket) -> Vec<Vec<u8>> {
        sock.set_read_timeout(Some(std::time::Duration::from_millis(250))).unwrap();
        let mut pkts = Vec::new();
        let mut buf = [0u8; 2048];
        while let Ok((n, _)) = sock.recv_from(&mut buf) {
            pkts.push(buf[..n].to_vec());
        }
        pkts
    }

    /// NV_VIDEO_PACKET.frameIndex of a video datagram (bytes 20..24, LE).
    fn frame_index_of(pkt: &[u8]) -> u32 {
        u32::from_le_bytes([pkt[20], pkt[21], pkt[22], pkt[23]])
    }

    /// The first transmitted frame MUST carry NV_VIDEO_PACKET.frameIndex == 1.
    /// moonlight-common-c initializes `nextFrameNumber = 1` and discards any
    /// frame 0 as stale; on a loss-free link the client never re-requests an
    /// IDR (`waitingForNextSuccessfulFrame` stays false), so a frame-0 opening
    /// IDR = permanent black screen + ML_ERROR_NO_VIDEO_FRAME ("reduce your
    /// bitrate") after 10 s. Sunshine starts at frame_nr = 1.
    #[test]
    fn first_frame_carries_frame_index_1_and_reset_restarts_at_1() {
        let mut sender = RtpSender::new(0).expect("bind ephemeral");
        let dst = loopback_addr(&sender);
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();

        ping(&client, dst, 1);
        sender.try_learn_target().expect("learn client");

        // Minimal H.264 Annex-B IDR payload (00 00 01 65 ...).
        let mut frame = vec![0u8, 0, 1, 0x65];
        frame.extend_from_slice(&[0xAA; 64]);

        sender.send_frame(&frame);
        let pkts = recv_frame_datagrams(&client);
        assert!(!pkts.is_empty(), "no datagrams received for frame 1");
        assert_eq!(frame_index_of(&pkts[0]), 1, "first frame must be index 1, not 0");
        assert_eq!(pkts[0][24] & FLAG_SOF, FLAG_SOF, "first data shard must carry SOF");

        sender.send_frame(&frame);
        let pkts = recv_frame_datagrams(&client);
        assert_eq!(frame_index_of(&pkts[0]), 2, "second frame must be index 2");

        // A new session must restart at 1 — the client's depacketizer is
        // reinitialized per connection and expects frame 1 again.
        sender.reset();
        ping(&client, dst, 1);
        sender.try_learn_target().expect("re-learn client after reset");
        sender.send_frame(&frame);
        let pkts = recv_frame_datagrams(&client);
        assert_eq!(frame_index_of(&pkts[0]), 1, "post-reset first frame must be index 1");
    }

    /// Draining must keep the most recent sender when pings from an old and a
    /// new source port are interleaved in the buffer.
    #[test]
    fn learn_target_keeps_latest_sender() {
        let mut sender = RtpSender::new(0).unwrap();
        let dst = loopback_addr(&sender);
        let a = UdpSocket::bind("127.0.0.1:0").unwrap();
        let b = UdpSocket::bind("127.0.0.1:0").unwrap();

        ping(&a, dst, 1);
        assert_eq!(sender.try_learn_target().unwrap().port(), a.local_addr().unwrap().port());

        a.send_to(b"PING", dst).unwrap();
        ping(&b, dst, 1);
        assert_eq!(
            sender.try_learn_target().unwrap().port(),
            b.local_addr().unwrap().port()
        );
    }
}
