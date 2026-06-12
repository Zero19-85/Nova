#include <windows.h>
#include <d3d11.h>
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
static ID3D11Texture2D*                g_nv12Texture   = nullptr;
static ID3D11VideoProcessorOutputView* g_vpOutView     = nullptr;

// ==================== ENCODER GLOBALS ====================
static ID3D11Device*        g_device    = nullptr;
static ID3D11DeviceContext* g_context   = nullptr;
static NvEncoderD3D11*      g_nvEncoder = nullptr;
static std::atomic<bool>    g_force_idr{false};

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

static ID3D11VertexShader*       g_cursorVS      = nullptr;
static ID3D11PixelShader*        g_cursorPS      = nullptr;
static ID3D11BlendState*         g_cursorBlend   = nullptr;
static ID3D11SamplerState*       g_cursorSampler = nullptr;

// Current cursor shape, uploaded as a small BGRA texture whenever DXGI
// reports a shape change (PointerShapeBufferSize > 0).
static ID3D11Texture2D*          g_cursorTex     = nullptr;
static ID3D11ShaderResourceView* g_cursorSRV     = nullptr;
static UINT                      g_cursorTexW    = 0;
static UINT                      g_cursorTexH    = 0;

// Intermediate render-targetable copy of the captured frame — the DXGI
// duplication texture isn't guaranteed to support D3D11_BIND_RENDER_TARGET,
// so the cursor is drawn onto this copy before NV12 conversion.
static ID3D11Texture2D*          g_compositeTex  = nullptr;
static ID3D11RenderTargetView*   g_compositeRTV  = nullptr;

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
// MONOCHROME / COLOR / MASKED_COLOR pointer shape types. The XOR-blended
// "inverse of screen" pass (make_cursor_xor_image) is intentionally omitted
// for this first working version — affected pixels are simply left
// transparent, which only affects rare invert-style cursors.
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
                    // XOR-blended pixel (inverse of screen) — not implemented, leave transparent.
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

