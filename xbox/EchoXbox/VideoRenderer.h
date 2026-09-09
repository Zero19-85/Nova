// The D3D11 device, the swap chain behind the SwapChainPanel, and the video
// processor that turns a decoded NV12 picture into what the TV shows.
//
// ── Why the HDMI mode is ours to set ────────────────────────────────────────
//
// An Xbox UWP XAML view is 1920x1080 LOGICAL whatever the TV is doing, and the
// console will happily leave its HDMI output at 1080p for something it thinks
// is an app. `HdmiDisplayInformation::RequestSetCurrentDisplayModeAsync` is the
// documented way for a title to take that decision back — it is what 4K media
// apps use — and it must run BEFORE the swap chain is created, because it is
// what makes the composition scale report 2.0 in the first place.
#pragma once

#include <cstdint>
#include <string>
#include <atomic>
#include <thread>
#include <mutex>
#include <functional>

#include <d3d11_4.h>
#include <dxgi1_6.h>
#include <winrt/base.h>
#include <winrt/Windows.UI.Xaml.Controls.h>

namespace echo {

// What the console is ACTUALLY outputting once we have asked.
//
// Read back rather than assumed, and that is the point of the struct: asking
// for 4K and being refused leaves the TV at 1080p, and a client that believed
// its own request would then ask the host to encode four times the pixels the
// panel can show. Zeroes mean there is no HDMI information at all - a PC,
// not a console.
struct HdmiOutcome {
    uint32_t width = 0;
    uint32_t height = 0;
    double   refreshHz = 0.0;
    std::wstring note;   // human-readable, for the log
    /// Every refresh rate the console offers at the requested size. The one
    /// thing that separates "the console refused 4K120" from "this console was
    /// never offered 4K120" — an app bug versus a console setting.
    std::wstring offered;
};

// Ask the console to output at `width` x `height`, preferring the highest
// refresh rate it offers at that size. Blocking — call it from a background
// thread, never the UI thread. Never throws.
HdmiOutcome RequestBestHdmiMode(uint32_t width, uint32_t height) noexcept;

struct DisplayFacts {
    uint32_t panelWidth = 0;        // logical (DIP) size of the SwapChainPanel
    uint32_t panelHeight = 0;
    float    scaleX = 1.0f;         // composition scale
    float    scaleY = 1.0f;
    uint32_t backBufferWidth = 0;   // REAL pixels: panel * scale
    uint32_t backBufferHeight = 0;
    std::wstring adapter;
    std::wstring hdmiMode;
    std::wstring note;
    bool ok = false;
};

// Hands the renderer the next decoded picture. Returns false when none is
// ready, which is the normal case between frames.
using FrameSource = std::function<bool(winrt::com_ptr<ID3D11Texture2D>&, uint32_t&)>;

class VideoRenderer {
public:
    ~VideoRenderer();

    // UI thread only: ISwapChainPanelNative::SetSwapChain requires it and the
    // panel's size is only readable there. Never throws — a failure lands in
    // DisplayFacts::note so the app can put the reason on the TV.
    DisplayFacts Initialize(
        winrt::Windows::UI::Xaml::Controls::SwapChainPanel const& panel) noexcept;

    void Shutdown() noexcept;
    void Resize(uint32_t panelWidth, uint32_t panelHeight, float scaleX, float scaleY) noexcept;

    // Repaint the background once. Call it when a session ends, or the last
    // frame of a dead stream stays on the TV and reads as a frozen one.
    void ShowBackground() noexcept;

    // Install the decoder. Until this is set the renderer shows a solid
    // background; once frames arrive it holds the last one between them.
    void SetFrameSource(FrameSource source) noexcept;

    /// The CODED size of the decoded picture, which is not the size of the
    /// texture it arrives in — a decoder surface is allocated at the aligned
    /// size (1088 rows for 1080). Without this the processor scales the
    /// padding onto the screen; see the source rect in BlitFrame.
    void SetSourceSize(uint32_t width, uint32_t height) noexcept;

