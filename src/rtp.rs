use std::net::{SocketAddr, UdpSocket};

// NV_VIDEO_PACKET flags (from moonlight-common-c VideoDepacketizer.c)
const FLAG_CONTAINS_PIC_DATA: u8 = 0x01;
const FLAG_EOF:                u8 = 0x02;
const FLAG_SOF:                u8 = 0x04;

// Max H.264 payload bytes per UDP datagram (conservative, well under 1500-byte MTU)
const MAX_PAYLOAD: usize = 1200;
// 8-byte frame header for appversion 7.1.415–7.1.445: first byte 0x01 → frameHeaderSize=8
const FRAME_HEADER_SIZE: usize = 8;

pub struct RtpSender {
    socket: UdpSocket,
    sequence_number: u16,
    timestamp: u32,
    ssrc: u32,
    /// Client's video address, learned from its first incoming "ping" packet —
    /// the client's source port is ephemeral and cannot be known in advance.
    target: Option<SocketAddr>,
    /// Global packet counter — lower 24 bits stored in NV_VIDEO_PACKET.streamPacketIndex
    packet_counter: u32,
    /// Per-frame counter stored in NV_VIDEO_PACKET.frameIndex
    frame_index: u32,
    fps: u32,
}

impl RtpSender {
    pub fn new(bind_port: u16) -> std::io::Result<Self> {
        let socket = UdpSocket::bind(("0.0.0.0", bind_port))?;
        socket.set_nonblocking(true)?;
        Ok(Self {
            socket,
            sequence_number: 0,
            timestamp: 0,
            ssrc: 0x12345678,
            target: None,
            packet_counter: 0,
            frame_index: 0,
            fps: 60,
        })
    }

    /// Non-blocking poll for an incoming "ping" packet from the client. GameStream
    /// clients send these to the video/audio ports right after PLAY so the server
    /// can learn the client's NAT-mapped source port. Returns the learned address
    /// the first time it changes.
    pub fn try_learn_target(&mut self) -> Option<SocketAddr> {
        let mut buf = [0u8; 64];
        match self.socket.recv_from(&mut buf) {
            Ok((_n, addr)) => {
                if self.target != Some(addr) {
                    self.target = Some(addr);
                    return Some(addr);
                }
                None
            }
            Err(_) => None,
        }
    }

    pub fn set_fps(&mut self, fps: u32) {
        self.fps = fps.max(1);
    }

    /// Send a complete H.264 Annex-B frame using the GameStream NV_VIDEO_PACKET wire format.
    ///
    /// Packet layout per UDP datagram:
    ///   [RTP header 12 B] + [NV_VIDEO_PACKET 16 B] + [frame header 8 B (first pkt only)] + [H.264 data]
    ///
    /// The frame header first byte = 0x01 tells Moonlight frameHeaderSize=8 for
    /// appversion 7.1.415–7.1.445 (which includes our advertised 7.1.431.0).
    pub fn send_frame(&mut self, data: &[u8]) {
        if data.is_empty() { return; }
        let Some(target) = self.target else { return };

        let frame_type = detect_frame_type(data);
        let chunks: Vec<&[u8]> = data.chunks(MAX_PAYLOAD).collect();
        let total = chunks.len();

        for (i, chunk) in chunks.iter().enumerate() {
            let is_first = i == 0;
            let is_last  = i == total - 1;

            let mut flags = FLAG_CONTAINS_PIC_DATA;
            if is_first { flags |= FLAG_SOF; }
            if is_last  { flags |= FLAG_EOF; }

            let frame_hdr_bytes = if is_first { FRAME_HEADER_SIZE } else { 0 };
            let mut pkt = Vec::with_capacity(12 + 16 + frame_hdr_bytes + chunk.len());

            // ── RTP header (12 bytes, RFC 3550) ──────────────────────────────
            pkt.push(0x80); // V=2, P=0, X=0, CC=0
            // Marker bit on the last packet of each frame (tells decoder to render)
            pkt.push((if is_last { 0x80 } else { 0x00 }) | 96); // PT=96
            pkt.extend_from_slice(&self.sequence_number.to_be_bytes());
            pkt.extend_from_slice(&self.timestamp.to_be_bytes());
            pkt.extend_from_slice(&self.ssrc.to_be_bytes());

            // ── NV_VIDEO_PACKET header (16 bytes, little-endian) ──────────────
            // streamPacketIndex: packet counter occupies bits 8-31; byte 0 = FEC type (0=data)
            let spi: u32 = (self.packet_counter & 0x00FF_FFFF) << 8;
            pkt.extend_from_slice(&spi.to_le_bytes());
            pkt.extend_from_slice(&self.frame_index.to_le_bytes());
            pkt.push(flags);
            pkt.push(0); // extraFlags (LTR indicators — unused)
            pkt.push(0); // multiFecFlags
            pkt.push(0); // multiFecBlocks
            pkt.extend_from_slice(&[0u8; 4]); // fecInfo

            // ── Frame header (8 bytes, first packet of each frame only) ──────
            // byte 0 = 0x01 (selects frameHeaderSize=8), bytes 1-2 = processing
            // latency (unused, 0), byte 3 = frame type (1=P, 2=IDR), 4-7 unused.
            if is_first {
                pkt.push(0x01);
                pkt.extend_from_slice(&[0u8; 2]);
                pkt.push(frame_type);
                pkt.extend_from_slice(&[0u8; 4]);
            }

            // ── H.264 Annex-B data ─────────────────────────────────────────────
            pkt.extend_from_slice(chunk);

            let _ = self.socket.send_to(&pkt, target);
            self.sequence_number = self.sequence_number.wrapping_add(1);
            self.packet_counter  = self.packet_counter.wrapping_add(1);
        }

        // Advance 90 kHz RTP clock by one frame period
        self.timestamp   = self.timestamp.wrapping_add(90000 / self.fps);
        self.frame_index = self.frame_index.wrapping_add(1);
    }
}

/// Scan every NAL unit in an Annex-B H.264 access unit and classify the
/// frame: if any NAL is SPS (type 7) or an IDR slice (type 5), this is a
/// keyframe. NVENC often prefixes the first frame with an AUD (type 9)
/// before SPS/PPS/IDR, so we can't just look at the first NAL.
/// Returned value goes in byte 3 of the NV_VIDEO_PACKET frame header
/// (1 = P-frame, 2 = IDR frame).
fn detect_frame_type(data: &[u8]) -> u8 {
    let mut i = 0;
    while i + 3 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let nal_type = data[i + 3] & 0x1F;
            if nal_type == 7 || nal_type == 5 {
                return 2;
            }
            i += 4;
        } else {
            i += 1;
        }
    }
    1
}
