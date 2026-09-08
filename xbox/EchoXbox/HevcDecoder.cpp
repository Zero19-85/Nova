#include "pch.h"
#include "HevcDecoder.h"

#include <mferror.h>
#include <codecapi.h>
#include <icodecapi.h>

#pragma comment(lib, "mfplat.lib")
#pragma comment(lib, "mfuuid.lib")

using namespace winrt;

namespace echo {
namespace {

std::wstring Hr(const wchar_t* what, HRESULT hr) {
    wchar_t buf[160]{};
    swprintf_s(buf, L"%s failed: 0x%08X", what, static_cast<unsigned>(hr));
    return buf;
}

}  // namespace

HevcDecoder::~HevcDecoder() { Shutdown(); }

bool HevcDecoder::Initialize(ID3D11Device* device, uint32_t width, uint32_t height,
                             std::wstring& error) noexcept {
    m_device.copy_from(device);
    m_width = width;
    m_height = height;

    // MFSTARTUP_LITE: we want the platform, not the full media session, the
    // source resolver or the topology loader. None of that is on this path.
    HRESULT hr = MFStartup(MF_VERSION, MFSTARTUP_LITE);
    if (FAILED(hr)) { error = Hr(L"MFStartup", hr); return false; }
    m_mfStarted = true;

    // ── The DXGI device manager ─────────────────────────────────────────────
    // This is what hands the decoder our D3D11 device, so decoded surfaces are
    // already on the GPU the renderer draws with.
    hr = MFCreateDXGIDeviceManager(&m_resetToken, m_deviceManager.put());
    if (FAILED(hr)) { error = Hr(L"MFCreateDXGIDeviceManager", hr); return false; }
    hr = m_deviceManager->ResetDevice(device, m_resetToken);
    if (FAILED(hr)) { error = Hr(L"IMFDXGIDeviceManager::ResetDevice", hr); return false; }

    // ── Find a hardware HEVC decoder ────────────────────────────────────────
    MFT_REGISTER_TYPE_INFO inputType{ MFMediaType_Video, MFVideoFormat_HEVC };
    IMFActivate** activates = nullptr;
    UINT32 count = 0;

    // SORTANDFILTER puts the preferred (hardware, non-blocked) transform first.
    UINT32 flags = MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER;
    hr = MFTEnumEx(MFT_CATEGORY_VIDEO_DECODER, flags, &inputType, nullptr,
                   &activates, &count);

    if (FAILED(hr) || count == 0) {
        // No hardware HEVC. Worth trying software rather than giving up: it
        // will not keep up at 4K, but it turns "nothing on screen" into "a
        // slow picture", which is a far better diagnostic.
        if (activates) { CoTaskMemFree(activates); activates = nullptr; }
        flags = MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_ASYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER;
        hr = MFTEnumEx(MFT_CATEGORY_VIDEO_DECODER, flags, &inputType, nullptr,
                       &activates, &count);
        if (FAILED(hr) || count == 0) {
            if (activates) CoTaskMemFree(activates);
            error = L"no HEVC decoder is registered on this console";
            return false;
        }
    } else {
        m_hardware = true;
    }

    com_ptr<IMFActivate> activate;
    activate.copy_from(activates[0]);

    // Name it, so the overlay can say which decoder is actually running —
    // "hardware" is a claim worth being able to check.
    {
        LPWSTR friendly = nullptr;
        UINT32 len = 0;
        if (SUCCEEDED(activate->GetAllocatedString(MFT_FRIENDLY_NAME_Attribute, &friendly, &len))) {
            m_name = friendly;
            CoTaskMemFree(friendly);
        }
    }

    for (UINT32 i = 0; i < count; ++i) activates[i]->Release();
    CoTaskMemFree(activates);

    hr = activate->ActivateObject(IID_PPV_ARGS(m_transform.put()));
    if (FAILED(hr)) { error = Hr(L"IMFActivate::ActivateObject", hr); return false; }

    // ── Unlock async MFTs ───────────────────────────────────────────────────
    // Hardware decoders are usually asynchronous and refuse to do anything at
    // all until told the caller knows that. This is a no-op on a sync MFT.
    if (auto attributes = com_ptr<IMFAttributes>{}; true) {
        if (SUCCEEDED(m_transform->GetAttributes(attributes.put())) && attributes) {
            UINT32 isAsync = 0;
            attributes->GetUINT32(MF_TRANSFORM_ASYNC, &isAsync);
            if (isAsync) {
                attributes->SetUINT32(MF_TRANSFORM_ASYNC_UNLOCK, TRUE);
            }
            // THE latency lever. Without it the decoder buffers for reordering
            // that a B-frame-free stream never needs.
            attributes->SetUINT32(MF_LOW_LATENCY, TRUE);
        }
    }

    // Same request through the codec API, because not every decoder honours the
    // attribute and not every decoder honours the property — ask both ways.
    if (auto codecApi = m_transform.try_as<ICodecAPI>()) {
        VARIANT v{};
        v.vt = VT_BOOL;
        v.boolVal = VARIANT_TRUE;
        codecApi->SetValue(&CODECAPI_AVLowLatencyMode, &v);
    }

    // ── Hand it the device, BEFORE media types ──────────────────────────────
    hr = m_transform->ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER,
                                     reinterpret_cast<ULONG_PTR>(m_deviceManager.get()));
    if (FAILED(hr)) {
        // Not fatal on a software decoder — it simply has no D3D manager to
        // set — but on a hardware one it means we are about to decode on the
        // CPU without noticing.
        if (m_hardware) { error = Hr(L"MFT_MESSAGE_SET_D3D_MANAGER", hr); return false; }
    }

