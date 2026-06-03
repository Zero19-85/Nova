#include <windows.h>
#include <d3d11.h>
#include <stdio.h>
#include <fstream>
#include <vector>

#include "nvEncodeAPI.h"
#include "NvEncoderD3D11.h"

// ==================== VIDEO PROCESSOR GLOBALS ====================
static ID3D11VideoDevice*              g_videoDevice   = nullptr;
static ID3D11VideoContext*             g_videoContext  = nullptr;
static ID3D11VideoProcessorEnumerator* g_vpEnum        = nullptr;
static ID3D11VideoProcessor*           g_vp            = nullptr;
static ID3D11Texture2D*                g_nv12Texture   = nullptr;
static ID3D11VideoProcessorOutputView* g_vpOutView     = nullptr;

// ==================== EXISTING GLOBALS ====================
static ID3D11Device*        g_device    = nullptr;
static ID3D11DeviceContext* g_context   = nullptr;
static NvEncoderD3D11*      g_nvEncoder = nullptr;
static std::ofstream        g_h264File;

// ==================== INIT COLOR CONVERSION ====================
extern "C" __declspec(dllexport) int InitColorConversion(ID3D11Device* device, int width, int height) {
    if (!device || !g_context) return -1;

    HRESULT hr = device->QueryInterface(__uuidof(ID3D11VideoDevice), (void**)&g_videoDevice);
    if (FAILED(hr)) return -2;

    hr = g_context->QueryInterface(__uuidof(ID3D11VideoContext), (void**)&g_videoContext);
    if (FAILED(hr)) return -3;

    // Create NV12 target texture
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

    // Video Processor setup
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC contentDesc = {};
    contentDesc.InputFrameFormat           = D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE;
    contentDesc.InputFrameRate.Numerator   = 60;
    contentDesc.InputFrameRate.Denominator = 1;
    contentDesc.InputWidth                 = width;
    contentDesc.InputHeight                = height;
    contentDesc.OutputWidth                = width;
    contentDesc.OutputHeight               = height;
    contentDesc.OutputFrameRate.Numerator  = 60;
    contentDesc.OutputFrameRate.Denominator = 1;
    contentDesc.Usage                      = D3D11_VIDEO_USAGE_PLAYBACK_NORMAL;

    hr = g_videoDevice->CreateVideoProcessorEnumerator(&contentDesc, &g_vpEnum);
    if (FAILED(hr)) return -5;

    hr = g_videoDevice->CreateVideoProcessor(g_vpEnum, 0, &g_vp);
    if (FAILED(hr)) return -6;

    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC ovDesc = {};
    ovDesc.ViewDimension      = D3D11_VPOV_DIMENSION_TEXTURE2D;
    ovDesc.Texture2D.MipSlice = 0;

    hr = g_videoDevice->CreateVideoProcessorOutputView(g_nv12Texture, g_vpEnum, &ovDesc, &g_vpOutView);
    if (FAILED(hr)) return -7;

    printf("✅ Video Processor (BGRA → NV12) initialized\n");
    return 0;
}

// ==================== ENCODE FRAME ====================
extern "C" __declspec(dllexport) int EncodeFrame(void* encoder, void* d3d11_texture, int width, int height, uint8_t* out_buffer, int max_size) {
    if (!g_nvEncoder || !d3d11_texture) return -1;

    ID3D11Texture2D* dxgiFrame = (ID3D11Texture2D*)d3d11_texture;

    // Video Processor BGRA → NV12
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC ivDesc = {};
    ivDesc.ViewDimension = D3D11_VPIV_DIMENSION_TEXTURE2D;

    ID3D11VideoProcessorInputView* vpInputView = nullptr;
    HRESULT hr = g_videoDevice->CreateVideoProcessorInputView(dxgiFrame, g_vpEnum, &ivDesc, &vpInputView);

    if (SUCCEEDED(hr)) {
        D3D11_VIDEO_PROCESSOR_STREAM stream = {};
        stream.Enable = TRUE;
        stream.pInputSurface = vpInputView;

        g_videoContext->VideoProcessorBlt(g_vp, g_vpOutView, 0, 1, &stream);
        vpInputView->Release();
    }

    // Feed NV12 into NVENC
    const NvEncInputFrame* encoderInputFrame = g_nvEncoder->GetNextInputFrame();
    g_context->CopyResource((ID3D11Texture2D*)encoderInputFrame->inputPtr, g_nv12Texture);

    std::vector<NvEncOutputFrame> vPacket;
    g_nvEncoder->EncodeFrame(vPacket);

    int total_size = 0;

    if (!vPacket.empty()) {
        for (const auto& packet : vPacket) {
            if (total_size + packet.frame.size() > max_size) break;
            memcpy(out_buffer + total_size, packet.frame.data(), packet.frame.size());
            total_size += packet.frame.size();
        }
    }

    return total_size;   // Return how many bytes were written
}

// ==================== CLEANUP ====================
extern "C" __declspec(dllexport) int CleanupEncoder(void* encoder) {
    if (g_vpOutView)     { g_vpOutView->Release();     g_vpOutView = nullptr; }
    if (g_nv12Texture)   { g_nv12Texture->Release();   g_nv12Texture = nullptr; }
    if (g_vp)            { g_vp->Release();            g_vp = nullptr; }
    if (g_vpEnum)        { g_vpEnum->Release();        g_vpEnum = nullptr; }
    if (g_videoContext)  { g_videoContext->Release();  g_videoContext = nullptr; }
    if (g_videoDevice)   { g_videoDevice->Release();   g_videoDevice = nullptr; }

    if (g_nvEncoder) { g_nvEncoder->DestroyEncoder(); delete g_nvEncoder; g_nvEncoder = nullptr; }
    if (g_context)   { g_context->Release();   g_context = nullptr; }
    if (g_device)    { g_device->Release();    g_device = nullptr; }
    if (g_h264File.is_open()) g_h264File.close();

    printf("✅ Cleanup complete.\n");
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

extern "C" __declspec(dllexport) int InitEncoder(void* encoder, int width, int height, const char* codec) {
    if (!g_device) return -1;

    GUID codecGuid = NV_ENC_CODEC_H264_GUID;
    if (strcmp(codec, "hevc") == 0) {
        codecGuid = NV_ENC_CODEC_HEVC_GUID;
    } else if (strcmp(codec, "av1") == 0) {
        codecGuid = NV_ENC_CODEC_AV1_GUID;
    }

    printf("🔧 Initializing NVENC (%s @ %dx%d)...\n", codec, width, height);

    try {
        g_nvEncoder = new NvEncoderD3D11(g_device, width, height, NV_ENC_BUFFER_FORMAT_NV12);

        NV_ENC_INITIALIZE_PARAMS initializeParams = { NV_ENC_INITIALIZE_PARAMS_VER };
        NV_ENC_CONFIG encodeConfig = { NV_ENC_CONFIG_VER };
        initializeParams.encodeConfig = &encodeConfig;

        g_nvEncoder->CreateDefaultEncoderParams(&initializeParams, codecGuid, NV_ENC_PRESET_P1_GUID, NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY);

        // ... rest of rate control settings stay the same ...

        g_nvEncoder->CreateEncoder(&initializeParams);
        // ... file open etc ...

        return 0;
    }
    catch (const std::exception& e) {
        printf("❌ Init failed: %s\n", e.what());
        return -1;
    }
}