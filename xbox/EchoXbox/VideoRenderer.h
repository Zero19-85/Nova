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
#include <vector>
#include <array>

#include <d3d11_4.h>
#include <dxgi1_6.h>
#include <winrt/base.h>
#include <winrt/Windows.UI.Xaml.Controls.h>

#include "VideoDecoder.h"   // FrameColor

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
    /// Whether the console ended up in BT.2020 PQ. **Read this rather than
    /// assuming the request was honoured**: it decides whether the session may
    /// ask the host for HDR at all. Asking for PQ content and then presenting
    /// it on a panel driven in Rec.709 is the washed-out picture, and the only
    /// thing that reliably prevents it is believing the read-back.
    bool hdrActive = false;
};

// Ask the console to output at `width` x `height`, preferring the highest
// refresh rate it offers at that size. Blocking — call it from a background
// thread, never the UI thread. Never throws.
//
// **Does nothing at all when the console is already that size**, which is the
// common case on a console whose dashboard is set to 4K: touching a mode that
// is already right cannot improve it and can cost the dynamic range, because
// every mode request carries a transfer function with it.
//
// It never chooses a dynamic range. When the size does have to change it
// carries the console's CURRENT transfer function through the change;
// [`RequestHdrMode`] owns that decision, and the two must not undo each other.
HdmiOutcome RequestBestHdmiMode(uint32_t width, uint32_t height) noexcept;

// Switch the console's transfer function, leaving resolution and refresh
// exactly where they are.
//
// **Separate from the resolution request on purpose.** This mirrors
// `moonlight-xbox`'s `MoonlightClient::SetDisplayHDR`, which never asks for a
// size or a refresh: it finds the mode that matches the CURRENT one in every
// respect except `IsSmpte2084Supported` and applies that. A console asked for a
// resolution, a refresh and a transfer function at once can refuse the whole
// request for any one of them and does not say which -- and bundling them is
// what left the stream clamped to HEVC Main 8 on 2026-09-22.
//
// Returns whether the console is in PQ AFTERWARDS, read back from it. Never
// throws. Blocking: background thread only.
//
// `note` receives a human-readable account of what happened, including the
// mode table when no suitable mode exists -- the difference between "this TV
// is not HDR" and "we asked wrongly" is otherwise invisible.
bool RequestHdrMode(bool enable, std::wstring& note) noexcept;

// Is the console's output ACTUALLY in HDR right now?
//
// Asked of `DisplayInformation::GetAdvancedColorInfo()`, which is how Kodi's
// UWP build reports HDR status (`CWIN32Util::GetWindowsHDRStatus`), and not of
// the HDMI mode's `IsSmpte2084Supported` flag. This is the question the HDR
// stream gate must ask: on Xbox, a swap chain accepts a BT.2020 PQ declaration
// even while the output is SDR (`CanPresentPq` said yes on 2026-10-07), and
// Microsoft's docs say HDR10-to-SDR tone mapping happens "in the media
// pipeline" -- which a swap-chain renderer does not use. So PQ sent to an SDR
// output is shown untone-mapped: an almost entirely white picture.
//
// UI thread ONLY: `GetForCurrentView` throws anywhere else. `how` names the
// kind it saw, for the diagnostics line. Never throws.
bool ConsoleOutputIsHdr(std::wstring& how) noexcept;

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

// What `SetColorSpace1` ended up doing. Kept as a small code rather than a
// string because it is written on the render thread and read on the UI thread
// every stats tick, and an `std::atomic<int>` needs no lock where a
// `std::wstring` would need one on the hot path.
enum class ColorSpaceState : int {
    Unknown = -1,
    Pq,             // BT.2020 PQ declared and accepted - a real HDR10 present
    Srgb,           // Rec.709 declared and accepted
    PqRefused,      // the console will not present PQ; the buffer stays sRGB
    SrgbRefused,
    SetRefused,     // the support check passed and the set still failed
    NoSwapChain3,
};

