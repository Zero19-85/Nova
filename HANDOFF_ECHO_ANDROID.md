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

---

## 11. The APK builds (2026-08-14)

`android/app/build/outputs/apk/debug/app-debug.apk` — 12.20 MB, containing
`lib/arm64-v8a/libecho.so` (3,241,128 bytes, stored **uncompressed**, which is
what `jniLibs.useLegacyPackaging = false` buys: the loader mmaps it instead of
extracting, and it satisfies the 16 KB page-alignment requirement). A clean
`gradlew clean assembleDebug` reproduces it.

### Version reality, and how much of §1 it invalidates

§1 was written from a May 2026 knowledge cutoff and its version pins were wrong.
What is actually current, queried from the source of truth rather than guessed:

| | §1 said | Reality |
|---|---|---|
| Android Studio JBR | JDK 21 | **JDK 25** |
| Gradle | 8.9 | **9.7.0** |
| AGP | 8.7.2 | **9.3.1** |
| Kotlin | 2.0.21 | **2.4.10** |

The advice to avoid JDK 25 was therefore backwards: Studio *ships* 25, and the
current AGP/Gradle expect it. Query `services.gradle.org/versions/current` and
Google's maven metadata rather than trusting any pinned number in this document.

### Four things that broke, and why

1. **`org.jetbrains.kotlin.android` is now an error, not redundancy.** AGP 9 has
   built-in Kotlin support and rejects the standalone plugin outright. The
   Compose compiler plugin is still applied separately.
2. **`kotlinOptions { jvmTarget }` no longer exists** under built-in Kotlin.
   Removed; the default follows `compileOptions`.
3. **Gradle 9 fails the build on Kotlin DSL deprecations.** `tasks.registering`
   and `sourceSets[...].jniLibs.srcDirs(...)` are deprecated and were compile
   *errors*, not warnings. Use `tasks.register<T>("name")`; the jniLibs override
   was redundant anyway, since `src/main/jniLibs` is already the default.
4. **`local.properties` is a Java properties file.** `sdk.dir=C:\Users\…` fails
   with a bare `java.io.IOException: Invalid file path`, because `\U` is an
   invalid escape. Use forward slashes or double the backslashes.

### cargo-ndk 4.x changed a flag

`-P` (capital) is the Android API level. Lowercase `-p` is cargo's `--package`,
passed through after `build`. Mixing them up reads as `unknown package: 26`.

```
cargo ndk -t arm64-v8a -P 26 -o android/app/src/main/jniLibs build --release -p echo-android
```

### The `ring` decision paid off

`libecho.so` cross-compiled with nothing but the NDK — no cmake, no bindgen, no
`aws-lc-sys`. That was the whole point of the provider feature in §1.3, and it is
now confirmed rather than predicted.

### Still not verified

The APK has never been installed or run. The JNI symbols match the Kotlin
declarations by name (checked mechanically), but no native method has ever been
*called*, so `UnsatisfiedLinkError` from a signature mismatch remains possible.
And the host still needs redeploying — the live binary has no WAN control
transport, so the phone will hit the same `tls handshake eof` the CLI did.

---

## UI overhaul: the Ion dashboard, persistent hosts, per-host controls (2026-08-20)

Built and installed on the Pixel 9 Pro XL; a stream ran end to end on this build.
The setup screen — five text fields, two 64-character hex strings and a raw event
log — is gone. Nothing on the protocol side changed: no Rust file was touched,
and `cargo test --workspace` is 147 passed / 0 failed.

### New files (all `android/app/src/main/java/com/nova/echo/`)

| File | Owns |
|---|---|
| `Theme.kt` | The palette and the telemetry type. One place colour is decided. |
| `HostStore.kt` | The persistent host list — the vanishing-host fix |
| `EchoSettings.kt` | Global stream prefs behind the gear |
| `Probe.kt` | TCP reachability for the badges and the diagnostics panel |
| `Dashboard.kt` | The host list, cards, badges, top bar |
| `Sheets.kt` | The settings sheet, the long-press host sheet, add-host dialog |

`MainActivity.kt` lost `ControlPanel` and `rememberPref` and now hosts the
Activity, the streaming overlay, and nothing else.

### The rule the store exists to enforce

**mDNS is evidence, not the list.** The old screen rendered `HostDiscovery`
directly, so a host vanished when Nova restarted, when the phone moved to
cellular, or whenever the platform resolver dropped a record — taking with it the
only route to a machine that was still perfectly reachable over the relay. A
`KnownHost` now persists the alias, the relay pair, the last LAN address and the
paired identity; a sighting is folded in with `observed()`, and absence only
changes the badge.

Two things that must stay true:

1. **An advertised fingerprint is a LABEL, never a promotion.** `observed()` will
   adopt `fp` for a record that has none, and it never sets `paired`. Only
   `HostStore.paired()`, called from a completed PIN handshake, does that — the
   same boundary `HostDiscovery` documents from the other side.
