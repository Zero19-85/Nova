# Echo Android — Phase 1 Build Plan (2026-08-13)

Companion to `HANDOFF_ECHO_P2P.md`, which remains the authority on the protocol.
This document covers only the Android target: toolchain, crate restructure, and
the Rust↔Kotlin boundary.

---

## 0. Verified starting state

Checked against the tree at HEAD `5f8045e`, not assumed:

| Fact | Consequence |
|---|---|
| `nova-core` and `echo-client` contain **zero** platform code (only comments name Windows) | Both cross-compile as-is, modulo the crypto provider below |
| `echo-client` is **binary-only** — `main.rs` with `mod control; mod receiver;` | An `.so` cannot depend on it. This is the first blocker. |
| `identity.rs:128` hardcodes `rustls::crypto::aws_lc_rs::default_provider()` | The one dependency that will fight the NDK. Second blocker. |
| `receiver.rs` already exposes `trait FrameSink` and `DecodedFrame::is_keyframe()` | The bridge seam already exists; the keyframe *gate* still does not. |
| `rcgen::KeyPair::generate()` defaults to ECDSA P-256 | `ring` can generate it — a provider swap does not change cert format, so existing pairings survive. |
| Dev box has JDK 25 only — no SDK, NDK, cmake, ninja, cargo-ndk, adb, gradle | Section 1 is a from-scratch install. |
| `echo id` prints a fingerprint and tells the human to pair it **manually** | There is no client-side pairing implementation. Third blocker — see §5. |

---

## 1. Toolchain

### 1.1 Rust targets

```powershell
rustup target add aarch64-linux-android   # every real device
rustup target add x86_64-linux-android    # emulator on this x86 box
```

Deliberately **not** `armv7-linux-androideabi` / `i686-linux-android`. A 32-bit
device cannot sustain the decode this client exists to do, and each extra ABI is
another full cross-build plus APK weight.

### 1.2 NDK

Install **NDK r27 (LTS) or newer** via Android Studio's SDK Manager, set
`ANDROID_NDK_HOME`, and build against **API 26** (`minSdk 26`).

r27 specifically because 16 KB page alignment became the default there. Play has
required it for apps targeting Android 15+ since November 2025; on r26 you would
have to carry `-C link-arg=-Wl,-z,max-page-size=16384` by hand and would find out
it was missing at store-upload time.

Also needed: `cargo install cargo-ndk`, plus **cmake and ninja** *only if* you keep
aws-lc-rs (see below). The ring path needs neither.

### 1.3 The crypto provider — the real cross-compilation decision

`aws-lc-sys` builds C and assembly through cmake and needs bindgen on targets
without prebuilt bindings. A Windows-host → Android cross build is exactly where
that costs a day. `ring` is **already in the dependency graph** (pulled by
`rcgen`, `rustls-webpki`, `x509-parser`) and cross-compiles with nothing but the
NDK clang.

Make it a feature, do not switch it globally — the host build is shipping and
should not move:

```toml
# nova-core/Cargo.toml
[features]
default   = ["aws-lc-rs"]                        # host: byte-identical to today
aws-lc-rs = ["rustls/aws-lc-rs", "rcgen/aws_lc_rs"]
ring      = ["rustls/ring", "rcgen/ring"]        # Android

[dependencies]
rustls = { workspace = true, default-features = false, features = ["std", "tls12", "logging"] }
rcgen  = { workspace = true, default-features = false, features = ["pem"] }
```

`provider_algs()` becomes cfg-selected, and — this part is load-bearing —
`echo-android`'s `nativeInit` must call `CryptoProvider::install_default()`
explicitly rather than relying on the process default. rustls 0.23 panics at
runtime if two providers are compiled in and none is installed. Verify with
`cargo tree -e features -p nova-core --target aarch64-linux-android`.

Everything else is clean: `aes-gcm`, `sha2`, `serde`, `hex`, `tokio`, `hyper` are
pure Rust; `reed-solomon-erasure` is too **as long as `simd-accel` stays off** —
and it must stay off anyway, because §2.6 of the P2P handoff requires both peers
to reconstruct with an identical generator matrix.

---

## 2. Workspace restructure

```
Cargo.toml                    members += "echo-android"

echo-client/
  src/lib.rs      NEW   pub mod control; pub mod receiver; pub mod session;
  src/session.rs  NEW   the connect sequence, lifted verbatim out of main.rs
  src/gate.rs     NEW   keyframe gate + bounded frame queue (closes a known gap)
  src/main.rs     thin CLI over the lib — no logic of its own

echo-android/     NEW   crate-type = ["cdylib"]
  src/lib.rs            JNI surface only, nothing else
  src/handle.rs         opaque session handle, runtime ownership

android/          NEW   Gradle project (outside the cargo workspace)
```

