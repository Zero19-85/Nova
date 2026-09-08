#include "pch.h"
#include "HostDiscovery.h"

#include <winrt/Windows.Foundation.Collections.h>

using namespace winrt;
using namespace Windows::Devices::Enumeration;
using namespace Windows::Foundation;
using namespace Windows::Foundation::Collections;

namespace echo {
namespace {

// The DNS-SD protocol id. Discovery on UWP goes through DeviceInformation with
// this protocol rather than a raw multicast socket, which is what lets it work
// inside an app container at all — the platform holds the multicast
// membership, we just get told about services.
constexpr wchar_t kDnssdProtocol[] = L"{4526e8c1-8aac-4153-9b16-55e86ada0e54}";

// Nova's own service type. Deliberately NOT `_nvstream._tcp`: that record
// describes a GameStream host to every Moonlight client on the LAN, and Echo is
// a different protocol on a different port. One daemon, two registrations.
constexpr wchar_t kServiceName[] = L"_echo._tcp";

constexpr wchar_t kPropHostName[]   = L"System.Devices.Dnssd.HostName";
constexpr wchar_t kPropInstance[]   = L"System.Devices.Dnssd.InstanceName";
constexpr wchar_t kPropPort[]       = L"System.Devices.Dnssd.PortNumber";
constexpr wchar_t kPropTextAttrs[]  = L"System.Devices.Dnssd.TextAttributes";
constexpr wchar_t kPropIpAddress[]  = L"System.Devices.IpAddress";

// Pull one TXT record. They arrive as a string array of "key=value", exactly as
// the host wrote them.
std::wstring TxtValue(IInspectable const& attrs, std::wstring_view key) {
    auto array = attrs.try_as<IReferenceArray<hstring>>();
    if (!array) return {};
    for (auto const& entry : array.Value()) {
        std::wstring_view text{ entry };
        const auto eq = text.find(L'=');
        if (eq == std::wstring_view::npos) continue;
        if (text.substr(0, eq) == key) return std::wstring(text.substr(eq + 1));
    }
    return {};
}

std::wstring FirstString(IInspectable const& value) {
    if (!value) return {};
    if (auto array = value.try_as<IReferenceArray<hstring>>()) {
        for (auto const& entry : array.Value()) {
            std::wstring_view text{ entry };
            // Prefer IPv4: the transport cascade's LAN endpoint is written as
            // "a.b.c.d:port", and an IPv6 literal there would not parse.
            if (text.find(L':') == std::wstring_view::npos && !text.empty()) {
                return std::wstring(text);
            }
        }
        // Nothing IPv4-looking; take whatever there is rather than nothing.
        for (auto const& entry : array.Value()) {
            if (!entry.empty()) return std::wstring(entry);
        }
        return {};
    }
    if (auto single = value.try_as<IReference<hstring>>()) {
        return std::wstring(single.Value());
    }
    return {};
}

uint32_t AsUint32(IInspectable const& value) {
    if (!value) return 0;
    if (auto n = value.try_as<IReference<uint32_t>>()) return n.Value();
    if (auto n = value.try_as<IReference<int32_t>>())  return static_cast<uint32_t>(n.Value());
    if (auto n = value.try_as<IReference<uint16_t>>()) return n.Value();
    return 0;
}

DiscoveredHost FromDevice(IMapView<hstring, IInspectable> const& props,
                          hstring const& id, hstring const& fallbackName) {
    DiscoveredHost host;
    host.id = id;

    const auto get = [&](wchar_t const* key) -> IInspectable {
        return props.HasKey(key) ? props.Lookup(key) : nullptr;
    };

    host.address = FirstString(get(kPropIpAddress));
    host.port    = AsUint32(get(kPropPort));

    if (auto attrs = get(kPropTextAttrs)) {
        host.name            = TxtValue(attrs, L"name");
        host.relayUrl        = TxtValue(attrs, L"relay");
        host.relayPin        = TxtValue(attrs, L"relaypin");
        host.fingerprintHint = TxtValue(attrs, L"fp");
    }

    if (host.name.empty()) {
        if (auto instance = get(kPropInstance); instance) host.name = FirstString(instance);
    }
    if (host.name.empty()) host.name = std::wstring(fallbackName);
    if (host.name.empty()) {
        if (auto hostName = get(kPropHostName); hostName) host.name = FirstString(hostName);
    }
    if (host.name.empty()) host.name = host.address;

    return host;
}

}  // namespace

HostDiscovery::~HostDiscovery() { Stop(); }

bool HostDiscovery::Start(Changed onChanged, std::wstring& error) noexcept {
    try {
        m_onChanged = std::move(onChanged);

        const hstring selector =
            L"System.Devices.AepService.ProtocolId:=\"" + hstring(kDnssdProtocol) + L"\"" +
            L" AND System.Devices.Dnssd.ServiceName:=\"" + hstring(kServiceName) + L"\"" +
            L" AND System.Devices.Dnssd.Domain:=\"local\"";

        // These have to be asked for by name; a DeviceInformation carries none
        // of them by default, and their absence looks like a host that
        // advertised nothing rather than a query that requested nothing.
        auto properties = single_threaded_vector<hstring>({
            kPropHostName, kPropInstance, kPropPort, kPropTextAttrs, kPropIpAddress,
        });

        m_watcher = DeviceInformation::CreateWatcher(
            selector, properties, DeviceInformationKind::AssociationEndpointService);

        m_added = m_watcher.Added([this](DeviceWatcher const&, DeviceInformation const& info) {
            Upsert(FromDevice(info.Properties(), info.Id(), info.Name()));
        });

        // Update carries only the changed properties, so a host that gains its
        // relay TXT later arrives here rather than in Added. Merging rather
        // than replacing keeps the address we already resolved.
        m_updated = m_watcher.Updated([this](DeviceWatcher const&, DeviceInformationUpdate const& update) {
            std::lock_guard<std::mutex> guard(m_lock);
            for (auto& host : m_hosts) {
                if (host.id != std::wstring_view(update.Id())) continue;
                auto props = update.Properties();
                auto merged = FromDevice(props, update.Id(), hstring(host.name));
                if (!merged.address.empty())         host.address = merged.address;
                if (merged.port != 0)                host.port = merged.port;
                if (!merged.relayUrl.empty())        host.relayUrl = merged.relayUrl;
                if (!merged.relayPin.empty())        host.relayPin = merged.relayPin;
                if (!merged.fingerprintHint.empty()) host.fingerprintHint = merged.fingerprintHint;
                break;
            }
            if (m_onChanged) m_onChanged();
        });

        m_removed = m_watcher.Removed([this](DeviceWatcher const&, DeviceInformationUpdate const& update) {
            {
                std::lock_guard<std::mutex> guard(m_lock);
                std::wstring_view gone{ update.Id() };
                m_hosts.erase(
                    std::remove_if(m_hosts.begin(), m_hosts.end(),
                                   [&](DiscoveredHost const& h) { return h.id == gone; }),
                    m_hosts.end());
            }
            if (m_onChanged) m_onChanged();
        });

        m_enumerated = m_watcher.EnumerationCompleted([this](DeviceWatcher const&, IInspectable const&) {
            // Not a stopping point: the watcher keeps running, so a host that
            // boots later still appears. The UI uses this only to stop saying
            // "searching".
            if (m_onChanged) m_onChanged();
        });

        m_watcher.Start();
        return true;
    } catch (hresult_error const& e) {
        error = std::wstring(L"discovery unavailable: ") + e.message().c_str();
        return false;
    } catch (...) {
        error = L"discovery unavailable";
        return false;
    }
}

void HostDiscovery::Upsert(DiscoveredHost host) noexcept {
    {
        std::lock_guard<std::mutex> guard(m_lock);
        bool replaced = false;
        for (auto& existing : m_hosts) {
            if (existing.id == host.id) { existing = std::move(host); replaced = true; break; }
        }
        if (!replaced) m_hosts.push_back(std::move(host));
    }
    if (m_onChanged) m_onChanged();
}

std::vector<DiscoveredHost> HostDiscovery::Hosts() const noexcept {
    std::lock_guard<std::mutex> guard(m_lock);
    return m_hosts;
}

void HostDiscovery::Stop() noexcept {
    try {
        if (!m_watcher) return;
        m_watcher.Added(m_added);
        m_watcher.Updated(m_updated);
        m_watcher.Removed(m_removed);
        m_watcher.EnumerationCompleted(m_enumerated);
        const auto status = m_watcher.Status();
        if (status == DeviceWatcherStatus::Started ||
            status == DeviceWatcherStatus::EnumerationCompleted) {
            m_watcher.Stop();
        }
        m_watcher = nullptr;
    } catch (...) {
        // Tearing down a watcher that already stopped is not worth a crash on
        // the way out of the app.
    }
}

}  // namespace echo
