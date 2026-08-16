use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::time::{Duration, Instant};

use reed_solomon_erasure::galois_8::ReedSolomon;
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
};

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
// batching (≤64KB batches at 80% of link rate). The gap is computed per
// frame in `TxEngine::send_frame` from the frame's shard count so the whole
// frame spreads across a fixed fraction of the frame interval — bounding the
// burst rate independently of frame size (heavy HEVC/HDR10 pan frames get
// more, closer-spaced batches instead of blasting the queue).
const PACE_BATCH_PACKETS: usize = 10;

/// Maximum encoded frames queued to the send thread at once. The capture
/// thread never blocks on the network: if the send thread falls this far
/// behind (link saturated / pacing budget exceeded for several consecutive
/// frames), `send_frame` refuses the frame and the caller must request an IDR
/// (a silently dropped frame would break the P-frame reference chain).
const MAX_FRAMES_IN_FLIGHT: usize = 3;

/// How long one datagram may wait for socket-buffer space before it is
/// dropped.
///
/// The socket is non-blocking with an 8 MB send buffer, so `WouldBlock` means
/// the NIC/stack is genuinely backed up — during Bobby's background-download
/// stress test, for instance. Retrying is right for a transient completion
/// delay; retrying *forever* (the previous behaviour) is not: it stalls the
/// `nova-rtp-send` thread inside one packet while the rest of the frame's
/// pacing budget burns, and it does so in a `yield_now` loop on a
/// THREAD_PRIORITY_HIGHEST thread. 1 ms is comfortably longer than any
/// plausible transient at these rates and still a fraction of the 8.33 ms
/// budget at 120 fps.
///
/// Dropping the odd shard is survivable by design — that is what the 20% RS
/// parity is for — and the client's resulting loss report now actually drives
/// the encoder down (see `lib.rs::qos_tick`), which is the correct answer to a
/// saturated link. Forcing an IDR from here would be wrong: it adds a large
/// frame to a link that just told us it is full.
const SEND_BLOCK_BUDGET: Duration = Duration::from_millis(1);

/// Send one UDP datagram. Retries briefly on `WouldBlock`, then gives up and
/// reports the drop via `dropped` (folded into the per-second RTP diagnostic,
/// so socket-buffer saturation is visible in the log rather than silent).
/// Free function (not a method) so callers can pass `&socket` and a shard
/// slice simultaneously without borrow-checker conflicts.
fn send_packet(socket: &UdpSocket, pkt: &[u8], target: SocketAddr, dropped: &mut u32) {
    let deadline = Instant::now() + SEND_BLOCK_BUDGET;
    loop {
        match socket.send_to(pkt, target) {
            Ok(_) => return,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    *dropped += 1;
                    return;
                }
                std::thread::yield_now();
            }
            Err(_) => return,
        }
    }
}

/// Busy-wait for sub-millisecond pacing gaps. `thread::sleep` is unusable
/// here: Windows sleep granularity is 1-15ms, which would stretch a 45-packet
/// IDR over tens of milliseconds and wreck 60fps frame pacing. Runs on the
/// dedicated send thread, so the spin never steals capture/encode budget.
fn spin_wait(d: Duration) {
    let end = Instant::now() + d;
    while Instant::now() < end {
        std::hint::spin_loop();
    }
}

/// Commands processed in order by the send thread. Frames use `try_send`
/// (dropped with an error signal when the sender is hopelessly behind);
/// control commands use blocking `send` (rare, must never be lost).
enum TxCmd {
    Frame { frame_index: u32, buf: Vec<u8>, frame_type: u8 },
    SetTarget(SocketAddr),
    /// Byte 0 of every outgoing datagram: `None` = Moonlight's RTP version
    /// byte, `Some(tag)` = an Echo demux tag. See `TxEngine::demux_tag`.
    SetDemuxTag(Option<u8>),
    SetFps(u32),
    SetCodec { is_hevc: bool, is_av1: bool },
    Configure { packet_size: usize, fec_percentage: usize, min_parity_shards: usize },
    Reset,
}

/// Capture-thread handle to the video RTP pipeline.
///
/// All heavy per-frame work — shard building, Reed-Solomon parity, pacing
/// gaps, and the `sendto` syscalls — happens on the dedicated `nova-rtp-send`
/// worker thread (Phase 15 micro-stutter fix). Previously `send_frame` ran
/// synchronously on the capture/encode thread, where a large HEVC/HDR10 pan
/// frame (60+ packets) cost 1.5–3 ms of spin-paced sending per frame — a
/// third of the 8.33 ms budget at 120 fps, and the single biggest rhythmic
/// jitter source for 10-bit streams.
///
/// The handle keeps ping-learning (`try_learn_target`) and session state on
/// the capture thread using a cloned socket handle (recv only); learned
/// targets are forwarded to the worker through the same ordered command
/// channel as frames, so a `Reset` can never race a stale target.
pub struct RtpSender {
    /// Cloned handle of the send socket — used ONLY to drain client "ping"
    /// packets on the capture thread (the worker owns the send side).
    recv_socket: UdpSocket,
    /// Capture-thread copy of the learned client address (session state).
    target: Option<SocketAddr>,
    /// True when the target was set explicitly (an Echo session's punched peer)
    /// rather than learned from inbound pings, in which case `try_learn_target`
    /// must not overwrite it. See [`pin_target`](Self::pin_target).
    pinned: bool,
    tx: SyncSender<TxCmd>,
    /// Frame payload buffers coming back from the worker for reuse.
    buf_rx: Receiver<Vec<u8>>,
    /// Recycled buffers ready for the next frame.
    spare_bufs: Vec<Vec<u8>>,
    /// Buffers currently owned by the worker (queued or being sent).
    bufs_in_flight: usize,
    /// Set while a newly-started Echo session is waiting for the Worker to
    /// finish applying its `ConfigureStart`; frames are dropped until then.
    ///
    /// Retargeting is instant but reconfiguration is not: `pin_target` makes an
    /// Echo peer the destination immediately, while the Worker still has to
    /// rebind capture and rebuild NVENC. Frames encoded with the *previous*
    /// session's settings are addressed to the new client in that window. Live
    /// 2026-08-15: a 4K HDR10 Main10 Moonlight session handing over to a
    /// 1080p SDR Echo session sent P010 frames to a decoder configured for 8-bit
    /// NV12 — "blinding light and smearing of blue", clearing only on reconnect
    /// (by which time the Worker had caught up).
    ///
    /// Holds the instant it was armed so the hold can **fail open** — a Worker
    /// that never reports a matching config must not silence the stream forever.
    config_hold: Option<Instant>,
    /// Address and arrival time of the most recent datagram of ANY kind seen on
    /// the media port.
    ///
    /// Echo's control tunnel needs this because a healthy Echo session is
    /// *silent on the control channel*: its keepalive is a raw `b"PING"` on the
    /// media socket, which classifies as `Other` and never reaches the control
    /// inbox. Judging tunnel liveness by control traffic alone therefore reaps
    /// sessions that are streaming perfectly — observed live 2026-08-15 as a
    /// black screen exactly 30 s in. See `echo::transport`'s idle sweep.
    last_rx: Option<(SocketAddr, Instant)>,
    /// Highest wire frame index queued this client session (0 = none yet).
    /// Master reads it to stamp `ConfigureStart::start_frame_index` so a
    /// replacement Worker adopted mid-session continues the client's frame
    /// timeline instead of restarting at 1 (which the client would discard
    /// forever — see ipc::ConfigureStart::start_frame_index).
    last_sent_index: u32,
    /// Where STUN datagrams arriving on the media port are delivered, when
    /// Echo's WAN layer has registered for them. `None` = no NAT traversal
    /// configured, and STUN packets are simply not treated as client pings.
    ///
    /// This exists because a NAT mapping belongs to a *socket*: the binding
    /// request that discovers Nova's public address has to leave from THIS
    /// socket, so its response arrives here, mixed in with the client's pings.
    /// See `echo::wan` for why the two can never be confused.
    stun_inbox: Option<tokio::sync::mpsc::UnboundedSender<(Vec<u8>, SocketAddr)>>,
    /// Where Echo's own control datagrams go — a separate inbox from
    /// `stun_inbox` because they have separate owners. The gatherer is the sole
    /// consumer of STUN (two consumers would race for each other's punch
    /// responses); the Echo transport is the sole consumer of control. Sharing
    /// one channel would put both in the same race.
    echo_inbox: Option<tokio::sync::mpsc::UnboundedSender<(Vec<u8>, SocketAddr)>>,
    /// Microphone datagrams, on their own channel rather than sharing
    /// `echo_inbox`.
    ///
    /// Input shares the control inbox because `echo::transport`'s loop can
    /// dispatch it in microseconds. Audio is different in kind: it is decoded
    /// and handed to a renderer, at fifty packets a second, forever. Putting
    /// that on the loop that also carries the reliable control tunnel and every
    /// input packet would make the microphone's cost part of input's latency
    /// budget — which is the shape of the bug that made the pointer lag half a
    /// second (one drain, two rates, and the slower job set the pace).
    mic_inbox: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,
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

