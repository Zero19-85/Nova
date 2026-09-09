// The hardware HEVC decoder: a Media Foundation transform on our D3D11 device.
//
// Input is exactly what `echo_fill_buffer` produces — Annex-B access units with
// start codes, already reassembled, FEC-repaired, decrypted and gated by
// `echo-client`. Output is an NV12 texture on the GPU that the renderer blits
// straight to the back buffer. Nothing is ever read back to the CPU.
//
// ── The settings that are not optional ──────────────────────────────────────
//
// Four of these separate a ~10 ms decoder from a ~60 ms one, or from one that
// silently produces nothing. They are called out here because each has a
// failure mode that points somewhere else:
//
//   MF_LOW_LATENCY          without it the decoder holds frames to reorder
//                           them. Nova emits no B-frames, so that is pure
//                           added latency and it looks like network delay.
//   SET_D3D_MANAGER         must be sent BEFORE media types. Out of order it
//                           quietly falls back to software: high CPU, keeps up
//                           at 1080p, drowns at 4K.
//   MFVideoFormat_HEVC      the byte-stream (Annex-B) subtype. `_HEVC_ES`
//                           expects length-prefixed NALs and decodes nothing.
//   SetMultithreadProtected set on the device at creation (VideoRenderer) —
//                           the MFT decodes on its own threads.
#pragma once

#include <cstdint>
#include <mutex>
#include <string>

#include <d3d11_4.h>
#include <mfapi.h>
#include <mfidl.h>
#include <mftransform.h>
#include <winrt/base.h>

#include "VideoDecoder.h"

namespace echo {

// What this console will actually admit to being able to decode.
//
// This exists because `kMaxDecodePixelRate` in MainPage.cpp — the budget that
// refuses 4K120 — was INFERRED from a run of blank sessions, not read off the
// hardware. An inferred ceiling is fine as a safety net and useless as
// evidence: it cannot tell "the silicon cannot do this" apart from "the MFT
// declined to", and those two answers point at completely different work.
//
// Two independent questions, deliberately asked through two different APIs:
//
//   maxMbPerSec   what the Media Foundation transform DECLARES, through
//                 MF_VIDEO_MAX_MB_PER_SEC. A macroblock is 16x16, so
//                 `* 256` is the pixel rate it is claiming. Zero means the
//                 decoder does not expose the attribute, which is allowed and
//                 is itself worth knowing.
//   dxva*         what the D3D11 video device offers, which is the SAME query
//                 FFmpeg's d3d11va hwaccel makes when it sets up. A refusal
//                 here says an FFmpeg port would hit the identical wall; a
//                 pass says the MF layer is the thing in the way, and that
//                 the port is worth building.
struct DecoderProbe {
    bool         ran = false;
    std::wstring name = L"(none)";
    bool         hardware = false;
    bool         async = false;

    // Both enumerations, reported separately. "Which decoder did we get" is a
    // conclusion; these are the evidence. A hardwareCount of 0 means the app
    // container was offered no hardware HEVC decoder at all — at ANY
    // resolution — which is a different fact from a decoder refusing a mode,
    // and the two point at different work.
    uint32_t     hardwareCount = 0;
    uint32_t     softwareCount = 0;
    std::wstring hardwareName;
    std::wstring softwareName;

    uint32_t     maxMbPerSec = 0;    // MF_VIDEO_MAX_MB_PER_SEC, 0 = not exposed
    uint64_t     maxPixelRate = 0;   // maxMbPerSec * 256, 0 when not exposed

    bool         dxvaMain = false;   // HEVC_VLD_MAIN offered by the video device
    bool         dxvaMain10 = false; // HEVC_VLD_MAIN10 — the HDR10 question too
    bool         nv12At4K = false;   // CheckVideoDecoderFormat(MAIN, NV12)
    uint32_t     configs4K = 0;      // decoder configurations at 3840x2160

    std::wstring notes;              // HRESULTs for whatever refused

