// Capture the last frame the encoder actually produced, as a PNG.
//
// This exists for the bug reporter: "the picture was wrong" is the single least
// actionable report Nova receives, and the one piece of evidence that would
// settle it — what the host was encoding at that moment — has never left the
// GPU. A screenshot from the client answers a different question, because
// everything between NVENC and the client's Surface can also be at fault.
//
// ## Armed on demand, never running
//
// The obvious implementation keeps a rolling CPU copy of every frame so a
// snapshot is always available. That is a full-frame GPU->CPU readback per
// frame: at 4K FP16 it is ~66 MB across PCIe every 8.3 ms, plus a Map that
// stalls the capture thread for a queue drain. This project has already removed
// exactly one such per-frame stall for exactly that reason (the fence spin in
// EncodeFrame, 15.4), and re-adding one so a dialog can be fast would be the
// same mistake wearing a different hat.
//
// So nothing happens until somebody arms it. ArmFrameSnapshot sets a flag; the
// next EncodeFrame does one readback and clears it. The cost when nothing is
// armed is a relaxed atomic load on the hot path. The cost when armed is one
// frame's worth of stall, once, at the moment a human is filling in a dialog
// and cannot tell.
//
// ## Why the readback and the encode are separate
//
// The Map has to happen on the thread that owns the immediate context — the
// D3D11 immediate context is not thread-safe and capture/convert/encode is
// single-owner on it by deliberate design. PNG encoding is a CPU-bound
// compression of several megabytes, and doing that on the capture thread would
// drop frames for as long as it took.
//
// So the capture thread's whole job is: copy to staging, Map, memcpy out,
// Unmap. Everything after that runs on whichever thread called
// TakeFrameSnapshotPng, which is the bug reporter's.
//
// ## Colour
//
// This is a diagnostic image, not a colour-accurate one. An HDR session's
// composite is FP16 scRGB, which has no honest 8-bit representation; the
// conversion below applies the session's SDR white level and clamps. A snapshot
// of an HDR stream will look flat next to the real thing. That is the correct
// trade for the purpose: the questions it answers are "was the frame black",
// "was it the wrong desktop", "was it torn", "was the cursor stamped" — none of
// which need the last stop of highlight detail.

#include "snapshot.h"

#include <windows.h>
#include <wincodec.h>
#include <cmath>
#include <cstring>
#include <cstdint>
#include <atomic>
#include <mutex>
#include <vector>

#pragma comment(lib, "windowscodecs.lib")

// shim.cpp owns the logger; this file borrows it rather than opening a second
// handle on the same file (see debug.rs on why that matters here).
extern void NovaShimLogExternal(const char* fmt, ...);

