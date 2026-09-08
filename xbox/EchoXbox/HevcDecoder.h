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

namespace echo {

class HevcDecoder {
public:
    ~HevcDecoder();

    // `device` is the renderer's device, so decoded textures need no cross-
    // device copy. Returns false and fills `error` with something worth putting
    // on the TV.
    bool Initialize(ID3D11Device* device, uint32_t width, uint32_t height,
                    std::wstring& error) noexcept;

    void Shutdown() noexcept;

    // Feed one access unit. `pts100ns` comes from echo_fill_buffer's meta[2].
    // `keyframe` sets MFSampleExtension_CleanPoint; `discontinuity` should be
    // true for the first frame after any gap, so the decoder does not conceal
    // against references it never received.
    bool Submit(const uint8_t* data, uint32_t length, bool keyframe,
                bool discontinuity, int64_t pts100ns) noexcept;

    // Pull the next decoded picture, if one is ready. `texture`/`subresource`
    // describe an NV12 surface owned by the decoder's pool — valid until the
    // next call. Returns false when nothing is ready, which is normal.
    bool TryGetFrame(winrt::com_ptr<ID3D11Texture2D>& texture,
                     uint32_t& subresource) noexcept;

    // The decoder's own output geometry, which is what the renderer must scale
    // from. Only valid after the first successful TryGetFrame.
    uint32_t Width()  const noexcept { return m_width; }
    uint32_t Height() const noexcept { return m_height; }

    const wchar_t* Name() const noexcept { return m_name.c_str(); }
    bool IsHardware() const noexcept { return m_hardware; }
    uint64_t DecodedFrames() const noexcept { return m_decoded; }
    /// Decoded pictures thrown away because a newer one was already waiting.
    /// Not an error: a steady trickle is this client refusing to fall behind,
    /// and a number that climbs with the stream is the sign the panel cannot
    /// keep up with the frame rate being negotiated.
    uint64_t DroppedFrames() const noexcept { return m_dropped; }

private:
    // How many superseded pictures one call may throw away. Bounded so a
    // decoder that answers "ready" forever cannot wedge the present thread;
    // whatever is left is dropped on the next pass a moment later.
    static constexpr int kMaxDrain = 32;

    bool ConfigureTypes(uint32_t width, uint32_t height, std::wstring& error) noexcept;
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