        // Two handles to ONE socket: the capture thread drains pings (recv),
        // the worker sends datagrams. Concurrent recv/send on a UDP socket
        // from different threads is safe — they're independent operations.
        let recv_socket = socket.try_clone()?;

        let (tx, rx) = sync_channel::<TxCmd>(MAX_FRAMES_IN_FLIGHT + 5);
        let (buf_tx, buf_rx) = sync_channel::<Vec<u8>>(MAX_FRAMES_IN_FLIGHT);
        std::thread::Builder::new()
            .name("nova-rtp-send".into())
            .spawn(move || tx_worker(socket, rx, buf_tx))?;

        Ok(Self {
            recv_socket,
            target: None,
            pinned: false,
            tx,
            buf_rx,
            spare_bufs: Vec::new(),
            bufs_in_flight: 0,
            config_hold: None,
            last_rx: None,
            last_sent_index: 0,
            stun_inbox: None,
            echo_inbox: None,
            mic_inbox: None,
        })
    }

    /// Register a sink for STUN datagrams seen on the media port.
    ///
    /// Until this is called, [`try_learn_target`](Self::try_learn_target)
    /// merely declines to mistake STUN for a client ping — nothing else
    /// changes, so an install with no WAN configuration behaves exactly as
    /// before.
    pub fn set_stun_inbox(&mut self, tx: tokio::sync::mpsc::UnboundedSender<(Vec<u8>, SocketAddr)>) {
        self.stun_inbox = Some(tx);
    }

    /// Register a sink for Echo control datagrams seen on the media port.
    ///
    /// Same reasoning as [`set_stun_inbox`](Self::set_stun_inbox), one layer
    /// up: Echo's control channel has to share the media socket because that
    /// socket is the one with a punched hole through the NAT. Opening a second
    /// socket for control would mean a second mapping to punch, keep alive, and
    /// lose independently.
    pub fn set_echo_inbox(&mut self, tx: tokio::sync::mpsc::UnboundedSender<(Vec<u8>, SocketAddr)>) {
        self.echo_inbox = Some(tx);
    }

    /// Where microphone datagrams go. Unset until the renderer starts, which is
    /// what makes the microphone genuinely optional: with no VB-CABLE installed
    /// nothing sets this, and mic datagrams are classified and dropped for the
    /// cost of one byte comparison.
    ///
    /// The source address is deliberately **not** carried. Nothing downstream
    /// may make a decision from it — the session key is the entire owner check
    /// — so it is dropped here rather than passed along to be misused later.
    pub fn set_mic_inbox(&mut self, tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>) {
        self.mic_inbox = Some(tx);
    }

    /// Send a datagram from the **media socket**, so it carries the same
    /// source address the stream will.
    ///
    /// This is the whole point of routing NAT traversal through `RtpSender`
    /// rather than a socket of its own: a binding request sent from anywhere
    /// else discovers a mapping that belongs to that other socket and
    /// evaporates with it. The send side is owned by the `nova-rtp-send`
    /// worker thread, but `recv_socket` is a `try_clone` of the same
    /// underlying socket — concurrent send/recv from different threads is
    /// safe, and both share one 5-tuple, which is what makes the discovered
    /// mapping the correct one.
    ///
    /// Deliberately NOT a path for media: it bypasses the worker's pacing and
    /// FEC entirely. STUN-sized control datagrams only.
    pub fn send_raw(&self, data: &[u8], to: SocketAddr) -> std::io::Result<usize> {
        self.recv_socket.send_to(data, to)
    }

    /// Point the stream at an explicitly-known peer — an Echo session's
    /// punched address — and stop learning the target from inbound traffic
    /// until [`reset`](Self::reset) or [`unpin_target`](Self::unpin_target).
    ///
    /// ## Why pinning is required, and what race it closes
    ///
    /// GameStream has no way to tell Nova where to send video: the client's
    /// media port is ephemeral and NAT-translated, so `try_learn_target` infers
    /// it from whoever pings the media socket. That heuristic is correct for
    /// Moonlight and actively wrong for Echo, whose address is already *known*
    /// (the punch proved it) — and worse, the heuristic would fight the known
    /// value. Every subsequent datagram from any other source retargets the
    /// stream: a stale ping from the previous session still in the receive
    /// queue, a Moonlight client reconnecting, or simply an internet scanner
    /// touching port 47998, which now genuinely happens because the port is
    /// reachable from the WAN. The failure mode is not a crash but a silent
    /// misdelivery of the desktop to an arbitrary address.
    ///
    /// Pinning does NOT stop the drain: `try_learn_target` still empties the
    /// socket every tick (STUN demux depends on it, and an unread socket fills
    /// and drops the punch responses). It only stops the *adoption*.
    ///
    /// The retarget itself is race-free for a different reason, already built
    /// in: the send thread owns transmission, and `SetTarget` travels the same
    /// ordered channel as frames. A retarget therefore lands strictly between
    /// two frames, never mid-frame, so no frame can be split across two
    /// destinations no matter when this is called relative to the video loop.
    pub fn pin_target(&mut self, addr: SocketAddr) {
        self.target = Some(addr);
        self.pinned = true;
        // A pinned target IS an Echo session — the two are the same fact, so
        // the demux tag is set here rather than through a separate call a
        // future caller could forget to make.
        let _ = self.tx.send(TxCmd::SetDemuxTag(Some(nova_core::demux::ECHO_MEDIA)));
        let _ = self.tx.send(TxCmd::SetTarget(addr));
    }

    /// Resume learning the target from inbound pings (Moonlight's model).
    /// The current target is left in place; the next ping from elsewhere
    /// replaces it, exactly as before an Echo session pinned it.
    pub fn unpin_target(&mut self) {
        self.pinned = false;
    }

    /// Highest wire frame index queued this session; 0 when nothing has been
    /// sent since the last `reset()`. `+ 1` = the index the next Worker must
    /// start from.
    pub fn last_sent_index(&self) -> u32 {
        self.last_sent_index
    }

    /// Whether a client video target is currently known — i.e. frames sent now
    /// would actually go somewhere.
    ///
    /// Distinct from [`try_learn_target`], which reports only a CHANGE of
    /// address. Callers gating frame forwarding must use THIS: a Worker
    /// respawn mid-session (sign-in/sign-out handoff) leaves the client's
    /// address unchanged, so the change-detector returns `None` forever and a
    /// gate keyed on it never re-opens — every frame from the replacement
    /// Worker was silently dropped and the client froze until it gave up
    /// (confirmed live 2026-08-11: the ConfigureStart replay and the Worker's
    /// `worker configured` both succeeded, yet not one `📊 RTP/s` line
    /// followed).
    pub fn has_target(&self) -> bool {
        self.target.is_some()
    }

    /// Non-blocking drain of all queued "ping" packets, keeping the most recent
    /// sender. GameStream clients ping the video port every ~500ms for the WHOLE
    /// session (not just after PLAY), so this must be called every loop iteration
    /// — if the socket goes unread after the first learn, stale pings from a
    /// previous session pile up in the receive buffer and the next session
    /// latches onto a dead source port (black screen on reconnect).
    /// Returns the address only when it changes.
    pub fn try_learn_target(&mut self) -> Option<SocketAddr> {
        // 1500 rather than the original 64: a STUN binding response carrying
        // XOR-MAPPED-ADDRESS plus the SOFTWARE/alternate attributes real
        // servers add exceeds 64 bytes easily, and on Windows `recv_from` with
        // an undersized buffer FAILS the call (WSAEMSGSIZE) and discards the
        // datagram — which would also have ended this drain loop early and
        // left genuine pings sitting in the receive queue. An MTU-sized buffer
        // removes that failure mode for every datagram, not just STUN.
        let mut buf = [0u8; 1500];
        let mut latest = None;
        while let Ok((n, addr)) = self.recv_socket.recv_from(&mut buf) {
            // Liveness is recorded for EVERY datagram, before classification and
            // regardless of what it turns out to be — an Echo keepalive is a
            // raw `b"PING"` that classifies as `Other`, and Echo's control
            // tunnel judges its peer alive from exactly this.
            self.last_rx = Some((addr, Instant::now()));
            // STUN and RTP/ping traffic share this port. They are bitwise
            // unambiguous — STUN's leading two bits are always 00, RTP carries
            // version 2 (0x80) — and `is_stun_message` also checks the magic
            // cookie, so a client ping can never be mistaken for a binding
            // response or vice versa. See echo::wan::is_stun_message.
            match nova_core::demux::classify(&buf[..n]) {
                nova_core::demux::Class::Stun => {
                    if let Some(inbox) = &self.stun_inbox {
                        // Unbounded, non-blocking, and best-effort: a send
                        // failure means the WAN task is gone, which must never
                        // disturb the media path.
                        let _ = inbox.send((buf[..n].to_vec(), addr));
                    }
                    // Never a ping: latching the client target onto a STUN
                    // server's address would send the whole video stream to it.
                    continue;
                }
                // Input rides the same inbox as control. It is neither
                // acknowledged nor ordered (see `nova_core::input_channel`), so
                // it does not belong to the RUDP tunnel — `echo::transport`
                // splits the two by class on arrival.
                nova_core::demux::Class::EchoControl
                | nova_core::demux::Class::EchoControlAck
                | nova_core::demux::Class::EchoInput => {
                    if let Some(inbox) = &self.echo_inbox {
                        let _ = inbox.send((buf[..n].to_vec(), addr));
                    }
                    continue;
                }
                // Microphone audio goes to its own consumer, never to the
                // control inbox: `echo::transport`'s accept loop treats an
                // unrecognised datagram from a new address as the opening of a
                // TLS tunnel, so routing audio there would have every mic
                // packet from an unlatched peer attempt a handshake.
                //
                // With no renderer running the inbox is unset and the datagram
                // is dropped here — which is how a host with no VB-CABLE
                // installed pays nothing for a client that keeps sending.
                nova_core::demux::Class::EchoMic => {
                    if let Some(inbox) = &self.mic_inbox {
                        let _ = inbox.send(buf[..n].to_vec());
                    }
                    continue;
                }
                // Our own media echoed back by a misconfigured middlebox, or a
                // spoof. Either way it is not a client ping and must not become
                // the send target.
                nova_core::demux::Class::EchoMedia => continue,
                nova_core::demux::Class::Other => {}
            }
            latest = Some(addr);
        }
        let addr = latest?;
        // An Echo session's target came from a completed hole punch, not from
        // guessing at whoever pinged us. Draining above was still required
        // (STUN demux, and an unread socket drops punch traffic); adopting is
        // not. See `pin_target`.
        if self.pinned {
            return None;
        }
        if self.target != Some(addr) {
            self.target = Some(addr);
            let _ = self.tx.send(TxCmd::SetTarget(addr));
            return Some(addr);
        }
        None
    }

    /// Longest a stream may be held waiting for `WorkerConfigured`. Generous
    /// next to an observed reconfiguration (capture rebind + NVENC rebuild),
    /// but short enough that failing open costs a moment of bad picture rather
    /// than a dead session.
    const CONFIG_HOLD_MAX: Duration = Duration::from_secs(5);

    /// Drop frames until the Worker confirms it applied the new configuration.
    ///
    /// See [`config_hold`](Self::config_hold) for why. Dropping is the right
    /// response rather than buffering: these frames are encoded wrong for this
    /// client, so the best of them is worthless, and the client's own keyframe
    /// gate already expects to wait for an IDR.
    pub fn hold_until_configured(&mut self) {
        self.config_hold = Some(Instant::now());
    }

    /// Release a [`hold_until_configured`](Self::hold_until_configured) hold.
    ///
    /// The caller MUST request an IDR after this: with an infinite GOP the
    /// keyframe that followed reconfiguration may already have been dropped by
    /// the hold, and the next one only arrives on request — so without it the
    /// client waits forever on a keyframe that is never coming.
    pub fn release_config_hold(&mut self) -> bool {
        self.config_hold.take().is_some()
    }

    /// True while frames should be dropped. Fails open past
    /// [`CONFIG_HOLD_MAX`].
    fn config_held(&mut self) -> bool {
        match self.config_hold {
            Some(since) if since.elapsed() <= Self::CONFIG_HOLD_MAX => true,
            Some(_) => {
                println!(
                    "⚠️  RTP: worker never confirmed its configuration within {}s — \
                     releasing the hold and sending anyway",
                    Self::CONFIG_HOLD_MAX.as_secs()
                );
                self.config_hold = None;
                false
            }
            None => false,
        }
    }

    /// How long since a datagram of any kind arrived from `peer`, or `None` if
    /// the most recent one came from somewhere else.
    ///
    /// Deliberately reports only the single most recent sender rather than
    /// keeping a per-address table: the media port is internet-reachable, so a
    /// map keyed by source address is a memory footgun a scanner can pull. The
    /// one caller (Echo's tunnel sweep) asks about a peer that pings every
    /// 500 ms while its window of interest is 30 s, so "the last datagram was
    /// somebody else's" is both rare and safe — it only falls back to the
    /// stricter control-traffic test.
    pub fn idle_since(&self, peer: SocketAddr) -> Option<Duration> {
        match self.last_rx {
            Some((addr, at)) if addr == peer => Some(at.elapsed()),
            _ => None,
        }
    }

    pub fn set_fps(&mut self, fps: u32) {
        let _ = self.tx.send(TxCmd::SetFps(fps.max(1)));
    }

    /// Set the codec used for the worker's early-session NAL diagnostics.
    /// Frame-type classification itself is done by the CALLER (which already
    /// runs `detect_frame_type` for the first-IDR gate) and passed per frame —
    /// avoiding a second full scan of every payload.
    pub fn set_codec(&mut self, is_hevc: bool, is_av1: bool) {
        let _ = self.tx.send(TxCmd::SetCodec { is_hevc, is_av1 });
    }

    /// Apply per-session stream parameters. `packet_size` must be the client's
    /// negotiated x-nv-video[0].packetSize; `fec_percentage` 0 disables FEC.
    pub fn configure(&mut self, packet_size: usize, fec_percentage: usize, min_parity_shards: usize) {
        let _ = self.tx.send(TxCmd::Configure { packet_size, fec_percentage, min_parity_shards });
    }

    /// Drop per-session state so a future PLAY starts a clean stream
    /// (fresh sequence numbers/timestamps, and re-learn the client's
    /// ephemeral video source port via `try_learn_target`).
    pub fn reset(&mut self) {
        // Flush pings buffered from the ending session so the next learn
        // can't latch onto a stale source port. MTU-sized for the same reason
        // as try_learn_target's buffer — an undersized one errors instead of
        // draining on Windows, defeating the flush.
        let mut buf = [0u8; 1500];
        while self.recv_socket.recv_from(&mut buf).is_ok() {}
        self.target = None;
        // A pin belongs to the session that set it. Clearing it here means a
        // finished Echo session can never leave the sender deaf to the next
        // Moonlight client's pings — reset is the one call every session
        // teardown path already makes.
        self.pinned = false;
        self.last_sent_index = 0; // next session's client expects frame 1 again
        // Back to Moonlight's wire format: the next session may well be a
        // GameStream client, which would discard datagrams carrying an Echo
        // tag where it expects an RTP version.
        let _ = self.tx.send(TxCmd::SetDemuxTag(None));
        let _ = self.tx.send(TxCmd::Reset);
    }

    /// Queue one complete encoded frame (Annex-B NALs, or AV1 OBUs) for
    /// transmission. `frame_type` is the NV_VIDEO_PACKET classification the
    /// caller already computed: 2 = IDR/keyframe, 1 = P-frame.
    ///
    /// Returns `false` when the frame had to be DROPPED because the send
    /// thread is more than `MAX_FRAMES_IN_FLIGHT` frames behind — the caller
    /// MUST request an IDR in that case (the dropped frame breaks the P-frame
    /// reference chain). Never blocks the capture thread.
    pub fn send_frame(&mut self, frame_index: u32, data: &[u8], frame_type: u8) -> bool {
        if data.is_empty() || self.target.is_none() {
            return true; // nothing to send / nobody to send to — not a drop
        }

        // This frame was encoded for the PREVIOUS session's configuration and
        // would decode as garbage on the new client. Reported as sent, not as a
        // drop: a `false` here makes the caller request an IDR, and an IDR
        // encoded with the wrong configuration is exactly what we are trying to
        // keep off the wire.
        if self.config_held() {
            return true;
        }

        // Harvest buffers the worker has finished with.
        while let Ok(b) = self.buf_rx.try_recv() {
            self.bufs_in_flight -= 1;
            self.spare_bufs.push(b);
        }

        let mut buf = match self.spare_bufs.pop() {
            Some(b) => b,
            None if self.bufs_in_flight < MAX_FRAMES_IN_FLIGHT => Vec::new(),
            None => return false, // worker owns every buffer — it's ≥3 frames behind
        };
        buf.clear();
        buf.extend_from_slice(data);

        match self.tx.try_send(TxCmd::Frame { frame_index, buf, frame_type }) {
            Ok(()) => {
                self.bufs_in_flight += 1;
                // max(): the media keepalive retransmits an OLD IDR under its
                // original index — that must not walk the session's high-water
                // mark backwards.
                self.last_sent_index = self.last_sent_index.max(frame_index);
                true
            }
            Err(TrySendError::Full(TxCmd::Frame { buf, .. }))
            | Err(TrySendError::Disconnected(TxCmd::Frame { buf, .. })) => {
                self.spare_bufs.push(buf);
                false
            }
            Err(_) => false,
        }
    }
}

