#include "pch.h"
#include "VideoRenderer.h"

#include <windows.ui.xaml.media.dxinterop.h>   // ISwapChainPanelNative
#include <winrt/Windows.Graphics.Display.Core.h>
#include <winrt/Windows.Foundation.Collections.h>

#include <cmath>
#include <vector>
#include <algorithm>

using namespace winrt;
using namespace Windows::UI::Xaml::Controls;
using namespace Windows::Graphics::Display::Core;

namespace echo {
namespace {

std::wstring DescribeMode(HdmiDisplayMode const& mode) {
    std::wstring out = std::to_wstring(mode.ResolutionWidthInRawPixels()) + L"x" +
                       std::to_wstring(mode.ResolutionHeightInRawPixels());
    wchar_t hz[40]{};
    swprintf_s(hz, L" @ %.2f Hz", mode.RefreshRate());
    out += hz;
    out += L", " + std::to_wstring(mode.BitsPerPixel()) + L" bpp";
    // What this mode IS, which is not what it SUPPORTS. `IsSmpte2084Supported`
    // says the mode is *capable* of PQ; the colour space says which variant
    // this entry actually is. Only the second answers "is the console about to
    // be driven in a colour space this app does not render in", and that
    // distinction is why the log could look right while the picture was not.
    switch (mode.ColorSpace()) {
        case HdmiDisplayColorSpace::BT2020:      out += L", BT2020"; break;
        case HdmiDisplayColorSpace::BT709:       out += L", BT709"; break;
        case HdmiDisplayColorSpace::RgbFull:     out += L", RGB full"; break;
        case HdmiDisplayColorSpace::RgbLimited:  out += L", RGB limited"; break;
    }
    if (mode.IsSmpte2084Supported())    out += L" (PQ capable)";
    if (mode.Is2086MetadataSupported()) out += L" (HDR10 capable)";
    return out;
}

}  // namespace

// ── HDMI output mode ────────────────────────────────────────────────────────

HdmiOutcome RequestBestHdmiMode(uint32_t width, uint32_t height) noexcept {
    HdmiOutcome outcome;
    HdmiDisplayInformation hdmi{ nullptr };
    try {
        hdmi = HdmiDisplayInformation::GetForCurrentView();
    } catch (...) {
    }
    if (!hdmi) {
        outcome.note = L"(not a console - HDMI mode left alone)";
        return outcome;
    }

    // Whatever happens below, the answer to "what is the TV showing" is read
    // from the console at the end, not inferred from what we asked for.
    auto readBack = [&hdmi](HdmiOutcome& out) {
        try {
            if (auto mode = hdmi.GetCurrentDisplayMode()) {
                out.width = mode.ResolutionWidthInRawPixels();
                out.height = mode.ResolutionHeightInRawPixels();
                out.refreshHz = mode.RefreshRate();
            }
        } catch (...) {
        }
    };

    try {
        auto current = hdmi.GetCurrentDisplayMode();
        const std::wstring before = current ? DescribeMode(current) : L"(unknown)";

        // Pick the highest refresh rate at the requested size. Refresh is the
        // tie-break rather than bit depth because this is a *latency* path: a
        // 120 Hz mode halves the time a finished frame waits for a scanout,
        // and that is worth more here than 10-bit colour.
        //
        // SDR modes are preferred at equal refresh, and that is not a
        // preference — it is a correctness requirement today. This pipeline is
        // SDR end to end: BGRA8 swap chain, BT.709 video processor output, no
        // PQ anywhere (see "Not done: HDR10"). `GetSupportedDisplayModes`
        // returns SDR and HDR10 variants of the SAME resolution and refresh,
        // and nothing here used to tell them apart — so the console could be
        // driven in BT.2020 PQ while the app fed it Rec.709 values. Bright
        // content survives that surprisingly well; near-black does not, which
        // is why it shows up as a background that has gone blue and washed out
        // rather than as an obviously broken picture.
        //
        // When HDR10 lands, this becomes a real choice rather than a filter.
        HdmiDisplayMode best{ nullptr };
        double bestRefresh = 0.0;
        bool bestIsSdr = false;
        for (auto const& mode : hdmi.GetSupportedDisplayModes()) {
            if (mode.ResolutionWidthInRawPixels() != width ||
                mode.ResolutionHeightInRawPixels() != height) {
                continue;
            }
            // There is no `IsSdr`. The colour space is the classification:
            // BT2020 is the wide-gamut entry the console pairs with PQ, and
            // BT709 / RgbFull / RgbLimited are the Rec.709 ones this pipeline
            // actually renders.
            const bool isSdr =
                mode.ColorSpace() != HdmiDisplayColorSpace::BT2020;
            const bool better = mode.RefreshRate() > bestRefresh ||
                                (mode.RefreshRate() == bestRefresh && isSdr && !bestIsSdr);
            if (better) {
                bestRefresh = mode.RefreshRate();
                bestIsSdr = isSdr;
                best = mode;
            }
        }

        // Every refresh rate offered at the requested size, listed once.
        //
        // `moonlight-xbox` logs the whole mode table and it is the first thing
        // worth having when a mode request disappoints: "the console refused
        // 4K120" and "this console was never offered 4K120" look identical
        // from the sofa and mean completely different things — the first is an
        // app bug, the second is Settings → General → TV & display options →
        // Video modes → Allow 4K120 being off, or a cable that cannot carry it.
        {
            std::vector<uint32_t> rates;
            for (auto const& mode : hdmi.GetSupportedDisplayModes()) {
                if (mode.ResolutionWidthInRawPixels() != width ||
                    mode.ResolutionHeightInRawPixels() != height) {
                    continue;
                }
                const auto hz = static_cast<uint32_t>(mode.RefreshRate() + 0.5);
                if (std::find(rates.begin(), rates.end(), hz) == rates.end()) rates.push_back(hz);
            }
            std::sort(rates.begin(), rates.end());
            outcome.offered = L"offered at " + std::to_wstring(width) + L"x" +
                              std::to_wstring(height) + L": ";
            if (rates.empty()) {
                outcome.offered += L"nothing";
            } else {
                for (size_t i = 0; i < rates.size(); ++i) {
                    if (i) outcome.offered += L", ";
                    outcome.offered += std::to_wstring(rates[i]) + L" Hz";
                }
            }
        }

        if (!best) {
            readBack(outcome);
            outcome.note = L"no " + std::to_wstring(width) + L"x" + std::to_wstring(height) +
                           L" mode offered; staying at " + before;
            return outcome;
        }

        // Blocking. This runs on a background thread - see the header.
        //
        // `EotfSdr` is stated rather than left to the console's discretion.
        // Choosing an SDR-colour-space mode above says which ENTRY we want;
        // this says which transfer function the console should actually drive,
        // and the two are separate knobs. Asking for both is what makes it
        // deterministic instead of dependent on whatever the console last had
        // configured — and until HDR10 exists in this pipeline, SDR is simply
        // the truth about what we are sending.
        // ── `None`, NOT `EotfSdr` ───────────────────────────────────────────
        //
        // `EotfSdr` looks like the obvious way to say "drive this SDR" and the
        // console answers it with E_INVALIDARG — thrown, not returned. That
        // took the whole function out through the catch below, so the mode was
        // never set, the console stayed at 1920x1080@60, and the swap chain
        // sized itself to match. One wrong enum value cost the app 4K entirely.
        //
        // `moonlight-xbox` uses `HdmiDisplayHdrOption::None` for SDR and
        // `Eotf2084` for HDR, and never anything else. Checked against
        // `State/MoonlightClient.cpp:137-150` rather than reasoned about.
        //
        // Each attempt gets its own try/catch, because "returns false" and
        // "throws" are both real answers here and only one of them was being
        // handled. A fallback that cannot run is not a fallback.
        const auto attempt = [&hdmi, &best](HdmiDisplayHdrOption option) noexcept {
            try {
                return hdmi.RequestSetCurrentDisplayModeAsync(best, option).get();
            } catch (...) {
                return false;
            }
        };

        bool applied = attempt(HdmiDisplayHdrOption::None);
        if (!applied) {
            // The overload without an HDR option at all: let the console keep
            // whatever transfer function it was using. Losing the transfer
            // function is a far better trade than losing the resolution.
            try {
                applied = hdmi.RequestSetCurrentDisplayModeAsync(best).get();
                if (applied) outcome.note = L"HDR option refused - took the mode as offered: ";
            } catch (...) {
                applied = false;
            }
        }
        readBack(outcome);
        outcome.note += applied ? (L"set " + DescribeMode(best) + L"  (was " + before + L")")
                                : (L"console refused " + DescribeMode(best) +
                                   L"; staying at " + before);
        // What the console is ACTUALLY driving, which is the number every later
        // decision should use. `readBack` has already filled it in from the
        // console rather than from what we asked for — see HdmiOutcome.
        {
            wchar_t actual[80]{};
            swprintf_s(actual, L"  [output now %ux%u @ %.0f Hz]",
                       outcome.width, outcome.height, outcome.refreshHz);
            outcome.note += actual;
        }
        return outcome;
    } catch (hresult_error const& e) {
        readBack(outcome);
        outcome.note = std::wstring(L"HDMI mode request failed: ") + e.message().c_str();
        return outcome;
    } catch (...) {
        readBack(outcome);
        outcome.note = L"HDMI mode request failed";
        return outcome;
    }
}

// ── Renderer ────────────────────────────────────────────────────────────────

VideoRenderer::~VideoRenderer() { Shutdown(); }

void VideoRenderer::SetFrameSource(FrameSource source) noexcept {
    std::lock_guard<std::mutex> guard(m_sourceLock);
    m_frameSource = std::move(source);
}

void VideoRenderer::SetSourceSize(uint32_t width, uint32_t height) noexcept {
    m_sourceWidth.store(width, std::memory_order_relaxed);
    m_sourceHeight.store(height, std::memory_order_relaxed);
}

DisplayFacts VideoRenderer::Initialize(SwapChainPanel const& panel) noexcept {
    DisplayFacts facts;

    try {
        // BGRA_SUPPORT for XAML composition; VIDEO_SUPPORT because the Media
        // Foundation decoder and the video processor both need it on this same
        // device — a second device would mean copying every frame through
        // system memory.
        const UINT flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
        const D3D_FEATURE_LEVEL levels[] = { D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0 };

        D3D_FEATURE_LEVEL achieved{};
        HRESULT hr = D3D11CreateDevice(
            nullptr, D3D_DRIVER_TYPE_HARDWARE, nullptr, flags,
            levels, ARRAYSIZE(levels), D3D11_SDK_VERSION,
            m_device.put(), &achieved, m_context.put());
        if (FAILED(hr)) {
            wchar_t buf[96]{};
            swprintf_s(buf, L"D3D11CreateDevice failed: 0x%08X", static_cast<unsigned>(hr));
            facts.note = buf;
            return facts;
        }

        // NOT optional: the decoder runs on its own threads and shares this
        // context. Without it the corruption looks exactly like packet loss,
        // which sends the search to the network where nothing is wrong.
        if (auto mt = m_device.try_as<ID3D10Multithread>()) {
            mt->SetMultithreadProtected(TRUE);
        }

        m_videoDevice  = m_device.try_as<ID3D11VideoDevice>();
        m_videoContext = m_context.try_as<ID3D11VideoContext>();
        m_videoContext1 = m_context.try_as<ID3D11VideoContext1>();
        if (!m_videoDevice || !m_videoContext) {
            facts.note = L"no ID3D11VideoDevice — hardware video is unavailable";
            return facts;
        }

        // Adapter name.
        if (auto dxgiDevice = m_device.try_as<IDXGIDevice>()) {
            com_ptr<IDXGIAdapter> adapter;
            if (SUCCEEDED(dxgiDevice->GetAdapter(adapter.put()))) {
                DXGI_ADAPTER_DESC desc{};
                if (SUCCEEDED(adapter->GetDesc(&desc))) facts.adapter = desc.Description;
            }
        }

        try {
            if (auto hdmi = HdmiDisplayInformation::GetForCurrentView()) {
                if (auto mode = hdmi.GetCurrentDisplayMode()) facts.hdmiMode = DescribeMode(mode);
            } else {
                facts.hdmiMode = L"(not a console)";
            }
        } catch (...) { facts.hdmiMode = L"(query failed)"; }

        // ── Panel geometry and the composition-scale trick ───────────────────
        //
        // The swap chain is sized in REAL pixels (logical x composition scale)
        // and an inverse matrix keeps XAML laying out in logical units. Sizing
        // it to the logical size instead renders 1080p and lets the console
        // upscale — a soft picture with no error anywhere to explain it.
        const float scaleX = panel.CompositionScaleX();
        const float scaleY = panel.CompositionScaleY();
        const float dipW   = static_cast<float>(panel.ActualWidth());
        const float dipH   = static_cast<float>(panel.ActualHeight());

        facts.panelWidth  = static_cast<uint32_t>(std::lround(dipW));
        facts.panelHeight = static_cast<uint32_t>(std::lround(dipH));
        facts.scaleX = scaleX;
        facts.scaleY = scaleY;

        uint32_t width  = static_cast<uint32_t>(std::lround(dipW * scaleX));
        uint32_t height = static_cast<uint32_t>(std::lround(dipH * scaleY));
        if (width == 0 || height == 0) {
            width = 1920; height = 1080;
            facts.note = L"panel had no size yet; started at 1920x1080";
        }
        facts.backBufferWidth  = width;
        facts.backBufferHeight = height;

        // ── Swap chain ──────────────────────────────────────────────────────
        auto dxgiDevice = m_device.as<IDXGIDevice>();
        com_ptr<IDXGIAdapter> adapter;
        hr = dxgiDevice->GetAdapter(adapter.put());
        if (FAILED(hr)) { facts.note = L"GetAdapter failed"; return facts; }

        com_ptr<IDXGIFactory2> factory;
        hr = adapter->GetParent(IID_PPV_ARGS(factory.put()));
        if (FAILED(hr)) { facts.note = L"GetParent(IDXGIFactory2) failed"; return facts; }

        // ── Tearing ─────────────────────────────────────────────────────
        //
        // Asked for, and checked rather than assumed. A composition swap chain
        // is drawn by the compositor, which owns the scanout — so even where
        // DXGI reports the feature, a SwapChainPanel is very unlikely to
        // actually tear. The flag is set when it is offered because it costs
        // nothing and removes a present-time vblank wait where it is honoured,
        // and `facts.note` reports which of the two happened instead of
        // claiming a latency win that may not exist.
        //
        // What does the real work here is SyncInterval 0 plus FLIP_DISCARD:
        // present the newest buffer and do not queue.
        if (auto factory5 = factory.try_as<IDXGIFactory5>()) {
            BOOL allowed = FALSE;
            if (SUCCEEDED(factory5->CheckFeatureSupport(DXGI_FEATURE_PRESENT_ALLOW_TEARING,
                                                        &allowed, sizeof(allowed)))) {
                m_tearingSupported = allowed != FALSE;
            }
        }

        DXGI_SWAP_CHAIN_DESC1 desc{};
        desc.Width  = width;
        desc.Height = height;
        desc.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
        desc.SampleDesc.Count = 1;
        desc.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
        desc.BufferCount = 2;
        desc.Scaling = DXGI_SCALING_STRETCH;              // required for composition
        // FLIP_DISCARD, not FLIP_SEQUENTIAL. Sequential presents every buffer in
        // order, so a present at SyncInterval 0 still enqueues one — which is
        // the queue this whole change exists to remove. Discard shows the
        // newest and abandons the rest, which is what a live stream wants.
        desc.SwapEffect = DXGI_SWAP_EFFECT_FLIP_DISCARD;
        desc.AlphaMode = DXGI_ALPHA_MODE_IGNORE;
        desc.Flags = SwapChainFlags();

        com_ptr<IDXGISwapChain1> swapChain1;
        hr = factory->CreateSwapChainForComposition(m_device.get(), &desc, nullptr, swapChain1.put());
        if (FAILED(hr) && m_tearingSupported) {
            // Some drivers advertise tearing and then refuse it on a composition
            // swap chain. Losing the picture over an optimisation would be a bad
            // trade, so drop the flag and take the compositor's pacing.
            m_tearingSupported = false;
            desc.Flags = SwapChainFlags();
            hr = factory->CreateSwapChainForComposition(m_device.get(), &desc, nullptr,
                                                        swapChain1.put());
        }
        if (FAILED(hr)) {
            wchar_t buf[96]{};
            swprintf_s(buf, L"CreateSwapChainForComposition failed: 0x%08X", static_cast<unsigned>(hr));
            facts.note = buf;
            return facts;
        }

        m_swapChain = swapChain1.as<IDXGISwapChain2>();
        m_swapChain->SetMaximumFrameLatency(1);
        m_frameLatencyWaitable.attach(m_swapChain->GetFrameLatencyWaitableObject());

        DXGI_MATRIX_3X2_F transform{};
        transform._11 = 1.0f / scaleX;
        transform._22 = 1.0f / scaleY;
        m_swapChain->SetMatrixTransform(&transform);

        {
            std::lock_guard<std::mutex> guard(m_lock);
            if (!CreateSwapChainSurfaces()) { facts.note = L"back-buffer view failed"; return facts; }
        }

        auto panelNative = panel.as<ISwapChainPanelNative>();
        hr = panelNative->SetSwapChain(m_swapChain.get());
        if (FAILED(hr)) { facts.note = L"SetSwapChain failed"; return facts; }

        m_running.store(true, std::memory_order_release);
        m_presentThread = std::thread([this] { PresentLoop(); });

        // Say which of the two we got. "Tearing supported" on a composited panel
        // is a claim worth being able to check against the console rather than
        // against a comment.
        {
            const std::wstring present = m_tearingSupported
                ? L"present     immediate, tearing allowed (flip-discard, latency 1)"
                : L"present     immediate, compositor-paced (flip-discard, latency 1)";
            facts.note = facts.note.empty() ? present : facts.note + L"\n" + present;
        }

        facts.ok = true;
        return facts;
    } catch (hresult_error const& e) {
        facts.note = std::wstring(e.message().c_str());
        return facts;
    } catch (...) {
        facts.note = L"unknown failure creating the renderer";
        return facts;
    }
}

bool VideoRenderer::CreateSwapChainSurfaces() noexcept {
    m_backBufferView = nullptr;
    m_outputView = nullptr;          // bound to the old back buffer
    com_ptr<ID3D11Texture2D> backBuffer;
    if (FAILED(m_swapChain->GetBuffer(0, IID_PPV_ARGS(backBuffer.put())))) return false;
    return SUCCEEDED(m_device->CreateRenderTargetView(backBuffer.get(), nullptr, m_backBufferView.put()));
}

bool VideoRenderer::EnsureVideoProcessor(uint32_t srcWidth, uint32_t srcHeight) noexcept {
    if (m_processor && srcWidth == m_processorSrcWidth && srcHeight == m_processorSrcHeight) {
        return true;
    }
    m_processor = nullptr;
    m_processorEnum = nullptr;
    m_outputView = nullptr;

    DXGI_SWAP_CHAIN_DESC1 desc{};
    if (FAILED(m_swapChain->GetDesc1(&desc))) return false;

    D3D11_VIDEO_PROCESSOR_CONTENT_DESC content{};
    content.InputFrameFormat = D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE;
    content.InputWidth   = srcWidth;
    content.InputHeight  = srcHeight;
    content.OutputWidth  = desc.Width;
    content.OutputHeight = desc.Height;
    // The stream sets our cadence, not the processor; NORMAL keeps it from
    // trying to be clever about frame rate conversion.
    content.Usage = D3D11_VIDEO_USAGE_PLAYBACK_NORMAL;

    if (FAILED(m_videoDevice->CreateVideoProcessorEnumerator(&content, m_processorEnum.put()))) {
        return false;
    }
    if (FAILED(m_videoDevice->CreateVideoProcessor(m_processorEnum.get(), 0, m_processor.put()))) {
        return false;
    }

    // ── Colour space, and the classic washed-out-video bug ──────────────────
    //
    // GameStream video is BT.709 STUDIO range (16-235). Telling the processor
    // it is full range stretches those levels again: blacks go grey, whites
    // clip, and it reads as "the encoder is wrong" rather than as a conversion
    // setting. If a future HDR path lands, this is the line that changes.
    if (m_videoContext1) {
        m_videoContext1->VideoProcessorSetStreamColorSpace1(
            m_processor.get(), 0, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
        m_videoContext1->VideoProcessorSetOutputColorSpace1(
            m_processor.get(), DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
    } else {
        D3D11_VIDEO_PROCESSOR_COLOR_SPACE stream{};
        stream.Usage        = 0;  // 0 = playback (video), 1 = processing
        stream.RGB_Range    = 0;  // full range RGB out
        stream.YCbCr_Matrix = 1;  // 1 = BT.709
        stream.Nominal_Range = D3D11_VIDEO_PROCESSOR_NOMINAL_RANGE_16_235;
        m_videoContext->VideoProcessorSetStreamColorSpace(m_processor.get(), 0, &stream);
        D3D11_VIDEO_PROCESSOR_COLOR_SPACE output = stream;
        output.Nominal_Range = D3D11_VIDEO_PROCESSOR_NOMINAL_RANGE_0_255;
        m_videoContext->VideoProcessorSetOutputColorSpace(m_processor.get(), &output);
    }

    // No frame-rate conversion, no deinterlacing: the source is progressive and
    // already paced by the host.
    m_videoContext->VideoProcessorSetStreamOutputRate(
        m_processor.get(), 0, D3D11_VIDEO_PROCESSOR_OUTPUT_RATE_NORMAL, FALSE, nullptr);

    m_processorSrcWidth = srcWidth;
    m_processorSrcHeight = srcHeight;
    return true;
}

bool VideoRenderer::BlitFrame(ID3D11Texture2D* nv12, uint32_t subresource) noexcept {
    D3D11_TEXTURE2D_DESC srcDesc{};
    nv12->GetDesc(&srcDesc);
    if (!EnsureVideoProcessor(srcDesc.Width, srcDesc.Height)) return false;

    if (!m_outputView) {
        com_ptr<ID3D11Texture2D> backBuffer;
        if (FAILED(m_swapChain->GetBuffer(0, IID_PPV_ARGS(backBuffer.put())))) return false;
        D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC outDesc{};
        outDesc.ViewDimension = D3D11_VPOV_DIMENSION_TEXTURE2D;
        if (FAILED(m_videoDevice->CreateVideoProcessorOutputView(
                backBuffer.get(), m_processorEnum.get(), &outDesc, m_outputView.put()))) {
            return false;
        }
    }

    // The decoder hands back a texture ARRAY plus an index; the input view has
    // to name that slice or every frame shows whatever is in slice 0.
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC inDesc{};
    inDesc.FourCC = 0;                       // inherit the texture's format
    inDesc.ViewDimension = D3D11_VPIV_DIMENSION_TEXTURE2D;
    inDesc.Texture2D.MipSlice = 0;
    inDesc.Texture2D.ArraySlice = subresource;

    com_ptr<ID3D11VideoProcessorInputView> inputView;
    if (FAILED(m_videoDevice->CreateVideoProcessorInputView(
            nv12, m_processorEnum.get(), &inDesc, inputView.put()))) {
        return false;
    }

    // ── Crop the decoder's padding ──────────────────────────────────────────
    //
    // A decoder surface is allocated at the ALIGNED size — 1088 rows for a
    // 1080-line stream, 2176 for 2160 — and `srcDesc` above reports that
    // allocation, not the picture. Without a source rect the processor scales
    // all 1088 rows onto the output, so eight rows of undefined memory are
    // stretched across the screen. That is the green bar along the edge at
    // 1080p, and it is a crop rather than anything to do with stride.
    //
    // Free: the video processor is already scaling, so it costs nothing to
    // scale from the right rectangle.
    const uint32_t codedW = m_sourceWidth.load(std::memory_order_relaxed);
    const uint32_t codedH = m_sourceHeight.load(std::memory_order_relaxed);
    if (codedW && codedH &&
        (codedW != srcDesc.Width || codedH != srcDesc.Height)) {
        const RECT src{ 0, 0, static_cast<LONG>(codedW),
                        static_cast<LONG>(codedH) };
        m_videoContext->VideoProcessorSetStreamSourceRect(m_processor.get(), 0, TRUE, &src);
    }

    D3D11_VIDEO_PROCESSOR_STREAM stream{};
    stream.Enable = TRUE;
    stream.OutputIndex = 0;
    stream.InputFrameOrField = 0;
    stream.pInputSurface = inputView.get();

    return SUCCEEDED(m_videoContext->VideoProcessorBlt(
        m_processor.get(), m_outputView.get(), 0, 1, &stream));
}

void VideoRenderer::Resize(uint32_t panelWidth, uint32_t panelHeight,
                           float scaleX, float scaleY) noexcept {
    if (!m_swapChain) return;
    const uint32_t width  = static_cast<uint32_t>(std::lround(panelWidth  * scaleX));
    const uint32_t height = static_cast<uint32_t>(std::lround(panelHeight * scaleY));
    if (width == 0 || height == 0) return;

    std::lock_guard<std::mutex> guard(m_lock);
    m_pendingWidth  = width;
    m_pendingHeight = height;
    m_resizePending.store(true, std::memory_order_release);

    DXGI_MATRIX_3X2_F transform{};
    transform._11 = 1.0f / scaleX;
    transform._22 = 1.0f / scaleY;
    m_swapChain->SetMatrixTransform(&transform);
}

void VideoRenderer::IdleWait() noexcept {
    // A high-resolution waitable timer, not Sleep(1): the default Windows timer
    // tick is ~15.6 ms, which is coarser than a whole frame at 60 Hz and nearly
    // two at 120. Sleeping on it would add up to a frame of latency to every
    // picture that arrives just after a poll. `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`
    // gives sub-millisecond precision without changing the process-wide timer
    // resolution the way `timeBeginPeriod` does.
    if (!m_idleTimer) {
        m_idleTimer.attach(CreateWaitableTimerExW(
            nullptr, nullptr, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS));
    }
    if (m_idleTimer) {
        LARGE_INTEGER due{};
        due.QuadPart = -10000;   // 1 ms, relative
        if (SetWaitableTimer(m_idleTimer.get(), &due, 0, nullptr, nullptr, FALSE)) {
            WaitForSingleObjectEx(m_idleTimer.get(), 5, FALSE);
            return;
        }
    }
    Sleep(1);   // the timer is unavailable; coarse, but never a hot spin
}

void VideoRenderer::PresentLoop() noexcept {
    while (m_running.load(std::memory_order_acquire)) {
        std::unique_lock<std::mutex> guard(m_lock);
        if (!m_swapChain) break;

        // ── Resize ──────────────────────────────────────────────────────────
        bool resized = false;
        if (m_resizePending.exchange(false, std::memory_order_acq_rel)) {
            m_backBufferView = nullptr;
            m_outputView = nullptr;
            m_context->OMSetRenderTargets(0, nullptr, nullptr);
            m_context->Flush();
            if (SUCCEEDED(m_swapChain->ResizeBuffers(
                    0, m_pendingWidth, m_pendingHeight, DXGI_FORMAT_UNKNOWN,
                    SwapChainFlags()))) {
                CreateSwapChainSurfaces();
                // Output geometry changed, so the processor must be rebuilt.
                m_processor = nullptr;
                m_processorEnum = nullptr;
                resized = true;
            }
        }
        if (!m_backBufferView) { guard.unlock(); IdleWait(); continue; }

        // ── A decoded frame, if there is one ────────────────────────────────
        bool drew = false;
        {
            std::lock_guard<std::mutex> sourceGuard(m_sourceLock);
            if (m_frameSource) {
                com_ptr<ID3D11Texture2D> frame;
                uint32_t subresource = 0;
                if (m_frameSource(frame, subresource) && frame) {
                    drew = BlitFrame(frame.get(), subresource);
                    if (drew) m_blitted.fetch_add(1, std::memory_order_relaxed);
                }
            }
        }

        // ── Nothing new: HOLD the last picture ──────────────────────────────
        //
        // This is the whole fix for the idle flashing. The host throttles a
        // motionless desktop to a 200 ms keep-alive (Tier 0's
        // `static_duplicate_is_due`), so at 60 Hz roughly eleven of every
        // twelve passes through this loop have no new frame. Painting a
        // background on those and presenting it produced a picture that
        // strobed between the desktop and blue whenever nothing moved — and
        // stopped the instant the mouse did, which is exactly the reported
        // symptom.
        //
        // Not presenting leaves the previously presented buffer on screen,
        // which is precisely the "hold the last frame" behaviour wanted. The
        // background is painted only when there is genuinely nothing to hold:
        // before the first frame, and after an explicit request.
        if (!drew) {
            const bool nothingToHold =
                m_blitted.load(std::memory_order_relaxed) == 0 ||
                m_showBackground.load(std::memory_order_acquire) || resized;

            if (!nothingToHold) {
                guard.unlock();
                IdleWait();
                continue;
            }

            // A solid field, deliberately not the animated pulse the bring-up
            // build used: an animation here is indistinguishable from the
            // flashing bug it caused.
            //
            // Ion Void (#050508), and it must MATCH the XAML ground exactly.
            // This was 0.043/0.051/0.063 — #0B0D10 — which is both twice as
            // bright as the page behind it and blue-dominant (R11 G13 B16).
            // The swap chain covers the whole screen, so that was the app's
            // real background colour whatever the Page said, and a lifted
            // blue-leaning near-black is precisely what a television's shadow
            // handling exaggerates into "the background is blue".
            //
            // The format is B8G8R8A8_UNORM, not _SRGB, so these are written
            // straight through: 5/255, 5/255, 8/255. If the swap chain ever
            // becomes an _SRGB format these must be linearised or the field
            // comes back four times too bright — and blue-tinted again.
            const float clear[4] = { 5.0f / 255.0f, 5.0f / 255.0f, 8.0f / 255.0f, 1.0f };
            ID3D11RenderTargetView* rtv = m_backBufferView.get();
            m_context->OMSetRenderTargets(1, &rtv, nullptr);
            m_context->ClearRenderTargetView(rtv, clear);
            m_showBackground.store(false, std::memory_order_release);
        }

        // ── Present ─────────────────────────────────────────────────────────
        //
        // SyncInterval 0: show this frame now, do not wait for a vblank to
        // pass first. At SyncInterval 1 a present costs a refresh interval
        // whether or not the frame is late, so a picture that fell behind
        // could never catch up — it could only stay exactly as far behind as
        // the worst moment of the session. That, and not the network, is
        // where the lag-then-race came from.
        //
        // The waitable object stays, and is now the ONLY pacer: it blocks
        // until the compositor is ready for another frame, so at
        // MaximumFrameLatency 1 there is never more than one frame in flight
        // and the loop cannot spin. The 100 ms cap keeps a stalled compositor
        // from making this thread unkillable.
        if (m_frameLatencyWaitable) {
            WaitForSingleObjectEx(m_frameLatencyWaitable.get(), 100, FALSE);
        }
        if (!m_running.load(std::memory_order_acquire)) break;

        DXGI_PRESENT_PARAMETERS params{};
        // ALLOW_TEARING is only legal at SyncInterval 0, and only on a swap
        // chain created with the matching flag: passing it otherwise is an
        // invalid-call failure, not a silent ignore.
        const UINT interval = m_syncInterval.load(std::memory_order_relaxed);
        const UINT flags =
            (m_tearingSupported && interval == 0) ? DXGI_PRESENT_ALLOW_TEARING : 0u;
        const HRESULT hr = m_swapChain->Present1(interval, flags, &params);
        if (SUCCEEDED(hr)) {
            m_presented.fetch_add(1, std::memory_order_relaxed);
            continue;
        }

        // ── A failed present must never be fatal ─────────────────────────
        //
        // This used to `break`, and that made one refused present a permanently
        // blank screen: the thread exited, nothing restarted it, and the XAML
        // layer above kept drawing perfectly — so the app looked alive and the
        // video simply never appeared. A renderer that gives up silently is
        // worse than one that stutters.
        m_lastPresentHr.store(static_cast<uint32_t>(hr), std::memory_order_relaxed);

        if (hr == DXGI_ERROR_INVALID_CALL && interval == 0) {
            // A composition swap chain is presented by the compositor, and not
            // every configuration will accept "show this immediately" or a
            // tearing flag. Fall back to the compositor pace ONCE and keep
            // going: the frame dropping in the decoder is what removes the
            // backlog, and it works at either interval. This is the whole
            // reason the interval is a variable and not a literal.
            m_tearingSupported = false;
            m_syncInterval.store(1, std::memory_order_relaxed);
            continue;
        }
        if (hr == DXGI_ERROR_DEVICE_REMOVED || hr == DXGI_ERROR_DEVICE_RESET) {
            break;   // the device is gone; nothing here can recover it
        }
        // Anything else: leave the last picture up and try again next pass.
        guard.unlock();
        IdleWait();
    }
}

void VideoRenderer::ShowBackground() noexcept {
    // Used when a session ends: without it the last frame of a dead stream
    // stays on the TV under the host picker, which reads as a frozen stream
    // rather than a finished one.
    m_showBackground.store(true, std::memory_order_release);
}

void VideoRenderer::Shutdown() noexcept {
    m_running.store(false, std::memory_order_release);
    if (m_presentThread.joinable()) m_presentThread.join();

    {
        std::lock_guard<std::mutex> guard(m_sourceLock);
        m_frameSource = nullptr;
    }
    std::lock_guard<std::mutex> guard(m_lock);
    m_outputView = nullptr;
    m_processor = nullptr;
    m_processorEnum = nullptr;
    m_videoContext = nullptr;
    m_videoDevice = nullptr;
    m_backBufferView = nullptr;
    m_swapChain = nullptr;
    m_frameLatencyWaitable.close();
    m_idleTimer.close();
    m_context = nullptr;
    m_device = nullptr;
}

}  // namespace echo