    if (!ConfigureTypes(width, height, error)) return false;

    m_transform->ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
    m_transform->ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
    m_started = true;
    return true;
}

bool HevcDecoder::ConfigureTypes(uint32_t width, uint32_t height,
                                 std::wstring& error) noexcept {
    com_ptr<IMFMediaType> input;
    HRESULT hr = MFCreateMediaType(input.put());
    if (FAILED(hr)) { error = Hr(L"MFCreateMediaType", hr); return false; }

    input->SetGUID(MF_MT_MAJOR_TYPE, MFMediaType_Video);
    // Annex-B byte stream. See the header: the _ES variant decodes nothing.
    input->SetGUID(MF_MT_SUBTYPE, MFVideoFormat_HEVC);
    input->SetUINT32(MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive);
    MFSetAttributeSize(input.get(), MF_MT_FRAME_SIZE, width, height);
    // Nominal. The stream is live and its real cadence varies; this only helps
    // the decoder size its internal pool.
    MFSetAttributeRatio(input.get(), MF_MT_FRAME_RATE, 60, 1);
    MFSetAttributeRatio(input.get(), MF_MT_PIXEL_ASPECT_RATIO, 1, 1);

    hr = m_transform->SetInputType(0, input.get(), 0);
    if (FAILED(hr)) { error = Hr(L"SetInputType(HEVC)", hr); return false; }

    return NegotiateOutputType(error);
}

bool HevcDecoder::NegotiateOutputType(std::wstring& error) noexcept {
    // Walk what the decoder offers and take NV12 — the format every hardware
    // HEVC decoder produces natively and the one the video processor converts
    // from without a second pass.
    for (DWORD i = 0;; ++i) {
        com_ptr<IMFMediaType> candidate;
        HRESULT hr = m_transform->GetOutputAvailableType(0, i, candidate.put());
        if (hr == MF_E_NO_MORE_TYPES) break;
        if (FAILED(hr)) { error = Hr(L"GetOutputAvailableType", hr); return false; }

        GUID subtype{};
        if (SUCCEEDED(candidate->GetGUID(MF_MT_SUBTYPE, &subtype)) &&
            subtype == MFVideoFormat_NV12) {
            hr = m_transform->SetOutputType(0, candidate.get(), 0);
            if (FAILED(hr)) { error = Hr(L"SetOutputType(NV12)", hr); return false; }
            UINT32 w = 0, h = 0;
            if (SUCCEEDED(MFGetAttributeSize(candidate.get(), MF_MT_FRAME_SIZE, &w, &h))) {
                m_width = w;
                m_height = h;
            }
            return true;
        }
    }
    error = L"the decoder offered no NV12 output type";
    return false;
}

