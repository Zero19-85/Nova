// audio_shim.cpp — WASAPI desktop loopback capture + virtual-sink routing
// WIN32_LEAN_AND_MEAN is already defined by the build system; using Windows-native
// types (UINT32/UINT16/BYTE) throughout so <stdint.h> isn't required.
//
// Client-only audio (Sunshine's approach, src/platform/windows/audio.cpp):
// instead of muting the host endpoint, the default render device is switched
// to a virtual sink (Steam Streaming Speakers / VB-CABLE). Windows migrates
// all application streams to the new default, so the physical speakers go
// silent naturally, and we loopback-capture the virtual sink by device id.
// The original default is restored when streaming stops.

#include <windows.h>
#include <initguid.h>
#include <mmdeviceapi.h>
#include <audioclient.h>
#include <functiondiscoverykeys_devpkey.h>
#include <stdio.h>

// ---------------------------------------------------------------------------
// IPolicyConfig — undocumented COM interface for setting the default audio
// endpoint. Same declaration Sunshine ships (PolicyConfig.h, author EreTIk).
// Compatible with Windows 7 and later.
// ---------------------------------------------------------------------------
DEFINE_GUID(IID_IPolicyConfig, 0xf8679f50, 0x850a, 0x41cf, 0x9c, 0x72, 0x43, 0x0f, 0x29, 0x02, 0x90, 0xc8);
DEFINE_GUID(CLSID_CPolicyConfigClient, 0x870af99c, 0x171d, 0x4f9e, 0xaf, 0x0d, 0xe6, 0x3d, 0xf4, 0x0c, 0x2b, 0xc9);

interface IPolicyConfig : public IUnknown
{
public:
    virtual HRESULT GetMixFormat(PCWSTR, WAVEFORMATEX**);
    virtual HRESULT STDMETHODCALLTYPE GetDeviceFormat(PCWSTR, INT, WAVEFORMATEX**);
    virtual HRESULT STDMETHODCALLTYPE ResetDeviceFormat(PCWSTR);
    virtual HRESULT STDMETHODCALLTYPE SetDeviceFormat(PCWSTR, WAVEFORMATEX*, WAVEFORMATEX*);
    virtual HRESULT STDMETHODCALLTYPE GetProcessingPeriod(PCWSTR, INT, PINT64, PINT64);
    virtual HRESULT STDMETHODCALLTYPE SetProcessingPeriod(PCWSTR, PINT64);
    virtual HRESULT STDMETHODCALLTYPE GetShareMode(PCWSTR, struct DeviceShareMode*);
    virtual HRESULT STDMETHODCALLTYPE SetShareMode(PCWSTR, struct DeviceShareMode*);
    virtual HRESULT STDMETHODCALLTYPE GetPropertyValue(PCWSTR, const PROPERTYKEY&, PROPVARIANT*);
    virtual HRESULT STDMETHODCALLTYPE SetPropertyValue(PCWSTR, const PROPERTYKEY&, PROPVARIANT*);
    virtual HRESULT STDMETHODCALLTYPE SetDefaultEndpoint(PCWSTR wszDeviceId, ERole eRole);
    virtual HRESULT STDMETHODCALLTYPE SetEndpointVisibility(PCWSTR, INT);
};

static IMMDeviceEnumerator* g_enum     = nullptr;
static IMMDevice*           g_device   = nullptr;
static IAudioClient*        g_client   = nullptr;
static IAudioCaptureClient* g_capture  = nullptr;
static WAVEFORMATEX*        g_pwfx     = nullptr;