namespace {

// A frame that has been pulled back to the CPU and is waiting to be encoded.
struct CpuSnapshot {
    std::vector<uint8_t> pixels;  // tightly packed, `stride` bytes per row
    UINT width = 0;
    UINT height = 0;
    UINT stride = 0;
    DXGI_FORMAT format = DXGI_FORMAT_UNKNOWN;
    bool valid = false;
};

std::atomic<bool> g_armed{false};
std::mutex g_lock;             // guards g_pending only
CpuSnapshot g_pending;

// Reused across snapshots so a second report does not reallocate. Released with
// the device it was created on — tracked so a backend swap (which builds a new
// device) cannot hand a stale staging texture a texture from a different one.
ID3D11Texture2D* g_staging = nullptr;
ID3D11Device* g_stagingDevice = nullptr;
UINT g_stagingW = 0, g_stagingH = 0;
DXGI_FORMAT g_stagingFmt = DXGI_FORMAT_UNKNOWN;

void ReleaseStaging() {
    if (g_staging) { g_staging->Release(); g_staging = nullptr; }
    g_stagingDevice = nullptr;
    g_stagingW = g_stagingH = 0;
    g_stagingFmt = DXGI_FORMAT_UNKNOWN;
}

bool EnsureStaging(ID3D11Device* device, const D3D11_TEXTURE2D_DESC& src) {
    if (g_staging && g_stagingDevice == device && g_stagingW == src.Width &&
        g_stagingH == src.Height && g_stagingFmt == src.Format) {
        return true;
    }
    ReleaseStaging();

    D3D11_TEXTURE2D_DESC desc = {};
    desc.Width = src.Width;
    desc.Height = src.Height;
    desc.MipLevels = 1;
    desc.ArraySize = 1;
    desc.Format = src.Format;
    desc.SampleDesc.Count = 1;
    desc.Usage = D3D11_USAGE_STAGING;
    desc.BindFlags = 0;
    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    desc.MiscFlags = 0;

    if (FAILED(device->CreateTexture2D(&desc, nullptr, &g_staging))) {
        NovaShimLogExternal("[Snapshot] staging texture %ux%u fmt=0x%X could not be created\n",
                            src.Width, src.Height, (unsigned)src.Format);
        return false;
    }
    g_stagingDevice = device;
    g_stagingW = src.Width;
    g_stagingH = src.Height;
    g_stagingFmt = src.Format;
    return true;
}

// ── Pixel conversion ────────────────────────────────────────────────────────

float HalfToFloat(uint16_t h) {
    const uint32_t sign = (uint32_t)(h & 0x8000u) << 16;
    uint32_t exp = (h >> 10) & 0x1Fu;
    uint32_t man = h & 0x3FFu;
    if (exp == 0) {
        if (man == 0) { uint32_t bits = sign; float f; memcpy(&f, &bits, 4); return f; }
        // Subnormal: normalise it.
        while (!(man & 0x400u)) { man <<= 1; exp--; }
        exp++;
        man &= 0x3FFu;
    } else if (exp == 31) {
        uint32_t bits = sign | 0x7F800000u | (man << 13);
        float f; memcpy(&f, &bits, 4); return f;
    }
    uint32_t bits = sign | ((exp + 112u) << 23) | (man << 13);
    float f;
    memcpy(&f, &bits, 4);
    return f;
}

uint8_t LinearToSrgb8(float v) {
    if (!(v > 0.0f)) return 0;      // also catches NaN
    if (v > 1.0f) v = 1.0f;
    const float s = (v <= 0.0031308f) ? (v * 12.92f)
                                      : (1.055f * powf(v, 1.0f / 2.4f) - 0.055f);
    const int i = (int)(s * 255.0f + 0.5f);
    return (uint8_t)(i < 0 ? 0 : (i > 255 ? 255 : i));
}

// Convert whatever the composite is into the 32bpp BGRA that WIC will encode.
//
// Returns false for a format nothing here understands, which is deliberately
// preferred over emitting a plausible-looking wrong picture: a bug report whose
// attached frame is subtly mis-decoded sends the reader after the wrong bug.
bool ToBgra8(const CpuSnapshot& src, std::vector<uint8_t>& out, UINT& outStride) {
    outStride = src.width * 4;
    out.resize((size_t)outStride * src.height);

    switch (src.format) {
    case DXGI_FORMAT_B8G8R8A8_UNORM:
    case DXGI_FORMAT_B8G8R8A8_UNORM_SRGB:
    case DXGI_FORMAT_B8G8R8X8_UNORM:
        for (UINT y = 0; y < src.height; ++y) {
            const uint8_t* row = src.pixels.data() + (size_t)y * src.stride;
            uint8_t* dst = out.data() + (size_t)y * outStride;
            memcpy(dst, row, outStride);
            // Force alpha opaque. The composite's alpha is whatever the capture
            // left there and is not meaningful; a PNG that honoured it would
            // show a checkerboard in half the viewers people open it in.
            for (UINT x = 0; x < src.width; ++x) dst[x * 4 + 3] = 0xFF;
        }
        return true;

    case DXGI_FORMAT_R8G8B8A8_UNORM:
    case DXGI_FORMAT_R8G8B8A8_UNORM_SRGB:
        for (UINT y = 0; y < src.height; ++y) {
            const uint8_t* row = src.pixels.data() + (size_t)y * src.stride;
            uint8_t* dst = out.data() + (size_t)y * outStride;
            for (UINT x = 0; x < src.width; ++x) {
                dst[x * 4 + 0] = row[x * 4 + 2];  // B
                dst[x * 4 + 1] = row[x * 4 + 1];  // G
                dst[x * 4 + 2] = row[x * 4 + 0];  // R
                dst[x * 4 + 3] = 0xFF;
            }
        }
        return true;

    case DXGI_FORMAT_R16G16B16A16_FLOAT: {
        // scRGB: linear, 1.0 == SDR white, values above 1.0 are HDR highlights.
        // Clamped rather than tone-mapped — see the colour note at the top.
        for (UINT y = 0; y < src.height; ++y) {
            const uint16_t* row =
                reinterpret_cast<const uint16_t*>(src.pixels.data() + (size_t)y * src.stride);
            uint8_t* dst = out.data() + (size_t)y * outStride;
            for (UINT x = 0; x < src.width; ++x) {
                const float r = HalfToFloat(row[x * 4 + 0]);
                const float g = HalfToFloat(row[x * 4 + 1]);
                const float b = HalfToFloat(row[x * 4 + 2]);
                dst[x * 4 + 0] = LinearToSrgb8(b);
                dst[x * 4 + 1] = LinearToSrgb8(g);
                dst[x * 4 + 2] = LinearToSrgb8(r);
                dst[x * 4 + 3] = 0xFF;
            }
        }
        return true;
    }

    default:
        NovaShimLogExternal("[Snapshot] no conversion for capture format 0x%X — refusing to "
                            "guess rather than attach a mis-decoded frame\n",
                            (unsigned)src.format);
        return false;
    }
}

// ── PNG ─────────────────────────────────────────────────────────────────────

int WritePng(const wchar_t* path, const std::vector<uint8_t>& bgra, UINT width, UINT height,
             UINT stride) {
    // WIC needs COM. The bug reporter's thread is not guaranteed to have
    // initialised it, and RPC_E_CHANGED_MODE means somebody already did with a
    // different apartment — which is fine, we just must not uninitialise it.
    const HRESULT init = CoInitializeEx(nullptr, COINIT_MULTITHREADED);
    const bool uninit = SUCCEEDED(init);

    IWICImagingFactory* factory = nullptr;
    IWICBitmapEncoder* encoder = nullptr;
    IWICBitmapFrameEncode* frame = nullptr;
    IWICStream* stream = nullptr;
    int rc = -1;

    HRESULT hr = CoCreateInstance(CLSID_WICImagingFactory, nullptr, CLSCTX_INPROC_SERVER,
                                  IID_PPV_ARGS(&factory));
    if (FAILED(hr)) { rc = -2; goto done; }

    hr = factory->CreateStream(&stream);
    if (FAILED(hr)) { rc = -3; goto done; }
    hr = stream->InitializeFromFilename(path, GENERIC_WRITE);
    if (FAILED(hr)) { rc = -4; goto done; }

    hr = factory->CreateEncoder(GUID_ContainerFormatPng, nullptr, &encoder);
    if (FAILED(hr)) { rc = -5; goto done; }
    hr = encoder->Initialize(stream, WICBitmapEncoderNoCache);
    if (FAILED(hr)) { rc = -6; goto done; }

    hr = encoder->CreateNewFrame(&frame, nullptr);
    if (FAILED(hr)) { rc = -7; goto done; }
    hr = frame->Initialize(nullptr);
    if (FAILED(hr)) { rc = -8; goto done; }
    hr = frame->SetSize(width, height);
    if (FAILED(hr)) { rc = -9; goto done; }
    {
        WICPixelFormatGUID fmt = GUID_WICPixelFormat32bppBGRA;
        hr = frame->SetPixelFormat(&fmt);
        if (FAILED(hr)) { rc = -10; goto done; }
    }
    hr = frame->WritePixels(height, stride, (UINT)bgra.size(), const_cast<BYTE*>(bgra.data()));
    if (FAILED(hr)) { rc = -11; goto done; }
    hr = frame->Commit();
    if (FAILED(hr)) { rc = -12; goto done; }
    hr = encoder->Commit();
    if (FAILED(hr)) { rc = -13; goto done; }
    rc = 0;

done:
    if (frame) frame->Release();
    if (encoder) encoder->Release();
    if (stream) stream->Release();
    if (factory) factory->Release();
    if (uninit) CoUninitialize();
    if (rc != 0) {
        NovaShimLogExternal("[Snapshot] PNG encode failed at step %d (hr=0x%08X)\n", rc,
                            (unsigned)hr);
    }
    return rc;
}

}  // namespace

