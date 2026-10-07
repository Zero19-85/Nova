#include "pch.h"
#include "MicCapture.h"
#include "EchoBridge.h"

#include <winrt/Windows.Media.h>
#include <winrt/Windows.Media.Capture.h>
#include <winrt/Windows.Media.MediaProperties.h>
#include <winrt/Windows.Media.Render.h>

#include <algorithm>
#include <cmath>

#include <opus.h>

// Staged by xbox\build-opus.ps1 (static, /MD, store CRT -- see that script).
#pragma comment(lib, "opus.lib")

using namespace winrt;
using namespace winrt::Windows::Media;
using namespace winrt::Windows::Media::Audio;
using namespace winrt::Windows::Media::Capture;
using namespace winrt::Windows::Media::MediaProperties;
using namespace winrt::Windows::Media::Render;

namespace {

// The classic-COM view of an AudioBuffer's bytes. Declared here rather than
// pulled from <MemoryBuffer.h> because that header's ABI namespace collides
// with winrt::Windows::Foundation under the projection; this is the
// declaration Microsoft's C++/WinRT audio samples use.
struct __declspec(uuid("5b0d3235-4dba-4d44-865e-8f1d0e4fd04d")) __declspec(novtable)
IMemoryBufferByteAccess : ::IUnknown {
    virtual HRESULT __stdcall GetBuffer(uint8_t** value, uint32_t* capacity) = 0;
};

const wchar_t* GraphStatus(AudioGraphCreationStatus s) {
    switch (s) {
    case AudioGraphCreationStatus::DeviceNotAvailable: return L"no audio device";
    case AudioGraphCreationStatus::FormatNotSupported: return L"format not supported";
    default: return L"unknown failure";
    }
}

const wchar_t* NodeStatus(AudioDeviceNodeCreationStatus s) {
    switch (s) {
    case AudioDeviceNodeCreationStatus::DeviceNotAvailable:
        return L"no microphone (is a headset plugged into the controller?)";
    case AudioDeviceNodeCreationStatus::AccessDenied:
        return L"microphone access denied (Settings > Privacy > Microphone)";
    case AudioDeviceNodeCreationStatus::FormatNotSupported: return L"format not supported";
    default: return L"unknown failure";
    }
}

}  // namespace

