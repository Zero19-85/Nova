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
