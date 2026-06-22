#include <windows.h>
#include <d3d11.h>
#include <d3d11_1.h>
#include <d3d11_3.h>
#include <d3dcompiler.h>
#include <stdio.h>
#include <vector>
#include <atomic>

#include "nvEncodeAPI.h"
#include "NvEncoderD3D11.h"

// ==================== VIDEO PROCESSOR GLOBALS ====================
static ID3D11VideoDevice*              g_videoDevice   = nullptr;
static ID3D11VideoContext*             g_videoContext  = nullptr;
static ID3D11VideoProcessorEnumerator* g_vpEnum        = nullptr;
static ID3D11VideoProcessor*           g_vp            = nullptr;
// g_vpOutView targets NVENC's own input texture directly (see
// InitColorConversion) — there is no separate intermediate NV12 texture.
static ID3D11VideoProcessorOutputView* g_vpOutView     = nullptr;

// ==================== ENCODER GLOBALS ====================
static ID3D11Device*        g_device    = nullptr;
static ID3D11DeviceContext* g_context   = nullptr;
static NvEncoderD3D11*      g_nvEncoder = nullptr;
static std::atomic<bool>    g_force_idr{false};

// Persisted encoder configuration so ReconfigureBitrate() can rebuild
// NV_ENC_RECONFIGURE_PARAMS from the exact params the encoder was created
// with (only rate-control fields changed).
static NV_ENC_INITIALIZE_PARAMS g_initParams = {};
static NV_ENC_CONFIG            g_encConfig  = {};
static int                      g_encoderFps = 60;

// ==================== HDR GLOBALS ====================
static int                    g_encoderCodec     = 0;    // 0=H264, 1=HEVC, 2=AV1
static bool                   g_isHdr            = false;
static MASTERING_DISPLAY_INFO g_masteringDisplay  = {};
static CONTENT_LIGHT_LEVEL    g_contentLightLevel = {};
static bool                   g_hdrMetadataReady  = false;

// Raw HEVC SEI byte payloads built by BuildHdrSeiPayloads() and injected via
// NV_ENC_PIC_PARAMS_HEVC::seiPayloadArray on every forced IDR. This replicates
// exactly what FFmpeg (and therefore Apollo/Sunshine) does internally — the
// NVENC native pMasteringDisplay/pMaxCll path is silently ignored by the driver.
static uint8_t          g_mdcvSeiBytes[24]    = {};  // SEI type 137 payload
static uint8_t          g_cllSeiBytes[4]      = {};  // SEI type 144 payload
static NV_ENC_SEI_PAYLOAD g_hdrSeiPayloads[2] = {};

// HDR compute shader bridge: converts WGC's R16G16B16A16_FLOAT (linear scRGB)
// directly to P010 YCbCr 4:2:0 via ID3D11Device3 per-plane typed UAVs on a
// single DXGI_FORMAT_P010 intermediate texture (R16_UNORM for Y, R16G16_UNORM
// for UV). A single CopyResource (P010→P010, same format) then feeds NVENC —
// bypassing both the VP (which zeros chroma on P2020 declarations) and NVENC's
// internal RGB→YCbCr converter (hardwired BT.709, ignores HDR input).
// Each 8×8 thread group covers a 16×16 pixel tile (2×2 pixels per thread),
// writing 4 luma samples and 1 averaged chroma sample per thread.
static const char* kHdrCsHlsl = R"(
Texture2D<float4>         InputTex : register(t0);
RWTexture2D<unorm float>  YPlane   : register(u0);  // P010 plane 0 via R16_UNORM UAV
RWTexture2D<unorm float2> UVPlane  : register(u1);  // P010 plane 1 via R16G16_UNORM UAV

static const float3x3 scRGB_to_BT2020 = {
    0.627404f, 0.329282f, 0.0433136f,
    0.069097f, 0.919540f, 0.0113612f,
    0.016391f, 0.088013f, 0.895595f
};

// BT.2020 Non-Constant Luminance RGB to YCbCr (full-range coefficients).
static const float3x3 BT2020_to_YUV = {
     0.262700f,  0.678000f,  0.059300f,
    -0.139630f, -0.360370f,  0.500000f,
     0.500000f, -0.459786f, -0.040214f
};

float3 LinearToPQ(float3 linearColor) {
    // Windows scRGB: 1.0 = 80 nit SDR white, 125.0 = 10000 nit HDR peak.
    float3 L  = saturate(linearColor / 125.0f);
    float  m1 = 2610.0f / 16384.0f;
    float  m2 = (2523.0f / 4096.0f) * 128.0f;
    float  c1 = 3424.0f / 4096.0f;
    float  c2 = (2413.0f / 4096.0f) * 32.0f;
    float  c3 = (2392.0f / 4096.0f) * 32.0f;
    float3 Lp = pow(L, m1);
    return pow((c1 + c2 * Lp) / (1.0f + c3 * Lp), m2);
}

[numthreads(8, 8, 1)]
void main(uint3 DTid : SV_DispatchThreadID) {
    uint2 pos = DTid.xy * 2;

    // Bounds guard for non-16-aligned heights (e.g. 1080p: 1080 % 16 != 0).
    uint w, h;
    InputTex.GetDimensions(w, h);
    if (pos.x + 1 >= w || pos.y + 1 >= h) return;

    float3 rgb00 = LinearToPQ(mul(scRGB_to_BT2020, InputTex[pos].rgb));
    float3 rgb10 = LinearToPQ(mul(scRGB_to_BT2020, InputTex[pos + uint2(1, 0)].rgb));
    float3 rgb01 = LinearToPQ(mul(scRGB_to_BT2020, InputTex[pos + uint2(0, 1)].rgb));
    float3 rgb11 = LinearToPQ(mul(scRGB_to_BT2020, InputTex[pos + uint2(1, 1)].rgb));

    // Full Range Y: BT.2020 NCL luma maps naturally to [0, 1] for in-gamut PQ values.
    YPlane[pos]               = mul(BT2020_to_YUV, rgb00).x;
    YPlane[pos + uint2(1, 0)] = mul(BT2020_to_YUV, rgb10).x;
    YPlane[pos + uint2(0, 1)] = mul(BT2020_to_YUV, rgb01).x;
    YPlane[pos + uint2(1, 1)] = mul(BT2020_to_YUV, rgb11).x;

    // Average 4 PQ-converted pixels for 4:2:0 chroma (post-PQ averaging).
    float3 avg_rgb = (rgb00 + rgb10 + rgb01 + rgb11) * 0.25f;
    float3 yuv_avg = mul(BT2020_to_YUV, avg_rgb);
    // Shift Cb/Cr from [-0.5, 0.5] to [0.0, 1.0] for full-range UNORM storage.
    UVPlane[DTid.xy] = float2(yuv_avg.y + 0.5f, yuv_avg.z + 0.5f);
}
)";

// HDR compute shader bridge resources (lazily created on first HDR frame in EncodeFrame).
static ID3D11ComputeShader*       g_hdrCS      = nullptr; // compiled once at HDR init
static ID3D11Texture2D*           g_hdrP010Tex = nullptr; // DXGI_FORMAT_P010 intermediate
static ID3D11UnorderedAccessView* g_hdrYUAV    = nullptr; // Y plane (R16_UNORM, PlaneSlice=0)
static ID3D11UnorderedAccessView* g_hdrUVUAV   = nullptr; // UV plane (R16G16_UNORM, PlaneSlice=1)

// NVENC's pre-registered input texture — borrowed from NvEncoderD3D11 (do NOT Release).
// For HDR: P010/YUV420_10BIT — CS output CopyResource'd here, VP bypassed entirely.
// For SDR: NV12 — written by VideoProcessorBlt via g_vpOutView as before.
static ID3D11Texture2D* g_nvencInputTex = nullptr;

// ==================== CURSOR COMPOSITING GLOBALS ====================
// DXGI_OUTDUPL_POINTER_SHAPE_TYPE values (avoids pulling in dxgi1_2.h).
static const uint32_t kPointerShapeMonochrome  = 1;
static const uint32_t kPointerShapeColor       = 2;
static const uint32_t kPointerShapeMaskedColor = 4;

// Tiny fullscreen-triangle shader pair: the vertex shader emits a triangle
// that covers the whole viewport, and the pixel shader just samples the
// cursor texture. The cursor is positioned/sized purely via RSSetViewports
// (Sunshine's approach) — no per-vertex transform math needed.
static const char* kCursorShaderSrc = R"(
struct VS_OUT {
    float4 pos : SV_POSITION;
    float2 tex : TEXCOORD0;
};

VS_OUT main_vs(uint vid : SV_VertexID) {
    VS_OUT o;
    float2 t;
    if (vid == 0)      { o.pos = float4(-1, -1, 0, 1); t = float2(0, 1); }
    else if (vid == 1) { o.pos = float4(-1,  3, 0, 1); t = float2(0, -1); }
    else               { o.pos = float4( 3, -1, 0, 1); t = float2(2, 1); }
    o.tex = t;
    return o;
}

Texture2D cursorTex : register(t0);
SamplerState cursorSamp : register(s0);

float4 main_ps(VS_OUT input) : SV_TARGET {
    return cursorTex.Sample(cursorSamp, input.tex);
}
)";

