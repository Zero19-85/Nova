#include <windows.h>
#include <d3d11.h>
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

        // GOP: IDR every 2 s, no B-frames (B-frames add reorder latency)
        encodeConfig.gopLength       = fps * 2;
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
            encodeConfig.encodeCodecConfig.h264Config.idrPeriod    = fps * 2;
            encodeConfig.encodeCodecConfig.h264Config.repeatSPSPPS = 1;
            encodeConfig.encodeCodecConfig.h264Config.disableSPSPPS = 0;
            encodeConfig.encodeCodecConfig.h264Config.enableFillerDataInsertion = 0;
            encodeConfig.encodeCodecConfig.h264Config.h264VUIParameters = vuiParams;
        } else if (codecGuid == NV_ENC_CODEC_HEVC_GUID) {
            encodeConfig.encodeCodecConfig.hevcConfig.idrPeriod    = fps * 2;
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

    // BGRA → NV12 via D3D11 Video Processor
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC ivDesc = {};
    ivDesc.ViewDimension = D3D11_VPIV_DIMENSION_TEXTURE2D;

    ID3D11VideoProcessorInputView* vpInputView = nullptr;
    HRESULT hr = g_videoDevice->CreateVideoProcessorInputView(
        dxgiFrame, g_vpEnum, &ivDesc, &vpInputView);

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