// device_id: render endpoint to loopback-capture, or nullptr for the current
// default. Client-only routing passes the virtual sink's id explicitly so
// there is no race with the default-device switch.
extern "C" __declspec(dllexport)
int InitAudioCapture(const WCHAR* device_id, UINT32* out_rate, UINT16* out_ch, UINT16* out_bps)
{
    HRESULT hr = CoInitializeEx(nullptr, COINIT_MULTITHREADED);
    if (FAILED(hr) && hr != RPC_E_CHANGED_MODE) return -1;

    hr = CoCreateInstance(
        __uuidof(MMDeviceEnumerator), nullptr,
        CLSCTX_ALL, __uuidof(IMMDeviceEnumerator), (void**)&g_enum);
    if (FAILED(hr)) return -2;

    if (device_id && device_id[0])
        hr = g_enum->GetDevice(device_id, &g_device);
    else
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

    printf("\xF0\x9F\x8E\xB5 Audio capture: %u Hz  %u ch  %u-bit%s\n",
           g_pwfx->nSamplesPerSec, g_pwfx->nChannels, g_pwfx->wBitsPerSample,
           (device_id && device_id[0]) ? " (virtual sink)" : " (default device)");
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

// ---------------------------------------------------------------------------
// Virtual-sink routing helpers. Each is self-contained (own COM init +
// enumerator) so they can be called from any Rust thread at any time.
// ---------------------------------------------------------------------------

struct ComScope {
    bool needUninit;
    HRESULT hr;
    ComScope() {
        hr = CoInitializeEx(nullptr, COINIT_MULTITHREADED);
        needUninit = SUCCEEDED(hr); // RPC_E_CHANGED_MODE → already initialized
        if (hr == RPC_E_CHANGED_MODE) hr = S_OK;
    }
    ~ComScope() { if (needUninit) CoUninitialize(); }
};

// Writes the current default render endpoint id (null-terminated) to out_id.
// Returns 0 on success.
extern "C" __declspec(dllexport)
int GetDefaultAudioDeviceId(WCHAR* out_id, int cch)
{
    ComScope com;
    if (FAILED(com.hr)) return -1;

    IMMDeviceEnumerator* en = nullptr;
    IMMDevice* dev = nullptr;
    LPWSTR id = nullptr;
    int ret = -2;

    HRESULT hr = CoCreateInstance(__uuidof(MMDeviceEnumerator), nullptr, CLSCTX_ALL,
                                  __uuidof(IMMDeviceEnumerator), (void**)&en);
    if (SUCCEEDED(hr)) hr = en->GetDefaultAudioEndpoint(eRender, eConsole, &dev);
    if (SUCCEEDED(hr)) hr = dev->GetId(&id);
    if (SUCCEEDED(hr) && id && (int)wcslen(id) < cch) {
        wcscpy_s(out_id, cch, id);
        ret = 0;
    }

    if (id)  CoTaskMemFree(id);
    if (dev) dev->Release();
    if (en)  en->Release();
    return ret;
}

// Case-insensitive substring search (CRT-only; shlwapi's StrStrIW would add a
// link dependency for one call).
static bool contains_icase(const WCHAR* hay, const WCHAR* needle)
{
    if (!hay || !needle || !*needle) return false;
    size_t nlen = wcslen(needle);
    for (const WCHAR* p = hay; *p; ++p) {
        if (_wcsnicmp(p, needle, nlen) == 0) return true;
    }
    return false;
}

// Reads one string property off an endpoint. False when absent or too long.
static bool read_string_prop(IMMDevice* dev, const PROPERTYKEY& key, WCHAR* out, int cch)
{
    IPropertyStore* props = nullptr;
    if (FAILED(dev->OpenPropertyStore(STGM_READ, &props))) return false;

    bool ok = false;
    PROPVARIANT v;
    PropVariantInit(&v);
    if (SUCCEEDED(props->GetValue(key, &v)) && v.vt == VT_LPWSTR && v.pwszVal &&
        (int)wcslen(v.pwszVal) < cch) {
        wcscpy_s(out, cch, v.pwszVal);
        ok = true;
    }
    PropVariantClear(&v);
    props->Release();
    return ok;
}

// Matches a needle against BOTH names an endpoint answers to: its own friendly
// name ("Speakers (Steam Streaming Speakers)") and its adapter's
// ("NVIDIA Virtual Audio Device (Wave Extensible) (WDM)").
//
// The second lookup exists for NVIDIA specifically: its render endpoints are
// named after the attached display — "LG ULTRAGEAR (NVIDIA High Definition
// Audio)" on this box — so the adapter name is the ONLY place the string
// "NVIDIA Virtual Audio" ever appears. Matching the endpoint name alone would
// make that list entry permanently dead and look like a missing device.
static bool endpoint_matches(IMMDevice* dev, const WCHAR* needle)
{
    WCHAR buf[256];
    if (read_string_prop(dev, PKEY_Device_FriendlyName, buf, 256) &&
        contains_icase(buf, needle)) {
        return true;
    }
    if (read_string_prop(dev, PKEY_DeviceInterface_FriendlyName, buf, 256) &&
        contains_icase(buf, needle)) {
        return true;
    }
    return false;
}

// ── Two lists, because these are two different questions ─────────────────────
//
// A: kGhostSinkNames — endpoints Nova may switch the system default TO while
// streaming, so Windows migrates application audio off the physical speakers.
// ORDERED: `find_ghost_sink` iterates this list OUTERMOST, so preference is
// decided here rather than by WASAPI's enumeration order.
//
// VB-CABLE is deliberately absent. The Echo microphone renders into "CABLE
// Input"; a ghost sink on the same cable would feed the host's own game audio
// into the remote user's microphone — and, with "Listen to this device" enabled
// anywhere, into a feedback loop. `mic.rs::collides_with_ghost_sink` refuses the
// operator-override spelling of that mistake; this list refuses the automatic
// one. An operator who genuinely wants a cable can still name one in
// `[audio] endpoint_override`, which is checked against the mic's endpoint first.
static const WCHAR* kGhostSinkNames[] = {
    L"Steam Streaming Speakers",   // installed by Steam; Sunshine's default
    L"NVIDIA Virtual Audio",       // NVIDIA's virtual audio adapter
};

// B: kNotPlaybackNames — endpoints that are NOT a place a human is listening.
// Used only NEGATED, by `find_playback_device`, which picks where to restore the
// default output after an unclean exit. Restoring onto a virtual cable produces
// silence that reports success, so this list must be a SUPERSET of list A — every
// ghost sink is also not-a-real-speaker — plus every cable that is not a ghost
// sink candidate but is still not an output.
//
// Removing VB-CABLE from list A without adding it here would move the collision
// rather than fix it: crash recovery would be free to make the microphone's own
// cable the system default, arriving at the same feedback path from the other
// direction.
static const WCHAR* kNotPlaybackNames[] = {
    L"Steam Streaming Speakers",
    L"NVIDIA Virtual Audio",
    L"VB-Audio Virtual Cable",   // the suffix EVERY VB-CABLE endpoint carries:
                                 // "CABLE Input", "CABLE In 16ch", "CABLE Output"
    L"Virtual Audio Cable",      // VAC (Muzychenko) — a different product
    L"Virtual Audio Driver",     // VAD by MTT
};

// Finds the ghost sink: the first entry of kGhostSinkNames present as an ACTIVE
// render endpoint. Returns 0 + id, 1 if none is present, <0 on error.
static int find_ghost_sink(WCHAR* out_id, int cch)
{
    ComScope com;
    if (FAILED(com.hr)) return -1;

    IMMDeviceEnumerator* en = nullptr;
    IMMDeviceCollection* coll = nullptr;
    int ret = 1; // not found

    HRESULT hr = CoCreateInstance(__uuidof(MMDeviceEnumerator), nullptr, CLSCTX_ALL,
                                  __uuidof(IMMDeviceEnumerator), (void**)&en);
    if (SUCCEEDED(hr)) hr = en->EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE, &coll);

    UINT count = 0;
    if (SUCCEEDED(hr)) hr = coll->GetCount(&count);
    if (FAILED(hr)) ret = -2;

    // Preference list outermost — this is the whole point of the rewrite.
    for (const WCHAR* want : kGhostSinkNames) {
        for (UINT i = 0; SUCCEEDED(hr) && ret == 1 && i < count; ++i) {
            IMMDevice* dev = nullptr;
            if (FAILED(coll->Item(i, &dev))) continue;

            if (endpoint_matches(dev, want)) {
                LPWSTR id = nullptr;
                if (SUCCEEDED(dev->GetId(&id)) && id && (int)wcslen(id) < cch) {
                    wcscpy_s(out_id, cch, id);
                    WCHAR label[256];
                    if (!read_string_prop(dev, PKEY_Device_FriendlyName, label, 256)) {
                        wcscpy_s(label, 256, want);
                    }
                    printf("\xF0\x9F\x8E\xA7 Ghost sink: %ls\n", label);
                    ret = 0;
                }
                if (id) CoTaskMemFree(id);
            }
            dev->Release();
        }
        if (ret == 0) break;
    }

    if (coll) coll->Release();
    if (en)   en->Release();
    return ret;
}