static ID3D11VertexShader*       g_cursorVS          = nullptr;
static ID3D11PixelShader*        g_cursorPS          = nullptr;
static ID3D11BlendState*         g_cursorBlend       = nullptr;
static ID3D11BlendState*         g_cursorBlendInvert = nullptr;
static ID3D11SamplerState*       g_cursorSampler     = nullptr;

// Current cursor shape, uploaded as two small BGRA textures whenever DXGI
// reports a shape change (PointerShapeBufferSize > 0). Sunshine splits every
// cursor into an alpha-blended image and an XOR(invert)-blended image so
// monochrome/masked cursors (text I-beam etc.) render correctly.
static ID3D11Texture2D*          g_cursorTex     = nullptr;
static ID3D11ShaderResourceView* g_cursorSRV     = nullptr;
static ID3D11Texture2D*          g_cursorXorTex  = nullptr;
static ID3D11ShaderResourceView* g_cursorXorSRV  = nullptr;
static UINT                      g_cursorTexW    = 0;
static UINT                      g_cursorTexH    = 0;

// "Clean background" — a copy of the captured frame (dxgiFrame), refreshed
// every EncodeFrame call BEFORE the cursor overlay is drawn anywhere. Source
// for g_compositeTex below, so a DXGI_ERROR_WAIT_TIMEOUT replay (the same
// dxgiFrame re-submitted by Rust while the desktop is static) never
// re-composites onto an already cursor-stamped buffer.
static ID3D11Texture2D*          g_cleanBgTex    = nullptr;

// Render-targetable copy of g_cleanBgTex with the cursor overlay drawn on
// top — the VideoProcessorBlt source for NV12 conversion. Re-copied from
// g_cleanBgTex every EncodeFrame call (see EncodeFrame), so cursor pixels
// from a previous frame never persist into this one. The DXGI duplication
// texture isn't guaranteed to support D3D11_BIND_RENDER_TARGET, which is why
// the cursor is drawn onto this copy rather than dxgiFrame directly.
static ID3D11Texture2D*          g_compositeTex  = nullptr;
static ID3D11RenderTargetView*   g_compositeRTV  = nullptr;

// GPU fence used to block EncodeFrame() until the CopyResource of the DXGI
// duplication surface into g_cleanBgTex has actually finished on the GPU
// (see EncodeFrame for why this matters).
static ID3D11Query*              g_copyFence     = nullptr;

// Updated every frame from DXGI_OUTDUPL_FRAME_INFO.PointerPosition. Only
// touched from the single capture/encode thread, so no locking needed.
static int  g_cursorX       = 0;
static int  g_cursorY       = 0;
static bool g_cursorVisible = false;

// ==================== CURSOR COMPOSITING HELPERS ====================

// Compiles the cursor VS/PS, blend state and sampler once. Called from
// InitColorConversion since that's where we first have a device.
static bool InitCursorPipeline(ID3D11Device* device) {
    ID3DBlob* vsBlob = nullptr;
    ID3DBlob* psBlob = nullptr;
    ID3DBlob* errBlob = nullptr;

    HRESULT hr = D3DCompile(kCursorShaderSrc, strlen(kCursorShaderSrc), nullptr, nullptr, nullptr,
                             "main_vs", "vs_5_0", 0, 0, &vsBlob, &errBlob);
    if (FAILED(hr)) {
        if (errBlob) {
            printf("❌ Cursor VS compile error: %s\n", (char*)errBlob->GetBufferPointer());
            errBlob->Release();
        }
        return false;
    }
    hr = device->CreateVertexShader(vsBlob->GetBufferPointer(), vsBlob->GetBufferSize(), nullptr, &g_cursorVS);
    vsBlob->Release();
    if (FAILED(hr)) return false;

    hr = D3DCompile(kCursorShaderSrc, strlen(kCursorShaderSrc), nullptr, nullptr, nullptr,
                     "main_ps", "ps_5_0", 0, 0, &psBlob, &errBlob);
    if (FAILED(hr)) {
        if (errBlob) {
            printf("❌ Cursor PS compile error: %s\n", (char*)errBlob->GetBufferPointer());
            errBlob->Release();
        }
        return false;
    }
    hr = device->CreatePixelShader(psBlob->GetBufferPointer(), psBlob->GetBufferSize(), nullptr, &g_cursorPS);
    psBlob->Release();
    if (FAILED(hr)) return false;

    // Standard alpha blending (Sunshine's blend_alpha): out.rgb = src.rgb*srcA + dst.rgb*(1-srcA).
    D3D11_BLEND_DESC bdesc = {};
    D3D11_RENDER_TARGET_BLEND_DESC& rt = bdesc.RenderTarget[0];
    rt.BlendEnable           = TRUE;
    rt.SrcBlend              = D3D11_BLEND_SRC_ALPHA;
    rt.DestBlend             = D3D11_BLEND_INV_SRC_ALPHA;
    rt.BlendOp               = D3D11_BLEND_OP_ADD;
    rt.SrcBlendAlpha         = D3D11_BLEND_ZERO;
    rt.DestBlendAlpha        = D3D11_BLEND_ZERO;
    rt.BlendOpAlpha          = D3D11_BLEND_OP_ADD;
    rt.RenderTargetWriteMask = D3D11_COLOR_WRITE_ENABLE_ALL;
    hr = device->CreateBlendState(&bdesc, &g_cursorBlend);
    if (FAILED(hr)) return false;

    // Invert blending (Sunshine's blend_invert): out.rgb = src.rgb*(1-dst.rgb) + dst.rgb*(1-src.rgb).
    // Where the XOR image is white this inverts the screen; where it's
    // transparent (black) the screen passes through unchanged.
    rt.SrcBlend  = D3D11_BLEND_INV_DEST_COLOR;
    rt.DestBlend = D3D11_BLEND_INV_SRC_COLOR;
    hr = device->CreateBlendState(&bdesc, &g_cursorBlendInvert);
    if (FAILED(hr)) return false;

    D3D11_SAMPLER_DESC sdesc = {};
    sdesc.Filter         = D3D11_FILTER_MIN_MAG_MIP_LINEAR;
    sdesc.AddressU       = D3D11_TEXTURE_ADDRESS_CLAMP;
    sdesc.AddressV       = D3D11_TEXTURE_ADDRESS_CLAMP;
    sdesc.AddressW       = D3D11_TEXTURE_ADDRESS_CLAMP;
    sdesc.ComparisonFunc = D3D11_COMPARISON_NEVER;
    sdesc.MaxLOD         = D3D11_FLOAT32_MAX;
    hr = device->CreateSamplerState(&sdesc, &g_cursorSampler);
    if (FAILED(hr)) return false;

    printf("✅ Cursor compositing pipeline initialized\n");
    return true;
}

// Ports Sunshine's make_cursor_alpha_image (display_vram.cpp) for the
// MONOCHROME / COLOR / MASKED_COLOR pointer shape types. Pixels that need
// "inverse of screen" treatment are left transparent here and handled by
// build_cursor_xor_image + invert blending below.
static std::vector<uint8_t> build_cursor_alpha_image(
    const uint8_t* data, size_t data_len,
    uint32_t type, uint32_t width, uint32_t height, uint32_t pitch,
    uint32_t& out_width, uint32_t& out_height)
{
    constexpr uint32_t black       = 0xFF000000;
    constexpr uint32_t white       = 0xFFFFFFFF;
    constexpr uint32_t transparent = 0x00000000;

    out_width  = 0;
    out_height = 0;

    if (type == kPointerShapeColor || type == kPointerShapeMaskedColor) {
        if (pitch == 0 || width == 0 || height == 0) return {};
        if ((size_t)pitch * height > data_len) return {};

        std::vector<uint8_t> img((size_t)width * height * 4);
        for (uint32_t y = 0; y < height; ++y) {
            memcpy(img.data() + (size_t)y * width * 4, data + (size_t)y * pitch, (size_t)width * 4);
        }

        if (type == kPointerShapeMaskedColor) {
            uint32_t* pixels = (uint32_t*)img.data();
            for (size_t i = 0; i < (size_t)width * height; ++i) {
                uint8_t alpha = (uint8_t)((pixels[i] >> 24) & 0xFF);
                if (alpha == 0xFF) {
                    // Handled by build_cursor_xor_image — transparent here.
                    pixels[i] = transparent;
                } else if (alpha == 0x00) {
                    // Fully opaque in the alpha-blended image.
                    pixels[i] |= 0xFF000000;
                }
            }
        }

        out_width  = width;
        out_height = height;
        return img;
    }

    if (type == kPointerShapeMonochrome) {
        if (pitch == 0 || width == 0 || height < 2) return {};
        uint32_t out_h = height / 2;
        size_t bytes = (size_t)pitch * out_h;
        if (bytes * 2 > data_len) return {};

        std::vector<uint8_t> img((size_t)width * out_h * 4);
        uint32_t* pixel_data = (uint32_t*)img.data();
        const uint8_t* and_mask = data;
        const uint8_t* xor_mask = data + bytes;

        size_t total_pixels = (size_t)width * out_h;
        size_t pixel_index = 0;
        for (size_t b = 0; b < bytes && pixel_index < total_pixels; ++b) {
            uint8_t and_byte = and_mask[b];
            uint8_t xor_byte = xor_mask[b];
            for (int bit = 7; bit >= 0 && pixel_index < total_pixels; --bit) {
                uint32_t mask = 1u << bit;
                int color_type = ((and_byte & mask) ? 1 : 0) + ((xor_byte & mask) ? 2 : 0);
                uint32_t pixel;
                switch (color_type) {
                    case 0:  pixel = black;       break; // opaque black
                    case 2:  pixel = white;       break; // opaque white
                    default: pixel = transparent; break; // screen color / inverse (XOR-only)
                }
                pixel_data[pixel_index++] = pixel;
            }
        }

        out_width  = width;
        out_height = out_h;
        return img;
    }

    return {};
}