bool HevcDecoder::Submit(const uint8_t* data, uint32_t length, bool keyframe,
                         bool discontinuity, int64_t pts100ns) noexcept {
    if (!m_started || !data || length == 0) return false;

    com_ptr<IMFMediaBuffer> buffer;
    if (FAILED(MFCreateMemoryBuffer(length, buffer.put()))) return false;

    // One copy, into a buffer the decoder owns. `echo_fill_buffer` could write
    // here directly, but it is called from the network thread and this buffer
    // must be sized first — so the copy is a memcpy of an already-assembled
    // frame, not an extra trip through the network stack.
    BYTE* dst = nullptr;
    DWORD maxLen = 0;
    if (FAILED(buffer->Lock(&dst, &maxLen, nullptr))) return false;
    memcpy(dst, data, length);
    buffer->Unlock();
    buffer->SetCurrentLength(length);

    com_ptr<IMFSample> sample;
    if (FAILED(MFCreateSample(sample.put()))) return false;
    sample->AddBuffer(buffer.get());
    sample->SetSampleTime(pts100ns);

    if (keyframe) {
        sample->SetUINT32(MFSampleExtension_CleanPoint, TRUE);
    }
    if (discontinuity) {
        // Tells the decoder the reference chain is broken here, so it does not
        // conceal against pictures it never received — which is what produces
        // the smearing that "slowly corrects itself".
        sample->SetUINT32(MFSampleExtension_Discontinuity, TRUE);
    }

    std::lock_guard<std::mutex> guard(m_lock);
    HRESULT hr = m_transform->ProcessInput(0, sample.get(), 0);
    if (hr == MF_E_NOTACCEPTING) {
        // The decoder is full because nobody has drained it fast enough.
        //
        // Dropping the incoming frame here — which is what this used to do —
        // is exactly backwards: it discards the NEWEST picture in order to
        // preserve a queue of older ones, so the backlog is never paid off and
        // every frame from here on is late by however deep the queue got.
        //
        // Throw away what is queued instead and take the new frame. The stale
        // pictures were going to be skipped by `TryGetFrame` anyway; doing it
        // here is what stops the decoder wedging in the meantime.
        DiscardPendingOutputs();
        hr = m_transform->ProcessInput(0, sample.get(), 0);
    }
    return SUCCEEDED(hr);
}

// ── Never hold a queue of decoded frames ────────────────────────────────────
//
// A Media Foundation decoder will happily accumulate output samples, and a
// renderer that takes exactly one per present drains at the display's refresh
// rate and no faster. Any burst — a network hiccup, a stream that runs above
// the panel's refresh, one slow present — then becomes PERMANENT latency: the
// backlog can never shrink, because nothing in that arrangement ever consumes
// two frames in the time it takes to show one.
//
// The symptom is exact: video that lags input, then visibly races to catch up.
//
// So the drop policy lives here, at the queue, rather than in the renderer.
// `TryGetFrame` returns the NEWEST decoded picture and throws away anything the
// decoder was still holding behind it. A superseded frame has no value on a
// live stream — showing it costs a frame interval and displays something that
// was already wrong when it was decoded.

void HevcDecoder::DiscardPendingOutputs() noexcept {
    com_ptr<ID3D11Texture2D> discard;
    uint32_t index = 0;
    for (int i = 0; i < kMaxDrain; ++i) {
        if (!PullOne(discard, index)) break;
        discard = nullptr;
        ++m_dropped;
    }
}

