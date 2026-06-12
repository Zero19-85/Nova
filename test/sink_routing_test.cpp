// sink_routing_test.cpp — verifies client-only audio routing end-to-end
// without a Moonlight client, using the exact shim functions Nova calls:
//   1. FindVirtualAudioSink   → locate Steam Streaming Speakers / VB-CABLE
//   2. GetDefaultAudioDeviceId → remember the real default
//   3. SetDefaultAudioDevice(sink) → app audio now routes to the virtual sink
//   4. InitAudioCapture(sink) + Beep() → captured loopback must be non-silent
//   5. SetDefaultAudioDevice(orig) → default restored, verified
//
// Build (from repo root, VS dev prompt):
//   cl /nologo /std:c++17 /EHsc /DWIN32_LEAN_AND_MEAN test\sink_routing_test.cpp ^
//      shim\audio_shim.cpp /Fe:test\sink_routing_test.exe ole32.lib
#include <windows.h>
#include <stdio.h>
#include <math.h>

extern "C" int  InitAudioCapture(const WCHAR*, UINT32*, UINT16*, UINT16*);
extern "C" int  CaptureAudioFrame(BYTE*, int, UINT32*);
extern "C" void CleanupAudio();
extern "C" int  GetDefaultAudioDeviceId(WCHAR*, int);
extern "C" int  FindVirtualAudioSink(WCHAR*, int);
extern "C" int  SetDefaultAudioDevice(const WCHAR*);

static DWORD WINAPI beep_thread(LPVOID)
{
    Beep(750, 1500); // plays via the DEFAULT render device (the virtual sink now)
    return 0;
}

int main()
{
    WCHAR sink[512], orig[512], cur[512];

    if (FindVirtualAudioSink(sink, 512) != 0) {
        printf("FAIL: no virtual audio sink found\n");
        return 1;
    }
    if (GetDefaultAudioDeviceId(orig, 512) != 0) {
        printf("FAIL: couldn't read current default device\n");
        return 1;
    }
    printf("original default: %ls\n", orig);

    if (SetDefaultAudioDevice(sink) != 0) {
        printf("FAIL: SetDefaultAudioDevice(sink)\n");
        return 1;
    }
    GetDefaultAudioDeviceId(cur, 512);
    printf("switch to sink:   %s\n", wcscmp(cur, sink) == 0 ? "OK" : "MISMATCH");

    UINT32 rate = 0; UINT16 ch = 0, bps = 0;
    int ret = InitAudioCapture(sink, &rate, &ch, &bps);
    if (ret != 0) {
        printf("FAIL: InitAudioCapture on sink (code %d)\n", ret);
        SetDefaultAudioDevice(orig);
        return 1;
    }

    HANDLE bt = CreateThread(nullptr, 0, beep_thread, nullptr, 0, nullptr);

    static BYTE buf[1 << 20];
    DWORD t0 = GetTickCount();
    long long total_bytes = 0;
    float peak = 0.0f;
    while (GetTickCount() - t0 < 2000) {
        UINT32 frames = 0;
        int n = CaptureAudioFrame(buf, sizeof(buf), &frames);
        if (n > 0) {
            total_bytes += n;
            if (bps == 32) {
                for (int i = 0; i + 4 <= n; i += 4) {
                    float s = fabsf(*(float*)(buf + i));
                    if (s > peak) peak = s;
                }
            } else if (bps == 16) {
                for (int i = 0; i + 2 <= n; i += 2) {
                    float s = fabsf(*(short*)(buf + i) / 32768.0f);
                    if (s > peak) peak = s;
                }
            }
        } else if (n == 0) {
            Sleep(2);
        } else {
            printf("FAIL: CaptureAudioFrame error %d\n", n);
            break;
        }
    }
    WaitForSingleObject(bt, 3000);
    CloseHandle(bt);
    CleanupAudio();

    printf("captured %lld bytes from virtual sink, peak amplitude %.4f → %s\n",
           total_bytes, peak,
           (total_bytes > 0 && peak > 0.001f) ? "OK (beep routed into sink)" : "FAIL (sink silent)");

    if (SetDefaultAudioDevice(orig) != 0) {
        printf("FAIL: could not restore original default device!\n");
        return 1;
    }
    GetDefaultAudioDeviceId(cur, 512);
    printf("restore default:  %s\n", wcscmp(cur, orig) == 0 ? "OK" : "MISMATCH");

    return (total_bytes > 0 && peak > 0.001f) ? 0 : 1;
}