// Ports Sunshine's make_cursor_xor_image: builds the image drawn with invert
// blending. White pixels invert the screen underneath ("inverse of screen"
// regions of monochrome/masked-color cursors); transparent pixels leave it
// unchanged. COLOR cursors need no XOR pass and return empty.
static std::vector<uint8_t> build_cursor_xor_image(
    const uint8_t* data, size_t data_len,
    uint32_t type, uint32_t width, uint32_t height, uint32_t pitch,
    uint32_t& out_width, uint32_t& out_height)
{
    constexpr uint32_t inverted    = 0xFFFFFFFF;
    constexpr uint32_t transparent = 0x00000000;

    out_width  = 0;
    out_height = 0;

    if (type == kPointerShapeColor) return {};

    if (type == kPointerShapeMaskedColor) {
        if (pitch == 0 || width == 0 || height == 0) return {};
        if ((size_t)pitch * height > data_len) return {};

        std::vector<uint8_t> img((size_t)width * height * 4);
        for (uint32_t y = 0; y < height; ++y) {
            memcpy(img.data() + (size_t)y * width * 4, data + (size_t)y * pitch, (size_t)width * 4);
        }

        uint32_t* pixels = (uint32_t*)img.data();
        for (size_t i = 0; i < (size_t)width * height; ++i) {
            uint8_t alpha = (uint8_t)((pixels[i] >> 24) & 0xFF);
            if (alpha == 0xFF) {
                // XOR-blended as is.
            } else {
                // Handled by build_cursor_alpha_image — transparent here.
                pixels[i] = transparent;
            }
        }

        out_width  = width;
        out_height = height;
        return img;
    }

    if (type == kPointerShapeMonochrome) {
        if (pitch == 0 || width == 0 || height < 2) return {};
        uint32_t out_h = height / 2;
        size_t bytes = (size_t)pitch * out_h;
        if (bytes * 2 > data_len) return {};

        std::vector<uint8_t> img((size_t)width * out_h * 4);
        uint32_t* pixel_data = (uint32_t*)img.data();
        const uint8_t* and_mask = data;
        const uint8_t* xor_mask = data + bytes;

        size_t total_pixels = (size_t)width * out_h;
        size_t pixel_index = 0;
        for (size_t b = 0; b < bytes && pixel_index < total_pixels; ++b) {
            uint8_t and_byte = and_mask[b];
            uint8_t xor_byte = xor_mask[b];
            for (int bit = 7; bit >= 0 && pixel_index < total_pixels; --bit) {
                uint32_t mask = 1u << bit;
                int color_type = ((and_byte & mask) ? 1 : 0) + ((xor_byte & mask) ? 2 : 0);
                // case 3 = inverse of screen; everything else handled by the alpha image.
                pixel_data[pixel_index++] = (color_type == 3) ? inverted : transparent;
            }
        }

        out_width  = width;
        out_height = out_h;
        return img;
    }

    return {};
}

// Lazily (re)creates a Texture2D sized to width x height, optionally with a
// render-target view, releasing and recreating it if it already exists at a
// different size. A topology/resolution change mid-session (e.g. the Phase 5
// virtual-display swap) would otherwise leave a texture created at the old
// size: CopyResource into it silently no-ops on a size mismatch, so it keeps
// whatever stale (possibly cursor-stamped) content it last held.
static bool EnsureSizedTexture(ID3D11Texture2D** tex, ID3D11RenderTargetView** rtv, int width, int height, UINT bindFlags, DXGI_FORMAT format = DXGI_FORMAT_B8G8R8A8_UNORM) {
    if (*tex) {
        D3D11_TEXTURE2D_DESC existing = {};
        (*tex)->GetDesc(&existing);
        // Must match on ALL three: width, height, AND format. A same-resolution
        // SDR→HDR session switch keeps the same dimensions but needs to change
        // from BGRA8 to R16G16B16A16_FLOAT. Without the format check,
        // CopyResource(BGRA8_tex, FP16_src) silently no-ops → stale content →
        // VideoProcessorBlt reads wrong data → encode_frame returns 0 bytes.
        if ((int)existing.Width == width && (int)existing.Height == height && existing.Format == format) return true;

        if (rtv && *rtv) { (*rtv)->Release(); *rtv = nullptr; }
        (*tex)->Release();
        *tex = nullptr;
    }

    D3D11_TEXTURE2D_DESC desc = {};
    desc.Width            = width;
    desc.Height           = height;
    desc.MipLevels        = 1;
    desc.ArraySize        = 1;
    desc.Format           = format;
    desc.SampleDesc.Count = 1;
    desc.Usage            = D3D11_USAGE_DEFAULT;
    desc.BindFlags        = bindFlags;

    HRESULT hr = g_device->CreateTexture2D(&desc, nullptr, tex);
    if (FAILED(hr)) return false;

    if (rtv) {
        hr = g_device->CreateRenderTargetView(*tex, nullptr, rtv);
        if (FAILED(hr)) {
            (*tex)->Release();
            *tex = nullptr;
            return false;
        }
    }
    return true;
}

// Draws the current cursor texture onto g_compositeTex at (g_cursorX, g_cursorY),
// alpha-blended. The viewport — not a per-vertex transform — positions and
// sizes the draw, matching Sunshine's blend_cursor approach.
static void DrawCursorOverlay() {
    g_context->IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
    g_context->IASetInputLayout(nullptr);
    g_context->VSSetShader(g_cursorVS, nullptr, 0);
    g_context->PSSetShader(g_cursorPS, nullptr, 0);
    g_context->PSSetSamplers(0, 1, &g_cursorSampler);
    g_context->OMSetRenderTargets(1, &g_compositeRTV, nullptr);

    D3D11_VIEWPORT vp = {};
    vp.TopLeftX = (float)g_cursorX;
    vp.TopLeftY = (float)g_cursorY;
    vp.Width    = (float)g_cursorTexW;
    vp.Height   = (float)g_cursorTexH;
    vp.MinDepth = 0.0f;
    vp.MaxDepth = 1.0f;
    g_context->RSSetViewports(1, &vp);

    if (g_cursorSRV) {
        // Alpha-blended pass.
        g_context->OMSetBlendState(g_cursorBlend, nullptr, 0xFFFFFFFF);
        g_context->PSSetShaderResources(0, 1, &g_cursorSRV);
        g_context->Draw(3, 0);
    }

    if (g_cursorXorSRV) {
        // Invert pass for "inverse of screen" pixels, without touching alpha
        // (Sunshine masks alpha out via the 0x00FFFFFF sample mask).
        g_context->OMSetBlendState(g_cursorBlendInvert, nullptr, 0x00FFFFFF);
        g_context->PSSetShaderResources(0, 1, &g_cursorXorSRV);
        g_context->Draw(3, 0);
    }

    // Unbind so the Video Processor (and next frame's draw) start clean.
    ID3D11ShaderResourceView* nullSRV = nullptr;
    g_context->PSSetShaderResources(0, 1, &nullSRV);
    ID3D11RenderTargetView* nullRTV = nullptr;
    g_context->OMSetRenderTargets(1, &nullRTV, nullptr);
    g_context->OMSetBlendState(nullptr, nullptr, 0xFFFFFFFF);
    g_context->RSSetViewports(0, nullptr);
}

// ==================== CURSOR SHAPE / POSITION UPDATES ====================
// Called from the capture loop when DXGI_OUTDUPL_FRAME_INFO.PointerShapeBufferSize > 0
// — i.e. only when the cursor's shape actually changed, not every frame.
// Uploads one cursor image as an immutable BGRA texture + SRV. Empty images
// leave the texture null (that blend pass is skipped).
static bool UploadCursorImage(
    const std::vector<uint8_t>& img, uint32_t w, uint32_t h,
    ID3D11Texture2D** tex, ID3D11ShaderResourceView** srv)
{
    if (img.empty() || w == 0 || h == 0) return true;

    D3D11_TEXTURE2D_DESC tdesc = {};
    tdesc.Width            = w;
    tdesc.Height           = h;
    tdesc.MipLevels        = 1;
    tdesc.ArraySize        = 1;
    tdesc.Format           = DXGI_FORMAT_B8G8R8A8_UNORM;
    tdesc.SampleDesc.Count = 1;
    tdesc.Usage            = D3D11_USAGE_IMMUTABLE;
    tdesc.BindFlags        = D3D11_BIND_SHADER_RESOURCE;

    D3D11_SUBRESOURCE_DATA sub = {};
    sub.pSysMem     = img.data();
    sub.SysMemPitch = w * 4;

    HRESULT hr = g_device->CreateTexture2D(&tdesc, &sub, tex);
    if (FAILED(hr)) return false;

    hr = g_device->CreateShaderResourceView(*tex, nullptr, srv);
    if (FAILED(hr)) {
        (*tex)->Release();
        *tex = nullptr;
        return false;
    }
    return true;
}

