use std::net::UdpSocket;

pub struct RtpSender {
    socket: UdpSocket,
    sequence_number: u16,
    timestamp: u32,
    ssrc: u32,
    current_ip: String,
    current_port: u16,
}

impl RtpSender {
    pub fn new(bind_port: u16, target_ip: &str, target_port: u16) -> std::io::Result<Self> {
        let socket = UdpSocket::bind(("0.0.0.0", bind_port))?;
        socket.connect((target_ip, target_port))?;

        Ok(Self {
            socket,
            sequence_number: 0,
            timestamp: 0,
            ssrc: 0x12345678,
            current_ip: target_ip.to_string(),
            current_port: target_port,
        })
    }

    /// Only reconnects if the target actually changed
    pub fn update_target_if_changed(&mut self, ip: &str, port: u16) -> std::io::Result<()> {
        if self.current_ip != ip || self.current_port != port {
            self.current_ip = ip.to_string();
            self.current_port = port;
            self.socket.connect((ip, port))?;
        }
        Ok(())
    }

    // We added `is_last_nal_in_frame` to signal Moonlight to draw the frame
    pub fn send_nal(&mut self, nal: &[u8], is_last_nal_in_frame: bool) {
        if nal.len() < 1 { return; } // Allowed short NALs like PPS/SPS

        const MAX_PAYLOAD: usize = 1200;

        if nal.len() <= MAX_PAYLOAD {
            self.send_single(nal, is_last_nal_in_frame);
        } else {
            self.send_fragmented(nal, is_last_nal_in_frame);
        }

        // Only increment the RTP timestamp AFTER a full frame is sent
        if is_last_nal_in_frame {
            self.timestamp = self.timestamp.wrapping_add(90000 / 60); // Assuming 60 FPS clock
        }
    }

    fn send_single(&mut self, nal: &[u8], marker: bool) {
        let mut pkt = Vec::with_capacity(12 + nal.len());
        self.write_rtp_header(&mut pkt, marker);
        pkt.extend_from_slice(nal);
        let _ = self.socket.send(&pkt);
        self.sequence_number = self.sequence_number.wrapping_add(1);
    }

    fn send_fragmented(&mut self, nal: &[u8], marker_for_frame: bool) {
        let nal_header = nal[0];
        let mut offset = 1;

        while offset < nal.len() {
            let remaining = nal.len() - offset;
            let chunk_size = remaining.min(1200);
            let is_start = offset == 1;
            let is_end = remaining <= 1200;

            let mut pkt = Vec::with_capacity(14 + chunk_size);
            // Only set the RTP marker bit if this is the absolute last fragment of the last NAL
            let set_marker = is_end && marker_for_frame;
            self.write_rtp_header(&mut pkt, set_marker);

            let fu_indicator = (nal_header & 0xE0) | 28;
            let mut fu_header = nal_header & 0x1F;
            if is_start { fu_header |= 0x80; }
            if is_end   { fu_header |= 0x40; }

            pkt.push(fu_indicator);
            pkt.push(fu_header);
            pkt.extend_from_slice(&nal[offset..offset + chunk_size]);

            let _ = self.socket.send(&pkt);
            self.sequence_number = self.sequence_number.wrapping_add(1);
            offset += chunk_size;
        }
    }

    fn write_rtp_header(&self, buf: &mut Vec<u8>, marker: bool) {
        buf.push(0x80); // V=2, P=0, X=0, CC=0
        // If marker is true, set the highest bit (0x80). Payload type is 96.
        let pt = if marker { 0x80 | 96 } else { 96 };
        buf.push(pt); 
        buf.extend_from_slice(&self.sequence_number.to_be_bytes());
        buf.extend_from_slice(&self.timestamp.to_be_bytes());
        buf.extend_from_slice(&self.ssrc.to_be_bytes());
    }
}