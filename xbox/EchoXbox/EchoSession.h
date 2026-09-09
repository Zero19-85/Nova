// One Echo session: the handle, the event pump, the thread that feeds decoded
// frames into Media Foundation — and the small store that remembers which
// hosts this console has paired with.
#pragma once

#include <cstdint>
#include <string>
#include <functional>
#include <thread>
#include <atomic>
#include <vector>

namespace echo {

class VideoDecoder;
struct DiscoveredHost;

// Every control-plane event, as the JSON the bridge emits. Called from the pump
// thread, never the UI thread.
using EventSink = std::function<void(std::string const&)>;

// ── What pairing earned ─────────────────────────────────────────────────────
//
// A host's certificate fingerprint, keyed by address. This is written ONLY from
// a completed PIN handshake — never from the `fp` an mDNS record advertises,
// because mDNS is unauthenticated and that field is what pins the host's
// identity for every later session. See HostDiscovery.h.
std::string LoadHostFingerprint(std::string const& stateDir,
                                std::wstring const& address) noexcept;
void SaveHostFingerprint(std::string const& stateDir, std::wstring const& address,
                         std::wstring const& name, std::string const& fingerprint) noexcept;

// Build the JSON `echo_pair` / `echo_connect` take. `identityDir` is
// LocalState — it holds this console's private key.
std::string BuildPairConfig(std::string const& identityDir,
                            DiscoveredHost const& host) noexcept;
std::string BuildConnectConfig(std::string const& identityDir,
                               DiscoveredHost const& host,
                               std::string const& hostFingerprint,
                               std::string const& res, uint32_t fps,
                               uint32_t bitrateKbps, uint32_t appId) noexcept;

// Escape hatch: if `echo.json` exists in LocalState it is used verbatim for
// STREAM, bypassing discovery entirely. Only needed if DNS-SD turns out to be
// unavailable on a console — the native path never writes this file.
std::string LoadConfigOverride(std::string const& stateDir) noexcept;

// Pull one flat field out of an event's JSON. Empty if absent — these come off
// the wire, so nothing here may throw on a shape it did not expect.
std::string JsonField(std::string const& json, char const* key) noexcept;

class EchoSession {
public:
    ~EchoSession();

    // Both return immediately; progress arrives through `sink`.
    bool Pair(std::string const& configJson, EventSink sink, std::wstring& error) noexcept;
    bool Connect(std::string const& configJson, EventSink sink,
                 VideoDecoder* decoder, std::wstring& error) noexcept;

    // Uplink. All fire-and-forget, all safe on a closed session: the bridge
    // answers a dead handle with `false` rather than faulting, which is what
    // lets the input threads keep running through a disconnect.
    bool SendInput(int32_t kind, int32_t a, int32_t b, int32_t c, int32_t d) noexcept;
    bool SendGamepad(int32_t slot, int32_t activeMask, int32_t buttons,
                     int32_t leftTrigger, int32_t rightTrigger,
                     int32_t leftX, int32_t leftY,
                     int32_t rightX, int32_t rightY) noexcept;

    // Ask the host to re-mode the display this session is watching, live.
    // See `echo_set_display` in EchoBridge.h for what makes that safe here.
    bool SetDisplay(uint32_t width, uint32_t height, uint32_t refreshHz) noexcept;

    void Close() noexcept;

    bool IsOpen() const noexcept { return m_handle != 0; }
    uint64_t FramesFed() const noexcept { return m_framesFed.load(std::memory_order_relaxed); }
    /// Keyframes asked for because the DECODER failed, not because a frame was
    /// lost. A steady climb here means the decode path is unhappy in a way the
    /// network counters cannot show.
    uint64_t IdrRequests() const noexcept { return m_idrRequests.load(std::memory_order_relaxed); }

    /// The shared core's receive-path counters as JSON. Empty when no session
    /// is open. See the note on the implementation.
    std::string Stats() const noexcept;

private:
    void PumpEvents(EventSink sink) noexcept;
    void FeedFrames(VideoDecoder* decoder) noexcept;

    uint64_t m_handle = 0;
    std::thread m_pump;
    std::thread m_feed;
    std::atomic<bool> m_running{false};
    std::atomic<uint64_t> m_framesFed{0};
    std::atomic<uint64_t> m_idrRequests{0};
};

}  // namespace echo