extern "C" __declspec(dllexport) int UpdateCursorShape(
    const uint8_t* data, int data_len,
    uint32_t type, uint32_t width, uint32_t height, uint32_t pitch)
{
    if (!g_device) return -1;

    uint32_t alpha_w = 0, alpha_h = 0, xor_w = 0, xor_h = 0;
    std::vector<uint8_t> alpha_img = build_cursor_alpha_image(data, (size_t)data_len, type, width, height, pitch, alpha_w, alpha_h);
    std::vector<uint8_t> xor_img   = build_cursor_xor_image(data, (size_t)data_len, type, width, height, pitch, xor_w, xor_h);

    if (g_cursorSRV)    { g_cursorSRV->Release();    g_cursorSRV    = nullptr; }
    if (g_cursorTex)    { g_cursorTex->Release();    g_cursorTex    = nullptr; }
    if (g_cursorXorSRV) { g_cursorXorSRV->Release(); g_cursorXorSRV = nullptr; }
    if (g_cursorXorTex) { g_cursorXorTex->Release(); g_cursorXorTex = nullptr; }
    g_cursorTexW = 0;
    g_cursorTexH = 0;

    if (alpha_img.empty() && xor_img.empty()) {
        // Unsupported/empty shape — cursor stays hidden until the next shape update.
        return 0;
    }

    if (!UploadCursorImage(alpha_img, alpha_w, alpha_h, &g_cursorTex, &g_cursorSRV)) return -2;
    if (!UploadCursorImage(xor_img, xor_w, xor_h, &g_cursorXorTex, &g_cursorXorSRV)) return -3;

    // Both images (when present) share the shape's dimensions.
    g_cursorTexW = alpha_img.empty() ? xor_w : alpha_w;
    g_cursorTexH = alpha_img.empty() ? xor_h : alpha_h;
    return 0;
}

// Called every frame with DXGI_OUTDUPL_FRAME_INFO.PointerPosition.
extern "C" __declspec(dllexport) void UpdateCursorPosition(int x, int y, int visible) {
    g_cursorX       = x;
    g_cursorY       = y;
    g_cursorVisible = visible != 0;
}

// ==================== HDR METADATA ====================

// Serialises g_masteringDisplay / g_contentLightLevel into raw big-endian HEVC SEI
// payload bytes. HEVC spec D.2.28 (MDCV, type 137) = 24 bytes G/B/R primaries +
// white point + luminance. HEVC spec D.2.35 (MaxCLL, type 144) = 4 bytes.
// This matches what FFmpeg packs internally before calling NVENC seiPayloadArray.
static void BuildHdrSeiPayloads() {
    auto be16 = [](uint8_t* b, uint16_t v) {
        b[0] = (uint8_t)(v >> 8); b[1] = (uint8_t)v;
    };
    auto be32 = [](uint8_t* b, uint32_t v) {
        b[0] = (uint8_t)(v >> 24); b[1] = (uint8_t)(v >> 16);
        b[2] = (uint8_t)(v >>  8); b[3] = (uint8_t)v;
    };
    // MDCV: G, B, R primaries (HEVC D.2.28 order), white point, max/min luminance
    be16(g_mdcvSeiBytes +  0, g_masteringDisplay.g.x);
    be16(g_mdcvSeiBytes +  2, g_masteringDisplay.g.y);
    be16(g_mdcvSeiBytes +  4, g_masteringDisplay.b.x);
    be16(g_mdcvSeiBytes +  6, g_masteringDisplay.b.y);
    be16(g_mdcvSeiBytes +  8, g_masteringDisplay.r.x);
    be16(g_mdcvSeiBytes + 10, g_masteringDisplay.r.y);
    be16(g_mdcvSeiBytes + 12, g_masteringDisplay.whitePoint.x);
    be16(g_mdcvSeiBytes + 14, g_masteringDisplay.whitePoint.y);
    be32(g_mdcvSeiBytes + 16, g_masteringDisplay.maxLuma);
    be32(g_mdcvSeiBytes + 20, g_masteringDisplay.minLuma);
    g_hdrSeiPayloads[0] = { 24, 137, g_mdcvSeiBytes };

    // MaxCLL: MaxCLL (u16), MaxFALL (u16)
    be16(g_cllSeiBytes + 0, g_contentLightLevel.maxContentLightLevel);
    be16(g_cllSeiBytes + 2, g_contentLightLevel.maxPicAverageLightLevel);
    g_hdrSeiPayloads[1] = { 4, 144, g_cllSeiBytes };
}

// Fills g_masteringDisplay / g_contentLightLevel with session-constant BT.2020 / 1000-nit
// values, then serialises them to SEI bytes via BuildHdrSeiPayloads().
static void BuildHdrMetadata() {
    // BT.2020 primaries in units of 1/50000 (HEVC D.2.28, G/B/R order):
    g_masteringDisplay.g          = { 8500, 39850 }; // G(0.170, 0.797)
    g_masteringDisplay.b          = { 6550,  2300 }; // B(0.131, 0.046)
    g_masteringDisplay.r          = { 35400, 14600 }; // R(0.708, 0.292)
    g_masteringDisplay.whitePoint = { 15635, 16450 }; // D65(0.3127, 0.3290)
    g_masteringDisplay.maxLuma    = 10000000;          // 1000 nit × 10000
    g_masteringDisplay.minLuma    = 500;               // 0.05 nit × 10000
    // Content Light Level (HEVC D.2.35 / AV1 6.7.3):
    g_contentLightLevel.maxContentLightLevel    = 1000; // MaxCLL  = 1000 nit
    g_contentLightLevel.maxPicAverageLightLevel = 400;  // MaxFALL = 400 nit
    g_hdrMetadataReady = true;
    BuildHdrSeiPayloads();
}