extern "C" __declspec(dllexport)
int FindVirtualAudioSink(WCHAR* out_id, int cch)
{
    return find_ghost_sink(out_id, cch);
}

// Finds somewhere a human can actually hear: the first ACTIVE render endpoint
// matching nothing in kNotPlaybackNames. Returns 0 + id, 1 if none, <0 on error.
static int find_playback_device(WCHAR* out_id, int cch)
{
    ComScope com;
    if (FAILED(com.hr)) return -1;

    IMMDeviceEnumerator* en = nullptr;
    IMMDeviceCollection* coll = nullptr;
    int ret = 1; // not found

    HRESULT hr = CoCreateInstance(__uuidof(MMDeviceEnumerator), nullptr, CLSCTX_ALL,
                                  __uuidof(IMMDeviceEnumerator), (void**)&en);
    if (SUCCEEDED(hr)) hr = en->EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE, &coll);

    UINT count = 0;
    if (SUCCEEDED(hr)) hr = coll->GetCount(&count);
    if (FAILED(hr)) ret = -2;

    for (UINT i = 0; SUCCEEDED(hr) && ret == 1 && i < count; ++i) {
        IMMDevice* dev = nullptr;
        if (FAILED(coll->Item(i, &dev))) continue;

        bool excluded = false;
        for (const WCHAR* skip : kNotPlaybackNames) {
            if (endpoint_matches(dev, skip)) { excluded = true; break; }
        }

        if (!excluded) {
            LPWSTR id = nullptr;
            if (SUCCEEDED(dev->GetId(&id)) && id && (int)wcslen(id) < cch) {
                wcscpy_s(out_id, cch, id);
                WCHAR label[256];
                if (!read_string_prop(dev, PKEY_Device_FriendlyName, label, 256)) {
                    wcscpy_s(label, 256, L"(unnamed endpoint)");
                }
                printf("\xF0\x9F\x94\x8A Real audio device: %ls\n", label);
                ret = 0;
            }
            if (id) CoTaskMemFree(id);
        }
        dev->Release();
    }

    if (coll) coll->Release();
    if (en)   en->Release();
    return ret;
}