Two properties this buys:

- **The CLI and the Android app run identical library code.** A desktop LAN test
  exercises everything except JNI and MediaCodec, which is the mitigation for
  going to Android before the live tether validation in P2P handoff §4 Step 1.
- **`echo-android` stays a workspace member**, so `cargo check` on this Windows
  box type-checks the JNI code every build. The `jni` crate is pure Rust and
  links nothing — it only ever receives a `JNIEnv` pointer — so it compiles on
  msvc perfectly well.

`nova-core` gains nothing and changes only its feature table. The client-shared
code belongs in `echo-client`'s lib, not core — core's line is "shared between
two *peers*", and a depacketiser is one peer's business. This also keeps the
eventual decoupling clean: the `echo-*` crates move to their own repo together,
with `nova-core` as the single vendored boundary.

---

## 3. Bridge: `jni`, not uniffi

**Recommendation: hand-rolled `jni` (0.21), kept deliberately thin.**

Four reasons, all specific to this project rather than general preference:

1. **uniffi cannot express the hot path.** Frames must land in a MediaCodec input
   buffer with no intermediate allocation. uniffi marshals values — a `Vec<u8>`
   becomes a Java `byte[]`, so every frame costs an allocation, a copy, and an
   array pin, and there is no opt-out. At 120 fps that is the wrong default.
2. **The surface is eight functions.** uniffi pays off across a large, evolving,
   multi-language API. Echo has one consumer language and a tiny surface.
3. **The control plane is already NDJSON.** `nova_core::envelope` and
   `ControlChannel::call` speak `serde_json::Value`, so control crosses JNI as a
   **JSON string** and stays in sync with the host *by construction* — it is
   literally the host's wire format. uniffi would have you redeclare those shapes
   and then maintain two definitions of each.
4. No `uniffi-bindgen` step to wire into Gradle.

The honest cost: hand-written `unsafe` and JNI signatures, where a typo is a
runtime `UnsatisfiedLinkError` rather than a compile error. Contain it —

- every `extern "C"` function ≤20 lines: parse args, call a *safe* Rust function,
  marshal the result;
- `catch_unwind` at each boundary, so a Rust panic throws a Java exception
  instead of aborting the VM;
- one host-target test that loads the library and calls each entry point.

### 3.1 Pull, never push — the decision that matters most

**Kotlin pulls frames from Rust. Rust never calls up into Kotlin.**

A push callback would need `AttachCurrentThread` plus a `GlobalRef` on the Rust
receive thread for every frame, and it inverts control so the decoder's
backpressure cannot reach the network layer. With pull, the Kotlin feeder thread
blocks *inside* Rust; the bounded queue **is** the backpressure signal, and the
drop policy lives in Rust where the keyframe flag already is. It also means the
bridge never needs a `JavaVM` handle at all.

The zero-copy shape: Kotlin dequeues a MediaCodec input buffer — already a direct
`ByteBuffer` — and passes it down. Rust writes the reassembled frame straight
into it via `get_direct_buffer_address`. One copy, no allocation, no thread
attach.

### 3.2 The surface

```
nativeInit()                                     → void      logger, provider, panic hook
nativeIdentityFingerprint(dir: String)           → String    64-hex, for pairing
nativeConnect(configJson: String)                → long      handle; 0 + throws on failure
nativePollEvent(h, timeoutMs: Int)               → String?   control-plane JSON; null on timeout
nativeFillBuffer(h, buf: ByteBuffer, meta: LongArray, timeoutMs: Int) → Int
nativeStats(h)                                   → String    ReceiveStats as JSON
nativeSendInput(h, data: ByteArray)              → Int       Phase 2 — stub for now
nativeClose(h)                                   → void
```

`nativeFillBuffer` returns bytes written, or `-1` timeout, `-2` buffer too small
(`meta[0]` = required size), `-3` session ended. `meta` is `long[3]` =
`[size, flags, ptsUs]`, flags bit 0 = keyframe.

`nativeConnect` takes the CLI's own argument set as JSON — `identity_dir`,
`relay_url`, `relay_pin`, `host_fingerprint`, geometry, `codec`, `bitrate_kbps` —
so CLI and Android configure through one code path.

Events are objects: `{"type":"state","state":"punching"}`,
`{"type":"granted","session":3,"width":1920,"height":1080,"fps":60,"codec":"hevc"}`,
`{"type":"error",...}`, `{"type":"ended",...}`. **Kotlin must not configure
MediaCodec until `granted` arrives** — geometry comes from the host's grant, per
P2P handoff §3 step 8.