// ==================== INIT COLOR CONVERSION ====================
extern "C" __declspec(dllexport) int InitColorConversion(ID3D11Device* device, int width, int height, bool is_hdr, int fps) {
    g_isHdr = is_hdr;
    if (!device || !g_context) return -1;

    HRESULT hr = device->QueryInterface(__uuidof(ID3D11VideoDevice), (void**)&g_videoDevice);
    if (FAILED(hr)) return -2;

    hr = g_context->QueryInterface(__uuidof(ID3D11VideoContext), (void**)&g_videoContext);
    if (FAILED(hr)) return -3;

    // Zero-copy handoff: rather than converting BGRA -> NV12 into our own
    // texture and then CopyResource-ing that into NVENC's input buffer, point
    // the Video Processor's output view directly at the ID3D11Texture2D NVENC
    // already allocated and registered for encoding. InitEncoder() runs before
    // this function (see encoder.rs's Encoder::new()) and its CreateEncoder()
    // call synchronously calls AllocateInputBuffers(), so g_nvEncoder's input
    // buffer exists by now. With frameIntervalP=1/lookahead=0/no extra output
    // delay, NVENC uses a single input buffer, so the same texture returned
    // here is reused for every frame.
    if (!g_nvEncoder) return -8;
    const NvEncInputFrame* encoderInputFrame = g_nvEncoder->GetNextInputFrame();
    if (!encoderInputFrame || !encoderInputFrame->inputPtr) return -9;
    ID3D11Texture2D* nvencInputTex = (ID3D11Texture2D*)encoderInputFrame->inputPtr;

    // ── Verify NVENC staging texture format ───────────────────────────────────
    // AllocateInputBuffers creates this texture with GetD3D11Format(GetPixelFormat()).
    // For is_hdr=true: NV_ENC_BUFFER_FORMAT_YUV420_10BIT → DXGI_FORMAT_P010 (0x68).
    // For is_hdr=false: NV_ENC_BUFFER_FORMAT_NV12        → DXGI_FORMAT_NV12 (0x67).
    {
        D3D11_TEXTURE2D_DESC nvencDesc = {};
        nvencInputTex->GetDesc(&nvencDesc);
        DXGI_FORMAT expectedFmt = is_hdr ? DXGI_FORMAT_P010 : DXGI_FORMAT_NV12;
        if (nvencDesc.Format == expectedFmt) {
            printf("✅ NVENC input texture: %dx%d format=0x%X (%s) — correct\n",
                nvencDesc.Width, nvencDesc.Height, (unsigned)nvencDesc.Format,
                is_hdr ? "YUV420_10BIT/P010" : "NV12");
        } else {
            printf("❌ NVENC input texture format MISMATCH: got=0x%X expected=0x%X "
                   "(%s) — image will be corrupted!\n",
                   (unsigned)nvencDesc.Format, (unsigned)expectedFmt,
                   is_hdr ? "wanted P010=0x68" : "wanted NV12=103");
        }
    }

    UINT vp_fps = (fps > 0) ? (UINT)fps : 60;
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC contentDesc = {};
    contentDesc.InputFrameFormat            = D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE;
    contentDesc.InputFrameRate.Numerator    = vp_fps;
    contentDesc.InputFrameRate.Denominator  = 1;
    contentDesc.InputWidth                  = width;
    contentDesc.InputHeight                 = height;
    contentDesc.OutputWidth                 = width;
    contentDesc.OutputHeight                = height;
    contentDesc.OutputFrameRate.Numerator   = vp_fps;
    contentDesc.OutputFrameRate.Denominator = 1;
    contentDesc.Usage                       = D3D11_VIDEO_USAGE_PLAYBACK_NORMAL;

    hr = g_videoDevice->CreateVideoProcessorEnumerator(&contentDesc, &g_vpEnum);
    if (FAILED(hr)) return -5;

    hr = g_videoDevice->CreateVideoProcessor(g_vpEnum, 0, &g_vp);
    if (FAILED(hr)) return -6;

    // HDR: CS writes directly into g_hdrP010Tex; CopyResource feeds NVENC. VP is unused.
    // SDR: VP output view on NVENC's NV12 texture — VideoProcessorBlt writes here zero-copy.
    if (is_hdr) {
        g_nvencInputTex = nvencInputTex; // borrowed — do NOT AddRef or Release
        D3D11_TEXTURE2D_DESC d = {};
        nvencInputTex->GetDesc(&d);
        printf("ℹ️  HDR VP bypass: NVENC P010 input cached (%dx%d format=0x%X)\n",
               d.Width, d.Height, (unsigned)d.Format);
    } else {
        D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC ovDesc = {};
        ovDesc.ViewDimension      = D3D11_VPOV_DIMENSION_TEXTURE2D;
        ovDesc.Texture2D.MipSlice = 0;
        hr = g_videoDevice->CreateVideoProcessorOutputView(nvencInputTex, g_vpEnum, &ovDesc, &g_vpOutView);
        if (FAILED(hr)) {
            printf("❌ CreateVideoProcessorOutputView failed: hr=0x%08X\n", (unsigned)hr);
            return -7;
        }
        D3D11_TEXTURE2D_DESC d = {};
        nvencInputTex->GetDesc(&d);
        printf("✅ VP output view created on %dx%d format=0x%X texture\n",
               d.Width, d.Height, (unsigned)d.Format);
    }

    // Pin source/dest/output rects to the full dynamic surface. Without this,
    // VideoProcessorBlt on NVIDIA drivers can default to a stale/partial
    // rectangle, leaving everything below it black — exactly the "bottom of
    // frame black" symptom.
    RECT fullRect = {0, 0, (LONG)width, (LONG)height};
    g_videoContext->VideoProcessorSetStreamSourceRect(g_vp, 0, TRUE, &fullRect);
    g_videoContext->VideoProcessorSetStreamDestRect(g_vp, 0, TRUE, &fullRect);
    g_videoContext->VideoProcessorSetOutputTargetRect(g_vp, TRUE, &fullRect);

    if (is_hdr) {
        // HDR: CS writes BT.2020 NCL PQ YCbCr directly to P010 plane UAVs.
        // VP is not involved — no colorspace declarations needed here.
        if (!g_hdrCS) {
            ID3DBlob* csBlob  = nullptr;
            ID3DBlob* errBlob = nullptr;
            HRESULT hrCs = D3DCompile(kHdrCsHlsl, strlen(kHdrCsHlsl),
                                      nullptr, nullptr, nullptr,
                                      "main", "cs_5_0", 0, 0, &csBlob, &errBlob);
            if (FAILED(hrCs)) {
                if (errBlob) {
                    printf("❌ HDR CS compile error: %s\n", (char*)errBlob->GetBufferPointer());
                    errBlob->Release();
                }
                return -11;
            }
            hrCs = device->CreateComputeShader(csBlob->GetBufferPointer(),
                                               csBlob->GetBufferSize(), nullptr, &g_hdrCS);
            csBlob->Release();
            if (FAILED(hrCs)) {
                printf("❌ CreateComputeShader failed: hr=0x%08X\n", (unsigned)hrCs);
                return -12;
            }
            printf("✅ HDR CS compiled: FP16 scRGB → P010 BT.2020 NCL PQ (plane UAVs, D3D11.3)\n");
        }
        printf("✅ HDR encoder ready: CS → P010 (plane UAVs) → CopyResource → NVENC (VP bypassed)\n");
    } else {
        // SDR path: BGRA8 desktop full-range → NV12 BT.709 limited-range — UNCHANGED.
        // A mismatch here vs the VUI parameters set in InitEncoder produces structurally-
        // correct but wrong-colored output (blocky pixelation from the wrong YUV matrix/range).
        D3D11_VIDEO_PROCESSOR_COLOR_SPACE inputColorSpace = {};
        inputColorSpace.Usage         = 0;
        inputColorSpace.RGB_Range     = 0; // full range 0-255 (desktop BGRA)
        inputColorSpace.YCbCr_Matrix  = 1; // BT.709
        inputColorSpace.Nominal_Range = D3D11_VIDEO_PROCESSOR_NOMINAL_RANGE_0_255;
        g_videoContext->VideoProcessorSetStreamColorSpace(g_vp, 0, &inputColorSpace);

        D3D11_VIDEO_PROCESSOR_COLOR_SPACE outputColorSpace = {};
        outputColorSpace.Usage         = 0;
        outputColorSpace.RGB_Range     = 0;
        outputColorSpace.YCbCr_Matrix  = 1; // BT.709
        outputColorSpace.Nominal_Range = D3D11_VIDEO_PROCESSOR_NOMINAL_RANGE_16_235;
        g_videoContext->VideoProcessorSetOutputColorSpace(g_vp, &outputColorSpace);
        printf("✅ Video Processor (BGRA → NV12 BT.709) initialized\n");
    }

    if (!InitCursorPipeline(device)) {
        printf("⚠️  Cursor compositing pipeline failed to initialize — stream will have no cursor\n");
    }

    return 0;
}

// ==================== OPEN + INIT ====================
extern "C" __declspec(dllexport) int OpenNvEncSession(void* d3d11_device, void** out_encoder) {
    if (!d3d11_device || !out_encoder) return -1;
    // Disable C stdout buffering so shim printf appears immediately in the
    // redirected log file even when Stop-Process -Force is used to kill the
    // server (Force skips CRT flush, losing any pending buffered output).
    setvbuf(stdout, nullptr, _IONBF, 0);
    g_device = (ID3D11Device*)d3d11_device;
    g_device->AddRef();
    g_device->GetImmediateContext(&g_context);
    printf("✅ NVENC SESSION OPENED\n");
    *out_encoder = g_device;
    return 0;
}