/// Send-thread entry: processes commands in order, returns frame buffers for
/// reuse. Exits when the capture-thread handle is dropped (channel closes).
fn tx_worker(socket: UdpSocket, rx: Receiver<TxCmd>, buf_tx: SyncSender<Vec<u8>>) {
    // HIGHEST (one notch below the capture thread's TIME_CRITICAL): the
    // pacing spins must not be preempted by background threads, but must
    // never starve capture/encode either.
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
    }

    let mut eng = TxEngine::new(socket);
    while let Ok(cmd) = rx.recv() {
        match cmd {
            TxCmd::Frame { frame_index, buf, frame_type } => {
                eng.send_frame(frame_index, &buf, frame_type);
                // Return the buffer for reuse. Capacity == MAX_FRAMES_IN_FLIGHT
                // so this can never be Full; a Disconnected handle just means
                // shutdown, where dropping the buffer is fine.
                let _ = buf_tx.try_send(buf);
            }
            TxCmd::SetTarget(addr) => eng.target = Some(addr),
            TxCmd::SetDemuxTag(tag) => eng.demux_tag = tag,
            TxCmd::SetFps(fps) => eng.fps = fps.max(1),
            TxCmd::SetCodec { is_hevc, is_av1 } => {
                eng.is_hevc = is_hevc;
                eng.is_av1 = is_av1;
            }
            TxCmd::Configure { packet_size, fec_percentage, min_parity_shards } => {
                eng.configure(packet_size, fec_percentage, min_parity_shards);
            }
            TxCmd::Reset => eng.reset(),
        }
    }
}

