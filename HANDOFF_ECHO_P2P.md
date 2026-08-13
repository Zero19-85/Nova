# Master Handoff Blueprint — Echo P2P (2026-08-13)

Read this before touching anything under `nova-core/`, `echo-client/`, or
`nova-server/src/echo/`. `CLAUDE.md` still describes Nova as a single-crate
project; that is stale — the repo is a Cargo workspace now (see §1).

---

## 0. Status at a glance

| Component | State |
|---|---|
| `nova-server` | Streaming pipeline unchanged and healthy. Echo host surfaces built: RPC, session manager, WAN transport, media sealing. Release-built, **not deployed**. |
| `nova-core` | New shared crate: `stun`, `punch`, `demux`, `rudp`, `media_crypto`, `identity`, `envelope`, `relay`. |
| `echo-client` | Headless CLI: `id`, `probe`, `connect`, `stream`. Punches, opens the TLS tunnel, receives + decrypts + FEC-repairs frames. **No decoder.** |
| `nova-relay` | Dev signaling relay. **Process killed**, port 8443 forward closed, `nova.toml` restored from `.bak`. Code intact; rerun when needed. |
| Tests | **141 passing** (12 echo-client, 51 nova-core, 78 nova-server), 8 ignored. |
| Live validation | **None of the Echo P2P work has run against real hardware yet.** The cross-internet punch was validated 2026-08-13; everything layered on top of it is code-complete and unproven live. |

### ⚠️ Everything is uncommitted

`git status` shows the entire workspace refactor plus all Echo work as
unstaged/untracked on `main`. HEAD is still `2af45bb`. **Commit early in the
next session** — a lost working tree here costs days.

### Deployment note (user's standing rule)

The live install is `C:\Program Files\Nova Server`. Hot-patching is normal, but
if Nova is killed it must be brought back in the *same* command — the user
streams from their bedroom. Current binaries in `target/release/` are newer than
what is deployed; nothing in this batch has been pushed to the live install.

---

## 1. Workspace layout

```
Cargo.toml            workspace root — [profile.release] only works here
nova-core/            shared by BOTH peers, no Windows, no host state
nova-server/          the host: capture, NVENC, VDD, pairing, GameStream, Echo host
echo-client/          Nova's native client (headless CLI today, Android next)
nova-relay/           dev-only signaling relay
```

The split line is **"shared between two peers" vs "belongs to one"** — *not*
"move `echo::` into a library". `echo::rpc` is a server that reads Nova's
pairing trust store; `echo::wan`'s gatherer drives `rtp.rs`'s socket. Both are
host-bound by nature.

---

## 2. The architecture, exactly

### 2.1 Zero-Byte Demuxer (`nova-core/src/demux.rs`)

One punched UDP socket carries five things. Byte 0 classifies all of them:

| Kind | Discriminator |
|---|---|
| STUN | `buf[0] & 0xC0 == 0` **and** magic cookie `0x2112A442` at `[4..8]` |
| Moonlight RTP | `0x90` (RTP v2 + extension bit) |
| Moonlight ping | ASCII `PING` (`0x50…`) |
| Echo media | `0xE0` (`ECHO_MEDIA`) |
| Echo control | `0xE1` / `0xE2` (`ECHO_CONTROL`, `ECHO_CONTROL_ACK`) |

**Why it costs zero bytes.** The tag is not prepended — it is written *into*
byte 0, the slot that holds the RTP version. Two facts make that free:

1. `0xC0..=0xFF` is unreachable for STUN (`0x00..=0x3F`) and RTP v2
   (`0x80..=0xBF`). Asserted in `demux::tests::tags_cannot_collide_*`.