extern "C" __declspec(dllexport) int InitEncoder(
    void* encoder, int width, int height, const char* codec,
    int bitrate_kbps, int fps, bool is_hdr)
{
    if (!g_device) return -1;
    g_hdrMetadataReady = false;

    GUID codecGuid = NV_ENC_CODEC_H264_GUID;
    g_encoderCodec = 0;
    if (strcmp(codec, "hevc") == 0) {
        codecGuid = NV_ENC_CODEC_HEVC_GUID;
        g_encoderCodec = 1;
    } else if (strcmp(codec, "av1") == 0) {
        codecGuid = NV_ENC_CODEC_AV1_GUID;
        g_encoderCodec = 2;
    }
    g_isHdr = is_hdr;

    printf("🔧 Initializing NVENC (%s%s @ %dx%d, %d Kbps, %d fps)...\n",
           codec, is_hdr ? "/HDR10" : "", width, height, bitrate_kbps, fps);

    try {
        // nExtraOutputDelay=0 eliminates the 3-frame pipeline buffer — zero-copy latency path.
        // HDR: YUV420_10BIT = DXGI_FORMAT_P010. Our CS outputs BT.2020 NCL PQ YCbCr 4:2:0
        // directly into P010 planes, bypassing NVENC's internal RGB→YCbCr converter (which
        // was hardwired to BT.709 coefficients regardless of the ABGR10 input format).
        // SDR: NV12 — VP still handles BGRA8→NV12 via VideoProcessorBlt.
        NV_ENC_BUFFER_FORMAT bufFmt = is_hdr ? NV_ENC_BUFFER_FORMAT_YUV420_10BIT
                                             : NV_ENC_BUFFER_FORMAT_NV12;
        printf("ℹ️  NVENC buffer format: %s (bufFmt=%u) — AllocateInputBuffers will create "
               "DXGI_FORMAT_%s texture\n",
               is_hdr ? "YUV420_10BIT" : "NV12", (unsigned)bufFmt,
               is_hdr ? "P010 (0x68)" : "NV12 (103)");
        g_nvEncoder = new NvEncoderD3D11(g_device, width, height, bufFmt, 0);

        g_initParams = NV_ENC_INITIALIZE_PARAMS{ NV_ENC_INITIALIZE_PARAMS_VER };
        g_encConfig  = NV_ENC_CONFIG{ NV_ENC_CONFIG_VER };
        g_encoderFps = fps;
        NV_ENC_INITIALIZE_PARAMS& initializeParams = g_initParams;
        NV_ENC_CONFIG&            encodeConfig     = g_encConfig;
        initializeParams.encodeConfig              = &encodeConfig;

        // Sunshine's encoder recipe (src/nvenc/nvenc_base.cpp), ported
        // verbatim. The combination that keeps the picture artifact-free
        // WITHOUT any forced IDRs:
        //   - CBR at the full client bitrate: on a near-static desktop every
        //     P-frame has a large surplus bit budget, which the encoder
        //     spends re-encoding/refining blocks — motion-compensation
        //     residue (cursor ghost trails) is scrubbed within a few frames
        //     instead of persisting until a keyframe.
        //   - Infinite GOP: no periodic IDRs to blow the VBV and drag QP up.
        //     IDRs happen only on demand (new client / decoder request).
        //   - Single-frame VBV: every frame fits one transmission window —
        //     consistent frame pacing and no multi-frame quality "payback".
        g_nvEncoder->CreateDefaultEncoderParams(
            &initializeParams, codecGuid,
            NV_ENC_PRESET_P1_GUID,
            NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY);

        // Framerate
        initializeParams.frameRateNum = fps;
        initializeParams.frameRateDen = 1;
        initializeParams.enablePTD    = 1; // driver picks P/I picture types

        encodeConfig.gopLength       = NVENC_INFINITE_GOPLENGTH;
        encodeConfig.frameIntervalP  = 1; // no B-frames (reorder latency)

        encodeConfig.rcParams.rateControlMode       = NV_ENC_PARAMS_RC_CBR;
        encodeConfig.rcParams.averageBitRate        = (uint32_t)bitrate_kbps * 1000;
        encodeConfig.rcParams.vbvBufferSize         = encodeConfig.rcParams.averageBitRate / (uint32_t)fps;
        encodeConfig.rcParams.zeroReorderDelay      = 1;
        encodeConfig.rcParams.enableLookahead       = 0;
        // Keep on-demand IDR frames the same size as P frames so they fit
        // the single-frame VBV instead of spiking it.
        encodeConfig.rcParams.lowDelayKeyFrameScale = 1;
        // Two-pass (quarter-res first pass): the preliminary pass catches
        // large motion vectors and enforces the strict single-frame VBV.
        encodeConfig.rcParams.multiPass             = NV_ENC_TWO_PASS_QUARTER_RESOLUTION;
        encodeConfig.rcParams.enableAQ              = 0; // Sunshine default: off

        // Codec-specific: inline SPS/PPS on every IDR so Moonlight can recover
        // VUI color description must match the BT.709 limited-range NV12
        // surface produced by the D3D11 Video Processor (InitColorConversion)
        // — otherwise the decoder applies the wrong YUV->RGB matrix/range and
        // the picture comes out blocky/wrong-colored despite decoding fine.
        NV_ENC_CONFIG_H264_VUI_PARAMETERS vuiParams = {};
        vuiParams.videoSignalTypePresentFlag   = 1;
        vuiParams.videoFormat                  = NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
        // CS outputs full-range [0,1] YCbCr; SDR VP outputs studio range.
        vuiParams.videoFullRangeFlag           = is_hdr ? 1 : 0;
        vuiParams.colourDescriptionPresentFlag = 1;
        vuiParams.bitstreamRestrictionFlag     = 1;
        if (is_hdr) {
            // BT.2020 / SMPTE ST 2084 (PQ) / BT.2020 non-constant luminance
            vuiParams.colourPrimaries         = NV_ENC_VUI_COLOR_PRIMARIES_BT2020;
            vuiParams.transferCharacteristics = NV_ENC_VUI_TRANSFER_CHARACTERISTIC_SMPTE2084;
            vuiParams.colourMatrix            = NV_ENC_VUI_MATRIX_COEFFS_BT2020_NCL;
        } else {
            vuiParams.colourPrimaries         = NV_ENC_VUI_COLOR_PRIMARIES_BT709;
            vuiParams.transferCharacteristics = NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
            vuiParams.colourMatrix            = NV_ENC_VUI_MATRIX_COEFFS_BT709;
        }

        if (codecGuid == NV_ENC_CODEC_H264_GUID) {
            encodeConfig.profileGUID = NV_ENC_H264_PROFILE_HIGH_GUID;
            auto& h264 = encodeConfig.encodeCodecConfig.h264Config;
            h264.repeatSPSPPS      = 1; // inline SPS/PPS on every IDR
            h264.idrPeriod         = NVENC_INFINITE_GOPLENGTH;
            h264.sliceMode         = 3;
            h264.sliceModeData     = 1; // single slice per frame
            h264.entropyCodingMode = NV_ENC_H264_ENTROPY_CODING_MODE_CABAC;
            // Deep DPB for future reference-frame invalidation; any single
            // frame still only references one frame back (numRefL0).
            h264.maxNumRefFrames   = 5;
            h264.numRefL0          = NV_ENC_NUM_REF_FRAMES_1;
            h264.enableFillerDataInsertion = 0;
            h264.h264VUIParameters = vuiParams;
            // Continuous intra refresh: with CBR + infinite GOP (no periodic
            // IDR — see above), this is what actually self-heals dropped or
            // corrupted reference data. Every intraRefreshPeriod frames, NVENC
            // starts cycling the whole frame through intra-coded macroblocks,
            // completing the cycle over the next intraRefreshCnt frames — an
            // IDR-less "rolling keyframe" that fits the single-frame VBV
            // instead of spiking it like a full IDR would.
            h264.enableIntraRefresh = 1;
            h264.intraRefreshPeriod = 30;
            h264.intraRefreshCnt    = 10;
        } else if (codecGuid == NV_ENC_CODEC_HEVC_GUID) {
            encodeConfig.profileGUID = is_hdr ? NV_ENC_HEVC_PROFILE_MAIN10_GUID
                                              : NV_ENC_HEVC_PROFILE_MAIN_GUID;
            auto& hevc = encodeConfig.encodeCodecConfig.hevcConfig;
            hevc.inputBitDepth        = is_hdr ? NV_ENC_BIT_DEPTH_10 : NV_ENC_BIT_DEPTH_8;
            hevc.outputBitDepth       = is_hdr ? NV_ENC_BIT_DEPTH_10 : NV_ENC_BIT_DEPTH_8;
            // Disable NVENC's native HDR SEI auto-generator (silently ignored by driver).
            // SEI is injected manually via seiPayloadArray in EncodeFrame instead.
            hevc.outputMasteringDisplay = 0;
            hevc.outputMaxCll           = 0;
            hevc.repeatSPSPPS         = 1;
            hevc.idrPeriod            = NVENC_INFINITE_GOPLENGTH;
            hevc.sliceMode            = 3;
            hevc.sliceModeData        = 1;
            hevc.maxNumRefFramesInDPB = 5;
            hevc.numRefL0             = NV_ENC_NUM_REF_FRAMES_1;
            hevc.enableFillerDataInsertion = 0;
            hevc.hevcVUIParameters    = vuiParams;
            if (is_hdr) {
                // Belt-and-suspenders: explicitly stamp HDR10 VUI as raw integer
                // values after the struct copy. Guards against any SDK version
                // where NV_ENC_CONFIG_HEVC_VUI_PARAMETERS diverges from the H264
                // typedef and the struct-by-value copy silently drops fields.
                hevc.hevcVUIParameters.videoSignalTypePresentFlag   = 1;
                hevc.hevcVUIParameters.videoFormat                  = NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
                hevc.hevcVUIParameters.videoFullRangeFlag           = 1; // full-range CS output
                hevc.hevcVUIParameters.colourDescriptionPresentFlag = 1;
                hevc.hevcVUIParameters.colourPrimaries              = NV_ENC_VUI_COLOR_PRIMARIES_BT2020;
                hevc.hevcVUIParameters.transferCharacteristics      = NV_ENC_VUI_TRANSFER_CHARACTERISTIC_SMPTE2084;
                hevc.hevcVUIParameters.colourMatrix                 = NV_ENC_VUI_MATRIX_COEFFS_BT2020_NCL;
            }
            hevc.enableIntraRefresh = 1;
            hevc.intraRefreshPeriod = 30;
            hevc.intraRefreshCnt    = 10;
        }

        printf("📊 NVENC RC config: CBR bitrate=%u vbvBufferSize=%u (1 frame) gop=infinite preset=P1/ULL\n",
               encodeConfig.rcParams.averageBitRate,
               encodeConfig.rcParams.vbvBufferSize);

        g_nvEncoder->CreateEncoder(&initializeParams);

        if (is_hdr) BuildHdrMetadata();

        printf("✅ NVENC READY (%s%s @ %dx%d, %d Kbps, %d fps)\n",
               codec, is_hdr ? "/HDR10/Main10" : "", width, height, bitrate_kbps, fps);
        return 0;
    }
    catch (const std::exception& e) {
        printf("❌ InitEncoder failed: %s\n", e.what());
        return -1;
    }
}

