// audio_shim.cpp — WASAPI desktop loopback capture
// WIN32_LEAN_AND_MEAN is already defined by the build system; using Windows-native
// types (UINT32/UINT16/BYTE) throughout so <stdint.h> isn't required.

#include <windows.h>
#include <mmdeviceapi.h>
#include <audioclient.h>
#include <stdio.h>

static IMMDeviceEnumerator* g_enum     = nullptr;
static IMMDevice*           g_device   = nullptr;
static IAudioClient*        g_client   = nullptr;
static IAudioCaptureClient* g_capture  = nullptr;
static WAVEFORMATEX*        g_pwfx     = nullptr;

extern "C" __declspec(dllexport)
int InitAudioCapture(UINT32* out_rate, UINT16* out_ch, UINT16* out_bps)
{
    HRESULT hr = CoInitializeEx(nullptr, COINIT_MULTITHREADED);
    if (FAILED(hr) && hr != RPC_E_CHANGED_MODE) return -1;

    hr = CoCreateInstance(
        __uuidof(MMDeviceEnumerator), nullptr,
        CLSCTX_ALL, __uuidof(IMMDeviceEnumerator), (void**)&g_enum);
    if (FAILED(hr)) return -2;

    hr = g_enum->GetDefaultAudioEndpoint(eRender, eConsole, &g_device);
    if (FAILED(hr)) return -3;

    hr = g_device->Activate(__uuidof(IAudioClient), CLSCTX_ALL, nullptr, (void**)&g_client);
    if (FAILED(hr)) return -4;

    hr = g_client->GetMixFormat(&g_pwfx);
    if (FAILED(hr)) return -5;

    *out_rate = g_pwfx->nSamplesPerSec;
    *out_ch   = g_pwfx->nChannels;
    *out_bps  = g_pwfx->wBitsPerSample;

    REFERENCE_TIME hnsRequestedDuration = 100000; // 10 ms in 100-ns units
    hr = g_client->Initialize(
        AUDCLNT_SHAREMODE_SHARED,
        AUDCLNT_STREAMFLAGS_LOOPBACK,
        hnsRequestedDuration, 0, g_pwfx, nullptr);
    if (FAILED(hr)) return -6;

    hr = g_client->GetService(__uuidof(IAudioCaptureClient), (void**)&g_capture);
    if (FAILED(hr)) return -7;

    hr = g_client->Start();
    if (FAILED(hr)) return -8;

    printf("\xF0\x9F\x8E\xB5 Audio capture: %u Hz  %u ch  %u-bit\n",
           g_pwfx->nSamplesPerSec, g_pwfx->nChannels, g_pwfx->wBitsPerSample);
    return 0;
}

// Returns bytes written to out_buffer (0 = no data yet, <0 = error).
extern "C" __declspec(dllexport)
int CaptureAudioFrame(BYTE* out_buffer, int max_bytes, UINT32* out_frames)
{
    if (!g_capture) return -1;

    UINT32 packetSize = 0;
    HRESULT hr = g_capture->GetNextPacketSize(&packetSize);
    if (FAILED(hr) || packetSize == 0) { *out_frames = 0; return 0; }

    BYTE*  pData     = nullptr;
    UINT32 numFrames = 0;
    DWORD  flags     = 0;

    hr = g_capture->GetBuffer(&pData, &numFrames, &flags, nullptr, nullptr);
    if (FAILED(hr)) return -2;

    int bytes = (int)(numFrames * g_pwfx->nBlockAlign);
    if (bytes > max_bytes) bytes = max_bytes;

    if (flags & AUDCLNT_BUFFERFLAGS_SILENT)
        memset(out_buffer, 0, bytes);
    else
        memcpy(out_buffer, pData, bytes);

    g_capture->ReleaseBuffer(numFrames);
    *out_frames = numFrames;
    return bytes;
}

extern "C" __declspec(dllexport)
void CleanupAudio()
{
    if (g_client)  g_client->Stop();
    if (g_capture) { g_capture->Release(); g_capture = nullptr; }
    if (g_client)  { g_client->Release();  g_client  = nullptr; }
    if (g_device)  { g_device->Release();  g_device  = nullptr; }
    if (g_enum)    { g_enum->Release();    g_enum    = nullptr; }
    if (g_pwfx)    { CoTaskMemFree(g_pwfx); g_pwfx   = nullptr; }
    printf("\xE2\x9C\x85 Audio cleanup complete.\n");
}
