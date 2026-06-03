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

    pub fn send_nal(&mut self, nal: &[u8]) {
        if nal.len() < 4 { return; }

        const MAX_PAYLOAD: usize = 1200;

        if nal.len() <= MAX_PAYLOAD {
            self.send_single(nal);
        } else {
            self.send_fragmented(nal);
        }

        self.timestamp = self.timestamp.wrapping_add(3600);
    }

    fn send_single(&mut self, nal: &[u8]) {
        let mut pkt = Vec::with_capacity(12 + nal.len());
        self.write_rtp_header(&mut pkt);
        pkt.extend_from_slice(nal);
        let _ = self.socket.send(&pkt);
        self.sequence_number = self.sequence_number.wrapping_add(1);
    }

    fn send_fragmented(&mut self, nal: &[u8]) {
        let nal_header = nal[0];
        let mut offset = 1;

        while offset < nal.len() {
            let remaining = nal.len() - offset;
            let chunk_size = remaining.min(1200);
            let is_start = offset == 1;
            let is_end = remaining <= 1200;

            let mut pkt = Vec::with_capacity(12 + 2 + chunk_size);
            self.write_rtp_header(&mut pkt);

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

    fn write_rtp_header(&self, buf: &mut Vec<u8>) {
        buf.push(0x80);
        buf.push(96);
        buf.extend_from_slice(&self.sequence_number.to_be_bytes());
        buf.extend_from_slice(&self.timestamp.to_be_bytes());
        buf.extend_from_slice(&self.ssrc.to_be_bytes());
    }
}