// Finds an ACTIVE render endpoint by the operator's nova.toml designation:
// matches `needle` case-insensitively as a SUBSTRING of the endpoint friendly
// name (e.g. "VDD by MTT", "VoiceMeeter Input") or as the EXACT endpoint id
// ("{0.0.0.00000000}.{guid}"). Lets any render device serve as the streaming
// sink without extending kVirtualSinkNames. Returns 0 + id, 1 if no match,
// <0 on error.
extern "C" __declspec(dllexport)
int FindAudioDeviceByName(const WCHAR* needle, WCHAR* out_id, int cch)
{
    if (!needle || !*needle) return 1;

    ComScope com;
    if (FAILED(com.hr)) return -1;

    IMMDeviceEnumerator* en = nullptr;
    IMMDeviceCollection* coll = nullptr;
    int ret = 1; // not found

    HRESULT hr = CoCreateInstance(__uuidof(MMDeviceEnumerator), nullptr, CLSCTX_ALL,
                                  __uuidof(IMMDeviceEnumerator), (void**)&en);
    if (SUCCEEDED(hr)) hr = en->EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE, &coll);

    UINT count = 0;
    if (SUCCEEDED(hr)) hr = coll->GetCount(&count);
    if (FAILED(hr)) ret = -2;

    for (UINT i = 0; SUCCEEDED(hr) && ret == 1 && i < count; ++i) {
        IMMDevice* dev = nullptr;
        if (FAILED(coll->Item(i, &dev))) continue;

        LPWSTR id = nullptr;
        if (SUCCEEDED(dev->GetId(&id)) && id) {
            bool matched = (_wcsicmp(id, needle) == 0);

            if (!matched) {
                IPropertyStore* props = nullptr;
                if (SUCCEEDED(dev->OpenPropertyStore(STGM_READ, &props))) {
                    PROPVARIANT name;
                    PropVariantInit(&name);
                    if (SUCCEEDED(props->GetValue(PKEY_Device_FriendlyName, &name)) &&
                        name.vt == VT_LPWSTR && name.pwszVal &&
                        contains_icase(name.pwszVal, needle)) {
                        matched = true;
                        printf("\xF0\x9F\x8E\xA7 Sink override matched: %ls\n", name.pwszVal);
                    }
                    PropVariantClear(&name);
                    props->Release();
                }
            }

            if (matched && (int)wcslen(id) < cch) {
                wcscpy_s(out_id, cch, id);
                ret = 0;
            }
            CoTaskMemFree(id);
        }
        dev->Release();
    }

    if (coll) coll->Release();
    if (en)   en->Release();
    return ret;
}

