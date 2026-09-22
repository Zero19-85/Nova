// The shape both decoders present, so nothing above them has to know which is
// running.
//
// There are two HEVC decoders in this app on purpose:
//
//   HevcDecoder        Media Foundation. Works, and is capped at 4K60 on this
//                      console — not by the silicon, but because MF hands the
//                      app container no hardware decoder at all (the probe's
//                      `enumerated hardware: 0`), leaving a software MFT.
//   FfmpegHevcDecoder  FFmpeg driving D3D11VA directly, which is the layer
//                      that DID offer 4K HEVC when asked (`dxva … configs > 0`).
//
// Keeping both is not indecision. The MF path is a known-good 4K60 fallback and
// a one-button A/B on real hardware, and §4b of the handoff is a list of things
// that were only ever settled by comparing two behaviours on a television.
//
// ── The frame contract, which is the only subtle part ──────────────────────
//
// `TryGetFrame` hands back a texture the DECODER still owns, valid only until
// the next call on this object. Neither implementation copies: MF returns a
// surface from the MFT's pool, FFmpeg returns the ID3D11Texture2D inside an
// AVFrame it holds a reference to. The renderer blits it through the video
// processor and is done with it before it asks for another, which is what makes
// the whole path zero-copy — and what makes "valid until the next call" safe
// rather than a trap.
#pragma once

#include <cstdint>
#include <string>

#include <d3d11_4.h>
#include <winrt/base.h>

namespace echo {

// What the renderer must know to turn YCbCr into RGB correctly.
//
// Read from the STREAM rather than assumed from the session, which is the
// thing that makes the colour conversion honest. Nova's host has a fixed
// convention today -- SDR is BT.709 limited range, HDR is BT.2020 PQ FULL
// range (`videoFullRangeFlag = is_hdr ? 1 : 0` in the shim) -- and it would be
// less code to hardcode that here. It would also be wrong the first time
// either side changed, and silently: a range mismatch does not fail, it just
// lifts the blacks. `moonlight-xbox` derives all of this per frame from the
// AVFrame (`getFramePremultipliedCscConstants`) and this mirrors it.
struct FrameColor {
    // 8 for NV12, 10 for P010. Drives the offsets and scaling, not the
    // sampling: a P010 SRV is R16_UNORM and normalises to [0,1] either way.
    int  bitsPerChannel = 8;
    // AVCOL_RANGE_JPEG. Limited (16-235) is the SDR default and full is what
    // Nova sends for HDR, which is precisely why this cannot be a constant.
    bool fullRange = false;
    enum class Matrix { Bt601, Bt709, Bt2020 };
    Matrix matrix = Matrix::Bt709;
    // AVCOL_TRC_SMPTE2084. The renderer needs this separately from the matrix
    // because it selects the SWAP CHAIN's colour space, and a BT.2020 stream
    // is not necessarily PQ.
    bool pq = false;
};

class VideoDecoder {
public:
    virtual ~VideoDecoder() = default;

    // Feed one Annex-B access unit, already reassembled, FEC-repaired and
    // decrypted by echo-client. `discontinuity` means the reference chain is
    // broken here, so the decoder must not conceal against pictures it never
    // received.
    virtual bool Submit(const uint8_t* data, uint32_t length, bool keyframe,
                        bool discontinuity, int64_t pts100ns) noexcept = 0;

    // The NEWEST decoded picture, discarding anything behind it. Returns false
    // when nothing is ready, which is normal. See the note above about
    // ownership.
    virtual bool TryGetFrame(winrt::com_ptr<ID3D11Texture2D>& texture,
                             uint32_t& subresource) noexcept = 0;

    /// Colour description of the picture the last `TryGetFrame` handed back.
    /// Defaulted rather than pure: the Media Foundation path decodes SDR only,
    /// and BT.709 limited 8-bit is exactly what it produces.
    virtual FrameColor LastFrameColor() const noexcept { return {}; }

    virtual void Shutdown() noexcept = 0;

    virtual uint32_t Width() const noexcept = 0;
    virtual uint32_t Height() const noexcept = 0;

    // For the overlay and the log. "hardware" is a claim worth being able to
    // check, which is exactly how the MF path's ceiling was found.
    virtual const wchar_t* Name() const noexcept = 0;
    virtual bool IsHardware() const noexcept = 0;

    virtual uint64_t DecodedFrames() const noexcept = 0;
    virtual uint64_t DroppedFrames() const noexcept = 0;

    /// Pictures the decoder was asked for and could not produce. Nonzero means
    /// the reference chain is broken in a way only a keyframe can repair —
    /// and on a static desktop under the keep-alive throttle, nothing else
    /// ever will, which is how a grey screen becomes permanent.
    virtual uint64_t DecodeErrors() const noexcept = 0;

    /// Which kind of error, in words. Empty when the implementation has no
    /// breakdown to offer.
    virtual std::wstring ErrorDetail() const noexcept { return {}; }
};

}  // namespace echo