2. `rtp.rs` writes byte 0 **after** Reed-Solomon parity is computed. Parity runs
   with bytes `0..16` and `28..32` zeroed (Sunshine's layout), so byte 0 is
   outside FEC coverage — the tag neither affects reconstruction nor is
   affected by it.

An Echo client is not speaking RTP to anything, so the displaced version field
has no reader. Datagram size, shard size, and MTU budget are all untouched.

Host side: `rtp::TxEngine::demux_tag`, set by `RtpSender::pin_target()` and
cleared by `reset()` (a pinned target *is* an Echo session — one fact, so one
call site).

### 2.2 RUDP (`nova-core/src/rudp.rs`)

Media tolerates loss; session state does not. A lost `stop_session` leaves the
host holding a pipeline nobody is watching, which blocks the next client —
including a Moonlight one.

Wire format:

```
DATA:  [0xE1][flags][seq u32 BE][payload …]
ACK:   [0xE2][flags][seq u32 BE]
```

- Reliable, **ordered**, exactly-once delivery. Sequence starts at 1.
- `MAX_PAYLOAD = 1200`, `HEADER_LEN = 6` → fits a 1400-byte MTU unfragmented.
  Oversized messages are **refused, not fragmented**.
- `MAX_IN_FLIGHT = 8` (bounded window), `MAX_BACKLOG = 64` chunks in the driver.
- Retry `150 ms` doubling to `1200 ms`, `MAX_ATTEMPTS = 8` (~8 s) then
  `PeerUnresponsive`.
- **Every received DATA is ACKed, including duplicates** — a duplicate usually
  means our previous ACK was what got lost.
- Transport-agnostic: consumes and produces datagrams, owns no socket (same
  reason `punch` takes a trait). Host rides `rtp.rs`'s socket; client owns its
  own.

`RudpStream` + `drive()` layer an `AsyncRead + AsyncWrite` byte stream on top.
Writes chunk to `MAX_PAYLOAD` and never block. `poll_flush` returns immediately
— delivery is guaranteed by retransmission, and blocking until every byte were
ACKed would stall TLS mid-handshake for a full round trip on every flush.

### 2.3 Whole-frame AES-128-GCM (`nova-core/src/media_crypto.rs`)

Sealed **before sharding**:

```
encode → SEAL (whole frame) → shard → FEC parity → wire
wire → FEC reconstruct → reassemble → OPEN → decoder
```

- One 16-byte tag per **frame**, not per packet. It can never enlarge a
  datagram; it can only make a frame need at most one more shard.
- Parity is computed over ciphertext, so the client repairs loss **without the
  key** and authenticates once, after the frame is whole.
- Nonce = `salt(4) ‖ stream_id(4 BE) ‖ counter(4 BE)`. For video the counter is
  the wire frame index. Key is fresh per session ⇒ no reuse.
  `media_crypto::tests::nonce_uniqueness` asserts it.
- AAD = `stream ‖ counter ‖ frame_type`, so a frame cannot be replayed at
  another index or relabelled P→IDR.
- **The frame index travels in the clear** (NV_VIDEO_PACKET header) — required,
  because the receiver needs it to derive the nonce, and encrypting it would
  make loss recovery undecryptable.

Host entry point: `EchoSession::seal_video` via
`SessionManager::seal_video(index, type, frame) -> Option<Vec<u8>>`, called in
`lib.rs::media_supervisor` immediately before `rtp_sender.send_frame`. Moonlight
pays one relaxed atomic load (`echo_active`), never the session mutex.

The keepalive-IDR retransmit caches **plaintext** and re-seals under the same
index — caching ciphertext would be a nonce replay.

### 2.4 TLS-over-RUDP security model

The threat: port 48011's protection was never TCP, it was mutual TLS against the
pairing trust store, refusing a peer before a single command byte was read. Raw
UDP would replace that with a **spoofable source address**, on an
internet-reachable socket, guarding commands that reconfigure displays and
retarget media on a **LocalSystem** service.

```
NDJSON commands      ← identical to the LAN port, same Handler
rustls mutual TLS    ← identical trust store, identical certificates
RudpStream           ← reliable ordered byte stream
demux tag 0xE1/0xE2  ← shares the media socket
punched UDP path
```

**Zero new cryptography.** Rejected alternatives: a bespoke challenge-response
(hand-rolled crypto in front of a SYSTEM command surface) and a relay-carried
key (makes the relay able to impersonate either peer, defeating P2P).

Host authenticates the client by cert fingerprint in `nova_paired.json`
(`rpc::authorize` + `pairing::trusted_device_name`, read per connection so a tray
revoke takes effect immediately). Client authenticates the host by **pin** —
Nova's cert is self-signed and its identity *is* its fingerprint, the same value
used for the relay lookup. There is no CA and no hostname worth verifying (the
address came from a punch, not DNS), so pinning is stronger than name
validation, not a weaker substitute.

**One `Handler`, two doors** (`rpc::build_handler`): the LAN TCP listener and
the WAN tunnel share it, so a command — and the anti-hijack gate — cannot behave
differently by route.

### 2.5 Session manager and the anti-hijack gate (`nova-server/src/echo/session.rs`)

- A punch proves **reachability, not entitlement**. The latched peer is recorded
  by `echo::wan`, never installed into `RtpSender` by the puncher.
- `SessionManager::start` refusal order: validate → **Moonlight live ⇒
  `MoonlightActive`** → another Echo device ⇒ `HeldByAnotherDevice` (same device
  ⇒ clean restart) → no latched path ⇒ `NoPathLatched` → `plane.begin`.
  Every refusal happens *before* anything is retargeted, so a denied request
  leaves the pipeline byte-identical.
- **Bidirectional**: `lib.rs::session_watcher` defers a Moonlight PLAY while
  `echo_holds_media()`. Only one direction was asked for; without the other,
  Moonlight silently steals a live Echo session.
- `WorkerMediaPlane::begin` order is load-bearing: `reset()` (clears pin, wire
  index, stale pings) → `configure`/`set_fps`/`set_codec` → `pin_target()` last.
  All are ordered commands on the send thread's channel, so a retarget lands
  **between frames** by construction — that was never a lock problem.
- **`RtpSender::pin_target` / `unpin_target`**: the real hazard was
  `try_learn_target` *adopting* whatever pings 47998 (a stale ping, a
  reconnecting Moonlight client, an internet scanner — 47998 is WAN-reachable
  once punched). Pinned sessions still **drain** the socket (STUN demux needs
  it) but never adopt. `reset()` clears the pin.
- Control tunnel closing **releases the session** (`transport.rs` →
  owner-checked `sessions.stop()`), or a vanished WAN client holds the pipeline
  forever.

### 2.6 Client FEC (`echo-client/src/receiver.rs`)

Two details that are silent corruption if wrong:

1. **Re-zero bytes `0..16` and `28..32` on every received shard** before
   `reconstruct`, and store the **whole datagram**, not just the payload —
   parity was computed over the full block with those ranges zeroed.
2. **The parity count is not transmitted.** Derive
   `parity = ceil(data_shards × fec_pct / 100)`. This inverts the host's
   minimum-parity adjustment exactly, because the host recomputes the percentage
   when it raises parity to the floor (`rtp.rs` `send_frame`).

Both sides must pin the same `reed-solomon-erasure` major version — a different
generator matrix reconstructs confidently **wrong** bytes rather than failing.

Also: the punched socket must have **exactly one reader**
(`receiver::demultiplex`); `run_receiver` consumes a channel. Two `recv_from`
tasks would race for each other's datagrams.

Client keepalive `PING`s are **gated on a granted session** — an unsolicited
datagram on the host's media port is exactly what `rtp.rs` learns a Moonlight
target from, so pinging pre-session could redirect someone else's live stream.

---

## 3. Connection sequence (what `echo stream` does)

1. Bind the UDP socket that will carry everything (mapping belongs to the
   socket — never discover on one and stream on another).
2. STUN gather + classify (`nova_core::stun`).
3. Relay `lookup` (host candidates) → relay `offer` (ours). Relay auth is mTLS;
   host is pinned by SHA-256.
4. Simultaneous-open punch (`nova_core::punch`, 25 ms rounds). Host initiates
   from the relay offer via `GatherHandle::punch_toward`.
5. Start `receiver::demultiplex` (before the tunnel — TLS needs its datagrams).
6. `ControlChannel::connect_wan` → TLS handshake over RUDP.
7. `hello` → check `capabilities.sessions` → `start_session`.
8. Grant returns `peer`, geometry, `media_key` (hex), `rikey`/`rikeyid`.
9. `run_receiver` — FEC repair → reassemble → GCM open → `FrameSink`.
10. `stop_session`; host also releases on tunnel close.

---

## 4. Immediate next steps (in order)

### Step 0 — Commit. Everything is untracked. Do this first.

### Step 1 — Live tether validation of the P2P stack (before Android)

Nothing above has run against real hardware. Restart `nova-relay`, restore the
`[echo.signaling]` block in the live `nova.toml`, redeploy `nova-server.exe` +
`nova_shim.dll`, then from a tethered machine:

```
echo stream --relay https://<wan-ip>:8443/v1/signal --relay-pin <fp> \
            --host <nova-fp> --seconds 30
```

Expect: punch → `🔐 Control tunnel authenticated` → `🎬 Session N granted` →
`🎞️ frame …` lines. Watch for `frames_recovered_by_fec > 0` (FEC working) and
`frames_failed_auth == 0` (sealing correct). This is the **first time frames are
sealed in flight** — a mismatch shows up as authentication failures, not a
picture glitch.

Host log lines to grep: `🛡️ Echo WAN control ready`, `🔗 Echo WAN: control
tunnel opening`, `🔓 Echo WAN: … authenticated as`, `🎬 Echo session N started`.

Requires port 8443 forwarded again for the relay (only the relay — the media and
control paths need nothing).

### Step 2 — Echo Android client (the user's chosen first target)

`nova-core` was built to compile for Android. What must be verified/decided:

- **Cross-compilation**: `aarch64-linux-android`. `rustls`/`aws-lc-rs` needs the
  NDK toolchain; if it fights, `rustls` with the `ring` provider is the
  fallback. `reed-solomon-erasure` and `aes-gcm` are pure Rust and should be
  clean. `nova-core` has **no Windows dependencies** — keep it that way.
- **Shape**: JNI library driving the existing `nova-core` + a port of
  `echo-client/src/{receiver,control}.rs`, with the Rust side owning the socket,
  the tunnel, and reassembly, handing `DecodedFrame` up to `MediaCodec`.
- **Decoder**: `MediaCodec` in async mode, `configure` with the negotiated
  codec/geometry from the grant. `DecodedFrame.data` is Annex-B NALs (H.264/HEVC)
  or OBUs (AV1) — feed IDR-first (the depacketiser already refuses to open a
  stream on a P-frame? *No* — it does not; the client must wait for
  `is_keyframe()` before feeding the decoder. **This gate does not exist yet.**)
- **Identity**: Android must generate + persist its own cert/key
  (`Identity::load_or_create` in app-private storage) and be paired via Nova's
  normal PIN flow. It does **not** read `nova_paired.json`.
- **Doze/background**: the 500 ms keepalive and the 25 s STUN keepalive both
  need a wake path, or the NAT mapping lapses when the screen turns off.

### Step 3 — Known gaps, deliberately open

- **Audio is not received by Echo.** The grant carries `rikey`/`rikeyid` and the
  host sends GameStream AES-CBC audio on 48000 — a separate socket, *not*
  punched. Either move audio onto the punched socket under `ECHO_MEDIA` with
  `STREAM_AUDIO`, or punch a second mapping. Recommend the former.
- **Input is not sent by Echo.** No path exists client→host yet.
- **Step 3 "hot format change" is still FROZEN.** `hello` advertises
  `hot_format_change: false`. Do not flip it until both the Worker apply path
  and a client decoder-rebuild handshake exist.
- **Multi-seat** is N capture pipelines, not a targeting change —
  `DesktopManager` owns one D3D11 device per process, and consumer GeForce caps
  concurrent NVENC sessions.
- **No keyframe gate on the client** (see Step 2) — feeding `MediaCodec` a
  P-frame first produces garbage until the next IDR.
- The white-flash artifact on display change remains unexplained (all three
  candidate sites were already guarded).

---

## 5. File map for the Echo work

| Path | Role |
|---|---|
| `nova-core/src/demux.rs` | Byte-0 classification, tag constants |
| `nova-core/src/rudp.rs` | Reliable ordered messages + `RudpStream` + `drive()` |
| `nova-core/src/media_crypto.rs` | AES-128-GCM whole-frame seal/open |
| `nova-core/src/punch.rs` | Simultaneous open over a `PunchIo` trait |
| `nova-core/src/stun.rs` | RFC 8489 codec, mapping classification |
| `nova-core/src/identity.rs` | Cert identity, pinning, mTLS configs |
| `nova-server/src/echo/rpc.rs` | Command surface, `Handler`, LAN listener, `is_lan_peer` |
| `nova-server/src/echo/session.rs` | `SessionManager`, anti-hijack gate, `WorkerMediaPlane` |
| `nova-server/src/echo/transport.rs` | WAN tunnel: TLS over RUDP on the media socket |
| `nova-server/src/echo/wan.rs` | STUN gathering, 25 s keepalive, host-side punch |
| `nova-server/src/echo/signaling.rs` | Relay long-poll client |
| `nova-server/src/rtp.rs` | `pin_target`, demux tag, `set_echo_inbox`, `send_raw` |
| `echo-client/src/receiver.rs` | Depacketise, FEC repair, GCM open, demultiplex |
| `echo-client/src/control.rs` | `connect_wan` (tunnel) / `connect_lan` (debug) |

## 6. Tests that encode the contracts

- `rudp::mutual_tls_completes_over_a_lossy_rudp_link` — real rustls handshake +
  intact peer cert over a link dropping every 7th/5th datagram.
- `echo::transport::the_command_surface_answers_over_a_lossy_tunnel_and_the_gate_still_holds`
  — `hello`, `moonlight_active` refusal, then a grant with usable keys.
- `receiver::a_repaired_frame_still_authenticates` — FEC + GCM composed.
- `receiver::a_lost_data_shard_is_rebuilt_from_parity`.
- `session::a_live_moonlight_session_blocks_the_handoff_without_touching_the_pipeline`.
- `session::echo_datagrams_fit_the_wan_mtu` — 1040 B vs a 1400 B budget.
- `rtp::a_pinned_target_ignores_ping_learning_but_still_drains`.
- `media_crypto::nonce_uniqueness`, `…_replayed_at_the_wrong_index_or_type_is_refused`.

If one of these breaks, the contract it names is the thing that broke.