// The FIRST stage at which RenderFrame refused a picture. Every one of these
// used to be a bare `return false`, so `drawn 0` beside a healthy `decoded`
// could not say which of five different failures it was (live 2026-10-05:
// 274 decoded, 0 drawn, no reason anywhere). The first one is kept rather than
// the latest because the latest is usually a consequence of the first.
enum class RenderFailure : int {
    None = 0,
    ShaderFile,     // a .cso was missing or unreadable from the package
    ShaderCreate,   // the bytecode loaded and the device refused it
    PipelineState,  // input layout, sampler or constant buffer
    VideoTexture,   // our private copy target could not be created
    PlaneViews,     // the NV12/P010 plane SRVs could not be created
    Quad,           // the vertex buffer could not be created
    SwapChainDesc,  // GetDesc1 on the swap chain failed
};


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

    /// How to convert the next picture, read off the stream by the decoder.
    /// Pushed per frame from the frame-source callback for the same reason
    /// `SetSourceSize` is: a live re-mode or an HDR renegotiation changes it
    /// mid-session, and the decoder learns before anything else does.
    void SetFrameColor(FrameColor colour) noexcept { m_frameColor = colour; }

    uint64_t PresentedFrames() const noexcept { return m_presented.load(std::memory_order_relaxed); }
    uint64_t BlittedFrames()   const noexcept { return m_blitted.load(std::memory_order_relaxed); }
    uint32_t SyncInterval()    const noexcept { return m_syncInterval.load(std::memory_order_relaxed); }
    uint32_t LastPresentError() const noexcept { return m_lastPresentHr.load(std::memory_order_relaxed); }
    /// What the compositor was told is in the back buffer, and whether it
    /// agreed. **This is the line that explains a washed-out HDR picture**:
    /// the host can be sending PQ and the shader writing PQ while the console
    /// refuses to present BT.2020, in which case the values are passed through
    /// as Rec.709 and nothing anywhere reports an error. Safe from any thread.
    wchar_t const* ColorSpaceReport() const noexcept {
        switch (static_cast<ColorSpaceState>(m_colorSpaceState.load(std::memory_order_relaxed))) {
            case ColorSpaceState::Pq:          return L"BT.2020 PQ (HDR10)";
            case ColorSpaceState::Srgb:        return L"Rec.709 sRGB";
            case ColorSpaceState::PqRefused:   return L"console will not present BT.2020 PQ - back buffer left in sRGB";
            case ColorSpaceState::SrgbRefused: return L"console will not present sRGB - back buffer left as-is";
            case ColorSpaceState::SetRefused:  return L"SetColorSpace1 refused after the support check passed";
            case ColorSpaceState::NoSwapChain3:return L"no IDXGISwapChain3 - colour space undeclared";
            default:                           return L"not declared yet";
        }
    }
    ID3D11Device* Device() const noexcept { return m_device.get(); }

    /// Would the swap chain accept `RGB_FULL_G2084_NONE_P2020` right now?
    ///
    /// The gate on asking the host for HDR10. It asks the question that
    /// actually decides whether PQ is bleached -- the same
    /// `CheckColorSpaceSupport` that `ApplySwapChainColorSpace` (and
    /// moonlight) run before declaring it -- rather than the HDMI mode flag,
    /// which misreports at 119.88 Hz. A refusal here means PQ would be
    /// presented as sRGB, so SDR is the honest stream to ask for. False with
    /// no swap chain. A read-only DXGI query, safe from the UI thread.
    bool CanPresentPq() const noexcept;

    /// Declare BT.2020 PQ on the swap chain while there is no picture yet.
    ///
    /// The HDR request has to be made the way moonlight-xbox makes it: its
    /// `SetDisplayHDR` runs mid-stream, from the host's `setHdrMode` callback,
    /// while its 10-bit swap chain is already presenting. Echo used to ask at
    /// app startup with no swap chain at all, and the console refused PQ on
    /// every attempt even with `hevcPlayback` and the app set to Game
    /// (2026-10-07). With this set, the background the renderer presents
    /// before the first frame is a live R10G10B10A2 surface declared
    /// G2084_P2020 -- the state moonlight is in when it asks. Black is zero
    /// under either transfer, so nothing visible changes. Decoded frames still
    /// set their own colour space from the stream's metadata. Any thread.
    void PreferPq(bool on) noexcept { m_pqPreferred.store(on, std::memory_order_release); }

    /// Why nothing is being drawn, if anything has said so. Empty while every
    /// stage has succeeded. Safe from any thread.
    std::wstring FailureReport() const;

    /// The GPU device is gone -- `DXGI_ERROR_DEVICE_REMOVED`/`RESET`. Nothing
    /// on this renderer can recover from that: its device, swap chain and
    /// every resource are dead, and the present thread has exited. The owner
    /// must destroy it and build a new one. Safe from any thread.
    bool DeviceLost() const noexcept {
        return m_deviceRemovedReason.load(std::memory_order_acquire) != 0;
    }

    /// Whether the owner should rebuild before the next stream: the device is
    /// lost, or a stage failed before a single picture was ever drawn. The
    /// second matters because the shader stage is deliberately sticky -- it
    /// never retries -- so one bad load would otherwise blank every session
    /// for the rest of the app's life.
    bool NeedsRebuild() const noexcept {
        return DeviceLost() ||
               (m_blitted.load(std::memory_order_relaxed) == 0 &&
                m_renderFailure.load(std::memory_order_relaxed) != 0);
    }