// Crash recovery: if Nova exited without restoring the default device
// (killed/closed rather than a clean shutdown), startup detects that the
// default is still the ghost sink and switches back to a real output — which
// is what this returns. See kNotPlaybackNames for what "real" excludes.
extern "C" __declspec(dllexport)
int FindRealAudioDevice(WCHAR* out_id, int cch)
{
    return find_playback_device(out_id, cch);
}

// Makes device_id the default render endpoint for all roles (console,
// multimedia, communications) via IPolicyConfig — exactly what the Windows
// Sound control panel does. Returns 0 on success.
extern "C" __declspec(dllexport)
int SetDefaultAudioDevice(const WCHAR* device_id)
{
    ComScope com;
    if (FAILED(com.hr)) return -1;

    IPolicyConfig* policy = nullptr;
    HRESULT hr = CoCreateInstance(CLSID_CPolicyConfigClient, nullptr, CLSCTX_ALL,
                                  IID_IPolicyConfig, (void**)&policy);
    if (FAILED(hr)) return -2;

    int failures = 0;
    for (int role = 0; role < ERole_enum_count; ++role) {
        HRESULT r = policy->SetDefaultEndpoint(device_id, (ERole)role);
        if (FAILED(r)) {
            ++failures;
            printf("\xE2\x9A\xA0 SetDefaultEndpoint role %d failed: 0x%08lx\n", role, (unsigned long)r);
        }
    }

    policy->Release();
    // Playback follows eConsole/eMultimedia; some devices reject the
    // communications role. Only report failure if NO role could be set —
    // a partial success must not abort client-only routing.
    return (failures == ERole_enum_count) ? -3 : 0;
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

// ===========================================================================
// Microphone passthrough â€” RENDER into a virtual cable, plus a probe capture
// ===========================================================================
//
// Echo's client sends the phone's microphone to the host; the host decodes it
// and renders it into VB-CABLE's input, so Windows applications can select
// "CABLE Output" as their microphone.
//
// This is a RENDER path and it keeps its own COM state (g_mic*) rather than
// sharing the loopback capture globals above. That separation is deliberate:
// InitAudioCapture/CleanupAudio are process-global and guarded on the Rust side
// by SHIM_CAPTURE_ACTIVE, which exists to stop two *capture* sessions
// overlapping. The microphone neither participates in nor may interfere with
// that lifecycle â€” a stream starting must never tear down the microphone, and
// vice versa.
//
// Format: the client always sends 48 kHz mono, which is what Opus decodes to.
// Rather than adopting the device mix format and converting by hand, the stream
// is opened with AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM and the format we actually
// have. WASAPI then does any rate and channel conversion inside the audio
// engine, which is both better than a hand-rolled resampler and far less code
// to get subtly wrong.

static IMMDeviceEnumerator* g_micEnum   = nullptr;
static IMMDevice*           g_micDev    = nullptr;
static IAudioClient*        g_micClient = nullptr;
static IAudioRenderClient*  g_micRender = nullptr;
static UINT32               g_micBufFrames = 0;

// 48 kHz mono 16-bit â€” what Opus decodes to, declared once for both directions.
static void mic_format(WAVEFORMATEX* w)
{
    w->wFormatTag      = WAVE_FORMAT_PCM;
    w->nChannels       = 1;
    w->nSamplesPerSec  = 48000;
    w->wBitsPerSample  = 16;
    w->nBlockAlign     = (WORD)(w->nChannels * w->wBitsPerSample / 8);
    w->nAvgBytesPerSec = w->nSamplesPerSec * w->nBlockAlign;
    w->cbSize          = 0;
}

// Generalised endpoint lookup: `is_capture` selects eCapture instead of
// eRender. Matches an exact endpoint id or a case-insensitive substring of the
// friendly name. Returns 0 + id, 1 if no match, <0 on error.
extern "C" __declspec(dllexport)
int FindEndpointByName(const WCHAR* needle, int is_capture, WCHAR* out_id, int cch)
{
    if (!needle || !*needle) return 1;

    ComScope com;
    if (FAILED(com.hr)) return -1;

    IMMDeviceEnumerator* en = nullptr;
    IMMDeviceCollection* coll = nullptr;
    int ret = 1;

    HRESULT hr = CoCreateInstance(__uuidof(MMDeviceEnumerator), nullptr, CLSCTX_ALL,
                                  __uuidof(IMMDeviceEnumerator), (void**)&en);
    if (SUCCEEDED(hr))
        hr = en->EnumAudioEndpoints(is_capture ? eCapture : eRender, DEVICE_STATE_ACTIVE, &coll);

    UINT count = 0;
    if (SUCCEEDED(hr)) hr = coll->GetCount(&count);
    if (FAILED(hr)) ret = -2;

    for (UINT i = 0; SUCCEEDED(hr) && ret == 1 && i < count; ++i) {
        IMMDevice* dev = nullptr;
        if (FAILED(coll->Item(i, &dev))) continue;

        LPWSTR id = nullptr;
        if (SUCCEEDED(dev->GetId(&id)) && id) {
            bool matched = (_wcsicmp(id, needle) == 0);
            if (!matched) {
                IPropertyStore* props = nullptr;
                if (SUCCEEDED(dev->OpenPropertyStore(STGM_READ, &props))) {
                    PROPVARIANT name;
                    PropVariantInit(&name);
                    if (SUCCEEDED(props->GetValue(PKEY_Device_FriendlyName, &name)) &&
                        name.vt == VT_LPWSTR && name.pwszVal &&
                        contains_icase(name.pwszVal, needle)) {
                        matched = true;
                        printf("\xF0\x9F\x8E\xA4 Endpoint matched (%s): %ls\n",
                               is_capture ? "capture" : "render", name.pwszVal);
                    }
                    PropVariantClear(&name);
                    props->Release();
                }
            }
            if (matched && (int)wcslen(id) < cch) {
                wcscpy_s(out_id, cch, id);
                ret = 0;
            }
            CoTaskMemFree(id);
        }
        dev->Release();
    }

    if (coll) coll->Release();
    if (en)   en->Release();
    return ret;
}

// Opens `device_id` for rendering 48 kHz mono 16-bit PCM.
//
// Returns 0 on success, <0 on failure, with `out_hr` receiving the failing
// HRESULT so a caller can report *why* rather than merely that it did not work
// â€” which is the entire point when the open question is whether Session 0 is
// permitted to do this at all.
extern "C" __declspec(dllexport)
int InitMicRender(const WCHAR* device_id, UINT32* out_buffer_frames, long* out_hr)
{
    if (out_hr) *out_hr = 0;

    HRESULT hr = CoInitializeEx(nullptr, COINIT_MULTITHREADED);
    if (FAILED(hr) && hr != RPC_E_CHANGED_MODE) { if (out_hr) *out_hr = hr; return -1; }

    hr = CoCreateInstance(__uuidof(MMDeviceEnumerator), nullptr, CLSCTX_ALL,
                          __uuidof(IMMDeviceEnumerator), (void**)&g_micEnum);
    if (FAILED(hr)) { if (out_hr) *out_hr = hr; return -2; }

    if (device_id && *device_id) hr = g_micEnum->GetDevice(device_id, &g_micDev);
    else                         hr = g_micEnum->GetDefaultAudioEndpoint(eRender, eConsole, &g_micDev);
    if (FAILED(hr)) { if (out_hr) *out_hr = hr; return -3; }

    hr = g_micDev->Activate(__uuidof(IAudioClient), CLSCTX_ALL, nullptr, (void**)&g_micClient);
    if (FAILED(hr)) { if (out_hr) *out_hr = hr; return -4; }

    WAVEFORMATEX want;
    mic_format(&want);

    // 200 ms of device buffer. Large enough that a scheduling hiccup on the
    // network side does not underrun, small enough that it adds nothing
    // meaningful next to the jitter buffer in front of it.
    const REFERENCE_TIME kBuffer = 2000000; // 200 ms, in 100 ns units
    hr = g_micClient->Initialize(
        AUDCLNT_SHAREMODE_SHARED,
        AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
        kBuffer, 0, &want, nullptr);
    if (FAILED(hr)) { if (out_hr) *out_hr = hr; return -5; }

    hr = g_micClient->GetBufferSize(&g_micBufFrames);
    if (FAILED(hr)) { if (out_hr) *out_hr = hr; return -6; }

    hr = g_micClient->GetService(__uuidof(IAudioRenderClient), (void**)&g_micRender);
    if (FAILED(hr)) { if (out_hr) *out_hr = hr; return -7; }

    hr = g_micClient->Start();
    if (FAILED(hr)) { if (out_hr) *out_hr = hr; return -8; }

    if (out_buffer_frames) *out_buffer_frames = g_micBufFrames;
    printf("\xF0\x9F\x8E\xA4 Mic render open: 48kHz mono s16, %u-frame buffer\n", g_micBufFrames);
    return 0;
}

// Writes up to `frames` mono samples, returning how many were actually written
// (fewer when the device buffer is full), or <0 on error.
extern "C" __declspec(dllexport)
int RenderMicFrames(const short* mono, UINT32 frames, long* out_hr)
{
    if (out_hr) *out_hr = 0;
    if (!g_micClient || !g_micRender || !mono) return -1;

    UINT32 padding = 0;
    HRESULT hr = g_micClient->GetCurrentPadding(&padding);
    if (FAILED(hr)) { if (out_hr) *out_hr = hr; return -2; }

    UINT32 room = (g_micBufFrames > padding) ? (g_micBufFrames - padding) : 0;
    UINT32 want = (frames < room) ? frames : room;
    if (want == 0) return 0;

    BYTE* dst = nullptr;
    hr = g_micRender->GetBuffer(want, &dst);
    if (FAILED(hr)) { if (out_hr) *out_hr = hr; return -3; }

    memcpy(dst, mono, (size_t)want * sizeof(short));

    hr = g_micRender->ReleaseBuffer(want, 0);
    if (FAILED(hr)) { if (out_hr) *out_hr = hr; return -4; }
    return (int)want;
}

extern "C" __declspec(dllexport)
void CleanupMicRender()
{
    if (g_micClient) g_micClient->Stop();
    if (g_micRender) { g_micRender->Release(); g_micRender = nullptr; }
    if (g_micClient) { g_micClient->Release(); g_micClient = nullptr; }
    if (g_micDev)    { g_micDev->Release();    g_micDev    = nullptr; }
    if (g_micEnum)   { g_micEnum->Release();   g_micEnum   = nullptr; }
    g_micBufFrames = 0;
    printf("\xF0\x9F\x8E\xA4 Mic render closed.\n");
}

// â”€â”€ Probe-only capture â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
// Opens a CAPTURE endpoint (e.g. "CABLE Output") and reports the loudest
// sample seen. This exists to answer one question by measurement rather than by
// reading documentation: does audio rendered by a Session 0 service actually
// reach a capture endpoint in the interactive user's session? A successful
// HRESULT on the render side proves only that the API accepted the call.

static IMMDeviceEnumerator* g_probeEnum   = nullptr;
static IMMDevice*           g_probeDev    = nullptr;
static IAudioClient*        g_probeClient = nullptr;
static IAudioCaptureClient* g_probeCap    = nullptr;

extern "C" __declspec(dllexport)
int InitProbeCapture(const WCHAR* device_id, long* out_hr)
{
    if (out_hr) *out_hr = 0;

    HRESULT hr = CoInitializeEx(nullptr, COINIT_MULTITHREADED);
    if (FAILED(hr) && hr != RPC_E_CHANGED_MODE) { if (out_hr) *out_hr = hr; return -1; }

    hr = CoCreateInstance(__uuidof(MMDeviceEnumerator), nullptr, CLSCTX_ALL,
                          __uuidof(IMMDeviceEnumerator), (void**)&g_probeEnum);
    if (FAILED(hr)) { if (out_hr) *out_hr = hr; return -2; }

    if (device_id && *device_id) hr = g_probeEnum->GetDevice(device_id, &g_probeDev);
    else                         hr = g_probeEnum->GetDefaultAudioEndpoint(eCapture, eConsole, &g_probeDev);
    if (FAILED(hr)) { if (out_hr) *out_hr = hr; return -3; }

    hr = g_probeDev->Activate(__uuidof(IAudioClient), CLSCTX_ALL, nullptr, (void**)&g_probeClient);
    if (FAILED(hr)) { if (out_hr) *out_hr = hr; return -4; }

    WAVEFORMATEX want;
    mic_format(&want);
    hr = g_probeClient->Initialize(
        AUDCLNT_SHAREMODE_SHARED,
        AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
        2000000, 0, &want, nullptr);
    if (FAILED(hr)) { if (out_hr) *out_hr = hr; return -5; }

    hr = g_probeClient->GetService(__uuidof(IAudioCaptureClient), (void**)&g_probeCap);
    if (FAILED(hr)) { if (out_hr) *out_hr = hr; return -6; }

    hr = g_probeClient->Start();
    if (FAILED(hr)) { if (out_hr) *out_hr = hr; return -7; }
    return 0;
}

// Drains whatever is available and reports the loudest absolute sample seen,
// normalised to 0..1, plus how many frames were read â€” because "silence" and
// "no data at all" are different answers to the question being asked.
extern "C" __declspec(dllexport)
int ProbeCapturePeak(float* out_peak, UINT32* out_frames)
{
    if (!g_probeCap) return -1;
    float peak = 0.0f;
    UINT32 total = 0;

    for (;;) {
        UINT32 packet = 0;
        if (FAILED(g_probeCap->GetNextPacketSize(&packet)) || packet == 0) break;

        BYTE* data = nullptr;
        UINT32 frames = 0;
        DWORD flags = 0;
        if (FAILED(g_probeCap->GetBuffer(&data, &frames, &flags, nullptr, nullptr))) break;

        if (!(flags & AUDCLNT_BUFFERFLAGS_SILENT) && data) {
            const short* s = (const short*)data;
            for (UINT32 i = 0; i < frames; ++i) {
                float v = (float)(s[i] < 0 ? -s[i] : s[i]) / 32768.0f;
                if (v > peak) peak = v;
            }
        }
        total += frames;
        g_probeCap->ReleaseBuffer(frames);
    }

    if (out_peak)   *out_peak = peak;
    if (out_frames) *out_frames = total;
    return 0;
}

extern "C" __declspec(dllexport)
void CleanupProbeCapture()
{
    if (g_probeClient) g_probeClient->Stop();
    if (g_probeCap)    { g_probeCap->Release();    g_probeCap    = nullptr; }
    if (g_probeClient) { g_probeClient->Release(); g_probeClient = nullptr; }
    if (g_probeDev)    { g_probeDev->Release();    g_probeDev    = nullptr; }
    if (g_probeEnum)   { g_probeEnum->Release();   g_probeEnum   = nullptr; }
}