    uint64_t PresentedFrames() const noexcept { return m_presented.load(std::memory_order_relaxed); }
    uint64_t BlittedFrames()   const noexcept { return m_blitted.load(std::memory_order_relaxed); }
    uint32_t SyncInterval()    const noexcept { return m_syncInterval.load(std::memory_order_relaxed); }
    uint32_t LastPresentError() const noexcept { return m_lastPresentHr.load(std::memory_order_relaxed); }
    ID3D11Device* Device() const noexcept { return m_device.get(); }

private:
    void PresentLoop() noexcept;
    void IdleWait() noexcept;   // 1 ms, high-resolution; never a hot spin
    bool CreateSwapChainSurfaces() noexcept;
    bool EnsureVideoProcessor(uint32_t srcWidth, uint32_t srcHeight) noexcept;
    bool BlitFrame(ID3D11Texture2D* nv12, uint32_t subresource) noexcept;

    winrt::com_ptr<ID3D11Device>           m_device;
    winrt::com_ptr<ID3D11DeviceContext>    m_context;
    winrt::com_ptr<IDXGISwapChain2>        m_swapChain;
    winrt::com_ptr<ID3D11RenderTargetView> m_backBufferView;

    // The colour-conversion path. A video processor rather than a pixel shader:
    // UWP cannot compile HLSL at runtime, so a shader would need a build-time
    // step, and the processor does BT.709 studio-range conversion in fixed
    // function hardware anyway.
    winrt::com_ptr<ID3D11VideoDevice>                m_videoDevice;
    winrt::com_ptr<ID3D11VideoContext>               m_videoContext;
    // The DXGI-colour-space entry points live on VideoContext1, not on the
    // base interface. Optional: without it we fall back to the older enum,
    // which expresses the same BT.709 studio-range intent less precisely.
    winrt::com_ptr<ID3D11VideoContext1>              m_videoContext1;
    winrt::com_ptr<ID3D11VideoProcessor>             m_processor;
    winrt::com_ptr<ID3D11VideoProcessorEnumerator>   m_processorEnum;
    winrt::com_ptr<ID3D11VideoProcessorOutputView>   m_outputView;
    uint32_t m_processorSrcWidth = 0;
    uint32_t m_processorSrcHeight = 0;

    winrt::handle m_frameLatencyWaitable;
    winrt::handle m_idleTimer;
    std::atomic<bool> m_showBackground{false};

    // Whether DXGI offered tearing on this adapter. Checked, never assumed —
    // a composition swap chain is composited, and the compositor owns the
    // scanout. Reported in DisplayFacts::note either way.
    bool m_tearingSupported = false;
    // 0 = present immediately, 1 = compositor pace. Starts at 0 and degrades
    // to 1 if DXGI refuses it, so an unsupported configuration costs one
    // rejected present rather than the whole picture.
    std::atomic<uint32_t> m_syncInterval{ 0 };
    // The last present failure, for the diagnostics panel. A silent renderer
    // thread is exactly what made this class of bug expensive to find.
    std::atomic<uint32_t> m_lastPresentHr{ 0 };
    // The creation/resize flags, which must MATCH: ResizeBuffers with a
    // different flag set than the swap chain was created with fails, and it
    // fails at the moment the picture changes size.
    UINT SwapChainFlags() const noexcept {
        return DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT |
               (m_tearingSupported ? DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING : 0u);
    }

    FrameSource m_frameSource;
    std::mutex  m_sourceLock;
    // Atomics rather than guarded by m_sourceLock: they are read on the present
    // thread every frame and written from the UI thread only on a geometry
    // change, and a torn read of a width is not worth a lock on that path.
    std::atomic<uint32_t> m_sourceWidth{ 0 };
    std::atomic<uint32_t> m_sourceHeight{ 0 };

    std::thread           m_presentThread;
    std::atomic<bool>     m_running{false};
    std::atomic<uint64_t> m_presented{0};
    std::atomic<uint64_t> m_blitted{0};

    std::mutex m_lock;
    std::atomic<bool> m_resizePending{false};
    uint32_t m_pendingWidth = 0;
    uint32_t m_pendingHeight = 0;
};

}  // namespace echo