bool HevcDecoder::PullOne(com_ptr<ID3D11Texture2D>& texture,
                          uint32_t& subresource) noexcept {
    MFT_OUTPUT_STREAM_INFO info{};
    if (FAILED(m_transform->GetOutputStreamInfo(0, &info))) return false;

    MFT_OUTPUT_DATA_BUFFER output{};
    output.dwStreamID = 0;

    // A decoder that allocates its own samples (every hardware one) wants a
    // null sample here and hands us one of its pool. Providing our own would
    // force it to copy into system memory.
    const bool providesSamples =
        (info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES |
                         MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES)) != 0;
    if (!providesSamples) {
        if (!m_outputSample) {
            if (FAILED(MFCreateSample(m_outputSample.put()))) return false;
            com_ptr<IMFMediaBuffer> buffer;
            if (FAILED(MFCreateMemoryBuffer(info.cbSize, buffer.put()))) return false;
            m_outputSample->AddBuffer(buffer.get());
        }
        output.pSample = m_outputSample.get();
    }

    DWORD status = 0;
    HRESULT hr = m_transform->ProcessOutput(0, 1, &output, &status);

    if (hr == MF_E_TRANSFORM_NEED_MORE_INPUT) {
        return false;  // normal: nothing decoded yet
    }
    if (hr == MF_E_TRANSFORM_STREAM_CHANGE) {
        // The stream's geometry changed under us — a resolution renegotiation
        // host-side. Re-run output negotiation and try again next tick.
        //
        // Dropping the sample is not tidiness: its buffer was allocated for the
        // OLD `cbSize`, and the branch above reuses `m_outputSample` whenever it
        // exists. Growing from 1080p to 4K on the software path would otherwise
        // hand the decoder a buffer a quarter of the size it now needs, on every
        // subsequent call, forever. Hardware decoders provide their own samples
        // and never take this branch — which is exactly why it would have gone
        // unnoticed until somebody streamed to a machine without one.
        m_outputSample = nullptr;
        std::wstring ignored;
        NegotiateOutputType(ignored);
        if (output.pEvents) output.pEvents->Release();
        return false;
    }
    if (FAILED(hr) || !output.pSample) {
        if (output.pEvents) output.pEvents->Release();
        return false;
    }

    com_ptr<IMFSample> sample;
    sample.attach(output.pSample);          // we own it now
    if (output.pEvents) output.pEvents->Release();

    com_ptr<IMFMediaBuffer> buffer;
    if (FAILED(sample->GetBufferByIndex(0, buffer.put()))) return false;

    // The whole point of the D3D manager: the buffer IS a D3D11 texture.
    auto dxgiBuffer = buffer.try_as<IMFDXGIBuffer>();
    if (!dxgiBuffer) return false;

    com_ptr<ID3D11Texture2D> decoded;
    if (FAILED(dxgiBuffer->GetResource(IID_PPV_ARGS(decoded.put())))) return false;

    UINT index = 0;
    dxgiBuffer->GetSubresourceIndex(&index);

    texture = decoded;
    subresource = index;
    ++m_decoded;
    return true;
}

bool HevcDecoder::TryGetFrame(com_ptr<ID3D11Texture2D>& texture,
                              uint32_t& subresource) noexcept {
    if (!m_started) return false;

    std::lock_guard<std::mutex> guard(m_lock);

    // Pull until the decoder says NEED_MORE_INPUT, keeping only the last one.
    //
    // Deliberately NOT gated on `GetOutputStatus`. A hardware MFT is not
    // required to implement it and the Xbox HEVC decoder does not: it answers
    // E_NOTIMPL, which reads as "nothing ready" and is indistinguishable from
    // an empty decoder. Asking is therefore worse than not asking, because the
    // only reliable way to learn whether a frame is waiting is to ask for it.
    //
    // Each attempt pulls into a FRESH `next` rather than straight into the
    // reference the caller gave us. `PullOne` writes as it goes, so a failed
    // pull on the last iteration could otherwise overwrite a good frame with a
    // half-filled one.
    bool got = false;
    for (int i = 0; i < kMaxDrain; ++i) {
        com_ptr<ID3D11Texture2D> next;
        uint32_t nextIndex = 0;
        if (!PullOne(next, nextIndex)) break;
        if (got) ++m_dropped;   // the one we were holding is superseded
        texture = next;
        subresource = nextIndex;
        got = true;
    }
    return got;
}

void HevcDecoder::Shutdown() noexcept {
    {
        std::lock_guard<std::mutex> guard(m_lock);
        if (m_transform) {
            m_transform->ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            m_transform->ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
        m_started = false;
        m_outputSample = nullptr;
        m_transform = nullptr;
        m_deviceManager = nullptr;
        m_device = nullptr;
    }
    if (m_mfStarted) {
        MFShutdown();
        m_mfStarted = false;
    }
}

}  // namespace echo