/// The actual packetizer/sender — lives entirely on the `nova-rtp-send`
/// thread; single-owner, no locks on any per-frame state.
struct TxEngine {
    /// Byte 0 of every datagram. `None` writes Moonlight's RTP version byte
    /// (`0x90`); `Some(tag)` writes an Echo demux tag instead.
    ///
    /// This costs nothing, which is the point. Byte 0 is written *after*
    /// Reed-Solomon parity is computed (parity runs with bytes 0..16 and 28..32
    /// zeroed — Sunshine's layout), so it is outside FEC coverage: changing it
    /// neither affects reconstruction nor is affected by it. And because the
    /// tag replaces a byte rather than being prepended, the datagram size, the
    /// shard size, and the MTU budget are all untouched. An Echo client is not
    /// speaking RTP to anything, so the version field it displaces has no
    /// reader. See `nova_core::demux` for why the tag values cannot collide
    /// with STUN or RTP.
    demux_tag: Option<u8>,
    socket: UdpSocket,
    sequence_number: u16,
    timestamp: u32,
    /// Client's video address, learned from its incoming "ping" packets by the
    /// capture-thread handle and forwarded via `TxCmd::SetTarget`.
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
    /// HEVC flag — only used for the early-session NAL-listing diagnostics
    /// (frame classification is computed by the caller and passed per frame).
    is_hevc: bool,
    /// AV1 flag — same diagnostics-only role as `is_hevc`.
    is_av1: bool,
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
    /// Datagrams abandoned this window because the socket send buffer stayed
    /// full past [`SEND_BLOCK_BUDGET`] — local uplink saturation, distinct from
    /// network-path loss (which the client reports instead).
    stat_buffer_drops: u32,
    stat_window_start: Instant,
    /// Persistent frame payload buffer — reused every frame to eliminate the
    /// per-call Vec heap allocation (frame_header ++ encoded_data).
    stream_buf: Vec<u8>,
    /// Persistent shard pool — grows to the high-watermark shard count (≈ 36
    /// for a typical 4K IDR at 20% FEC) and is reused/zeroed every frame,
    /// eliminating ~36 Vec alloc/dealloc cycles per frame at 60–120 Hz.
    shard_pool: Vec<Vec<u8>>,
}