Handle safety: `Box::into_raw` → `jlong`; `nativeClose` does `Box::from_raw` plus
`Runtime::shutdown_timeout`. Kotlin zeroes its handle under a lock, and the Rust
struct carries a magic field so a double-close is caught rather than exploited.

---

## 4. Kotlin side

- **`SurfaceView`, not `TextureView`** — it gets a hardware overlay plane, so no
  GPU composite and lower latency.
- **Sync mode, two threads** for Phase 1: a feeder thread (`dequeueInputBuffer` →
  `nativeFillBuffer` → `queueInputBuffer`) and a renderer thread
  (`dequeueOutputBuffer` → `releaseOutputBuffer(idx, true)`). This maps exactly
  onto the blocking pull; async callback mode can come later if it earns itself.
- `configure(format, surface, null, 0)` — decode straight to the Surface. Never
  `ImageReader`; that drags frames back through the CPU.
- `KEY_LOW_LATENCY = 1` (API 30+), plus the vendor key `"vdec-lowlatency"` for
  older devices.
- **No CSD extraction needed.** Nova inlines parameter sets on every IDR
  (`NV_ENC_PIC_FLAG_OUTPUT_SPSPPS`), so in-band Annex-B is sufficient and there is
  no `csd-0` to assemble.
- **Foreground service**, `foregroundServiceType="mediaPlayback"`, plus a
  `PARTIAL_WAKE_LOCK` and a HIGH_PERF `WifiLock` for the session. Doze suspends
  app threads regardless of socket state, which would silently lapse both the
  500 ms media keepalive and the 25 s STUN keepalive — P2P handoff §4 Step 2
  flags this and the foreground service is the answer.
- Permissions: `INTERNET` only. Raw UDP, so cleartext policy is not involved.

Build wiring:

```
cargo ndk -t arm64-v8a -t x86_64 -p 26 -o android/app/src/main/jniLibs build --release
```

as a Gradle task ordered before `mergeJniLibs`, with `android.ndkVersion` pinned.

---

## 5. Enrolment — RESOLVED (option a, built 2026-08-13)

Implemented in `echo-client/src/pairing.rs` + `echo-client pair --host <lan-ip>`.
See §9 for what it cost and what it uncovered. The rest of this section is the
original reasoning, kept because the rejected options are still rejected.

### The original decision

**There is no client-side pairing implementation.** `echo id` prints a
fingerprint and instructs a human to trust it; nothing in `echo-client` speaks
Nova's pairing handshake. On a CLI that is tolerable. On a phone, "hand-edit
`nova_paired.json` on the host" is not a first run.

Three options, and they are not close:

- **(a) Implement the Moonlight PIN pairing client in `echo-client`'s lib.**
  *Recommended.* The server half is already built and hardened (Phase 14.1:
  phase-order enforcement, `same_hash`, RSA-PKCS1-SHA256 signature verification),
  the tray PIN dialog already works and already relays through the Master
  (Phase 16.5), and Echo's fingerprint lands in the same trust store under the
  same MITM checks as every other device. The CLI gets `echo pair --host <lan-ip>`
  out of it too. Cost: the RSA/AES-ECB handshake, once, in shared code.
- **(b) A new unauthenticated enrolment command on the Echo RPC surface.**
  Rejected on the same grounds as P2P handoff §2.4 — that is new
  pre-authentication surface on a command channel that reconfigures displays on a
  LocalSystem service.
- **(c) Manual fingerprint entry in the tray.** Cheapest, but it is a 64-character
  hex string typed by a human, and it authenticates nothing.

Note either way: **first-run pairing is a LAN operation**, so Phase 1 is "pair
once on the LAN, then stream over WAN."

---

## 6. Order of work

1. ~~**Restructure**~~ — **DONE**, see §8.
2. ~~**Provider feature**~~ — **DONE**, see §8. Still unproven against a real
   NDK: the feature *set* is verified, the cross-compile is not.
3. ~~**`echo-android`** JNI surface~~ — **DONE**, see §8. Untested against a JVM.
4. ~~**Pairing**~~ — **DONE**, see §9.
5. **Live desktop end-to-end run** — pair, punch, tunnel, frames. The next
   thing owed, and the last checkpoint before Android. See §10.
6. **Install the NDK** and run the first real cross-compile (§1).
7. **Kotlin harness** — load the `.so`, connect, log frame sizes. Proves the
   bridge with no decoder in the way.
