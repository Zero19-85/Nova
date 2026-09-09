// HEVC through FFmpeg's D3D11VA hardware acceleration.
//
// ── Why this exists ────────────────────────────────────────────────────────
//
// Media Foundation offers this app container NO hardware HEVC decoder. The
// probe says so directly — `enumerated hardware: 0` — and what `HevcDecoder`
// then falls back to is "Microsoft H265 Video Decoder MFT", a SOFTWARE decoder
// that cannot sustain 4K120 and was never going to.
//
// The same probe asks the D3D11 video device the question FFmpeg's d3d11va
// hwaccel asks during setup, and that layer answers yes: HEVC Main, NV12 at
// 3840x2160, with decoder configurations available. So the ceiling was never
// the silicon. It was a Media Foundation policy boundary, and going straight to
// D3D11VA goes under it.
//
// ── The four things that decide whether this is worth having ───────────────
//
// 1. It shares the RENDERER'S D3D11 device. FFmpeg will happily create its own,
//    and then every frame costs a cross-device copy at 4K120 forever. The
//    device is injected into the hwdevice context before it is initialised.
//
// 2. `get_format` must return AV_PIX_FMT_D3D11. The default callback picks the
//    software format and the decode lands silently on the CPU — the same
//    failure as MF's out-of-order SET_D3D_MANAGER, with the same signature:
//    keeps up at 1080p, drowns at 4K.
//
// 3. The output maps 1:1 onto the existing frame contract. `AVFrame::data[0]`
//    is an ID3D11Texture2D* and `data[1]` is the array index — exactly the
//    (texture, subresource) pair `VideoRenderer::BlitFrame` already takes from
//    the MFT, whose output is also a texture array. The renderer is unchanged.
//
// 4. The drop policy from §4b.3 is reimplemented here, because it is a property
//    of the queue rather than of Media Foundation: take the NEWEST decoded
//    picture and discard everything behind it. Without it, a burst becomes
//    permanent latency — video that lags input and then visibly races to catch
//    up.
#pragma once

#include <atomic>
#include <cstdint>
#include <mutex>
#include <string>

#include "VideoDecoder.h"

struct AVCodecContext;
struct AVFrame;
struct AVPacket;
struct AVBufferRef;

namespace echo {

class FfmpegHevcDecoder final : public VideoDecoder {
public:
    ~FfmpegHevcDecoder() override;

    // `device` is the renderer's device, and it is AddRef'd for the lifetime of
    // the decoder. `fps` is advisory — FFmpeg does not size a pool from it the
    // way a Media Foundation transform does — but it is passed for parity and
    // for the log.
    bool Initialize(ID3D11Device* device, uint32_t width, uint32_t height,
                    uint32_t fps, std::wstring& error) noexcept;

    bool Submit(const uint8_t* data, uint32_t length, bool keyframe,
                bool discontinuity, int64_t pts100ns) noexcept override;
    bool TryGetFrame(winrt::com_ptr<ID3D11Texture2D>& texture,
                     uint32_t& subresource) noexcept override;
    void Shutdown() noexcept override;

    uint32_t Width()  const noexcept override { return m_width; }
    uint32_t Height() const noexcept override { return m_height; }
    const wchar_t* Name() const noexcept override { return m_name.c_str(); }
    bool IsHardware() const noexcept override { return m_hardware; }
    uint64_t DecodedFrames() const noexcept override { return m_decoded; }
    uint64_t DroppedFrames() const noexcept override { return m_dropped; }
    // The sum, because one number is what the repair path acts on.
    uint64_t DecodeErrors() const noexcept override {
        return m_submitFailures + m_receiveFailures + m_corruptFrames + m_concealedFrames;
    }
    // The breakdown, because the sum says something is wrong and only this
    // says WHICH of four quite different things it is.
    std::wstring ErrorDetail() const noexcept override;

private:
    // Bounded so a decoder that answers "ready" forever cannot wedge the
    // present thread; whatever is left is dropped on the next pass.
    static constexpr int kMaxDrain = 32;

    void DrainAndDiscard() noexcept;      // assumes m_lock held



    AVCodecContext* m_ctx = nullptr;
    AVBufferRef*    m_hwDevice = nullptr;
    AVPacket*       m_packet = nullptr;

    // The frame currently on loan to the renderer. Its texture is only valid
    // while this reference is held, so it is released on the NEXT call and not
    // before — that is the contract VideoDecoder.h describes.
    AVFrame*        m_current = nullptr;
    AVFrame*        m_scratch = nullptr;

    // Submit runs on the network thread, TryGetFrame on the present thread. An
    // AVCodecContext is not safe across both.
    std::mutex m_lock;

    winrt::com_ptr<ID3D11Device> m_device;

    std::wstring m_name = L"(none)";
    bool     m_hardware = false;
    bool     m_started  = false;
    uint32_t m_width = 0;
    uint32_t m_height = 0;
    uint64_t m_decoded = 0;
    uint64_t m_dropped = 0;
    // Four different failures, counted apart. They mean different things and
    // point at different fixes:
    //   submit    a frame never reached the decoder — a hole in the chain the
    //             host does not know about, because the RECEIVER saw it fine
    //   receive   the decoder was asked for a picture and errored
    //   corrupt   AV_FRAME_FLAG_CORRUPT: the frame is admitted-bad
    //   concealed decode_error_flags: it produced a picture by filling in for
    //             something it did not have. THE SILENT ONE.
    uint64_t m_submitFailures = 0;
    uint64_t m_receiveFailures = 0;
    uint64_t m_corruptFrames = 0;
    uint64_t m_concealedFrames = 0;
    uint64_t m_missingRefs = 0;
};

}  // namespace echo