impl TxEngine {
    fn new(socket: UdpSocket) -> Self {
        Self {
            socket,
            demux_tag: None,
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
            is_av1: false,
            stat_frames: 0,
            stat_data_pkts: 0,
            stat_parity_pkts: 0,
            stat_bytes: 0,
            stat_buffer_drops: 0,
            stat_window_start: Instant::now(),
            stream_buf: Vec::new(),
            shard_pool: Vec::new(),
        }
    }

    fn configure(&mut self, packet_size: usize, fec_percentage: usize, min_parity_shards: usize) {
        self.packet_size       = packet_size.clamp(512, 1392);
        self.fec_percentage    = fec_percentage.min(100);
        self.min_parity_shards = min_parity_shards.max(1);
        println!("📐 RTP configured: packetSize={} (datagram={}), fec={}%, minParity={}",
            self.packet_size, self.packet_size + MAX_RTP_HEADER_SIZE,
            self.fec_percentage, self.min_parity_shards);
    }

    fn reset(&mut self) {
        self.target          = None;
        self.sequence_number = 0;
        self.timestamp       = 0;
        self.packet_counter  = 0;
        self.frame_index     = 1; // Moonlight discards frame 0 — see field doc
        self.is_hevc         = false;
        self.is_av1          = false;
    }