    // One block of text for the log and the diagnostics panel. Deliberately
    // ends with a verdict in words: the numbers above are only useful to
    // somebody who already knows what they imply.
    std::wstring Report() const;
};

class HevcDecoder final : public VideoDecoder {
public:
    ~HevcDecoder() override;

    // Ask the hardware what it can do, without starting a session. Safe to
    // call before Initialize and cheap enough to run at startup; it activates
    // a transform, reads its attributes, and shuts it down again.
    static DecoderProbe Probe(ID3D11Device* device) noexcept;

    // `device` is the renderer's device, so decoded textures need no cross-
    // device copy. `fps` is the NEGOTIATED frame rate, not a nominal one —
    // see ConfigureTypes. Returns false and fills `error` with something worth
    // putting on the TV.
    bool Initialize(ID3D11Device* device, uint32_t width, uint32_t height,
                    uint32_t fps, std::wstring& error) noexcept;

    void Shutdown() noexcept override;

    // Feed one access unit. `pts100ns` comes from echo_fill_buffer's meta[2].
    // `keyframe` sets MFSampleExtension_CleanPoint; `discontinuity` should be
    // true for the first frame after any gap, so the decoder does not conceal
    // against references it never received.
    bool Submit(const uint8_t* data, uint32_t length, bool keyframe,
                bool discontinuity, int64_t pts100ns) noexcept override;

    // Pull the next decoded picture, if one is ready. `texture`/`subresource`
    // describe an NV12 surface owned by the decoder's pool — valid until the
    // next call. Returns false when nothing is ready, which is normal.
    bool TryGetFrame(winrt::com_ptr<ID3D11Texture2D>& texture,
                     uint32_t& subresource) noexcept override;

    // The decoder's own output geometry, which is what the renderer must scale
    // from. Only valid after the first successful TryGetFrame.
    uint32_t Width()  const noexcept override { return m_width; }
    uint32_t Height() const noexcept override { return m_height; }

    const wchar_t* Name() const noexcept override { return m_name.c_str(); }
    bool IsHardware() const noexcept override { return m_hardware; }
    uint64_t DecodedFrames() const noexcept override { return m_decoded; }
    /// Decoded pictures thrown away because a newer one was already waiting.
    /// Not an error: a steady trickle is this client refusing to fall behind,
    /// and a number that climbs with the stream is the sign the panel cannot
    /// keep up with the frame rate being negotiated.
    uint64_t DroppedFrames() const noexcept override { return m_dropped; }
    // The MF path has no equivalent signal, so it reports none rather than
    // inventing one. Zero here means "not measured", not "no errors".
    uint64_t DecodeErrors()  const noexcept override { return 0; }

private:
    // How many superseded pictures one call may throw away. Bounded so a
    // decoder that answers "ready" forever cannot wedge the present thread;
    // whatever is left is dropped on the next pass a moment later.
    static constexpr int kMaxDrain = 32;

    bool ConfigureTypes(uint32_t width, uint32_t height, uint32_t fps,
                        std::wstring& error) noexcept;
    bool NegotiateOutputType(std::wstring& error) noexcept;

    // Both assume `m_lock` is HELD by the caller.
    void DiscardPendingOutputs() noexcept;
    bool PullOne(winrt::com_ptr<ID3D11Texture2D>& texture, uint32_t& subresource) noexcept;

    winrt::com_ptr<IMFTransform>        m_transform;
    winrt::com_ptr<IMFDXGIDeviceManager> m_deviceManager;
    winrt::com_ptr<IMFSample>           m_outputSample;   // reused, never per-frame
    winrt::com_ptr<ID3D11Device>        m_device;

    // The MFT is fed from the network thread and drained from the present
    // thread. MF transforms are not required to be thread-safe across
    // ProcessInput/ProcessOutput, so one lock covers both.
    std::mutex m_lock;

    std::wstring m_name = L"(none)";
    bool     m_hardware = false;
    bool     m_started  = false;
    uint32_t m_width = 0;
    uint32_t m_height = 0;
    uint32_t m_resetToken = 0;
    uint64_t m_decoded = 0;
    uint64_t m_dropped = 0;
    bool     m_mfStarted = false;
};

}  // namespace echo