2. **The legacy adoption runs exactly once and writes its flag first.** The
   pre-dashboard setup screen kept one host in five loose `echo` prefs, and the
   pairing those describe is real, so `adoptLegacySetup` promotes or creates the
   matching card. Guarded because a host the user deliberately forgets must not
   come back on the next launch. Verified live: APEX came back `PAIRED` without a
   re-pair.

### Presence is three states, not two

`Presence.{Lan, Wan, Cached}` → `ONLINE // LAN` (green), `ONLINE // WAN_PUNCH`
(cyan), `OFFLINE // CACHED` (dim). Green is reserved for "answered on the local
segment"; a card not on the LAN is probed against its relay so the badge can tell
"one hop away" from "no route at all". The probe is a TCP connect, because
Android hands an unprivileged app no raw sockets — so it is never called a ping in
the UI.

### Settings are a REQUEST, not the running configuration

Codec / resolution / framerate / bitrate go into `connect()` as `StreamPrefs` and
the host negotiates from there. `onGranted` still configures the decoder from the
grant, which is what makes an H264 session capped at 24 fps by Level 5.2, or a
bitrate clamped to the resolution ceiling, correct rather than a mismatch. The
sheet says so on screen.

The microphone switch is the one setting the controller owns rather than the
settings object: two UIs toggle it (the sheet and the in-stream overlay), and
`setMicEnabled` persists it centrally so they cannot disagree.

### Not done

- **The WAN endpoint field is stored and displayed but never dialled.**
  `session::open_path` is still unconditionally relay-mediated, so this is a
  place to put the address the LAN-direct selector will need, not a working
  override. Wire it when the client-side staged selector lands.
- Network switching (Wi-Fi to 5G) was verified by construction and by the
  on-disk store, not by physically moving the phone — adb runs over that Wi-Fi.
- `network_security_config.xml` still lists IP literals that match no real host
  (pre-existing, still inert).

---

## Zero-config WAN, live to 5G (2026-08-20)

The Android client now streams over cellular with nothing forwarded by hand.
Verified end to end on Verizon 5G to the Pixel 9 Pro XL.

### What the client does

`session::open_path` runs a staged cascade and stops at the first route that
works: **LAN rendezvous** (TCP 48011 + punch, ~500 ms dial / 1.5 s punch) →
**relay + STUN punch** → the manual WAN endpoint offered as an extra candidate
inside stage 2. The badge on each host card reports the route the engine
actually took, because `path_open` now carries a `transport` field
(`lan` / `wan_punch` / `direct_wan`) — that string is API between Rust and
Kotlin.

**Classified from the latched peer, not the branch that ran.** A relay-signalled
punch that lands on a private address is `lan`, because the relay is signalling
and not transport.

### The host side of it, and the bug worth remembering

Nova asks the router over UPnP for its public address and two port mappings, then
publishes the public relay URL in the `_echo._tcp` record — which is how the
phone learns an endpoint it can dial from cellular.

**The failure that cost a round: SSDP bound to `0.0.0.0`.** The dev box has five
IPv4 addresses, four of them dead `169.254.x` stubs on disconnected Wi-Fi and
Bluetooth adapters. The multicast search left through one of those and nothing
answered, which is indistinguishable from a router with UPnP switched off — and
was diagnosed as exactly that, wrongly, because the three PowerShell probes used
to check it *shared the same bind bug*. Three agreeing probes meant one mistake
made three times. Binding the host's LAN address fixes it:

```
bind 0.0.0.0     -> 0 responders
bind 10.0.0.205  -> 1 responder: http://10.0.0.1:49153/IGDdevicedesc_brlan0.xml
```

### The discovery rule this creates, which the UI depends on

**A phone must see the host on Wi-Fi once before it can stream over cellular.**
The public endpoint travels in the mDNS record and mDNS is local-only, so a phone
that has never been on the network has no way to learn where to dial. `HostStore`
persists it — that is what the store is for — and `observed()` refreshes it from
each sighting, so changing the relay means opening the app once on Wi-Fi (or
editing the endpoint by hand from the host card's long-press sheet).

### Known edge case — next session starts here

**Swapping networks (Wi-Fi ↔ 5G) during a suspended session occasionally returns
to a black screen** until the stream is stopped and restarted. The session
survives and reconnects; the picture does not always come back with it.

Suspect the client decoder/Surface path rather than the transport — the cascade
re-establishes correctly and the host log shows the new path opening. The
2026-08-19 MediaCodec wedge is the obvious neighbour (a codec whose Surface died
never recovers, and nothing throws), but this is a *different* trigger: the
Surface is alive throughout. Worth checking first:

1. Does the host log a keyframe request after the swap? A flood means the client
   queue is overflowing and nothing is consuming — see
   `mediacodec-surface-wedge`. Silence means the client never asked.
2. Is `ConfigureStart` replayed to the reconnecting client, and does the RTP
   sender re-pin to the new peer address?
3. Does `VideoPlayer` still hold a codec configured for the pre-swap session, and
   would `nativeRequestIdr` alone unstick it?