// ── Public surface ──────────────────────────────────────────────────────────

void NovaSnapshotOnFrame(ID3D11Device* device, ID3D11DeviceContext* context,
                         ID3D11Texture2D* composite) {
    // The hot-path cost of this whole file, when nothing is armed.
    if (!g_armed.load(std::memory_order_relaxed)) return;
    if (!device || !context || !composite) return;

    // Disarm first. A readback that fails must not leave the flag set, or every
    // subsequent frame retries the same failing path forever.
    g_armed.store(false, std::memory_order_relaxed);

    D3D11_TEXTURE2D_DESC desc = {};
    composite->GetDesc(&desc);
    if (!EnsureStaging(device, desc)) return;

    context->CopyResource(g_staging, composite);

    D3D11_MAPPED_SUBRESOURCE mapped = {};
    // No D3D11_MAP_FLAG_DO_NOT_WAIT: this Map is *supposed* to stall until the
    // copy lands. The whole point is to get this exact frame, and a
    // returns-immediately variant would give us a race between the queue and a
    // dialog box.
    if (FAILED(context->Map(g_staging, 0, D3D11_MAP_READ, 0, &mapped))) {
        NovaShimLogExternal("[Snapshot] staging Map failed — no frame captured\n");
        return;
    }

    CpuSnapshot shot;
    shot.width = desc.Width;
    shot.height = desc.Height;
    shot.stride = mapped.RowPitch;
    shot.format = desc.Format;
    shot.pixels.resize((size_t)mapped.RowPitch * desc.Height);
    memcpy(shot.pixels.data(), mapped.pData, shot.pixels.size());
    shot.valid = true;
    context->Unmap(g_staging, 0);

    {
        std::lock_guard<std::mutex> guard(g_lock);
        g_pending = std::move(shot);
    }
    NovaShimLogExternal("[Snapshot] captured %ux%u fmt=0x%X\n", desc.Width, desc.Height,
                        (unsigned)desc.Format);
}