// Lazily creates a render-target+shader-resource copy of the captured frame,
// sized to match. Created once and reused for the lifetime of the session
// (capture resolution doesn't change mid-stream).
static bool EnsureCompositeTexture(int width, int height) {
    if (g_compositeTex) return true;

    D3D11_TEXTURE2D_DESC desc = {};
    desc.Width            = width;
    desc.Height           = height;
    desc.MipLevels        = 1;
    desc.ArraySize        = 1;
    desc.Format           = DXGI_FORMAT_B8G8R8A8_UNORM;
    desc.SampleDesc.Count = 1;
    desc.Usage            = D3D11_USAGE_DEFAULT;
    desc.BindFlags        = D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE;

    HRESULT hr = g_device->CreateTexture2D(&desc, nullptr, &g_compositeTex);
    if (FAILED(hr)) return false;

    hr = g_device->CreateRenderTargetView(g_compositeTex, nullptr, &g_compositeRTV);
    if (FAILED(hr)) {
        g_compositeTex->Release();
        g_compositeTex = nullptr;
        return false;
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
    g_context->PSSetShaderResources(0, 1, &g_cursorSRV);
    g_context->PSSetSamplers(0, 1, &g_cursorSampler);

    float blendFactor[4] = {0, 0, 0, 0};
    g_context->OMSetBlendState(g_cursorBlend, blendFactor, 0xFFFFFFFF);
    g_context->OMSetRenderTargets(1, &g_compositeRTV, nullptr);

    D3D11_VIEWPORT vp = {};
    vp.TopLeftX = (float)g_cursorX;
    vp.TopLeftY = (float)g_cursorY;
    vp.Width    = (float)g_cursorTexW;
    vp.Height   = (float)g_cursorTexH;
    vp.MinDepth = 0.0f;
    vp.MaxDepth = 1.0f;
    g_context->RSSetViewports(1, &vp);

    g_context->Draw(3, 0);

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
extern "C" __declspec(dllexport) int UpdateCursorShape(
    const uint8_t* data, int data_len,
    uint32_t type, uint32_t width, uint32_t height, uint32_t pitch)
{
    if (!g_device) return -1;

    uint32_t out_w = 0, out_h = 0;
    std::vector<uint8_t> img = build_cursor_alpha_image(data, (size_t)data_len, type, width, height, pitch, out_w, out_h);

    if (g_cursorSRV) { g_cursorSRV->Release(); g_cursorSRV = nullptr; }
    if (g_cursorTex) { g_cursorTex->Release(); g_cursorTex = nullptr; }
    g_cursorTexW = 0;
    g_cursorTexH = 0;

    if (img.empty() || out_w == 0 || out_h == 0) {
        // Unsupported/empty shape — cursor stays hidden until the next shape update.
        return 0;
    }

    D3D11_TEXTURE2D_DESC tdesc = {};
    tdesc.Width            = out_w;
    tdesc.Height           = out_h;
    tdesc.MipLevels        = 1;
    tdesc.ArraySize        = 1;
    tdesc.Format           = DXGI_FORMAT_B8G8R8A8_UNORM;
    tdesc.SampleDesc.Count = 1;
    tdesc.Usage            = D3D11_USAGE_IMMUTABLE;
    tdesc.BindFlags        = D3D11_BIND_SHADER_RESOURCE;

    D3D11_SUBRESOURCE_DATA sub = {};
    sub.pSysMem     = img.data();
    sub.SysMemPitch = out_w * 4;

    HRESULT hr = g_device->CreateTexture2D(&tdesc, &sub, &g_cursorTex);
    if (FAILED(hr)) return -2;

    hr = g_device->CreateShaderResourceView(g_cursorTex, nullptr, &g_cursorSRV);
    if (FAILED(hr)) {
        g_cursorTex->Release();
        g_cursorTex = nullptr;
        return -3;
    }

    g_cursorTexW = out_w;
    g_cursorTexH = out_h;
    return 0;
}

// Called every frame with DXGI_OUTDUPL_FRAME_INFO.PointerPosition.
extern "C" __declspec(dllexport) void UpdateCursorPosition(int x, int y, int visible) {
    g_cursorX       = x;
    g_cursorY       = y;
    g_cursorVisible = visible != 0;
}

// ==================== INIT COLOR CONVERSION ====================
extern "C" __declspec(dllexport) int InitColorConversion(ID3D11Device* device, int width, int height) {
    if (!device || !g_context) return -1;

    HRESULT hr = device->QueryInterface(__uuidof(ID3D11VideoDevice), (void**)&g_videoDevice);
    if (FAILED(hr)) return -2;

    hr = g_context->QueryInterface(__uuidof(ID3D11VideoContext), (void**)&g_videoContext);
    if (FAILED(hr)) return -3;

    D3D11_TEXTURE2D_DESC desc = {};
    desc.Width            = width;
    desc.Height           = height;
    desc.MipLevels        = 1;
    desc.ArraySize        = 1;
    desc.Format           = DXGI_FORMAT_NV12;
    desc.SampleDesc.Count = 1;
    desc.Usage            = D3D11_USAGE_DEFAULT;
    desc.BindFlags        = D3D11_BIND_RENDER_TARGET;

    hr = device->CreateTexture2D(&desc, nullptr, &g_nv12Texture);
    if (FAILED(hr)) return -4;

    D3D11_VIDEO_PROCESSOR_CONTENT_DESC contentDesc = {};
    contentDesc.InputFrameFormat            = D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE;
    contentDesc.InputFrameRate.Numerator    = 60;
    contentDesc.InputFrameRate.Denominator  = 1;
    contentDesc.InputWidth                  = width;
    contentDesc.InputHeight                 = height;
    contentDesc.OutputWidth                 = width;
    contentDesc.OutputHeight                = height;
    contentDesc.OutputFrameRate.Numerator   = 60;
    contentDesc.OutputFrameRate.Denominator = 1;
    contentDesc.Usage                       = D3D11_VIDEO_USAGE_PLAYBACK_NORMAL;

    hr = g_videoDevice->CreateVideoProcessorEnumerator(&contentDesc, &g_vpEnum);
    if (FAILED(hr)) return -5;

    hr = g_videoDevice->CreateVideoProcessor(g_vpEnum, 0, &g_vp);
    if (FAILED(hr)) return -6;

    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC ovDesc = {};
    ovDesc.ViewDimension      = D3D11_VPOV_DIMENSION_TEXTURE2D;
    ovDesc.Texture2D.MipSlice = 0;

    hr = g_videoDevice->CreateVideoProcessorOutputView(g_nv12Texture, g_vpEnum, &ovDesc, &g_vpOutView);
    if (FAILED(hr)) return -7;

    // Pin source/dest/output rects to the full dynamic surface. Without this,
    // VideoProcessorBlt on NVIDIA drivers can default to a stale/partial
    // rectangle, leaving everything below it black — exactly the "bottom of
    // frame black" symptom.
    RECT fullRect = {0, 0, (LONG)width, (LONG)height};
    g_videoContext->VideoProcessorSetStreamSourceRect(g_vp, 0, TRUE, &fullRect);
    g_videoContext->VideoProcessorSetStreamDestRect(g_vp, 0, TRUE, &fullRect);
    g_videoContext->VideoProcessorSetOutputTargetRect(g_vp, TRUE, &fullRect);

    // Pin the RGB->NV12 color-space conversion to BT.709 limited range,
    // matching the H264 VUI parameters set in InitEncoder. A mismatch here
    // between what the VP writes into the NV12 surface and what the decoder
    // assumes from the SPS produces a structurally-correct but blocky,
    // wrong-color picture (each 2x2 luma block shares one off chroma sample).
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

    printf("✅ Video Processor (BGRA → NV12) initialized\n");

    if (!InitCursorPipeline(device)) {
        printf("⚠️  Cursor compositing pipeline failed to initialize — stream will have no cursor\n");
    }

    return 0;
}

// ==================== OPEN + INIT ====================
extern "C" __declspec(dllexport) int OpenNvEncSession(void* d3d11_device, void** out_encoder) {
    if (!d3d11_device || !out_encoder) return -1;
    g_device = (ID3D11Device*)d3d11_device;
    g_device->AddRef();
    g_device->GetImmediateContext(&g_context);
    printf("✅ NVENC SESSION OPENED\n");
    *out_encoder = g_device;
    return 0;
}

extern "C" __declspec(dllexport) int InitEncoder(
    void* encoder, int width, int height, const char* codec,
    int bitrate_kbps, int fps)
{
    if (!g_device) return -1;

    GUID codecGuid = NV_ENC_CODEC_H264_GUID;
    if (strcmp(codec, "hevc") == 0) {
        codecGuid = NV_ENC_CODEC_HEVC_GUID;
    } else if (strcmp(codec, "av1") == 0) {
        codecGuid = NV_ENC_CODEC_AV1_GUID;
    }

    printf("🔧 Initializing NVENC (%s @ %dx%d, %d Kbps, %d fps)...\n",
           codec, width, height, bitrate_kbps, fps);

    try {
        // nExtraOutputDelay=0 eliminates the 3-frame pipeline buffer — zero-copy latency path.
        g_nvEncoder = new NvEncoderD3D11(g_device, width, height, NV_ENC_BUFFER_FORMAT_NV12, 0);

        NV_ENC_INITIALIZE_PARAMS initializeParams = { NV_ENC_INITIALIZE_PARAMS_VER };
        NV_ENC_CONFIG encodeConfig               = { NV_ENC_CONFIG_VER };
        initializeParams.encodeConfig            = &encodeConfig;

        // P1+ULTRA_LOW_LATENCY is NVENC's fastest *and lowest-quality* preset —
        // it was crushing every frame into heavy macroblocking. P4+LOW_LATENCY
        // gives a large quality jump for a small (sub-frame) latency cost.
        g_nvEncoder->CreateDefaultEncoderParams(
            &initializeParams, codecGuid,
            NV_ENC_PRESET_P4_GUID,
            NV_ENC_TUNING_INFO_LOW_LATENCY);

        // Framerate
        initializeParams.frameRateNum = fps;
        initializeParams.frameRateDen = 1;
        initializeParams.enablePTD    = 1; // driver picks P/I picture types

        // GOP: IDR every 1 s, no B-frames (B-frames add reorder latency).
        // Was 2 s — halved so any transient reference-frame corruption
        // (packet loss, FEC shortfall) self-heals roughly twice as fast.
        encodeConfig.gopLength       = fps * 1;
        encodeConfig.frameIntervalP  = 1;

        // VBR with a target average well below the cap, but the VBV/HRD
        // buffer must still be sized for the *biggest* frame (IDR), not the
        // average — IDRs measured up to ~27KB while a VBV sized off the
        // 5Mbps average (~20.8KB) was smaller than that. A bitstream that
        // overshoots its declared HRD buffer is non-conformant and corrupts
        // decode right at/after the IDR (matches "bottom of frame black").
        // So: average drives P-frame sizing, vbv (sized off maxBitRate, as
        // before) gives IDR frames room to fit within the declared buffer.
        // avgBitRate at maxBitRate/3 was starving P-frames of detail (heavy
        // macroblocking). The 2-frame VBV window below already gives IDRs
        // room to spike, so let average ride much closer to the cap.
        uint32_t maxBitRateVal     = (uint32_t)bitrate_kbps * 1000;
        uint32_t avgBitRateVal     = (maxBitRateVal * 3) / 4;
        encodeConfig.rcParams.rateControlMode = NV_ENC_PARAMS_RC_VBR;
        encodeConfig.rcParams.averageBitRate  = avgBitRateVal;
        encodeConfig.rcParams.maxBitRate      = maxBitRateVal;
        // 1x maxBitRate/fps (~31.25KB @ 15Mbps/60fps) is smaller than the ~36KB
        // IDRs we're seeing — give the VBV a 2-frame window so IDRs fit.
        encodeConfig.rcParams.vbvBufferSize   = (maxBitRateVal / (uint32_t)fps) * 2;
        encodeConfig.rcParams.vbvInitialDelay = encodeConfig.rcParams.vbvBufferSize;
        encodeConfig.rcParams.zeroReorderDelay = 1;
        // Two-pass (quarter-res first pass) — Sunshine's default. The
        // preliminary pass catches large motion vectors and distributes bits
        // far better on full-screen motion (window dragging was producing
        // severe blocking with single-pass).
        encodeConfig.rcParams.multiPass = NV_ENC_TWO_PASS_QUARTER_RESOLUTION;
        // Spatial AQ: shifts bits toward flat/low-detail regions where
        // quantization noise is most visible — targets the static-desktop
        // shimmer on text/UI edges.
        encodeConfig.rcParams.enableAQ = 1;

        // Codec-specific: inline SPS/PPS on every IDR so Moonlight can recover
        // VUI color description must match the BT.709 limited-range NV12
        // surface produced by the D3D11 Video Processor (InitColorConversion)
        // — otherwise the decoder applies the wrong YUV->RGB matrix/range and
        // the picture comes out blocky/wrong-colored despite decoding fine.
        NV_ENC_CONFIG_H264_VUI_PARAMETERS vuiParams = {};
        vuiParams.videoSignalTypePresentFlag   = 1;
        vuiParams.videoFormat                  = NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
        vuiParams.videoFullRangeFlag           = 0; // limited (16-235), matches VP output
        vuiParams.colourDescriptionPresentFlag = 1;
        vuiParams.colourPrimaries              = NV_ENC_VUI_COLOR_PRIMARIES_BT709;
        vuiParams.transferCharacteristics      = NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
        vuiParams.colourMatrix                 = NV_ENC_VUI_MATRIX_COEFFS_BT709;

        if (codecGuid == NV_ENC_CODEC_H264_GUID) {
            encodeConfig.encodeCodecConfig.h264Config.idrPeriod    = fps * 1;
            encodeConfig.encodeCodecConfig.h264Config.repeatSPSPPS = 1;
            encodeConfig.encodeCodecConfig.h264Config.disableSPSPPS = 0;
            encodeConfig.encodeCodecConfig.h264Config.enableFillerDataInsertion = 0;
            encodeConfig.encodeCodecConfig.h264Config.h264VUIParameters = vuiParams;
        } else if (codecGuid == NV_ENC_CODEC_HEVC_GUID) {
            encodeConfig.encodeCodecConfig.hevcConfig.idrPeriod    = fps * 1;
            encodeConfig.encodeCodecConfig.hevcConfig.repeatSPSPPS = 1;
            encodeConfig.encodeCodecConfig.hevcConfig.disableSPSPPS = 0;
            encodeConfig.encodeCodecConfig.hevcConfig.enableFillerDataInsertion = 0;
            encodeConfig.encodeCodecConfig.hevcConfig.hevcVUIParameters = vuiParams;
        }

        printf("📊 NVENC RC config: mode=%s avgBitRate=%u maxBitRate=%u vbvBufferSize=%u fillerData(h264)=%u\n",
               encodeConfig.rcParams.rateControlMode == NV_ENC_PARAMS_RC_VBR ? "VBR" : "CBR",
               encodeConfig.rcParams.averageBitRate,
               encodeConfig.rcParams.maxBitRate,
               encodeConfig.rcParams.vbvBufferSize,
               encodeConfig.encodeCodecConfig.h264Config.enableFillerDataInsertion);

        g_nvEncoder->CreateEncoder(&initializeParams);

        printf("✅ NVENC READY (%s @ %dx%d, %d Kbps, %d fps)\n",
               codec, width, height, bitrate_kbps, fps);
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
    ID3D11Texture2D* vpSourceTexture = dxgiFrame;

    // Composite the cursor onto a copy of the captured frame before NV12
    // conversion (Sunshine's approach: blend into an intermediate surface,
    // since the DXGI duplication texture may not be render-targetable).
    if (g_cursorVisible && g_cursorSRV && g_cursorTexW > 0 && g_cursorTexH > 0) {
        D3D11_TEXTURE2D_DESC frameDesc = {};
        dxgiFrame->GetDesc(&frameDesc);
        if (EnsureCompositeTexture((int)frameDesc.Width, (int)frameDesc.Height)) {
            g_context->CopyResource(g_compositeTex, dxgiFrame);
            DrawCursorOverlay();
            vpSourceTexture = g_compositeTex;
        }
    }

    // BGRA → NV12 via D3D11 Video Processor
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC ivDesc = {};
    ivDesc.ViewDimension = D3D11_VPIV_DIMENSION_TEXTURE2D;

    ID3D11VideoProcessorInputView* vpInputView = nullptr;
    HRESULT hr = g_videoDevice->CreateVideoProcessorInputView(
        vpSourceTexture, g_vpEnum, &ivDesc, &vpInputView);

    if (SUCCEEDED(hr)) {
        D3D11_VIDEO_PROCESSOR_STREAM stream = {};
        stream.Enable        = TRUE;
        stream.pInputSurface = vpInputView;
        g_videoContext->VideoProcessorBlt(g_vp, g_vpOutView, 0, 1, &stream);
        vpInputView->Release();
    } else {
        // VP input view creation failed — this frame is dropped
        return 0;
    }

    // Copy NV12 surface into NVENC's own input buffer
    const NvEncInputFrame* encoderInputFrame = g_nvEncoder->GetNextInputFrame();
    g_context->CopyResource(
        (ID3D11Texture2D*)encoderInputFrame->inputPtr, g_nv12Texture);

    std::vector<NvEncOutputFrame> vPacket;
    if (g_force_idr.exchange(false)) {
        NV_ENC_PIC_PARAMS picParams = {};
        picParams.encodePicFlags = NV_ENC_PIC_FLAG_FORCEIDR;
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

// ==================== FORCE IDR ====================
// Sets a flag picked up by the next EncodeFrame() call, which passes
// NV_ENC_PIC_FLAG_FORCEIDR to NVENC. Used to guarantee the first frame sent
// to a newly-connected Moonlight client is a keyframe (with inline SPS/PPS).
extern "C" __declspec(dllexport) void RequestIdrFrame(void* /*encoder*/) {
    g_force_idr.store(true);
}

// ==================== CLEANUP ====================
extern "C" __declspec(dllexport) int CleanupEncoder(void* /*encoder*/) {
    if (g_cursorSRV)     { g_cursorSRV->Release();     g_cursorSRV     = nullptr; }
    if (g_cursorTex)     { g_cursorTex->Release();     g_cursorTex     = nullptr; }
    if (g_compositeRTV)  { g_compositeRTV->Release();  g_compositeRTV  = nullptr; }
    if (g_compositeTex)  { g_compositeTex->Release();  g_compositeTex  = nullptr; }
    if (g_cursorSampler) { g_cursorSampler->Release();  g_cursorSampler = nullptr; }
    if (g_cursorBlend)   { g_cursorBlend->Release();   g_cursorBlend   = nullptr; }
    if (g_cursorPS)      { g_cursorPS->Release();      g_cursorPS      = nullptr; }
    if (g_cursorVS)      { g_cursorVS->Release();      g_cursorVS      = nullptr; }
    g_cursorTexW = 0;
    g_cursorTexH = 0;

    if (g_vpOutView)    { g_vpOutView->Release();    g_vpOutView    = nullptr; }
    if (g_nv12Texture)  { g_nv12Texture->Release();  g_nv12Texture  = nullptr; }
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
