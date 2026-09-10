// The Rust bridge, as C sees it.
//
// Hand-maintained against `echo-xbox/src/lib.rs`. That crate is the authority;
// if the two disagree, this file is wrong. Everything here is `extern "C"` with
// no decoration, so the linker will not catch a signature that has drifted —
// the ABI is checked by nothing but attention. Change one side, change both.
//
// ── The two-CRT rule ────────────────────────────────────────────────────────
//
// `echo_xbox.dll` links the CRT statically; this app links the Store CRT
// (`vcruntime140_app.dll`). Two C runtimes, two heaps. That is safe only
// because **no allocation ever crosses this boundary**: every string and buffer
// below is storage the *caller* owns, and there is deliberately no `echo_free`.
// If a new entry point seems to need one, it is the wrong shape.
#pragma once

#include <cstdint>

extern "C" {

// ── Status codes ────────────────────────────────────────────────────────────
constexpr int32_t ECHO_OK          =  0;  // succeeded, wrote nothing
constexpr int32_t ECHO_BAD_HANDLE  = -1;  // not a live session — stop
constexpr int32_t ECHO_BAD_ARG     = -2;  // null, negative, or not UTF-8
constexpr int32_t ECHO_TOO_SMALL   = -3;  // buffer too small; nothing written
constexpr int32_t ECHO_FAILED      = -4;  // see echo_last_error

// echo_fill_buffer
constexpr int32_t FILL_TIMEOUT     = -1;  // no frame yet — the normal state
constexpr int32_t FILL_TOO_SMALL   = -2;  // meta[0] holds the size needed
constexpr int32_t FILL_ENDED       = -3;  // session over; stop feeding
constexpr int32_t FILL_BAD_HANDLE  = -4;

// echo_poll_audio. Positive means "a packet is in the buffer", so the sign
// alone separates the one case with a payload from the four without.
constexpr int32_t AUDIO_PACKET     =  1;  // decode meta[0] bytes and submit
constexpr int32_t AUDIO_CONCEAL    =  0;  // a packet was lost — conceal one
constexpr int32_t AUDIO_SILENCE    = -1;  // buffer filling — submit silence
constexpr int32_t AUDIO_IDLE       = -2;  // nothing expected — voice may idle
constexpr int32_t AUDIO_BAD_HANDLE = -3;
constexpr int32_t AUDIO_TOO_SMALL  = -4;

// ── Diagnostics ─────────────────────────────────────────────────────────────

// Why the last failing call failed. Returns bytes written, excluding the NUL.
int32_t echo_last_error(char* out, int32_t cap);

// Escalating self-test. Run stages in order on a new console; the first failure
// names the problem. 0 = ABI only, 1 = heap/CRT, 2 = filesystem (arg = a
// writable dir), 3 = UDP socket on a Tokio runtime, 4 = RSA identity (slow).
int32_t echo_probe(uint32_t stage, const char* arg, char* out, int32_t cap);

// This client's certificate fingerprint, generating the identity on first call.
// `dir` must be app-private storage: it will hold a private key. 65 bytes out.
int32_t echo_identity_fingerprint(const char* dir, char* out, int32_t cap);

// ── Session lifecycle ───────────────────────────────────────────────────────

// Both return a handle immediately, or 0 (see echo_last_error). Progress and
// failures arrive through echo_poll_event, not as return values.
uint64_t echo_connect(const char* config_json);
uint64_t echo_pair(const char* config_json);

// Blocks up to timeout_ms. ECHO_OK with nothing written means "nothing
// happened, keep polling"; ECHO_BAD_HANDLE means the session is over.
int32_t echo_poll_event(uint64_t handle, int32_t timeout_ms, char* out, int32_t cap);

// Copy the next frame into `dst` — in practice an IMFMediaBuffer's locked
// pointer, which is what makes this one copy into memory the decoder owns.
// On success meta = [size, flags, pts_100ns]; flags bit 0 marks a keyframe.
int32_t echo_fill_buffer(uint64_t handle, uint8_t* dst, int32_t cap, int64_t* meta, int32_t timeout_ms);

// One step of downstream audio, on the renderer's clock. See the AUDIO_* codes.
int32_t echo_poll_audio(uint64_t handle, uint8_t* dst, int32_t cap, int64_t* meta);

// Receive and queue statistics as JSON, for an on-screen overlay. 1 KiB is fine.
int32_t echo_stats(uint64_t handle, char* out, int32_t cap);

// ── Uplink — all fire-and-forget ────────────────────────────────────────────

// kind: 1 mouse-rel(dx,dy) 2 mouse-abs(x,y,w,h) 3 button(code,down)
//       4 scroll(clicks) 5 key(vk,down,mods) 6 release-all 7 touch(ev,id,x,y)
bool echo_send_input(uint64_t handle, int32_t kind, int32_t a, int32_t b, int32_t c, int32_t d);

// One controller snapshot. `buttons` is XInput's bit layout — Windows.Gaming.Input
// is not, so translate first (GamepadInput.h). Axes are full int16, Y positive
// up, which matches both. `active_mask` is the plug: a set bit at
// controller_number plugs a virtual pad in host-side, a clear bit unplugs it.
bool echo_send_gamepad(uint64_t handle, int32_t controller_number, int32_t active_mask,
                       int32_t buttons, int32_t left_trigger, int32_t right_trigger,
                       int32_t left_stick_x, int32_t left_stick_y,
                       int32_t right_stick_x, int32_t right_stick_y);

// Re-mode the host display under a LIVE session: the desktop is resized in
// place, the encoder is rebuilt at the new size, and the geometry arrives in
// the bitstream as fresh VPS/SPS/PPS on the next IDR. `false` means this is not
// a live streaming handle, never that the host refused.
//
// The decoder on THIS side must handle MF_E_TRANSFORM_STREAM_CHANGE, because
// the bridge sends `force` — see the Rust doc comment, which says why that is
// a claim about this client and not a general permission.
bool echo_set_display(uint64_t handle, uint32_t width, uint32_t height,
                      uint32_t refresh_hz, bool hdr);

// One raw Opus packet. No container framing of any kind — see the Rust doc.
bool echo_send_mic(uint64_t handle, const uint8_t* data, int32_t len);

// ── Session controls ────────────────────────────────────────────────────────

// Flags a keyframe request. Under Nova's infinite GOP a decoder that has lost
// its reference chain and does not ask waits forever.
bool echo_request_idr(uint64_t handle);

// Hold video back to meet the audio device's latency. Returns the clamped value.
uint32_t echo_set_video_delay(uint64_t handle, uint32_t delay_ms);

// The network moved — resume from suspend, a cable, a router reboot. Returns
// the new epoch, or 0 if the handle is not a live session.
uint64_t echo_network_changed(uint64_t handle);

// Idempotent, and safe on 0. Wakes every blocked caller.
//
// Sends the host `stop_session` on the way out, so the host ENDS the session and
// gives back the display it was driving. That is the goodbye, not a detail of
// teardown — if the intent is to walk away and come back, use echo_detach.
void echo_close(uint64_t handle);

// Leave without the goodbye. The host sees the client go quiet, detaches, and
// HOLDS its virtual display for the grace period so a reconnect resumes into the
// same desktop. Ending such a session afterwards is echo_release's job.
void echo_detach(uint64_t handle);

// Ask the host to end a session it is holding for this device. BLOCKING, and
// deliberately: ending a session never needed a session. Worker thread only.
int32_t echo_release(const char* config_json, char* out, int32_t cap);

}  // extern "C"