    /// Send a complete encoded frame using the GameStream NV_VIDEO_PACKET wire format.
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
    fn send_frame(&mut self, frame_index: u32, data: &[u8], frame_type: u8) {
        if data.is_empty() { return; }
        let Some(target) = self.target else { return };

        // The wire frame index is chosen by the capture loop (lib.rs) and passed
        // in, NOT incremented here — it must equal the NVENC inputTimeStamp the
        // same frame was encoded with, so reference-frame invalidation can
        // target frames by the exact index the client references. A frame the
        // capture loop dropped after encode leaves a GAP in this sequence, which
        // the client detects as loss and recovers via an invalidation request.
        self.frame_index = frame_index;

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
            if self.is_av1 {
                println!("📦 frame {} : {} bytes, {} data + {} parity pkt(s), frame_type={} (AV1)",
                    self.frame_index, data.len(), data_shards, parity_shards, frame_type);
            } else {
                let nal_names: Vec<&str> = list_nal_types(data, self.is_hevc).iter().map(|t| nal_type_name(*t, self.is_hevc)).collect();
                println!("📦 frame {} : {} bytes, {} data + {} parity pkt(s), frame_type={}, NALs={:?}",
                    self.frame_index, data.len(), data_shards, parity_shards, frame_type, nal_names);
            }
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

        // ── Pacing gap — bound the burst rate independently of frame size ────
        // Spread the frame's batches across ≤40% of the frame interval: the
        // peak transmit rate is then ~2.5× the nominal stream bitrate no
        // matter how big the frame is. A massive HEVC/HDR10 pan frame or IDR
        // gets more, closer-spaced gaps (total send time grows but stays
        // bounded, and the send thread always finishes well before the next
        // frame arrives); small frames keep a gentle 300 µs ceiling per gap.
        // Clamped ≥40 µs so the gap never degenerates to a pure burst.
        let batches = (total_shards + PACE_BATCH_PACKETS - 1) / PACE_BATCH_PACKETS;
        let pace_gap = if batches > 1 {
            let frame_interval_us = 1_000_000u64 / self.fps.max(1) as u64;
            let budget_us = frame_interval_us * 2 / 5; // 40% of the frame budget
            Duration::from_micros((budget_us / batches as u64).clamp(40, 300))
        } else {
            Duration::ZERO
        };

        // ── Finalize per-packet fields and send (data + parity) ─────────────
        // Split borrow: socket and shard_pool are separate fields. `first_byte`
        // is hoisted for the same reason — reading `self.demux_tag` inside the
        // loop would overlap the mutable borrow of `self.shard_pool`.
        let first_byte = self.demux_tag.unwrap_or(0x80 | 0x10); // V=2, P=0, X=1, CC=0
        let socket = &self.socket;
        for x in 0..total_shards {
            let shard = &mut self.shard_pool[x];
            // RTP header — X bit set (Sunshine's FLAG_EXTENSION) → 4B reserved.
            // For an Echo session this same byte carries the demux tag; see
            // `TxEngine::demux_tag` for why that is free rather than a
            // compromise.
            shard[0] = first_byte;
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

            send_packet(socket, shard, target, &mut self.stat_buffer_drops);

            // Inter-batch pacing gap.
            if !pace_gap.is_zero()
                && (x + 1) % PACE_BATCH_PACKETS == 0
                && x + 1 < total_shards
            {
                spin_wait(pace_gap);
            }
        }

        self.sequence_number = self.sequence_number.wrapping_add(total_shards as u16);
        self.packet_counter  = self.packet_counter.wrapping_add(total_shards as u32);

        // Advance 90 kHz RTP clock by one frame period. frame_index is NOT
        // advanced here — the capture loop owns it (see the note at the top of
        // this function) and supplies it per frame.
        self.timestamp   = self.timestamp.wrapping_add(90000 / self.fps);

        // ── Per-second send diagnostics ──────────────────────────────────────
        self.stat_frames      += 1;
        self.stat_data_pkts   += data_shards as u32;
        self.stat_parity_pkts += parity_shards as u32;
        self.stat_bytes       += (total_shards * block_size) as u64;
        if self.stat_window_start.elapsed() >= Duration::from_secs(1) {
            // stat_buffer_drops is appended only when non-zero: it means the
            // socket send buffer stayed full past SEND_BLOCK_BUDGET, i.e. the
            // local link — not the network path — is the bottleneck. A steady
            // stream of these alongside client loss reports is the signature
            // of a saturated uplink rather than a lossy one.
            let drops = if self.stat_buffer_drops > 0 {
                format!(", {} pkt(s) dropped (socket buffer full)", self.stat_buffer_drops)
            } else {
                String::new()
            };
            println!("📊 RTP/s: {} frames, {} data + {} parity pkts, fec={}%, {:.1} KB/s, pktsize={}{}",
                self.stat_frames, self.stat_data_pkts, self.stat_parity_pkts,
                self.fec_percentage, self.stat_bytes as f64 / 1024.0, self.packet_size, drops);
            self.stat_frames       = 0;
            self.stat_data_pkts    = 0;
            self.stat_parity_pkts  = 0;
            self.stat_bytes        = 0;
            self.stat_buffer_drops = 0;
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
/// The returned value goes in byte 3 of the NV_VIDEO_PACKET frame header
/// (passed into `RtpSender::send_frame` by the caller, which needs the
/// classification anyway for its first-IDR gate — one scan per frame total).
pub(crate) fn detect_frame_type(data: &[u8], is_hevc: bool, is_av1: bool) -> u8 {
    if is_av1 {
        return if av1_is_keyframe(data) { 2 } else { 1 };
    }
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

/// AV1 keyframe detection over the low-overhead OBU bitstream NVENC emits.
///
/// A key frame's temporal unit carries an `OBU_SEQUENCE_HEADER` (type 1) — NVENC
/// emits the sequence header only with key frames (the AV1 analogue of
/// SPS/VPS-on-IDR), so its presence marks the keyframe. We walk OBUs by their
/// `obu_size` LEB128 field (NVENC sets `obu_has_size_field`); if an OBU lacks a
/// size field we can't advance past it, but the sequence header sits at the
/// front of a key temporal unit so it is seen before that matters.
fn av1_is_keyframe(data: &[u8]) -> bool {
    const OBU_SEQUENCE_HEADER: u8 = 1;
    let mut i = 0usize;
    while i < data.len() {
        let hdr = data[i];
        let obu_type = (hdr >> 3) & 0x0F;
        let ext_flag = (hdr >> 2) & 1;
        let has_size = (hdr >> 1) & 1;
        i += 1;
        if ext_flag == 1 {
            i += 1; // temporal/spatial id byte
        }
        if obu_type == OBU_SEQUENCE_HEADER {
            return true;
        }
        if has_size == 1 {
            let (size, consumed) = read_leb128(&data[i..]);
            i += consumed + size as usize;
        } else {
            break; // no size field — cannot walk further
        }
    }
    false
}

/// Read an unsigned LEB128 (AV1 `leb128()`), returning `(value, bytes_read)`.
fn read_leb128(data: &[u8]) -> (u64, usize) {
    let mut value = 0u64;
    let mut i = 0;
    while i < data.len() && i < 8 {
        let b = data[i];
        value |= ((b & 0x7F) as u64) << (i * 7);
        i += 1;
        if b & 0x80 == 0 {
            break;
        }
    }
    (value, i)
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
        let port = sender.recv_socket.local_addr().unwrap().port();
        format!("127.0.0.1:{}", port).parse().unwrap()
    }

    /// A granted Echo session is silent on its control tunnel — its only
    /// heartbeat is a raw `b"PING"` on the media socket, which classifies as
    /// `Other`. Echo's tunnel sweep asks `idle_since` whether the peer is alive,
    /// so a pinned target (the state an Echo session puts this sender in) must
    /// still record liveness even though it refuses to *adopt* the sender.
    ///
    /// Live regression: judging tunnel liveness by control traffic alone reaped
    /// a perfectly working stream at exactly 30 s (2026-08-15).
    #[test]
    fn a_ping_from_a_pinned_echo_peer_still_counts_as_liveness() {
        let mut sender = RtpSender::new(0).expect("bind ephemeral");
        let dst = loopback_addr(&sender);
        let echo_peer = UdpSocket::bind("127.0.0.1:0").unwrap();
        let peer_addr = echo_peer.local_addr().unwrap();

        // An Echo session pins the target to its punched peer.
        sender.pin_target(peer_addr);

        assert!(
            sender.idle_since(peer_addr).is_none(),
            "nothing heard yet, so there is no liveness to report"
        );

        ping(&echo_peer, dst, 3);
        assert!(
            sender.try_learn_target().is_none(),
            "a pinned sender must never re-adopt a target from inbound pings"
        );

        let idle = sender
            .idle_since(peer_addr)
            .expect("the peer's ping must register as liveness despite the pin");
        assert!(
            idle < Duration::from_secs(5),
            "just-received ping should read as ~0s idle, got {idle:?}"
        );

        let stranger: std::net::SocketAddr = "127.0.0.1:9".parse().unwrap();
        assert!(
            sender.idle_since(stranger).is_none(),
            "liveness must be attributed to the sender we actually heard from"
        );
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
    /// Transmission happens on the nova-rtp-send worker thread; the 250 ms
    /// read timeout comfortably covers command-channel handoff + send.
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
    fn wire_frame_index_is_exactly_what_the_caller_supplies() {
        // The capture loop (lib.rs) now owns the wire frame index — it must
        // equal the NVENC inputTimeStamp the same frame was encoded with, for
        // reference-frame invalidation. This test locks that the RTP layer puts
        // EXACTLY the supplied index on the wire (no internal renumbering), and
        // that a caller-created GAP (a frame dropped after encode) appears as a
        // gap on the wire so the client detects the loss. The Phase 13 "start
        // at 1" invariant now lives in the capture loop, not here.
        let mut sender = RtpSender::new(0).expect("bind ephemeral");
        let dst = loopback_addr(&sender);
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();

        ping(&client, dst, 1);
        sender.try_learn_target().expect("learn client");

        // Minimal H.264 Annex-B IDR payload (00 00 01 65 ...).
        let mut frame = vec![0u8, 0, 1, 0x65];
        frame.extend_from_slice(&[0xAA; 64]);
        let ft = detect_frame_type(&frame, false, false);

        assert!(sender.send_frame(1, &frame, ft), "frame 1 must be accepted");
        let pkts = recv_frame_datagrams(&client);
        assert!(!pkts.is_empty(), "no datagrams received for frame 1");
        assert_eq!(frame_index_of(&pkts[0]), 1, "wire index must be the supplied 1");
        assert_eq!(pkts[0][24] & FLAG_SOF, FLAG_SOF, "first data shard must carry SOF");

        // Skip index 2 (a dropped-after-encode frame) — the wire must show 3.
        assert!(sender.send_frame(3, &frame, ft), "frame 3 must be accepted");
        let pkts = recv_frame_datagrams(&client);
        assert_eq!(frame_index_of(&pkts[0]), 3, "wire must carry the supplied 3 (gap at 2)");
    }

    /// AV1 keyframe detection: an OBU stream containing a sequence header (type
    /// 1) is an IDR (2); one without is a P-frame (1). Locks the OBU walk +
    /// LEB128 size parsing that marks AV1 frames for Moonlight.
    #[test]
    fn av1_sequence_header_is_detected_as_idr() {
        // temporal delimiter (type 2, size 0) + sequence header (type 1, size 4).
        let key = [0x12, 0x00, 0x0A, 0x04, 0x11, 0x22, 0x33, 0x44];
        assert_eq!(detect_frame_type(&key, false, true), 2, "seq header ⇒ IDR");

        // temporal delimiter + frame OBU (type 6, size 3) — no sequence header.
        let inter = [0x12, 0x00, 0x32, 0x03, 0xAA, 0xBB, 0xCC];
        assert_eq!(detect_frame_type(&inter, false, true), 1, "no seq header ⇒ P");
    }

    /// `last_sent_index` is the session's high-water mark: Master stamps
    /// `ConfigureStart::start_frame_index = last_sent_index + 1` so a Worker
    /// adopted mid-session continues the client's frame timeline instead of
    /// restarting at 1 (which moonlight-common-c discards forever — the
    /// permanent-black-after-sign-out/upgrade bug, live 2026-08-10). The
    /// keepalive retransmit of an OLD IDR must not walk it backwards, and
    /// `reset()` must return it to 0 so a fresh session starts at 1 again.
    /// Retargeting is instant; the Worker's reconfiguration is not. Frames
    /// encoded for the previous session must not reach the new client in that
    /// window — live 2026-08-15, a 4K HDR10 Main10 session handing over to a
    /// 1080p SDR Echo client produced "blinding light and smearing of blue"
    /// (P010 decoded as 8-bit NV12).
    #[test]
    fn frames_encoded_before_the_worker_reconfigured_are_not_sent() {
        let mut sender = RtpSender::new(0).expect("bind ephemeral");
        let peer: std::net::SocketAddr = "127.0.0.1:9".parse().unwrap();
        sender.pin_target(peer);
        sender.hold_until_configured();

        // A stale-config IDR, exactly the frame that used to poison the decoder.
        assert!(
            sender.send_frame(1, &[0u8; 256], 2),
            "a held frame must not report as a drop — a drop makes the caller \
             request an IDR, which would be encoded with the same wrong config"
        );
        assert_eq!(
            sender.last_sent_index(),
            0,
            "nothing may reach the wire while the worker is still reconfiguring"
        );

        assert!(sender.release_config_hold(), "the hold was armed, so releasing reports true");
        assert!(
            !sender.release_config_hold(),
            "releasing twice must report false so the caller only requests one IDR"
        );

        assert!(sender.send_frame(2, &[0u8; 256], 2));
        assert_eq!(
            sender.last_sent_index(),
            2,
            "frames flow again once the worker has confirmed its configuration"
        );
    }

    /// A Worker that never confirms must cost a moment of bad picture, not a
    /// permanently silent stream.
    #[test]
    fn an_unconfirmed_configuration_hold_fails_open() {
        let mut sender = RtpSender::new(0).expect("bind ephemeral");
        let peer: std::net::SocketAddr = "127.0.0.1:9".parse().unwrap();
        sender.pin_target(peer);

        // Arm the hold in the past, beyond the ceiling.
        sender.config_hold =
            Some(Instant::now() - RtpSender::CONFIG_HOLD_MAX - Duration::from_secs(1));

        assert!(sender.send_frame(7, &[0u8; 256], 2));
        assert_eq!(
            sender.last_sent_index(),
            7,
            "an expired hold must release itself and let frames through"
        );
        assert!(
            sender.config_hold.is_none(),
            "the expired hold is cleared, not re-evaluated on every frame"
        );
    }

    #[test]
    fn last_sent_index_tracks_high_water_mark_and_resets() {
        let mut sender = RtpSender::new(0).expect("bind ephemeral");
        let dst = loopback_addr(&sender);
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        ping(&client, dst, 1);
        sender.try_learn_target().expect("learn client");

        let mut frame = vec![0u8, 0, 1, 0x65];
        frame.extend_from_slice(&[0xAA; 64]);
        let ft = detect_frame_type(&frame, false, false);

        assert_eq!(sender.last_sent_index(), 0, "nothing sent yet");
        assert!(sender.send_frame(41, &frame, ft));
        let _ = recv_frame_datagrams(&client);
        assert_eq!(sender.last_sent_index(), 41);

        // Keepalive retransmit of an older IDR: no regression.
        assert!(sender.send_frame(7, &frame, ft));
        let _ = recv_frame_datagrams(&client);
        assert_eq!(sender.last_sent_index(), 41, "old-index retransmit must not regress");

        sender.reset();
        assert_eq!(sender.last_sent_index(), 0, "fresh session must restart at index 1");
    }

    /// The Echo WAN demux hook. Two properties matter, and the second is the
    /// dangerous one: a STUN datagram arriving on the media port must be
    /// handed to the WAN layer, and must NEVER be mistaken for a client ping —
    /// latching the video target onto a STUN server's address would aim the
    /// entire stream at Google.
    #[test]
    fn stun_on_the_media_port_is_demuxed_and_never_learned_as_the_client() {
        let mut sender = RtpSender::new(0).expect("bind ephemeral");
        let dst = loopback_addr(&sender);
        let (stun_tx, mut stun_rx) = tokio::sync::mpsc::unbounded_channel();
        sender.set_stun_inbox(stun_tx);

        // A "STUN server" and a real client, both sending to the media port.
        let stun_server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        let client_port = client.local_addr().unwrap().port();

        let txid = crate::echo::wan::new_transaction_id();
        let binding = crate::echo::wan::build_binding_request(&txid);
        stun_server.send_to(&binding, dst).unwrap();
        ping(&client, dst, 1);
        std::thread::sleep(std::time::Duration::from_millis(50));

        let learned = sender.try_learn_target().expect("the client must still be learned");
        assert_eq!(learned.port(), client_port, "target must be the client, not the STUN peer");

        let (payload, from) = stun_rx.try_recv().expect("STUN datagram routed to the WAN inbox");
        assert!(crate::echo::wan::is_stun_message(&payload));
        assert_eq!(from.port(), stun_server.local_addr().unwrap().port());
        assert!(stun_rx.try_recv().is_err(), "exactly one STUN datagram");
    }

    /// With no WAN layer registered (every install today), STUN traffic is
    /// still refused as a ping rather than corrupting the target — the hook
    /// must be inert, not merely optional.
    #[test]
    fn stun_is_ignored_as_a_ping_even_with_no_inbox_registered() {
        let mut sender = RtpSender::new(0).expect("bind ephemeral");
        let dst = loopback_addr(&sender);
        let stun_server = UdpSocket::bind("127.0.0.1:0").unwrap();

        stun_server
            .send_to(&crate::echo::wan::build_binding_request(&[7u8; 12]), dst)
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));

        assert!(sender.try_learn_target().is_none(), "STUN alone must learn nothing");
        assert!(!sender.has_target());
    }

    /// The frame-forwarding gate must key on "a target is KNOWN"
    /// (`has_target`), never on "the target just CHANGED"
    /// (`try_learn_target`). A Worker respawn mid-session re-adopts a client
    /// that never went away, so its address never changes again — a gate keyed
    /// on the change-detector stays shut forever and every frame from the
    /// replacement Worker is dropped (the live 2026-08-11 sign-in freeze).
    #[test]
    fn target_stays_known_after_the_learning_edge_passes() {
        let mut sender = RtpSender::new(0).expect("bind ephemeral");
        let dst = loopback_addr(&sender);
        let client = UdpSocket::bind("127.0.0.1:0").unwrap();

        assert!(!sender.has_target(), "nothing learned yet");

        ping(&client, dst, 1);
        assert!(sender.try_learn_target().is_some(), "first ping is the learning edge");
        assert!(sender.has_target());

        // Same client keeps pinging: no CHANGE, so the edge detector goes quiet
        // — but the target is still perfectly well known.
        ping(&client, dst, 1);
        assert!(sender.try_learn_target().is_none(), "no change ⇒ no edge");
        assert!(sender.has_target(), "…yet frames must still be forwardable");

        // A genuine session end clears it, so the next session re-learns.
        sender.reset();
        assert!(!sender.has_target());
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

    /// The Echo retargeting guard. An Echo session's peer is *known* from the
    /// punch, so ping-learning must not be allowed to steal the stream — and
    /// the thief here is not hypothetical: port 47998 is reachable from the
    /// WAN once a pinhole is open, and internet scanners hit it (observed live
    /// on 8443 during the tether test). Learning from one would aim the
    /// desktop at a stranger.
    ///
    /// The drain itself must keep running while pinned, or STUN keepalives
    /// stop being demuxed and the receive buffer fills — so this also checks
    /// that a STUN datagram still reaches the WAN inbox during a pinned
    /// session.
    #[test]
    fn a_pinned_target_ignores_ping_learning_but_still_drains() {
        let mut sender = RtpSender::new(0).expect("bind ephemeral");
        let dst = loopback_addr(&sender);
        let (stun_tx, mut stun_rx) = tokio::sync::mpsc::unbounded_channel();
        sender.set_stun_inbox(stun_tx);

        let echo_peer: SocketAddr = "203.0.113.9:50000".parse().unwrap();
        sender.pin_target(echo_peer);
        assert!(sender.has_target());

        // An interloper pings the media port, and a STUN keepalive arrives.
        let stranger = UdpSocket::bind("127.0.0.1:0").unwrap();
        let stun_server = UdpSocket::bind("127.0.0.1:0").unwrap();
        ping(&stranger, dst, 1);
        stun_server
            .send_to(&crate::echo::wan::build_binding_request(&[3u8; 12]), dst)
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));

        assert!(sender.try_learn_target().is_none(), "a pinned session learns nothing");
        assert!(sender.has_target(), "…and keeps the punched peer");
        assert!(
            stun_rx.try_recv().is_ok(),
            "the socket must still be drained while pinned, or STUN demux dies"
        );

        // Session teardown returns the sender to Moonlight's learning model.
        sender.reset();
        assert!(!sender.has_target());
        ping(&stranger, dst, 1);
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(
            sender.try_learn_target().is_some(),
            "after reset the next client must be learnable again"
        );
    }
}