private:
    // Record a refusal (first one wins) and check whether the device itself
    // is what failed. Always returns false so a failing stage can
    // `return Fail(...)`.
    bool Fail(RenderFailure stage, HRESULT hr) noexcept;
    void NoteDeviceRemoved(HRESULT fallback) noexcept;

    void PresentLoop() noexcept;
    void IdleWait() noexcept;   // 1 ms, high-resolution; never a hot spin
    bool CreateSwapChainSurfaces() noexcept;
    bool EnsureShaderPipeline() noexcept;
    bool EnsurePlaneViews(ID3D11Texture2D* frame, uint32_t slice,
                          D3D11_TEXTURE2D_DESC const& desc,
                          ID3D11ShaderResourceView* out[2]) noexcept;
    void EnsureQuad(D3D11_TEXTURE2D_DESC const& desc) noexcept;
    void EnsureCscConstants(D3D11_TEXTURE2D_DESC const& desc) noexcept;
    void ApplySwapChainColorSpace(bool pq) noexcept;
    bool RenderFrame(ID3D11Texture2D* frame, uint32_t slice) noexcept;

    winrt::com_ptr<ID3D11Device>           m_device;
    winrt::com_ptr<ID3D11DeviceContext>    m_context;
    winrt::com_ptr<IDXGISwapChain2>        m_swapChain;
    winrt::com_ptr<ID3D11RenderTargetView> m_backBufferView;

    // ── The colour-conversion path: a pixel shader ─────────────────────────
    //
    // This was a `ID3D11VideoProcessor` because UWP has no runtime HLSL
    // compiler, and a fixed-function BT.709 studio-range conversion was all
    // the pipeline needed. HDR10 is what ended that: the host sends BT.2020
    // PQ at FULL range, and DXGI has no full-range PQ YCbCr colour space to
    // describe it with -- `VideoProcessorSetStreamColorSpace1` simply cannot
    // express the stream, so the processor would be told studio range and
    // crush every black in the picture.
    //
    // A shader can express it, because the range and the matrix are just
    // numbers in a constant buffer. The runtime-compiler problem is solved the
    // way `moonlight-xbox` solves it: compile ahead of time (build-app.ps1)
    // and package the bytecode.
    winrt::com_ptr<ID3D11VertexShader>       m_vertexShader;
    winrt::com_ptr<ID3D11PixelShader>        m_pixelShader;
    winrt::com_ptr<ID3D11InputLayout>        m_inputLayout;
    winrt::com_ptr<ID3D11Buffer>             m_quad;
    winrt::com_ptr<ID3D11Buffer>             m_cscBuffer;
    winrt::com_ptr<ID3D11SamplerState>       m_sampler;
    bool m_shaderReady = false;
    bool m_shaderTried = false;

    // Our OWN copy of the decoded picture, and the SRV pair onto it.
    //
    // Not the decoder's surface: each frame is copied in before anything
    // samples it, so the decoder can recycle its slice whenever it likes and
    // the render thread never shares memory with it. This is `moonlight-xbox`'s
    // arrangement (`m_VideoTexture` + `CopySubresourceRegion1`) and the reason
    // is in EnsurePlaneViews.
    //
    // Single slice, so the views are TEXTURE2D and there is no index to get
    // wrong. Rebuilt when the picture's size or pixel format changes.
    winrt::com_ptr<ID3D11Texture2D> m_videoTexture;
    uint32_t    m_videoTexW = 0, m_videoTexH = 0;
    DXGI_FORMAT m_videoTexFormat = DXGI_FORMAT_UNKNOWN;
    std::vector<std::array<winrt::com_ptr<ID3D11ShaderResourceView>, 2>> m_planeViews;

    // Geometry the quad was last built for. The quad carries the letterbox, so
    // it has to be rebuilt when either the picture or the back buffer changes.
    uint32_t m_quadSrcW = 0, m_quadSrcH = 0, m_quadDstW = 0, m_quadDstH = 0;
    uint32_t m_quadCodedW = 0, m_quadCodedH = 0;
    // Geometry the CSC constants were built for: chromaOffset and chromaTexMax
    // both depend on the texture size, so a re-mode must rebuild them even when
    // the colour description itself has not changed.
    uint32_t m_cscSrcW = 0, m_cscSrcH = 0;

    // The colour of the picture being rendered, pushed per frame from the
    // decoder. `m_cscFor` is what the constant buffer currently holds, so the
    // buffer is only rewritten when it actually changes.
    //
    // Not atomic, unlike `m_sourceWidth`: `SetFrameColor` is called from the
    // frame-source callback, which `PresentLoop` invokes on the render thread
    // itself. Writer and reader are the same thread.
    FrameColor m_frameColor{};
    FrameColor m_cscFor{};
    bool m_cscValid = false;
    int  m_colorSpaceApplied = -1;   // DXGI_COLOR_SPACE_TYPE, -1 = never set
    // Render thread writes, UI thread reads. See ColorSpaceReport().
    std::atomic<int> m_colorSpaceState{ static_cast<int>(ColorSpaceState::Unknown) };

    winrt::handle m_frameLatencyWaitable;
    winrt::handle m_idleTimer;
    std::atomic<bool> m_showBackground{false};
    // See PreferPq(). Applied by the present thread on the no-picture path.
    std::atomic<bool> m_pqPreferred{false};

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
    // See RenderFailure / FailureReport(). Render thread writes, UI reads.
    std::atomic<int>      m_renderFailure{ 0 };
    std::atomic<uint32_t> m_renderFailureHr{ 0 };
    // `GetDeviceRemovedReason()` at the moment the device was found dead; 0
    // while it is alive. This is the number that names the CAUSE (a GPU hang,
    // a driver reset, running out of memory), where 0x887A0005 on its own
    // only says that it happened.
    std::atomic<uint32_t> m_deviceRemovedReason{ 0 };
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
