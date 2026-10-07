// Upstream microphone: the console's headset, encoded as Opus, sent to the host.
//
// The last direction the Xbox client was missing (HANDOFF_ECHO_XBOX.md §11.3,
// §16). Android does this with AudioRecord + MediaCodec; Windows ships an Opus
// DECODER but no ENCODER, so the codec here is libopus, built by
// xbox\build-opus.ps1 as a static /MD library and linked into this exe.
//
// ── Shape ──────────────────────────────────────────────────────────────────
//
//   AudioGraph device input node  -->  AudioFrameOutputNode
//        (headset, graph format)          | QuantumStarted, ~10 ms, graph thread
//                                         v
//                     downmix to mono, resample to 48 kHz if the graph is not,
//                     gain, accumulate to 960 samples (20 ms)
//                                         v
//                     opus_encode_float  -->  echo_send_mic(one raw packet)
//
// The graph's quantum is the clock, exactly as AudioRecord.read is on Android:
// nothing here owns a timer. Encoding runs on the graph's own thread because a
// 20 ms mono voice frame costs well under a millisecond, and a hand-off queue
// would add latency to the one path that cannot absorb it.
//
// ── The format is the host's, not ours ─────────────────────────────────────
//
// 48 kHz MONO, 20 ms, 24 kbps, raw Opus packets -- identical to Android's
// MicCapture.kt, because the host's decoder (nova-server/src/mic.rs) is built
// once for that format and is shared by every client. Raw means raw: libopus
// emits no container, which is precisely what echo_send_mic demands (the AV1
// IVF lesson recorded on that function).
//
// ── Lifetime: the bridge handle is a raw pointer ───────────────────────────
//
// echo_send_mic validates a handle by reading a magic word THROUGH it, so a
// send that races echo_close is a use-after-free, not a `false`. Every send
// therefore happens under m_mutex, and Stop() zeroes the handle under the same
// mutex. Once Stop() returns, no packet can reach the bridge -- which is why
// EchoSession stops the microphone BEFORE it closes the handle, never after.
#pragma once

#include <atomic>
#include <cstdint>
#include <memory>
#include <mutex>
#include <string>
#include <vector>

#include <winrt/Windows.Media.Audio.h>

struct OpusEncoder;

namespace echo {

class MicCapture : public std::enable_shared_from_this<MicCapture> {
public:
    // Begin opening the headset and return immediately; the graph comes up
    // asynchronously and Report() says how it went. Call on the UI thread: the
    // first microphone access raises the system consent prompt, which needs it.
    // `level` is 1-100 (see SetLevel); 0 is the caller's business -- it means
    // "do not create one".
    static std::shared_ptr<MicCapture> Start(uint64_t handle, uint32_t level);

    ~MicCapture();
    MicCapture(MicCapture const&) = delete;
    MicCapture& operator=(MicCapture const&) = delete;

    // Release the device and guarantee no further echo_send_mic. Idempotent,
    // safe while the async open is still in flight (the open notices and
    // tears down what it built).
    void Stop() noexcept;

    // 1-100. 50 is the signal as captured, 100 is +6 dB, with a hard clip at
    // full scale. Applies live.
    void SetLevel(uint32_t level) noexcept;

    // One line for the diagnostics panel.
    std::wstring Report() const;

    explicit MicCapture(uint64_t handle, uint32_t level);   // use Start()

private:
    winrt::fire_and_forget OpenAsync();
    void OnQuantum() noexcept;
    void Consume(const float* samples, uint32_t frames, uint32_t channels) noexcept;
    void EncodeAndSend(const float* pcm) noexcept;
    void SetState(std::wstring state);

    static constexpr uint32_t kRate = 48000;
    static constexpr uint32_t kFrameSamples = 960;     // 20 ms at 48 kHz
    static constexpr int32_t  kBitrate = 24000;        // MicCapture.kt's BITRATE
    static constexpr size_t   kMaxPacket = 4000;       // > Opus's 1275-byte frame max

    // Guards m_handle, m_stopped, the graph objects and the encoder: the
    // graph thread, the opening coroutine and Stop() all touch them.
    mutable std::mutex m_mutex;
    uint64_t m_handle = 0;
    bool m_stopped = false;
    winrt::Windows::Media::Audio::AudioGraph m_graph{ nullptr };
    winrt::Windows::Media::Audio::AudioFrameOutputNode m_output{ nullptr };
    winrt::event_token m_quantumToken{};
    OpusEncoder* m_encoder = nullptr;

    // Graph thread only.
    uint32_t m_graphRate = kRate;
    uint32_t m_graphChannels = 1;
    std::vector<float> m_mono;          // one quantum, downmixed
    std::vector<float> m_pending;       // 48 kHz mono, waiting for 960
    double m_resamplePos = 0.0;         // only when the graph is not 48 kHz
    float m_resamplePrev = 0.0f;
    std::vector<uint8_t> m_packet = std::vector<uint8_t>(kMaxPacket);
    float m_peakWindow = 0.0f;
    uint32_t m_peakFrames = 0;

    std::atomic<float> m_gain{1.0f};

    // Read by the UI's stats timer.
    std::atomic<uint64_t> m_packets{0};
    std::atomic<uint64_t> m_bytes{0};
    std::atomic<uint64_t> m_refused{0};       // echo_send_mic said false
    std::atomic<uint64_t> m_encodeErrors{0};
    std::atomic<int32_t>  m_peakDbfs{-120};   // loudest sample, last ~1 s
    mutable std::mutex m_stateMutex;
    std::wstring m_state = L"opening";
};

}  // namespace echo