namespace echo {

std::shared_ptr<MicCapture> MicCapture::Start(uint64_t handle, uint32_t level) {
    auto mic = std::make_shared<MicCapture>(handle, level);
    mic->OpenAsync();
    return mic;
}

MicCapture::MicCapture(uint64_t handle, uint32_t level) : m_handle(handle) {
    SetLevel(level);
}

MicCapture::~MicCapture() {
    Stop();
}

void MicCapture::SetLevel(uint32_t level) noexcept {
    m_gain.store(static_cast<float>(std::min<uint32_t>(level, 100)) / 50.0f,
                 std::memory_order_relaxed);
}

void MicCapture::SetState(std::wstring state) {
    std::lock_guard<std::mutex> lk(m_stateMutex);
    m_state = std::move(state);
}

// ── Open ────────────────────────────────────────────────────────────────────
//
// Every await is followed by a stopped-check, because Stop() can land at any
// of them -- a user can drag the slider to zero, or end the stream, while the
// consent prompt is still on screen. Whatever was built by then is closed here
// rather than leaked.
fire_and_forget MicCapture::OpenAsync() {
    auto self = shared_from_this();
    // The first microphone access raises the consent prompt, which belongs to
    // the UI thread. Start() is called there; come back to it for the call
    // that triggers the prompt.
    apartment_context ui;

    AudioGraph graph{ nullptr };
    try {
        AudioGraphSettings settings(AudioRenderCategory::GameChat);
        settings.QuantumSizeSelectionMode(QuantumSizeSelectionMode::LowestLatency);
        auto created = co_await AudioGraph::CreateAsync(settings);
        if (created.Status() != AudioGraphCreationStatus::Success) {
            SetState(std::wstring(L"OFF - audio graph: ") + GraphStatus(created.Status()));
            co_return;
        }
        graph = created.Graph();

        // Other first, Communications second. Communications is the voice-
        // chat category, but on Windows opening a communications stream can
        // trigger the system's ducking of OTHER audio -- which here is the game
        // audio this same app is playing. The handoff's open question was
        // whether the headset is reachable at all while party chat holds it;
        // the report line names whichever category answered, so the first
        // session on hardware settles it.
        AudioDeviceInputNode input{ nullptr };
        const wchar_t* category = L"";
        std::wstring lastRefusal;
        for (auto cat : { MediaCategory::Other, MediaCategory::Communications }) {
            co_await ui;   // every attempt may be the one that prompts
            auto node = co_await graph.CreateDeviceInputNodeAsync(cat);
            if (node.Status() == AudioDeviceNodeCreationStatus::Success) {
                input = node.DeviceInputNode();
                category = cat == MediaCategory::Other ? L"Other" : L"Communications";
                break;
            }
            lastRefusal = NodeStatus(node.Status());
            // Access denied is the user's answer, not a category problem.
            if (node.Status() == AudioDeviceNodeCreationStatus::AccessDenied) break;
        }
        if (!input) {
            graph.Close();
            SetState(L"OFF - " + lastRefusal);
            co_return;
        }

        // The frame output node at the GRAPH's format. Asking it for 48 kHz
        // mono directly would be neater, but nothing documents that a frame
        // output node converts, and a wrong guess is silence with no error.
        // Downmix and (if ever needed) resample are done in Consume instead.
        auto output = graph.CreateFrameOutputNode();
        input.AddOutgoingConnection(output);
        const auto format = graph.EncodingProperties();
        m_graphRate = format.SampleRate();
        m_graphChannels = std::max<uint32_t>(1, format.ChannelCount());

        int err = OPUS_OK;
        OpusEncoder* encoder = opus_encoder_create(kRate, 1, OPUS_APPLICATION_VOIP, &err);
        if (err != OPUS_OK || !encoder) {
            graph.Close();
            SetState(L"OFF - opus_encoder_create failed (" + std::to_wstring(err) + L")");
            co_return;
        }
        opus_encoder_ctl(encoder, OPUS_SET_BITRATE(kBitrate));
        opus_encoder_ctl(encoder, OPUS_SET_SIGNAL(OPUS_SIGNAL_VOICE));
        // No DTX: the host's jitter buffer expects a packet every 20 ms, the
        // same cadence Android's MediaCodec encoder produces.
        opus_encoder_ctl(encoder, OPUS_SET_DTX(0));

        {
            std::lock_guard<std::mutex> lk(m_mutex);
            if (m_stopped) {
                opus_encoder_destroy(encoder);
                graph.Close();
                co_return;
            }
            m_encoder = encoder;
            m_graph = graph;
            m_output = output;
            m_quantumToken = graph.QuantumStarted([weak = weak_from_this()](auto&&, auto&&) {
                if (auto mic = weak.lock()) mic->OnQuantum();
            });
            graph.Start();
        }

        wchar_t line[200]{};
        swprintf_s(line, L"capturing (%ls, graph %u Hz x%u, %u-sample quantum) -> Opus 48 kHz mono %d kbps",
                   category, m_graphRate, m_graphChannels,
                   static_cast<uint32_t>(graph.SamplesPerQuantum()), kBitrate / 1000);
        SetState(line);
    } catch (hresult_error const& e) {
        if (graph) {
            std::lock_guard<std::mutex> lk(m_mutex);
            if (m_graph != graph) graph.Close();
        }
        wchar_t code[32]{};
        swprintf_s(code, L"0x%08X ", static_cast<uint32_t>(e.code()));
        SetState(std::wstring(L"OFF - ") + code + e.message().c_str());
    } catch (...) {
        SetState(L"OFF - unexpected exception while opening");
    }
}

void MicCapture::Stop() noexcept {
    AudioGraph graph{ nullptr };
    winrt::event_token token{};
    OpusEncoder* encoder = nullptr;
    {
        // After this block no send can start: OnQuantum checks m_stopped
        // under the same mutex, and an in-flight one holds it until done.
        std::lock_guard<std::mutex> lk(m_mutex);
        if (m_stopped) return;
        m_stopped = true;
        m_handle = 0;
        graph = std::exchange(m_graph, nullptr);
        m_output = nullptr;
        token = m_quantumToken;
        encoder = std::exchange(m_encoder, nullptr);
    }
    // Outside the lock: closing a graph may wait for its thread, and that
    // thread may be waiting on the lock to find m_stopped.
    if (graph) {
        try {
            graph.QuantumStarted(token);
            graph.Stop();
            graph.Close();
        } catch (...) {
        }
    }
    if (encoder) opus_encoder_destroy(encoder);
    std::lock_guard<std::mutex> lk(m_stateMutex);
    if (m_state.rfind(L"OFF", 0) != 0) m_state = L"stopped";
}

// ── The graph thread ────────────────────────────────────────────────────────

void MicCapture::OnQuantum() noexcept {
    std::lock_guard<std::mutex> lk(m_mutex);
    if (m_stopped || !m_output) return;
    try {
        AudioFrame frame = m_output.GetFrame();
        AudioBuffer buffer = frame.LockBuffer(AudioBufferAccessMode::Read);
        auto reference = buffer.CreateReference();
        uint8_t* data = nullptr;
        uint32_t capacity = 0;
        check_hresult(reference.as<IMemoryBufferByteAccess>()->GetBuffer(&data, &capacity));
        const uint32_t bytes = std::min(buffer.Length(), capacity);
        const uint32_t frames = bytes / (sizeof(float) * m_graphChannels);
        if (data && frames) {
            Consume(reinterpret_cast<const float*>(data), frames, m_graphChannels);
        }
        reference.Close();
        buffer.Close();
    } catch (...) {
        m_encodeErrors.fetch_add(1, std::memory_order_relaxed);
    }
}

void MicCapture::Consume(const float* samples, uint32_t frames, uint32_t channels) noexcept {
    // Downmix. A headset microphone is mono in practice; a graph at the render
    // device's stereo format carries it on both channels, so averaging is
    // exact for that case and sensible for any other.
    m_mono.resize(frames);
    const float inv = 1.0f / static_cast<float>(channels);
    for (uint32_t i = 0; i < frames; ++i) {
        float sum = 0.0f;
        for (uint32_t c = 0; c < channels; ++c) sum += samples[i * channels + c];
        m_mono[i] = sum * inv;
    }

    if (m_graphRate == kRate) {
        m_pending.insert(m_pending.end(), m_mono.begin(), m_mono.end());
    } else {
        // Linear resample to 48 kHz. Never expected on a console (the mix
        // format is 48 kHz), and adequate for speech where it is. Position 0
        // is the previous quantum's last sample, so blocks join seamlessly.
        const double step = static_cast<double>(m_graphRate) / kRate;
        while (m_resamplePos + 1.0 <= frames) {
            const auto k = static_cast<uint32_t>(m_resamplePos);
            const float frac = static_cast<float>(m_resamplePos - k);
            const float s0 = k == 0 ? m_resamplePrev : m_mono[k - 1];
            const float s1 = m_mono[k];
            m_pending.push_back(s0 + (s1 - s0) * frac);
            m_resamplePos += step;
        }
        m_resamplePos -= frames;
        m_resamplePrev = m_mono[frames - 1];
    }

    size_t offset = 0;
    while (m_pending.size() - offset >= kFrameSamples) {
        EncodeAndSend(m_pending.data() + offset);
        offset += kFrameSamples;
    }
    m_pending.erase(m_pending.begin(), m_pending.begin() + offset);
}

void MicCapture::EncodeAndSend(const float* pcm) noexcept {
    float frame[kFrameSamples];
    const float gain = m_gain.load(std::memory_order_relaxed);
    float peak = 0.0f;
    for (uint32_t i = 0; i < kFrameSamples; ++i) {
        const float s = std::clamp(pcm[i] * gain, -1.0f, 1.0f);
        peak = std::max(peak, std::fabs(s));
        frame[i] = s;
    }

    // Peak over ~1 s, as dBFS: the number that answers "is the microphone
    // hearing anything at all" -- the VB-CABLE lesson in CLAUDE.md, where a
    // healthy device delivered perfect digital silence and nothing said so.
    m_peakWindow = std::max(m_peakWindow, peak);
    if (++m_peakFrames >= 50) {
        const float db = m_peakWindow > 0.0f ? 20.0f * std::log10(m_peakWindow) : -120.0f;
        m_peakDbfs.store(static_cast<int32_t>(std::lround(std::max(db, -120.0f))),
                         std::memory_order_relaxed);
        m_peakWindow = 0.0f;
        m_peakFrames = 0;
    }

    const opus_int32 len = opus_encode_float(m_encoder, frame, kFrameSamples, m_packet.data(),
                                             static_cast<opus_int32>(m_packet.size()));
    if (len <= 0) {
        m_encodeErrors.fetch_add(1, std::memory_order_relaxed);
        return;
    }
    if (echo_send_mic(m_handle, m_packet.data(), len)) {
        m_packets.fetch_add(1, std::memory_order_relaxed);
        m_bytes.fetch_add(static_cast<uint64_t>(len), std::memory_order_relaxed);
    } else {
        m_refused.fetch_add(1, std::memory_order_relaxed);
    }
}

std::wstring MicCapture::Report() const {
    std::wstring state;
    {
        std::lock_guard<std::mutex> lk(m_stateMutex);
        state = m_state;
    }
    wchar_t counts[200]{};
    swprintf_s(counts, L"\n          sent %llu (%llu KB)   refused %llu   encode errors %llu   peak %d dBFS   gain %.2fx",
               static_cast<unsigned long long>(m_packets.load()),
               static_cast<unsigned long long>(m_bytes.load() / 1024),
               static_cast<unsigned long long>(m_refused.load()),
               static_cast<unsigned long long>(m_encodeErrors.load()),
               m_peakDbfs.load(),
               static_cast<double>(m_gain.load()));
    return state + counts;
}

}  // namespace echo