8. **MediaCodec + SurfaceView.** First picture.
9. **Foreground service, wake/wifi locks, reconnect.**

---

## 7. Two corrections to this document's own first draft

Both were found by reading the tree rather than trusting the plan:

- **rcgen needs no change.** rcgen 0.14.8's default features are
  `["crypto", "pem", "ring"]` — it already defaults to `ring`. That is *why*
  `ring` was in the lockfile. Only `rustls` and `tokio-rustls` needed gating.
- **The "both providers get linked" footprint worry was overstated.** Measured:
  forcing `echo-android` into a release build changes `nova-server.exe` by
  **1024 bytes** — one page, i.e. alignment noise. `ring` was already in the
  host graph via rcgen, and thin LTO strips the unused rustls provider module.
  `default-members` is still the right hygiene (deployment builds should not
  compile an Android bridge), but it buys build time, not megabytes.

---

## 8. What was built (2026-08-13)

Tests: **141 → 158 passing**, 8 ignored. Every original test still passes; the
17 new ones are the gate, the frame queue, the event contract, and the handle
safety checks.

| Change | Where |
|---|---|
| lib/bin split; all logic moved out of the CLI | `echo-client/src/lib.rs`, `main.rs` |
| Connection sequence, now emitting `Event`s instead of printing | `echo-client/src/session.rs` |
| Keyframe gate | `echo-client/src/gate.rs`, applied in `receiver::run_receiver` |
| `frames_dropped_before_keyframe` stat | `echo-client/src/receiver.rs` |
| JNI surface (8 entry points) | `echo-android/src/lib.rs` |
| Bounded drop-oldest frame queue + second gate | `echo-android/src/frames.rs` |
| Provider feature (`aws-lc-rs` default / `ring`) | `nova-core/Cargo.toml`, `identity.rs` |
| Explicit provider at every TLS builder | `nova-core/identity.rs`, `nova-server/{pairing,echo/signaling}.rs` |

### The `Progress` seam

`session.rs` no longer prints. It emits `Event`s through a `Progress` trait; the
CLI renders them as the exact lines it always printed, and the bridge serialises
them to JSON for `nativePollEvent`. This is what made the library reusable —
`println!` on Android goes to a stdout nobody reads, so a session that reported
progress by printing would be one an app could not narrate.

Frames deliberately do **not** travel this channel. They go to a `FrameSink`.
The split mirrors the JNI surface (`nativePollEvent` vs `nativeFillBuffer`)
because the two have genuinely different rates; mixing a 120 Hz stream into a
channel a UI thread polls would make the UI the bottleneck for video.

### A second gate, on purpose

There are now two keyframe gates, covering different events:

- `receiver::run_receiver`'s gate covers **session start** — the stream opening
  mid-GOP, or an opening IDR lost beyond FEC repair.
- `frames::FrameQueue`'s gate covers **post-receipt loss** — a frame evicted
  because the decoder fell behind, which breaks the reference chain just as
  thoroughly but happens after the first gate has already opened.

One gate cannot cover both, because they are triggered by different things.

### The bug this work surfaced

Making the provider explicit was not optional polish. `nova-server`'s TLS
builders used `builder_with_protocol_versions`, which resolves rustls's
**process-global default provider**. With two providers compiled — which is
exactly what a `--workspace` build now produces — there is no unambiguous
default, and two `echo::signaling` tests failed in a workspace run while passing
in isolation. Every builder in `nova-core` and `nova-server` now names its
provider, so configuration no longer depends on process-global state or on what
else happens to be linked in.

### What is verified, and what is not

**Verified:** all 158 tests; clean compile in three separate feature
configurations (host `aws-lc-rs`, Android `ring`-only, and both at once); the
Android dependency graph contains **no `aws-lc-rs` and no `clap`**; the release
build of `nova-server.exe` still succeeds.

**Not verified — no NDK or JVM on this machine:** the actual
`aarch64-linux-android` cross-compile, and every JNI signature. The bridge
type-checks on the host target, which catches Rust-level mistakes but *not* a
mismatch between a `Java_com_nova_echo_EchoNative_*` symbol and the Kotlin
`external fun` it is supposed to serve. That class of error surfaces as
`UnsatisfiedLinkError` at run time and can only be caught by step 6.

---

## 7. Deliberately out of scope for Phase 1

Carried forward from P2P handoff §4 Step 3, unchanged: **audio** (not on the
punched socket yet), **input** (no client→host path exists), **hot format change**
(frozen, `hot_format_change: false`), **HDR** (the `KEY_COLOR_STANDARD` /
`HDR_STATIC_INFO` path is real but later), and **multi-seat**.