// ==================== ENCODE FRAME ====================
extern "C" __declspec(dllexport) int EncodeFrame(
    void* /*encoder*/, void* d3d11_texture,
    int /*width*/, int /*height*/,
    uint8_t* out_buffer, int max_size)
{
    if (!g_nvEncoder || !d3d11_texture) return -1;

    ID3D11Texture2D* dxgiFrame = (ID3D11Texture2D*)d3d11_texture;

    // Copy the DXGI duplication surface into the clean-background texture
    // and block until the GPU has actually finished that copy before
    // returning. The Rust capture loop calls
    // IDXGIOutputDuplication::ReleaseFrame() immediately after this function
    // returns, which lets DWM start writing the NEXT frame into this same
    // recycled surface. Without this fence, the CopyResource below is only
    // *queued*, not executed — so the GPU could still be reading dxgiFrame
    // while DWM is already overwriting it, tearing the captured image. A
    // static desktop hides this (old/new pixels match); moving content
    // (cursor, text, scrolling) doesn't — visible as the smearing/ghosting
    // that only self-heals at the next IDR.
    D3D11_TEXTURE2D_DESC frameDesc = {};
    dxgiFrame->GetDesc(&frameDesc);
    // HDR path uses R16G16B16A16_FLOAT (linear scRGB from WGC).
    // WGC only supports BGRA8 (SDR) or FP16 (HDR) pool formats.
    DXGI_FORMAT captureFmt = g_isHdr ? DXGI_FORMAT_R16G16B16A16_FLOAT
                                     : DXGI_FORMAT_B8G8R8A8_UNORM;
    if (!EnsureSizedTexture(&g_cleanBgTex, nullptr, (int)frameDesc.Width, (int)frameDesc.Height, D3D11_BIND_SHADER_RESOURCE, captureFmt)) {
        return 0;
    }
    if (!EnsureSizedTexture(&g_compositeTex, &g_compositeRTV, (int)frameDesc.Width, (int)frameDesc.Height, D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE, captureFmt)) {
        return 0;
    }
    if (!g_copyFence) {
        D3D11_QUERY_DESC qdesc = {};
        qdesc.Query = D3D11_QUERY_EVENT;
        if (FAILED(g_device->CreateQuery(&qdesc, &g_copyFence))) return 0;
    }

    g_context->CopyResource(g_cleanBgTex, dxgiFrame);
    g_context->End(g_copyFence);
    while (g_context->GetData(g_copyFence, nullptr, 0, 0) == S_FALSE) {
        // Spin: this copy is a few hundred microseconds at most, and we must
        // not return (letting Rust call ReleaseFrame) before it completes.
    }

    // Refresh the encode buffer from the clean background on EVERY call —
    // including a DXGI_ERROR_WAIT_TIMEOUT replay of the same dxgiFrame while
    // the desktop is static — so the cursor drawn below never persists into
    // the next iteration's source. Previously g_compositeTex was copied
    // directly from dxgiFrame and cursor-overlaid in place: a static desktop
    // kept re-copying the same source frame onto a buffer that still had the
    // last cursor draw on it, "stamping" a permanent trail of cursor images.
    g_context->CopyResource(g_compositeTex, g_cleanBgTex);

    ID3D11Texture2D* vpSourceTexture = g_compositeTex;

    // Composite the cursor onto g_compositeTex before NV12 conversion
    // (Sunshine's approach: blend into an intermediate render-targetable
    // surface, since the DXGI duplication texture may not support that bind).
    // No cursor-motion IDR forcing: with CBR + infinite GOP (see InitEncoder)
    // the rate controller scrubs any P-frame residue within a few frames on
    // its own — forcing IDRs only degrades quality (Sunshine never does it).
    if (g_cursorVisible && (g_cursorSRV || g_cursorXorSRV) && g_cursorTexW > 0 && g_cursorTexH > 0) {
        DrawCursorOverlay();
    }

    // HDR: CS writes BT.2020 NCL PQ YCbCr directly to P010 plane UAVs (D3D11.3
    // per-plane typed views on a single DXGI_FORMAT_P010 intermediate texture).
    // CopyResource (P010→P010, same format) then feeds NVENC — no VP involved.
    if (g_isHdr && g_hdrCS) {
        // Lazily create / resize g_hdrP010Tex and its per-plane UAVs.
        if (g_hdrP010Tex) {
            D3D11_TEXTURE2D_DESC existing = {};
            g_hdrP010Tex->GetDesc(&existing);
            if (existing.Width != frameDesc.Width || existing.Height != frameDesc.Height) {
                if (g_hdrYUAV)  { g_hdrYUAV->Release();  g_hdrYUAV  = nullptr; }
                if (g_hdrUVUAV) { g_hdrUVUAV->Release(); g_hdrUVUAV = nullptr; }
                g_hdrP010Tex->Release(); g_hdrP010Tex = nullptr;
            }
        }
        if (!g_hdrP010Tex) {
            D3D11_TEXTURE2D_DESC d = {};
            d.Width            = frameDesc.Width;
            d.Height           = frameDesc.Height;
            d.MipLevels        = 1;
            d.ArraySize        = 1;
            d.Format           = DXGI_FORMAT_P010;
            d.SampleDesc.Count = 1;
            d.Usage            = D3D11_USAGE_DEFAULT;
            d.BindFlags        = D3D11_BIND_UNORDERED_ACCESS;
            if (FAILED(g_device->CreateTexture2D(&d, nullptr, &g_hdrP010Tex))) {
                printf("❌ Failed to create P010 UAV texture (driver may not support "
                       "D3D11_BIND_UNORDERED_ACCESS on DXGI_FORMAT_P010)\n");
                return 0;
            }

            // Per-plane typed UAVs via ID3D11Device3 (D3D11.3 PlaneSlice feature).
            ID3D11Device3* dev3 = nullptr;
            if (FAILED(g_device->QueryInterface(__uuidof(ID3D11Device3), (void**)&dev3))) {
                printf("❌ ID3D11Device3 unavailable — cannot create P010 plane UAVs\n");
                g_hdrP010Tex->Release(); g_hdrP010Tex = nullptr;
                return 0;
            }
            D3D11_UNORDERED_ACCESS_VIEW_DESC1 uavDesc = {};
            uavDesc.ViewDimension        = D3D11_UAV_DIMENSION_TEXTURE2D;
            uavDesc.Texture2D.MipSlice   = 0;

            uavDesc.Format               = DXGI_FORMAT_R16_UNORM;
            uavDesc.Texture2D.PlaneSlice = 0; // Y luma plane
            ID3D11UnorderedAccessView1* yUav = nullptr;
            HRESULT hrY = dev3->CreateUnorderedAccessView1(g_hdrP010Tex, &uavDesc, &yUav);

            uavDesc.Format               = DXGI_FORMAT_R16G16_UNORM;
            uavDesc.Texture2D.PlaneSlice = 1; // UV chroma plane (half-res in hardware)
            ID3D11UnorderedAccessView1* uvUav = nullptr;
            HRESULT hrUV = dev3->CreateUnorderedAccessView1(g_hdrP010Tex, &uavDesc, &uvUav);
            dev3->Release();

            if (FAILED(hrY) || FAILED(hrUV)) {
                printf("❌ P010 plane UAV creation failed: hrY=0x%08X hrUV=0x%08X\n",
                       (unsigned)hrY, (unsigned)hrUV);
                if (yUav)  yUav->Release();
                if (uvUav) uvUav->Release();
                g_hdrP010Tex->Release(); g_hdrP010Tex = nullptr;
                return 0;
            }
            // ID3D11UnorderedAccessView1 inherits ID3D11UnorderedAccessView — safe to upcast.
            g_hdrYUAV  = yUav;
            g_hdrUVUAV = uvUav;
            printf("✅ HDR P010 intermediate created (%dx%d) with R16/R16G16 plane UAVs\n",
                   (int)frameDesc.Width, (int)frameDesc.Height);
        }

        // Per-frame SRV on the FP16 composite texture for the CS to read.
        // Per-frame is intentional: g_compositeTex is recreated by EnsureSizedTexture
        // on resolution changes, so a cached SRV would silently go stale.
        ID3D11ShaderResourceView* srcSRV = nullptr;
        if (FAILED(g_device->CreateShaderResourceView(g_compositeTex, nullptr, &srcSRV))) {
            printf("❌ Failed to create FP16 SRV for HDR CS input\n");
            return 0;
        }

        // Clear any RTV the cursor overlay may have left bound (SRV/RTV hazard).
        {
            ID3D11RenderTargetView* nullRTV = nullptr;
            g_context->OMSetRenderTargets(1, &nullRTV, nullptr);
        }

        // Dispatch: each 8×8 thread group covers a 16×16 pixel tile (2×2 per thread).
        // Ceiling division handles non-16-aligned heights (e.g. 1080p: 1080 % 16 != 0).
        ID3D11UnorderedAccessView* uavs[2] = { g_hdrYUAV, g_hdrUVUAV };
        g_context->CSSetShader(g_hdrCS, nullptr, 0);
        g_context->CSSetShaderResources(0, 1, &srcSRV);
        g_context->CSSetUnorderedAccessViews(0, 2, uavs, nullptr);
        g_context->Dispatch((frameDesc.Width + 15) / 16, (frameDesc.Height + 15) / 16, 1);

        // Unbind before CopyResource — UAV hazard with source texture is UB.
        ID3D11UnorderedAccessView* nullUAVs[2] = { nullptr, nullptr };
        g_context->CSSetUnorderedAccessViews(0, 2, nullUAVs, nullptr);
        ID3D11ShaderResourceView* nullSRV = nullptr;
        g_context->CSSetShaderResources(0, 1, &nullSRV);
        g_context->CSSetShader(nullptr, nullptr, 0);
        srcSRV->Release();

        // CopyResource: P010 → P010 (same format, same dimensions) — always valid in D3D11.
        if (!g_nvencInputTex) {
            printf("❌ g_nvencInputTex not cached — HDR frame dropped\n");
            return 0;
        }
        g_context->CopyResource(g_nvencInputTex, g_hdrP010Tex);
    } else {
        // SDR path: VP converts BGRA8 composite → NV12 into NVENC's input buffer.
        D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC ivDesc = {};
        ivDesc.ViewDimension = D3D11_VPIV_DIMENSION_TEXTURE2D;
        ID3D11VideoProcessorInputView* vpInputView = nullptr;
        HRESULT hr = g_videoDevice->CreateVideoProcessorInputView(
            vpSourceTexture, g_vpEnum, &ivDesc, &vpInputView);
        if (SUCCEEDED(hr)) {
            D3D11_VIDEO_PROCESSOR_STREAM stream = {};
            stream.Enable        = TRUE;
            stream.pInputSurface = vpInputView;
            HRESULT bltHr = g_videoContext->VideoProcessorBlt(g_vp, g_vpOutView, 0, 1, &stream);
            vpInputView->Release();
            if (FAILED(bltHr)) {
                printf("⚠️  VideoProcessorBlt failed: hr=0x%08X — frame dropped\n", (unsigned)bltHr);
                return 0;
            }
        } else {
            D3D11_TEXTURE2D_DESC srcDesc = {};
            vpSourceTexture->GetDesc(&srcDesc);
            printf("⚠️  CreateVideoProcessorInputView failed: hr=0x%08X "
                   "(src format=0x%X %dx%d) — frame dropped\n",
                   (unsigned)hr, (unsigned)srcDesc.Format,
                   (int)srcDesc.Width, (int)srcDesc.Height);
            return 0;
        }
    }

    std::vector<NvEncOutputFrame> vPacket;
    if (g_force_idr.exchange(false)) {
        NV_ENC_PIC_PARAMS picParams = {};
        picParams.encodePicFlags = NV_ENC_PIC_FLAG_FORCEIDR;
        if (g_isHdr && g_hdrMetadataReady && g_encoderCodec == 1) {
            // Manual byte-packed SEI injection matching FFmpeg/Apollo/Sunshine:
            // MDCV (type 137, 24 bytes big-endian) + MaxCLL (type 144, 4 bytes).
            // seiPayloadArray bypasses NVENC's broken native pMasteringDisplay path
            // and writes the SEI NAL unit bytes directly into the bitstream.
            picParams.codecPicParams.hevcPicParams.seiPayloadArrayCnt = 2;
            picParams.codecPicParams.hevcPicParams.seiPayloadArray    = g_hdrSeiPayloads;
        }
        g_nvEncoder->EncodeFrame(vPacket, &picParams);
    } else {
        g_nvEncoder->EncodeFrame(vPacket);
    }

    int total_size = 0;
    for (const auto& packet : vPacket) {
        int chunk = (int)packet.frame.size();
        if (total_size + chunk > max_size) break;
        memcpy(out_buffer + total_size, packet.frame.data(), chunk);
        total_size += chunk;
    }

    return total_size;
}

