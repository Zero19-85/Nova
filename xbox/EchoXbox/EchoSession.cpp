#include "pch.h"
#include "EchoSession.h"
#include "EchoBridge.h"
#include "VideoDecoder.h"
#include "HostDiscovery.h"

#include <winrt/Windows.Data.Json.h>

#include <fstream>
#include <sstream>
#include <array>

using namespace winrt;
using namespace Windows::Data::Json;

namespace echo {
namespace {

// A 4K IDR under Nova's rate control is well under a megabyte, but a frame is
// not required to be — so the feeder grows on FILL_TOO_SMALL rather than
// assuming.
constexpr int32_t kInitialFrameBuffer = 2 * 1024 * 1024;
constexpr int32_t kMaxFrameBuffer     = 32 * 1024 * 1024;

std::string HostsPath(std::string const& dir) { return dir + "\\hosts.json"; }

std::string ReadFile(std::string const& path) {
    std::ifstream in(path, std::ios::binary);
    if (!in) return {};
    std::ostringstream ss;
    ss << in.rdbuf();
    return ss.str();
}

// JSON string escaping. Only ever applied to values we control (paths,
// addresses, fingerprints), but a Windows path is full of backslashes and an
// unescaped one silently corrupts the config the bridge then rejects for a
// reason that names the wrong field.
std::string Escape(std::string const& value) {
    std::string out;
    out.reserve(value.size() + 8);
    for (char c : value) {
        switch (c) {
            case '\\': out += "\\\\"; break;
            case '"':  out += "\\\""; break;
            case '\n': out += "\\n";  break;
            case '\r': out += "\\r";  break;
            case '\t': out += "\\t";  break;
            default:   out += c;      break;
        }
    }
    return out;
}

std::string Narrow(std::wstring const& value) { return to_string(hstring(value)); }

}  // namespace

std::string JsonField(std::string const& json, char const* key) noexcept {
    // Deliberately a scanner and not a parser: this reads events produced by
    // our own bridge, it only needs flat string/number fields, and 20
    // dependency-free lines cannot throw on a shape it did not expect.
    try {
        const std::string needle = std::string("\"") + key + "\"";
        auto pos = json.find(needle);
        if (pos == std::string::npos) return {};
        pos = json.find(':', pos + needle.size());
        if (pos == std::string::npos) return {};
        ++pos;
        while (pos < json.size() && (json[pos] == ' ' || json[pos] == '\t')) ++pos;
        if (pos >= json.size()) return {};

        if (json[pos] == '"') {
            ++pos;
            std::string out;
            while (pos < json.size() && json[pos] != '"') {
                if (json[pos] == '\\' && pos + 1 < json.size()) ++pos;
                out.push_back(json[pos++]);
            }
            return out;
        }
        const auto start = pos;
        while (pos < json.size() && json[pos] != ',' && json[pos] != '}') ++pos;
        auto out = json.substr(start, pos - start);
        while (!out.empty() && (out.back() == ' ' || out.back() == '\n' || out.back() == '\r')) {
            out.pop_back();
        }
        return out;
    } catch (...) {
        return {};
    }
}

// ── The paired-host store ───────────────────────────────────────────────────

std::string LoadHostFingerprint(std::string const& stateDir,
                                std::wstring const& address) noexcept {
    try {
        const auto raw = ReadFile(HostsPath(stateDir));
        if (raw.empty()) return {};
        JsonObject root{ nullptr };
        if (!JsonObject::TryParse(to_hstring(raw), root)) return {};
        const hstring key{ address };
        if (!root.HasKey(key)) return {};
        auto entry = root.GetNamedObject(key);
        return to_string(entry.GetNamedString(L"fingerprint", L""));
    } catch (...) {
        return {};
    }
}

void SaveHostFingerprint(std::string const& stateDir, std::wstring const& address,
                         std::wstring const& name, std::string const& fingerprint) noexcept {
    try {
        if (address.empty() || fingerprint.empty()) return;

        JsonObject root{ nullptr };
        const auto raw = ReadFile(HostsPath(stateDir));
        if (raw.empty() || !JsonObject::TryParse(to_hstring(raw), root)) {
            root = JsonObject();
        }

        JsonObject entry;
        entry.SetNamedValue(L"name", JsonValue::CreateStringValue(hstring(name)));
        entry.SetNamedValue(L"fingerprint",
                            JsonValue::CreateStringValue(to_hstring(fingerprint)));
        root.SetNamedValue(hstring(address), entry);

        const auto text = to_string(root.Stringify());
        std::ofstream out(HostsPath(stateDir), std::ios::binary | std::ios::trunc);
        if (out) out.write(text.data(), static_cast<std::streamsize>(text.size()));
    } catch (...) {
        // A failure here costs one re-pair, never a broken session.
    }
}

// ── Config assembly ─────────────────────────────────────────────────────────

std::string BuildPairConfig(std::string const& identityDir,
                            DiscoveredHost const& host) noexcept {
    // Pairing needs only the host's address: the handshake carries its own
    // mutual proof, which is exactly why it can run over Nova's
    // unauthenticated HTTP port.
    std::string json = "{";
    json += "\"identity_dir\":\"" + Escape(identityDir) + "\",";
    json += "\"host\":\"" + Escape(Narrow(host.address)) + "\",";
    json += "\"device_name\":\"Xbox\",";
    json += "\"consent_secs\":180";
    json += "}";
    return json;
}

std::string BuildConnectConfig(std::string const& identityDir,
                               DiscoveredHost const& host,
                               std::string const& hostFingerprint,
                               std::string const& res, uint32_t fps,
                               uint32_t bitrateKbps, uint32_t appId) noexcept {
    std::string json = "{";
    json += "\"identity_dir\":\"" + Escape(identityDir) + "\",";
    json += "\"relay_url\":\"" + Escape(Narrow(host.relayUrl)) + "\",";
    json += "\"relay_pin\":\"" + Escape(Narrow(host.relayPin)) + "\",";
    // From the PIN handshake, never from the mDNS `fp`. See EchoSession.h.
    json += "\"host_fingerprint\":\"" + Escape(hostFingerprint) + "\",";
    json += "\"res\":\"" + Escape(res) + "\",";
    json += "\"fps\":" + std::to_string(fps) + ",";
    json += "\"codec\":\"hevc\",";
    json += "\"bitrate_kbps\":" + std::to_string(bitrateKbps) + ",";
    // App 5 ("Virtual Desktop") is the HEADLESS one, and this single field is
    // what decides whether the console gets its own monitor or a mirror of the
    // PC's. `app_launcher::uses_virtual_display` early-returns false for app 1
    // (Desktop) before it ever consults `headless_for_all_apps`, so a Desktop
    // session always mirrors the physical display at the physical display's
    // size. App 5 launches no process; it exists precisely to mean "a desktop,
    // on a virtual monitor, at the size the client asked for".
    json += "\"app_id\":" + std::to_string(appId) + ",";
    // The LAN endpoint we observed ourselves, so the cascade tries the direct
    // path first and only falls back to the relay if it does not answer. The
    // host's Echo RPC port is fixed at 48011.
    if (!host.address.empty()) {
        json += "\"lan_endpoint\":\"" + Escape(Narrow(host.address)) + ":48011\",";
    }
    json += "\"punch_secs\":8";
    json += "}";
    return json;
}

std::string LoadConfigOverride(std::string const& stateDir) noexcept {
    try {
        return ReadFile(stateDir + "\\echo.json");
    } catch (...) {
        return {};
    }
}

// ── Session ─────────────────────────────────────────────────────────────────

EchoSession::~EchoSession() { Close(); }

bool EchoSession::Pair(std::string const& configJson, EventSink sink,
                       std::wstring& error) noexcept {
    if (m_handle) { error = L"a session is already open"; return false; }

    m_handle = echo_pair(configJson.c_str());
    if (m_handle == 0) {
        std::array<char, 1024> reason{};
        echo_last_error(reason.data(), static_cast<int32_t>(reason.size()));
        error = L"pair refused: " + std::wstring(to_hstring(reason.data()));
        return false;
    }
    m_running.store(true, std::memory_order_release);
    m_pump = std::thread([this, sink] { PumpEvents(sink); });
    return true;
}

bool EchoSession::Connect(std::string const& configJson, EventSink sink,
                          VideoDecoder* decoder, std::wstring& error) noexcept {
    if (m_handle) { error = L"a session is already open"; return false; }

    m_handle = echo_connect(configJson.c_str());
    if (m_handle == 0) {
        std::array<char, 1024> reason{};
        echo_last_error(reason.data(), static_cast<int32_t>(reason.size()));
        error = L"connect refused: " + std::wstring(to_hstring(reason.data()));
        return false;
    }
    m_running.store(true, std::memory_order_release);
    m_pump = std::thread([this, sink] { PumpEvents(sink); });
    m_feed = std::thread([this, decoder] { FeedFrames(decoder); });
    return true;
}

void EchoSession::PumpEvents(EventSink sink) noexcept {
    std::array<char, 4096> buffer{};
    while (m_running.load(std::memory_order_acquire)) {
        const int32_t code = echo_poll_event(m_handle, 250, buffer.data(),
                                             static_cast<int32_t>(buffer.size()));
        if (code == ECHO_BAD_HANDLE) break;   // the session ended
        if (code > 0 && sink) {
            sink(std::string(buffer.data(), static_cast<size_t>(code)));
        }
        // ECHO_OK means the timeout expired with nothing to report, which is
        // the normal state of a healthy session.
    }
}

void EchoSession::FeedFrames(VideoDecoder* decoder) noexcept {
    std::vector<uint8_t> buffer(kInitialFrameBuffer);
    int64_t meta[3]{};

    // The first frame of a session, and the first after any gap, must tell the
    // decoder its reference chain restarts here. `echo-client`'s keyframe gate
    // guarantees the first frame we ever see is an IDR.
    bool discontinuity = true;

    // Decode-failure repair state — see the block at the bottom of the loop.
    uint64_t lastErrors = 0;
    auto lastIdrRequest = std::chrono::steady_clock::now() - std::chrono::seconds(10);

    while (m_running.load(std::memory_order_acquire)) {
        const int32_t got = echo_fill_buffer(
            m_handle, buffer.data(), static_cast<int32_t>(buffer.size()), meta, 100);

        if (got == FILL_TIMEOUT) continue;
        if (got == FILL_ENDED || got == FILL_BAD_HANDLE) break;

        if (got == FILL_TOO_SMALL) {
            const auto needed = static_cast<size_t>(meta[0]);
            if (needed > static_cast<size_t>(kMaxFrameBuffer)) break;
            buffer.resize(needed);
            discontinuity = true;   // that frame is gone; the chain has a hole
            continue;
        }
        if (got <= 0) continue;

        const bool keyframe = (meta[1] & 1) != 0;
        if (decoder && !decoder->Submit(buffer.data(), static_cast<uint32_t>(got),
                                        keyframe, discontinuity, meta[2])) {
            discontinuity = true;
            continue;
        }
        discontinuity = false;
        m_framesFed.fetch_add(1, std::memory_order_relaxed);

        // ── A decode error has to reach the host, or it is permanent ────────
        //
        // `echo-client` has two keyframe gates and both work, but they guard
        // what ARRIVES: the session's first frame, and frames after a queue
        // drop. Neither can see a frame that arrived intact and then failed to
        // DECODE — a surface the decoder could not obtain, a reference it no
        // longer holds. Nothing upstream knows anything went wrong.
        //
        // On a moving picture that heals itself as blocks get intra-coded. On
        // a STATIC desktop it does not, and that is the case that produced a
        // grey screen with a mouse trail cut through it: the host had dropped
        // to the 5 fps keep-alive, was sending duplicate P-frames, and the only
        // regions ever repainted were the ones the cursor moved over. Measured
        // live on 2026-09-08 — 72 invalidations early in the session, ZERO for
        // the several minutes it then sat broken. Nothing was going to fix it.
        //
        // So: ask. Rate-limited to one request every two seconds, because the
        // opposite failure is on record too — the Android MediaCodec wedge sent
        // 198 keyframe requests in 59 seconds and buried the host.
        if (decoder) {
            const uint64_t errors = decoder->DecodeErrors();
            if (errors != lastErrors) {
                lastErrors = errors;
                const auto now = std::chrono::steady_clock::now();
                if (now - lastIdrRequest > std::chrono::seconds(2)) {
                    lastIdrRequest = now;
                    echo_request_idr(m_handle);
                    m_idrRequests.fetch_add(1, std::memory_order_relaxed);
                }
            }
        }
    }
}

// ── Uplink ──────────────────────────────────────────────────────────────────
//
// `m_handle` is read without a lock, deliberately. `Close` zeroes it before it
// calls `echo_close`, so the worst a racing input thread can do is pass a
// handle that has just been closed — and the bridge answers that with `false`
// rather than faulting. That guarantee is what makes a lock unnecessary on a
// path that runs 250 times a second.

bool EchoSession::SendInput(int32_t kind, int32_t a, int32_t b, int32_t c,
                            int32_t d) noexcept {
    const uint64_t handle = m_handle;
    return handle != 0 && echo_send_input(handle, kind, a, b, c, d);
}

bool EchoSession::SendGamepad(int32_t slot, int32_t activeMask, int32_t buttons,
                              int32_t leftTrigger, int32_t rightTrigger,
                              int32_t leftX, int32_t leftY,
                              int32_t rightX, int32_t rightY) noexcept {
    const uint64_t handle = m_handle;
    return handle != 0 &&
           echo_send_gamepad(handle, slot, activeMask, buttons, leftTrigger,
                             rightTrigger, leftX, leftY, rightX, rightY);
}

bool EchoSession::SetDisplay(uint32_t width, uint32_t height,
                             uint32_t refreshHz) noexcept {
    const uint64_t handle = m_handle;
    // HDR is not flipped from here: the encoder's dynamic range is fixed for a
    // session's life, and the Worker says so rather than pretending otherwise.
    return handle != 0 && echo_set_display(handle, width, height, refreshHz, false);
}

std::string EchoSession::Stats() const noexcept {
    if (!m_handle) return {};
    std::array<char, 2048> buffer{};
    const int32_t code = echo_stats(m_handle, buffer.data(), static_cast<int32_t>(buffer.size()));
    if (code <= 0) return {};
    return std::string(buffer.data(), static_cast<size_t>(code));
}

void EchoSession::Close() noexcept {
    m_running.store(false, std::memory_order_release);

    // Close the handle FIRST: it wakes both threads out of their blocking
    // waits, because `echo_poll_event` and `echo_fill_buffer` each answer their
    // own "not a session" code rather than faulting — which is exactly what the
    // bridge guarantees for a handle torn down underneath a caller.
    const uint64_t handle = m_handle;
    m_handle = 0;
    if (handle) echo_close(handle);

    if (m_feed.joinable()) m_feed.join();
    if (m_pump.joinable()) m_pump.join();
}

void EchoSession::Detach() noexcept {
    m_running.store(false, std::memory_order_release);

    // Identical shape to Close, with one different bridge call. `echo_detach`
    // suppresses the goodbye rather than merely skipping the wait for it, so
    // nothing here has to race anything: by the time this returns the host has
    // been told nothing at all, and it reaches its own detach path when the
    // tunnel goes quiet.
    const uint64_t handle = m_handle;
    m_handle = 0;
    if (handle) echo_detach(handle);

    if (m_feed.joinable()) m_feed.join();
    if (m_pump.joinable()) m_pump.join();
}

}  // namespace echo
