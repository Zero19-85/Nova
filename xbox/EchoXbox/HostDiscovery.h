// Finding Nova on the LAN, the way the Android client does.
//
// The host advertises `_echo._tcp` (nova-server/src/echo/discovery.rs) with TXT
// records carrying its name, certificate fingerprint, and — only when both are
// configured — the signalling relay's URL and PIN.
//
// ── The security rule this file must not break ──────────────────────────────
//
// mDNS is unauthenticated: anything on the LAN can advertise `_echo._tcp` and
// claim any fingerprint. The advertised `fp` is therefore a **label**, never a
// credential. It is used for exactly one thing — comparing against a
// fingerprint we already learned from a completed PIN handshake, so the list
// can say "paired" next to a host. It must never be written into a session's
// `host_fingerprint`, because that field is what pins the host's identity: a
// value taken from mDNS would let anyone on the network be trusted as Nova.
//
// The Android client draws the same line deliberately. See HANDOFF_ECHO_LANDIRECT
// and the `echo-lan-discovery` note.
#pragma once

#include <cstdint>
#include <string>
#include <vector>
#include <functional>
#include <mutex>

#include <winrt/Windows.Devices.Enumeration.h>

namespace echo {

struct DiscoveredHost {
    std::wstring id;          // DeviceInformation.Id — stable, used for updates
    std::wstring name;        // TXT "name", else the mDNS instance name
    std::wstring address;     // IPv4 literal
    uint32_t     port = 0;
    std::wstring relayUrl;    // TXT "relay"      — absent means LAN-only host
    std::wstring relayPin;    // TXT "relaypin"
    std::wstring fingerprintHint;  // TXT "fp" — A LABEL. Read the header.

    bool Usable() const noexcept {
        return !address.empty() && !relayUrl.empty() && !relayPin.empty();
    }
};

class HostDiscovery {
public:
    using Changed = std::function<void()>;

    ~HostDiscovery();

    // Starts a DNS-SD watcher. Never throws; returns false with `error` set if
    // the platform refused, so the UI can fall back to a typed-in address
    // rather than looking broken.
    bool Start(Changed onChanged, std::wstring& error) noexcept;
    void Stop() noexcept;

    std::vector<DiscoveredHost> Hosts() const noexcept;

private:
    void Upsert(DiscoveredHost host) noexcept;

    winrt::Windows::Devices::Enumeration::DeviceWatcher m_watcher{ nullptr };
    winrt::event_token m_added, m_updated, m_removed, m_enumerated;

    mutable std::mutex m_lock;
    std::vector<DiscoveredHost> m_hosts;
    Changed m_onChanged;
};

}  // namespace echo