// ==================== RECONFIGURE BITRATE ====================
// Retargets CBR rate control to the bitrate the client actually negotiated
// in its RTSP ANNOUNCE (x-nv-vqos[0].bw.maximumBitrateKbps). The encoder is
// created at process startup with the CLI default — with CBR the encoder
// holds that rate constantly, so streaming above what the client asked for
// saturates the link and Moonlight aborts with bitrate warnings.
extern "C" __declspec(dllexport) int ReconfigureBitrate(int bitrate_kbps, int fps) {
    if (!g_nvEncoder || bitrate_kbps <= 0) return -1;
    if (fps <= 0) fps = g_encoderFps;

    uint32_t newRate = (uint32_t)bitrate_kbps * 1000;
    bool rateChanged = (newRate != g_encConfig.rcParams.averageBitRate);
    bool fpsChanged  = ((uint32_t)fps != g_initParams.frameRateNum);
    if (!rateChanged && !fpsChanged) return 0;

    g_encConfig.rcParams.averageBitRate = newRate;
    g_encConfig.rcParams.vbvBufferSize  = newRate / (uint32_t)fps; // single-frame VBV at new fps

    if (fpsChanged) {
        g_initParams.frameRateNum = (uint32_t)fps;
        g_initParams.frameRateDen = 1;
        g_encoderFps = fps;
    }

    NV_ENC_RECONFIGURE_PARAMS rp = { NV_ENC_RECONFIGURE_PARAMS_VER };
    rp.reInitEncodeParams              = g_initParams;
    rp.reInitEncodeParams.encodeConfig = &g_encConfig;
    rp.forceIDR = 1; // rate/fps target changed — start clean at the new budget

    try {
        g_nvEncoder->Reconfigure(&rp);
    } catch (const std::exception& e) {
        printf("❌ ReconfigureBitrate failed: %s\n", e.what());
        return -2;
    }

    printf("📊 NVENC reconfigured: CBR bitrate=%u fps=%u vbvBufferSize=%u (client-negotiated)\n",
           g_encConfig.rcParams.averageBitRate, (uint32_t)fps, g_encConfig.rcParams.vbvBufferSize);
    return 0;
}

// ==================== FORCE IDR ====================
// Sets a flag picked up by the next EncodeFrame() call, which passes
// NV_ENC_PIC_FLAG_FORCEIDR to NVENC. Used to guarantee the first frame sent
// to a newly-connected Moonlight client is a keyframe (with inline SPS/PPS).
extern "C" __declspec(dllexport) void RequestIdrFrame(void* /*encoder*/) {
    g_force_idr.store(true);
}

// ==================== CLEANUP ====================
extern "C" __declspec(dllexport) int CleanupEncoder(void* /*encoder*/) {
    if (g_cursorSRV)         { g_cursorSRV->Release();         g_cursorSRV         = nullptr; }
    if (g_cursorTex)         { g_cursorTex->Release();         g_cursorTex         = nullptr; }
    if (g_cursorXorSRV)      { g_cursorXorSRV->Release();      g_cursorXorSRV      = nullptr; }
    if (g_cursorXorTex)      { g_cursorXorTex->Release();      g_cursorXorTex      = nullptr; }
    if (g_cleanBgTex)        { g_cleanBgTex->Release();        g_cleanBgTex        = nullptr; }
    if (g_compositeRTV)      { g_compositeRTV->Release();      g_compositeRTV      = nullptr; }
    if (g_compositeTex)      { g_compositeTex->Release();      g_compositeTex      = nullptr; }
    if (g_copyFence)         { g_copyFence->Release();         g_copyFence         = nullptr; }
    if (g_cursorSampler)     { g_cursorSampler->Release();     g_cursorSampler     = nullptr; }
    if (g_cursorBlend)       { g_cursorBlend->Release();       g_cursorBlend       = nullptr; }
    if (g_cursorBlendInvert) { g_cursorBlendInvert->Release(); g_cursorBlendInvert = nullptr; }
    if (g_cursorPS)          { g_cursorPS->Release();          g_cursorPS          = nullptr; }
    if (g_cursorVS)          { g_cursorVS->Release();          g_cursorVS          = nullptr; }
    g_cursorTexW = 0;
    g_cursorTexH = 0;

    // HDR compute shader bridge resources.
    if (g_hdrYUAV)    { g_hdrYUAV->Release();    g_hdrYUAV    = nullptr; }
    if (g_hdrUVUAV)   { g_hdrUVUAV->Release();   g_hdrUVUAV   = nullptr; }
    if (g_hdrP010Tex) { g_hdrP010Tex->Release();  g_hdrP010Tex = nullptr; }
    if (g_hdrCS)      { g_hdrCS->Release();       g_hdrCS      = nullptr; }
    g_nvencInputTex = nullptr; // borrowed from NvEncoderD3D11 — do NOT Release

    // g_vpOutView is our own view object onto NVENC's input texture — releasing
    // it does not destroy that texture (NvEncoderD3D11::ReleaseD3D11Resources,
    // called from DestroyEncoder() below, owns and releases it separately).
    if (g_vpOutView)    { g_vpOutView->Release();    g_vpOutView    = nullptr; }
    if (g_vp)           { g_vp->Release();           g_vp           = nullptr; }
    if (g_vpEnum)       { g_vpEnum->Release();       g_vpEnum       = nullptr; }
    if (g_videoContext) { g_videoContext->Release(); g_videoContext = nullptr; }
    if (g_videoDevice)  { g_videoDevice->Release();  g_videoDevice  = nullptr; }

    if (g_nvEncoder) {
        std::vector<NvEncOutputFrame> vFlush;
        g_nvEncoder->EndEncode(vFlush);   // flush any trailing packets
        g_nvEncoder->DestroyEncoder();
        delete g_nvEncoder;
        g_nvEncoder = nullptr;
    }
    if (g_context) { g_context->Release(); g_context = nullptr; }
    if (g_device)  { g_device->Release();  g_device  = nullptr; }

    printf("✅ Cleanup complete.\n");
    return 0;
}
