#include "pch.h"
#include "GameAudio.h"

#include <xaudio2.h>
#include <cstring>

extern "C" {
#include <libavcodec/avcodec.h>
#include <libavutil/channel_layout.h>
#include <libavutil/frame.h>
}

#pragma comment(lib, "xaudio2.lib")
#pragma comment(lib, "avcodec.lib")
#pragma comment(lib, "avutil.lib")

namespace echo {

// OnBufferEnd is the render loop's clock: each finished buffer is one step it
// may pull from the jitter buffer. Everything else XAudio2 reports is ignored.
struct GameAudio::Callback final : IXAudio2VoiceCallback {
    HANDLE event = nullptr;
    void STDMETHODCALLTYPE OnBufferEnd(void*) noexcept override { SetEvent(event); }
    void STDMETHODCALLTYPE OnVoiceProcessingPassStart(UINT32) noexcept override {}
    void STDMETHODCALLTYPE OnVoiceProcessingPassEnd() noexcept override {}
    void STDMETHODCALLTYPE OnStreamEnd() noexcept override {}
    void STDMETHODCALLTYPE OnBufferStart(void*) noexcept override {}
    void STDMETHODCALLTYPE OnLoopEnd(void*) noexcept override {}
    void STDMETHODCALLTYPE OnVoiceError(void*, HRESULT) noexcept override {}
};

namespace {

std::wstring Hr(const wchar_t* what, HRESULT hr) {
    wchar_t buf[160]{};
    swprintf_s(buf, L"%s failed (0x%08X)", what, static_cast<unsigned>(hr));
    return buf;
}

// RFC 7845 identification header for a 2-channel, mapping-family-0 stream. The
// decoder accepts a stereo stream without it, but handing it over makes the
// stream's shape explicit instead of inferred from `ch_layout`. Pre-skip is 0:
// this is a live stream, there is no encoder priming to trim.
constexpr uint8_t kOpusHead[19] = {
    'O', 'p', 'u', 's', 'H', 'e', 'a', 'd',
    1,                       // version
    2,                       // channels
    0, 0,                    // pre-skip (LE)
    0x80, 0xBB, 0x00, 0x00,  // input sample rate 48000 (LE)
    0, 0,                    // output gain
    0,                       // channel mapping family
};

}  // namespace

GameAudio::~GameAudio() { Close(); }

bool GameAudio::Open(std::wstring& error) noexcept {
    // ── Decoder ────────────────────────────────────────────────────────────
    const AVCodec* opus = avcodec_find_decoder(AV_CODEC_ID_OPUS);
    if (!opus) {
        error = L"this FFmpeg build has no Opus decoder - rebuild it with build-ffmpeg.ps1";
        return false;
    }
    m_codec = avcodec_alloc_context3(opus);
    m_packet = av_packet_alloc();
    m_frame = av_frame_alloc();
    if (!m_codec || !m_packet || !m_frame) { error = L"FFmpeg allocation failed"; Close(); return false; }

    m_codec->sample_rate = kRate;
    av_channel_layout_default(&m_codec->ch_layout, kChannels);
    m_codec->extradata = static_cast<uint8_t*>(av_mallocz(sizeof(kOpusHead) + AV_INPUT_BUFFER_PADDING_SIZE));
    if (m_codec->extradata) {
        std::memcpy(m_codec->extradata, kOpusHead, sizeof(kOpusHead));
        m_codec->extradata_size = sizeof(kOpusHead);
    }
    if (const int rc = avcodec_open2(m_codec, opus, nullptr); rc < 0) {
        wchar_t buf[96]{};
        swprintf_s(buf, L"avcodec_open2(opus) failed (%d)", rc);
        error = buf;
        Close();
        return false;
    }

    // ── Voice ──────────────────────────────────────────────────────────────
    HRESULT hr = XAudio2Create(&m_xaudio, 0, XAUDIO2_DEFAULT_PROCESSOR);
    if (FAILED(hr)) { error = Hr(L"XAudio2Create", hr); Close(); return false; }
    hr = m_xaudio->CreateMasteringVoice(&m_master);
    if (FAILED(hr)) { error = Hr(L"CreateMasteringVoice", hr); Close(); return false; }

    m_bufferEnd = CreateEventExW(nullptr, nullptr, 0, EVENT_ALL_ACCESS);
    m_callback = new (std::nothrow) Callback();
    if (!m_bufferEnd || !m_callback) { error = L"could not create the buffer-end event"; Close(); return false; }
    m_callback->event = static_cast<HANDLE>(m_bufferEnd);

    WAVEFORMATEX wfx{};
    wfx.wFormatTag = WAVE_FORMAT_IEEE_FLOAT;
    wfx.nChannels = kChannels;
    wfx.nSamplesPerSec = kRate;
    wfx.wBitsPerSample = 32;
    wfx.nBlockAlign = static_cast<WORD>(kChannels * sizeof(float));
    wfx.nAvgBytesPerSec = kRate * wfx.nBlockAlign;
    hr = m_xaudio->CreateSourceVoice(&m_voice, &wfx, 0, XAUDIO2_DEFAULT_FREQ_RATIO, m_callback);
    if (FAILED(hr)) { error = Hr(L"CreateSourceVoice", hr); Close(); return false; }

    for (auto& slot : m_ring) slot.assign(kMaxSamples * kChannels, 0.0f);
    m_packetBytes.assign(4096 + AV_INPUT_BUFFER_PADDING_SIZE, 0);

    hr = m_voice->Start(0);
    if (FAILED(hr)) { error = Hr(L"IXAudio2SourceVoice::Start", hr); Close(); return false; }
    return true;
}

void GameAudio::Close() noexcept {
    // The source voice first: destroying it stops callbacks, and only then is
    // it safe to free the callback object and the event it signals.
    if (m_voice)  { m_voice->DestroyVoice();  m_voice = nullptr; }
    if (m_master) { m_master->DestroyVoice(); m_master = nullptr; }
    if (m_xaudio) { m_xaudio->Release();      m_xaudio = nullptr; }
    delete m_callback;
    m_callback = nullptr;
    if (m_bufferEnd) { CloseHandle(static_cast<HANDLE>(m_bufferEnd)); m_bufferEnd = nullptr; }

    if (m_frame)  av_frame_free(&m_frame);
    if (m_packet) av_packet_free(&m_packet);
    if (m_codec)  avcodec_free_context(&m_codec);   // frees extradata too
}

uint32_t GameAudio::Queued() const noexcept {
    if (!m_voice) return 0;
    XAUDIO2_VOICE_STATE state{};
    m_voice->GetState(&state, XAUDIO2_VOICE_NOSAMPLESPLAYED);
    return state.BuffersQueued;
}

bool GameAudio::Queue(const float* interleaved, uint32_t frames) noexcept {
    if (!m_voice || frames == 0 || frames > kMaxSamples) return false;
    auto& slot = m_ring[m_next];
    m_next = (m_next + 1) % kRing;
    std::memcpy(slot.data(), interleaved, frames * kChannels * sizeof(float));

    XAUDIO2_BUFFER buffer{};
    buffer.AudioBytes = frames * kChannels * sizeof(float);
    buffer.pAudioData = reinterpret_cast<const BYTE*>(slot.data());
    return SUCCEEDED(m_voice->SubmitSourceBuffer(&buffer));
}

bool GameAudio::SubmitPacket(const uint8_t* data, size_t size) noexcept {
    if (!m_codec || !data || size == 0) return false;
    if (m_packetBytes.size() < size + AV_INPUT_BUFFER_PADDING_SIZE) {
        m_packetBytes.assign(size + AV_INPUT_BUFFER_PADDING_SIZE, 0);
    }
    std::memcpy(m_packetBytes.data(), data, size);
    std::memset(m_packetBytes.data() + size, 0, AV_INPUT_BUFFER_PADDING_SIZE);
    m_packet->data = m_packetBytes.data();
    m_packet->size = static_cast<int>(size);

    if (avcodec_send_packet(m_codec, m_packet) < 0) {
        m_errors.fetch_add(1, std::memory_order_relaxed);
        return false;
    }

    // Interleave into a scratch slot of the ring and queue THAT slot, so a
    // decoded frame costs exactly one copy.
    bool queued = false;
    while (avcodec_receive_frame(m_codec, m_frame) == 0) {
        const uint32_t frames = static_cast<uint32_t>(m_frame->nb_samples);
        const int channels = m_frame->ch_layout.nb_channels;
        if (frames == 0 || frames > kMaxSamples || channels < 1) {
            av_frame_unref(m_frame);
            continue;
        }
        auto& slot = m_ring[m_next];
        m_next = (m_next + 1) % kRing;
        float* out = slot.data();
        if (m_frame->format == AV_SAMPLE_FMT_FLTP) {
            const float* l = reinterpret_cast<const float*>(m_frame->extended_data[0]);
            const float* r = reinterpret_cast<const float*>(m_frame->extended_data[channels > 1 ? 1 : 0]);
            for (uint32_t i = 0; i < frames; ++i) { out[2 * i] = l[i]; out[2 * i + 1] = r[i]; }
        } else if (m_frame->format == AV_SAMPLE_FMT_FLT) {
            const float* in = reinterpret_cast<const float*>(m_frame->data[0]);
            for (uint32_t i = 0; i < frames; ++i) {
                out[2 * i]     = in[i * channels];
                out[2 * i + 1] = in[i * channels + (channels > 1 ? 1 : 0)];
            }
        } else {
            m_errors.fetch_add(1, std::memory_order_relaxed);   // unexpected sample format
            av_frame_unref(m_frame);
            continue;
        }
        av_frame_unref(m_frame);

        XAUDIO2_BUFFER buffer{};
        buffer.AudioBytes = frames * kChannels * sizeof(float);
        buffer.pAudioData = reinterpret_cast<const BYTE*>(out);
        if (SUCCEEDED(m_voice->SubmitSourceBuffer(&buffer))) {
            queued = true;
            m_decoded.fetch_add(1, std::memory_order_relaxed);
        }
    }
    if (!queued) m_errors.fetch_add(1, std::memory_order_relaxed);
    return queued;
}

void GameAudio::SubmitSilence() noexcept {
    static const std::array<float, kFrameSamples * kChannels> kSilence{};
    if (Queue(kSilence.data(), kFrameSamples)) m_silent.fetch_add(1, std::memory_order_relaxed);
}

void GameAudio::WaitForBufferEnd(uint32_t timeoutMs) noexcept {
    if (m_bufferEnd) WaitForSingleObjectEx(static_cast<HANDLE>(m_bufferEnd), timeoutMs, FALSE);
}

}  // namespace echo