// Ask the encode loop for its next frame.
//
// Returns nothing: whether a frame arrives depends on whether the encoder runs
// again, which the caller cannot be told synchronously and should not wait on
// forever. TakeFrameSnapshotPng reports the outcome.
extern "C" __declspec(dllexport) void ArmFrameSnapshot() {
    g_armed.store(true, std::memory_order_relaxed);
}

// Encode whatever the last armed capture produced.
//
//   0  written
//  -1  nothing captured (never armed, or no frame encoded since arming)
//  <-1 conversion or PNG failure, logged
//
// Safe to call from any thread. Deliberately does NOT arm-and-wait: a host that
// is not streaming never calls EncodeFrame, so a blocking version would hang the
// tray thread for as long as nobody was watching.
extern "C" __declspec(dllexport) int TakeFrameSnapshotPng(const wchar_t* path) {
    CpuSnapshot shot;
    {
        std::lock_guard<std::mutex> guard(g_lock);
        if (!g_pending.valid) return -1;
        shot = std::move(g_pending);
        g_pending = CpuSnapshot{};
    }

    std::vector<uint8_t> bgra;
    UINT stride = 0;
    if (!ToBgra8(shot, bgra, stride)) return -20;
    return WritePng(path, bgra, shot.width, shot.height, stride);
}

// Drop any held frame and free the staging texture.
//
// Called when a session ends. A snapshot is a picture of somebody's desktop; it
// has no business outliving the stream it came from just because nobody pressed
// the report button.
extern "C" __declspec(dllexport) void ResetFrameSnapshot() {
    g_armed.store(false, std::memory_order_relaxed);
    {
        std::lock_guard<std::mutex> guard(g_lock);
        g_pending = CpuSnapshot{};
    }
    ReleaseStaging();
}
