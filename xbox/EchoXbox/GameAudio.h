// Downstream game audio: Opus packets from the host, decoded and played.
//
// Until this existed the Xbox client had NO audio at all (handoff §5, milestone
// 5 "not started"), and the host was routing game audio into its ghost sink for
// a client that never asked for a single packet. The operator's report -- "the
// host keeps forcing Steam Streaming Speakers and I cannot get sound to the TV"
// (2026-10-07) -- was this, seen from the PC.
//
// ── Shape ──────────────────────────────────────────────────────────────────
//
// The bridge already owns the hard part. `echo_poll_audio` is a jitter buffer
// that answers one 20 ms step at a time, ON THE CALLER'S CLOCK, which is the
// only clock that matters for playout: XAudio2 consumes buffers at exactly the
// rate the console's audio device runs at, so pulling one step per buffer it
// finishes keeps the jitter buffer honest without any timing code here.
//
// ── Decoder: FFmpeg's, not Media Foundation's ──────────────────────────────
//
// Windows ships an Opus DECODER (handoff §5), but nothing establishes that the
// console's media stack exposes it to an app, and finding out would cost a
// sideload round trip per guess. FFmpeg is already in the package for HEVC and
// carries a native Opus decoder, so the decoder this file uses is one the build
// can prove is present: `build-ffmpeg.ps1` enables it (and swresample, which
// configure lists as `opus_decoder_deps` -- the decoder resamples SILK frames
// internally; nothing here calls it).
//
// ── Concealment ────────────────────────────────────────────────────────────
//
// AUDIO_CONCEAL and AUDIO_SILENCE both render 20 ms of silence. The handoff's
// rule is that the CODES must stay distinct, not that the sound must differ,
// and the codes are counted separately below. Real PLC is a later refinement.
#pragma once

#include <atomic>
#include <cstdint>
#include <string>
#include <vector>
#include <array>

struct IXAudio2;
struct IXAudio2MasteringVoice;
struct IXAudio2SourceVoice;
struct AVCodecContext;
struct AVPacket;
struct AVFrame;

namespace echo {

class GameAudio {
public:
    GameAudio() = default;
    ~GameAudio();
    GameAudio(GameAudio const&) = delete;
    GameAudio& operator=(GameAudio const&) = delete;

    // Opus decoder + XAudio2 voice, both at 48 kHz stereo float. On failure
    // `error` names the stage, for the diagnostics panel.
    bool Open(std::wstring& error) noexcept;
    void Close() noexcept;

    // Buffers the voice has not finished playing yet.
    uint32_t Queued() const noexcept;
    // Decode one Opus packet and queue the PCM. False on a decode failure, in
    // which case nothing was queued and the caller renders silence instead.
    bool SubmitPacket(const uint8_t* data, size_t size) noexcept;
    // Queue one 20 ms frame of silence (AUDIO_SILENCE and AUDIO_CONCEAL).
    void SubmitSilence() noexcept;
    // Block until the device finishes a buffer, or `timeoutMs` passes.
    void WaitForBufferEnd(uint32_t timeoutMs) noexcept;

    uint64_t Decoded() const noexcept { return m_decoded.load(std::memory_order_relaxed); }
    uint64_t Silent() const noexcept { return m_silent.load(std::memory_order_relaxed); }
    uint64_t DecodeErrors() const noexcept { return m_errors.load(std::memory_order_relaxed); }

private:
    bool Queue(const float* interleaved, uint32_t frames) noexcept;

    // 48 kHz, 20 ms. The host negotiates 20 ms frames for Echo (CLAUDE.md).
    static constexpr uint32_t kRate = 48000;
    static constexpr uint32_t kChannels = 2;
    static constexpr uint32_t kFrameSamples = 960;
    // Opus allows up to 120 ms in one packet; size for it so no packet the
    // host could legally send overflows a slot.
    static constexpr uint32_t kMaxSamples = 5760;
    // XAudio2 reads a submitted buffer in place until OnBufferEnd, so each
    // queued buffer needs storage of its own. Far more slots than the render
    // loop ever keeps queued (EchoSession keeps three).
    static constexpr size_t kRing = 8;

    IXAudio2* m_xaudio = nullptr;
    IXAudio2MasteringVoice* m_master = nullptr;
    IXAudio2SourceVoice* m_voice = nullptr;
    struct Callback;
    Callback* m_callback = nullptr;
    void* m_bufferEnd = nullptr;   // HANDLE, auto-reset

    AVCodecContext* m_codec = nullptr;
    AVPacket* m_packet = nullptr;
    AVFrame* m_frame = nullptr;
    std::vector<uint8_t> m_packetBytes;   // with AV_INPUT_BUFFER_PADDING_SIZE

    std::array<std::vector<float>, kRing> m_ring;
    size_t m_next = 0;

    std::atomic<uint64_t> m_decoded{0};
    std::atomic<uint64_t> m_silent{0};
    std::atomic<uint64_t> m_errors{0};
};

}  // namespace echo
