
| 🟢 NOVA CORE (FREE / OPEN SOURCE) | 🟣 NOVA PRO (DONATION TIER) |
| :--- | :--- |
| **Zero-Copy Pipelines** (DXGI Desktop Dup. → HW Encoder) | **Windows Service Mode** (Pre-login, hidden, boot-level) |
| **Intelligent Codec Tiering** (H.265 baseline, AV1, H.264 fallback) | **Native Echo Mic Passthrough** (UMDF Virtual Audio Driver) |
| **Token-Bucket Network Pacer** (Eliminates wireless micro-stutter) | **Virtual Monitor / IDD Sandbox** (Headless HDR/10-bit) |
| **Full Controller Support** (XInput + ViGEmBus virtual injection) | **Advanced Orchestration & Routing** |



---

## 🗺️ Master Roadmap

### 🏁 Phase 3: The Pipeline Finish Line `[✅ COMPLETE]`
*   **Zero-copy DXGI Desktop Duplication → NVENC** *(H.264/NV12, CBR, IDR every 2s, Annex-B, repeatSPSPPS=1)*
*   **Exact Sunshine RTSP wire protocol** on TCP 48010 *(per-connection messages, immediate shutdown after response, DEADBEEFCAFE session token, correct X-SS-Ping-Payload / X-SS-Connect-Data headers on SETUP)*
*   **ENet reliable UDP control stream** on 47999 via `rusty_enet`
*   **RTP video packetizer** on UDP 47998 *(NV_VIDEO_PACKET 8-byte header, MTU slicing, client address learning from post-PLAY ping packet, full NAL keyframe detection + force-IDR-on-connect shim)*
*   **HTTPS pairing server** with hardened TLS 1.2 + mutual-auth compatibility *(LAN IP SAN, proper CA/KeyCertSign chain, ALPN http/1.1, clock-skew & hostname verification fixes)*

> **Result:** Video bitstream generation + complete control plane is live and Moonlight-compatible! 🎉

### 🚧 Phase 4: Core Streaming MVP `[IN PROGRESS]`
*Prioritized for the fastest path to a usable Alpha:*
1.  **End-to-End Video Validation:** Real Moonlight client testing (decoder init, no black screen, flawless frame delivery).
2.  **Low-Latency Audio:** WASAPI audio capture → Opus → RTP packetization + hardware-timed AV sync engine.
3.  **Input Event Handling:** Gamepad via ViGEmBus/XInput, mouse lock + absolute positioning, keyboard injection.
4.  **Zero-Config & Bitrate:** mDNS discovery + dynamic bitrate adjustment via UDP feedback.
5.  **Quality of Life:** Auto-game presets (foreground window detection) + Headless Display Emulation.

### 📦 Phase 5: Single-Exe Release Engineering
*   Asset embedding with `include_bytes!`
*   Full LTO + `panic=abort` + symbol stripping.
*   100% Portable executable—**zero installer required**.

---

## 📡 Current Status *(Updated: June 10, 2026)*

**Phase 3 Complete ✅ | Phase 4 In Progress (Video pipeline + control plane live)**

### 🟢 Working Right Now:
*   ✅ **DXGI → NVENC** zero-copy encode path (Live RTP capable).
*   ✅ **RTSP + ENet + RTP channels** mirroring Sunshine/Moonlight semantics.
*   ✅ **HTTPS Pairing** hardened for desktop and beta Android clients.
*   ✅ **Force-IDR on connect** + proper keyframe signaling.

### 🟡 Next Immediate Actions:
*   Validate live video stream in Moonlight over LAN.
*   Wire the remaining audio RTP leg (WASAPI capture shim is ready).
*   Input injection on the control stream.
*   *Alpha MVP ships once video + audio render cleanly!*

---

## 🔥 Community Most-Wanted Features

We are listening. Here is what is on the high-priority radar:
- [ ] **Mouse lock** & virtual absolute input handling.
- [ ] **Headless display emulation** (Kill the HDMI dummy plugs for good).
- [ ] **Auto game presets** based on running titles (WoW, FPS, etc.).
- [ ] **Native microphone passthrough** via Echo.

---

## 🛠️ The Tech Stack

Nova is forged from modern, uncompromising tools:

*   🦀 **Core:** 100% Rust for memory safety, concurrency, and raw speed.
*   ⚙️ **Hardware Shims:** Thin C++ layer via `cc` builder (NVENC active; AMF/QuickSync incoming).
*   📸 **Capture:** DXGI Desktop Duplication.
*   🎥 **Encoding:** Direct bare-metal hardware access.
*   🌐 **Networking:** RTSP (TCP), ENet (reliable UDP), RTP (video/audio UDP).

---

## 🚀 Quick Start *(Alpha Coming Soon)*

Ready to compile from source? 

```bash
# Clone the repository
git clone [https://github.com/Zero19-85/Nova.git](https://github.com/Zero19-85/Nova.git)

# Enter the directory
cd Nova

# Build and run the optimized release binary
cargo run --release

---

### 💻 System Requirements
* **OS:** Windows 10 / Windows 11
* **Hardware:** Modern GPU (*NVIDIA RTX series highly recommended to leverage the current bare-metal NVENC path*)

> ⚠️ **NOTE:** Previous documentation referenced `nova-server` — this has been officially corrected to the actual **Nova** repository.

---



### 🤝 Contributing & Donations

We welcome pull requests from fellow optimizers! Check out [`BLUEPRINT.md`](#) for the full architectural vision and browse open issues for active tasks. 

If you want to support the blistering-fast development of **Nova Pro** features *(Windows Service mode, Virtual Audio, IDD)*, consider sponsoring the project on GitHub.

**If you want lower-latency, lighter game streaming... drop a ⭐ on this repo!**