---

## 9. Client-side pairing (2026-08-13)

Tests: **158 → 180 passing**, 8 ignored.

| Change | Where |
|---|---|
| Four-phase GameStream PIN handshake | `echo-client/src/pairing.rs` |
| `echo-client pair --host <lan-ip>` | `echo-client/src/main.rs` |
| RSA-2048 identity + PKCS#1 signing + PEM/base64 | `nova-core/src/identity.rs` |
| `rsa`/`num-bigint-dig` optimised in dev builds | root `Cargo.toml` |

### The finding that reshaped the task: Echo's identity had to change

Nova's `pairing.rs` does two things that are only satisfiable by an **RSA-2048**
client certificate:

```rust
let cert_signature = &cert_der[cert_der.len() - 256..];        // exactly 256 bytes
ee.verify_signature(RSA_PKCS1_2048_8192_SHA256, msg, sig)      // RSA, not ECDSA
```

Echo's identity was ECDSA P-256 (rcgen's default). It could never have paired,
so `Identity::load_or_create_rsa2048` was a prerequisite for this feature rather
than a refinement of it. Echo must use that **one identity for both pairing and
TLS**, because Nova keys its trust store by the certificate it saw during pairing.

Two consequences worth remembering:

- **rcgen cannot generate RSA keys under the `ring` backend** — its own source
  says "Ring doesn't have RSA key generation yet". It *signs* with a supplied
  RSA key under either backend, so the key comes from the pure-Rust `rsa` crate
  and the certificate is still built by rcgen. This keeps the Android build
  working, which the obvious alternative (switch nova-core to aws-lc-rs) would
  have quietly undone.
- **A length check cannot detect a wrong key type.** An ECDSA certificate is
  *larger* than 256 bytes, so slicing succeeds and returns certificate **body**
  bytes. Pairing would then fail at phase 4 with a hash mismatch indistinguishable
  from a wrong PIN. `cert_signature()` therefore validates the private key type,
  not the certificate length — a test asserts the ECDSA certificate is big enough
  to make the length check useless, so the test fails if anyone "simplifies" it.

### What is verified

- **The full handshake round trip**, real client against a mock host that
  reimplements `nova-server/src/pairing.rs` — phase ordering, query-string shape,
  hex/PEM encoding, both AES directions, both hash constructions, both signatures.
- **A wrong PIN is refused** and nothing is trusted.
- **An impostor host is refused**: a peer that learned the PIN but signs with its
  own key fails verification 2. (Verification 1 alone would not catch it — it
  would simply hash its own certificate's signature.)
- **HTTP framing against the live Nova.** Confirmed by querying the running
  host's `/serverinfo`: hyper honours `Connection: close`, so read-to-EOF is
  correct framing and no `Content-Length` parsing is needed.

Two defects were found by running the thing rather than by reading it:
`TcpStream::connect` sat outside the timeout (an unreachable host hung for the
OS TCP timeout, ~21 s, ignoring `--consent-secs`), and the mock host in the
wrong-PIN test waited for a fourth request that a failed handshake never sends.

### The limit of the mock

The mock host encodes *this* reading of `nova-server/src/pairing.rs`. If the
reading is wrong, the mock is wrong in the same direction and the test passes
anyway. Only a live run against Nova settles it — which is §10.

---

## 10. The live desktop end-to-end run (owed)

Nova is running on the dev box (service `Running`, 47989 listening), so this can
be done immediately. It needs a human at the host to answer the PIN dialog.

```
# 1. Pair (LAN only; the dialog appears on the host)
echo-client pair --host <nova-lan-ip>

# 2. Stream over the LAN control port, no relay needed for a first proof
echo-client stream --host <fingerprint printed by step 1> \
                   --control <nova-lan-ip>:48011 --seconds 30
```

`pair` prints the exact `stream` command on success.

**Expect:** a PIN box → Nova's tray dialog → `🔑 PIN accepted` → `🛡️ Host
verified` → `✅ Confirmed over HTTPS` (that last line is the one that proves the
trust store entry actually authorises Echo, which `paired=1` alone does not).
Then from `stream`: `🔐 Control tunnel authenticated` → `🎬 Session N granted` →
`🎞️ frame …`.

**Watch for:** `frames_recovered_by_fec > 0` (FEC working), `frames_failed_auth
== 0` (sealing correct), and `frames_dropped_before_keyframe == 0` — a nonzero
value there means Nova did not start the session with an IDR, which is worth
knowing before a decoder is attached to it.

**Then** the WAN path with the relay, and only then the NDK.
