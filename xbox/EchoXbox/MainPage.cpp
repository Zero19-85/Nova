#include "pch.h"
#include "MainPage.h"
#include "MainPage.g.cpp"

#include "EchoBridge.h"

#include <array>
#include <cmath>
#include <string>

using namespace winrt;
using namespace Windows::Foundation;
using namespace Windows::Storage;
using namespace Windows::UI::Core;
using namespace Windows::UI::Xaml;
using namespace Windows::UI::Xaml::Controls;

namespace winrt::EchoXbox::implementation
{
    namespace
    {
        constexpr int32_t kTextBuffer = 2048;

        // What we ask the console to output. An Xbox left to itself treats a
        // UWP app as a 1080p media player — see RequestBestHdmiMode.
        constexpr uint32_t kDesiredWidth  = 3840;
        constexpr uint32_t kDesiredHeight = 2160;

        constexpr uint32_t kStreamBitrate = 40000;   // the host budget caps this

        // ── What this console can DECODE, which is not what the TV can show ──
        //
        // Picking the frame rate from the panel refresh rate was wrong, and it
        // cost ten sessions of blank screen. A 120 Hz TV made this ask the host
        // for 3840x2160, the host obliged perfectly (VDD at 4K120, NVENC at
        // 4K120, frames on the wire), and the Xbox HEVC decoder produced nothing
        // at all from it. `Submit` succeeded, `ProcessOutput` never yielded a
        // picture, and the client asked for one keyframe and then sat quiet -
        // which is what a decoder that cannot decode the stream looks like from
        // the outside. Nothing failed loudly anywhere.
        //
        // The console decodes HEVC to about 4K60. So the frame rate comes from
        // a PIXEL RATE budget, and the display refresh only ever caps it:
        //
        //     4K120  995M px/s  - over budget, refused
        //     4K60   498M px/s  - fine
        //     1440p120 442M    - fine
        //     1080p120 249M    - fine
        //
        // Deliberately a budget rather than a table of known-good modes: the
        // overlay lets a user pick any resolution, and every combination has to
        // land somewhere sensible without being enumerated here.
        constexpr uint64_t kMaxDecodePixelRate = 520ull * 1000 * 1000;

        // Highest frame rate this console should be asked to decode at a given
        // size, never above what the panel can show.
        uint32_t DecodableFps(uint32_t width, uint32_t height, uint32_t panelFps)
        {
            const uint64_t pixels =
                static_cast<uint64_t>(width) * static_cast<uint64_t>(height);
            if (pixels == 0) return 60;
            const uint64_t affordable = kMaxDecodePixelRate / pixels;
            uint32_t fps = panelFps >= 100 ? 120u : 60u;
            if (affordable < fps) fps = 60;
            if (affordable < 60) fps = 30;   // nothing we offer reaches here
            return fps;
        }

        // App 5, "Virtual Desktop" — NOT app 1.
        //
        // `app_launcher::uses_virtual_display` early-returns false for app 1
        // (Desktop), which is why a Desktop session mirrors the physical
        // monitor no matter what `headless_for_all_apps` says. App 5 is the
        // headless desktop: it launches no process at all, and it is what makes
        // the Worker activate the Virtual Display Driver and hand this session
        // its own monitor at the size we asked for.
        constexpr uint32_t kAppVirtualDesktop = 5;

        struct ModeChoice { uint32_t width; uint32_t height; wchar_t const* label; };
        constexpr std::array<ModeChoice, 3> kModes{{
            { 3840, 2160, L"4K" },
            { 2560, 1440, L"1440p" },
            { 1920, 1080, L"1080p" },
        }};
        constexpr std::array<uint32_t, 2> kFrameRates{ 60, 120 };

        hstring Describe(std::array<char, kTextBuffer>& buffer, int32_t code)
        {
            if (code >= 0) return to_hstring(std::string(buffer.data()));
            std::array<char, kTextBuffer> reason{};
            if (echo_last_error(reason.data(), kTextBuffer) > 0) {
                return to_hstring("error " + std::to_string(code) + ": " + std::string(reason.data()));
            }
            return to_hstring("error " + std::to_string(code));
        }
    }

    MainPage::MainPage()
    {
        InitializeComponent();
        BuildOverlayChoices();
        StartupAsync();
    }

    MainPage::~MainPage()
    {
        // Order matters: the input threads call into the session, the session's
        // feed thread calls into the decoder, and the renderer's present thread
        // pulls from it. Stop producers first, working down the chain.
        if (m_input)     m_input->Stop();
        if (m_session)   m_session->Close();
        if (m_discovery) m_discovery->Stop();
        if (m_renderer)  m_renderer->Shutdown();
        if (m_decoder)   m_decoder->Shutdown();
    }

    // ── Small UI helpers, safe from any thread ──────────────────────────────

    void MainPage::Append(hstring const& line)
    {
        auto text = std::wstring(line);
        Dispatcher().RunAsync(CoreDispatcherPriority::Normal, [this, text]() {
            m_log = m_log.empty() ? text : m_log + L"\n" + text;
            // A session emits a lot; keep the tail rather than growing forever.
            if (m_log.size() > 8000) m_log.erase(0, m_log.size() - 8000);
            LogText().Text(hstring(m_log));
            LogScroller().ChangeView(nullptr, LogScroller().ScrollableHeight(), nullptr);
        });
    }

    void MainPage::SetStatus(hstring const& text)
    {
        auto copy = std::wstring(text);
        Dispatcher().RunAsync(CoreDispatcherPriority::Normal, [this, copy]() {
            StatusLine().Text(hstring(copy));
        });
    }

    // ── Startup ─────────────────────────────────────────────────────────────

    IAsyncAction MainPage::StartupAsync()
    {
        auto lifetime = get_strong();

        const auto localFolder = ApplicationData::Current().LocalFolder().Path();
        m_stateDir = to_string(localFolder);

        Append(L"LocalState: " + localFolder);
        co_await winrt::resume_background();

        // 1. The bridge still works inside the container.
        struct Stage { uint32_t index; const char* arg; const wchar_t* label; };
        const std::array<Stage, 5> stages{{
            { 0, nullptr,             L"0 abi       " },
            { 1, "echo",              L"1 heap/crt  " },
            { 2, m_stateDir.c_str(),  L"2 filesystem" },
            { 3, nullptr,             L"3 udp socket" },
            { 4, m_stateDir.c_str(),  L"4 identity  " },
        }};

        for (auto const& stage : stages) {
            std::array<char, kTextBuffer> buffer{};
            const int32_t code = echo_probe(stage.index, stage.arg, buffer.data(), kTextBuffer);
            Append(hstring(stage.label) + L"  " + Describe(buffer, code));
            if (code < 0) {
                SetStatus(L"bridge self-test failed — cannot continue");
                co_return;
            }
        }

        // 2. Take the display back from the shell. Must happen BEFORE the swap
        //    chain exists: the request is what makes the composition scale
        //    report 2.0, and the swap chain is sized from that.
        SetStatus(L"requesting 4K output…");
        const auto hdmi = echo::RequestBestHdmiMode(kDesiredWidth, kDesiredHeight);
        Append(L"\nhdmi        " + hstring(hdmi.note));

        // 3. What we ask the HOST to encode is the console's real output size —
        //    read back from the console, not inferred from the request above and
        //    NOT taken from the swap chain.
        //
        //    The swap chain was the wrong source and it is why the stream was
        //    stuck at 1080p: an Xbox composes a XAML app's surface at 1920x1080
        //    whatever the TV is doing, so sizing the request from the back
        //    buffer asked a 4K console for a 1080p desktop and then upscaled it.
        //    The virtual monitor the host spawns should be the size of the
        //    SCREEN, and the renderer scales into whatever surface XAML gives
        //    us — which it was already doing for free.
        if (hdmi.width >= 1280 && hdmi.height >= 720) {
            m_outputWidth = hdmi.width;
            m_outputHeight = hdmi.height;
            m_streamWidth = hdmi.width;
            m_streamHeight = hdmi.height;
            m_streamFps = DecodableFps(hdmi.width, hdmi.height,
                                       static_cast<uint32_t>(hdmi.refreshHz + 0.5));
            wchar_t line[160]{};
            swprintf_s(line, L"output      %ux%u @ %.0f Hz — asking the host for this",
                       hdmi.width, hdmi.height, hdmi.refreshHz);
            Append(hstring(line));
        } else {
            Append(L"output      unknown — falling back to the swap chain's size");
        }

        // 4. Renderer, decoder, input, discovery.
        co_await winrt::resume_foreground(Dispatcher());
        if (!StartRenderer()) co_return;
        StartInput();
        StartDiscovery();
        StartStatsTimer();
    }

    bool MainPage::StartRenderer()
    {
        m_renderer = std::make_unique<echo::VideoRenderer>();
        const auto facts = m_renderer->Initialize(VideoPanel());

        std::wstring report;
        wchar_t line[160]{};
        swprintf_s(line, L"panel       %ux%u logical, scale %.2f",
                   facts.panelWidth, facts.panelHeight, facts.scaleX);
        report += line;
        report += L"\nbackbuffer  " + std::to_wstring(facts.backBufferWidth) + L"x" +
                  std::to_wstring(facts.backBufferHeight) + L" real pixels";
        report += L"\ngpu         " + facts.adapter;

        // Only a fallback. The console's HDMI output decides the stream size
        // (see StartupAsync); this covers a PC, where there is no HDMI mode to
        // read and the window really is the screen.
        if (m_outputWidth == 0 && m_outputHeight == 0 &&
            facts.backBufferWidth > 0 && facts.backBufferHeight > 0) {
            m_streamWidth  = facts.backBufferWidth;
            m_streamHeight = facts.backBufferHeight;
        }
        if (!facts.note.empty()) report += L"\nnote        " + facts.note;

        if (!facts.ok) {
            Append(hstring(report + L"\n\nRENDERER FAILED"));
            SetStatus(L"renderer failed");
            return false;
        }

        // The decoder shares the renderer's device, so decoded surfaces never
        // leave the GPU. Sized to the back buffer as a first guess; the MFT
        // renegotiates to whatever the stream actually carries — which is also
        // what makes a live resolution change work at all.
        m_decoder = std::make_unique<echo::HevcDecoder>();
        std::wstring decoderError;
        if (m_decoder->Initialize(m_renderer->Device(),
                                  facts.backBufferWidth, facts.backBufferHeight,
                                  decoderError)) {
            m_decoderReady = true;
            report += std::wstring(L"\ndecoder     ") + m_decoder->Name() +
                      (m_decoder->IsHardware() ? L"  [hardware]"
                                               : L"  [SOFTWARE - will not keep up]");
            auto* decoder = m_decoder.get();
            m_renderer->SetFrameSource(
                [decoder](com_ptr<ID3D11Texture2D>& texture, uint32_t& subresource) {
                    return decoder->TryGetFrame(texture, subresource);
                });
        } else {
            report += L"\ndecoder     FAILED: " + decoderError;
        }
        Append(hstring(report));

        auto resize = [this](auto&&...) {
            if (!m_renderer) return;
            auto panel = VideoPanel();
            m_renderer->Resize(
                static_cast<uint32_t>(std::lround(panel.ActualWidth())),
                static_cast<uint32_t>(std::lround(panel.ActualHeight())),
                panel.CompositionScaleX(), panel.CompositionScaleY());
            // The input thread cannot read ActualWidth, so it is pushed.
            if (m_input) {
                m_input->SetSurfaceSize(static_cast<float>(panel.ActualWidth()),
                                        static_cast<float>(panel.ActualHeight()));
            }
        };
        VideoPanel().SizeChanged(resize);
        VideoPanel().CompositionScaleChanged(resize);

        m_session = std::make_unique<echo::EchoSession>();
        return true;
    }

    // ── Input ───────────────────────────────────────────────────────────────

    void MainPage::StartInput()
    {
        m_input = std::make_unique<echo::InputBridge>();

        auto* session = m_session.get();
        m_input->SetSinks(
            [session](echo::InputEvent const& event) {
                session->SendInput(event.kind, event.a, event.b, event.c, event.d);
            },
            [session](int32_t slot, int32_t activeMask, echo::PadState const& pad) {
                session->SendGamepad(slot, activeMask, pad.buttons, pad.leftTrigger,
                                     pad.rightTrigger, pad.leftX, pad.leftY,
                                     pad.rightX, pad.rightY);
            },
            [this] {
                // Fired from the pad thread. Everything it touches is XAML.
                Dispatcher().RunAsync(CoreDispatcherPriority::Normal, [this] {
                    if (m_overlayOpen) HideOverlay(); else ShowOverlay();
                });
            });

        std::wstring error;
        if (!m_input->Start(VideoPanel(), Window::Current().CoreWindow(), error)) {
            Append(L"input       " + hstring(error));
            return;
        }
        auto panel = VideoPanel();
        m_input->SetSurfaceSize(static_cast<float>(panel.ActualWidth()),
                                static_cast<float>(panel.ActualHeight()));
        Append(L"input       core-independent pointer + 250 Hz pad, hold VIEW for the overlay");
    }

    void MainPage::StartDiscovery()
    {
        m_discovery = std::make_unique<echo::HostDiscovery>();
        std::wstring error;

        // The watcher's callbacks arrive on a platform thread, so everything
        // they touch has to hop to the dispatcher first.
        const bool started = m_discovery->Start(
            [this] {
                Dispatcher().RunAsync(CoreDispatcherPriority::Normal,
                                      [this] { RefreshHostList(); });
            },
            error);

        if (!started) {
            Append(L"discovery   " + hstring(error));
            HostsHeader().Text(L"Discovery unavailable");
            SetStatus(L"discovery unavailable — see the log");
            return;
        }
        SetStatus(L"searching for Nova on the network…");
    }

    void MainPage::OnRescanClick(IInspectable const&, RoutedEventArgs const&)
    {
        if (m_discovery) m_discovery->Stop();
        HostList().Items().Clear();
        m_hosts.clear();
        m_selected = -1;
        UpdateButtons();
        StartDiscovery();
    }

    void MainPage::RefreshHostList()
    {
        if (!m_discovery) return;

        // Remember what was selected across a refresh: the watcher fires on
        // every TXT update, and a list that reset its selection each time would
        // be unusable while hosts are still settling.
        std::wstring selectedId;
        if (m_selected >= 0 && m_selected < static_cast<int>(m_hosts.size())) {
            selectedId = m_hosts[m_selected].id;
        }

        m_hosts = m_discovery->Hosts();
        HostList().Items().Clear();

        int restore = -1;
        for (size_t i = 0; i < m_hosts.size(); ++i) {
            auto const& host = m_hosts[i];
            const auto stored = echo::LoadHostFingerprint(m_stateDir, host.address);

            std::wstring badge = stored.empty() ? L"NOT PAIRED" : L"PAIRED";
            badge += L"  ·  ONLINE // LAN";
            if (host.relayUrl.empty()) badge += L"  ·  no relay advertised";

            StackPanel row;
            TextBlock name;
            name.Text(hstring(host.name));
            name.FontSize(26);
            name.Foreground(Media::SolidColorBrush(Windows::UI::ColorHelper::FromArgb(255, 232, 237, 242)));
            TextBlock address;
            address.Text(hstring(host.address));
            address.FontSize(18);
            address.Foreground(Media::SolidColorBrush(Windows::UI::ColorHelper::FromArgb(255, 138, 148, 166)));
            TextBlock state;
            state.Text(hstring(badge));
            state.FontSize(15);
            state.Foreground(stored.empty()
                ? Media::SolidColorBrush(Windows::UI::ColorHelper::FromArgb(255, 138, 148, 166))
                : Media::SolidColorBrush(Windows::UI::ColorHelper::FromArgb(255, 0, 229, 255)));
            row.Margin(ThicknessHelper::FromLengths(4, 10, 4, 10));
            row.Children().Append(name);
            row.Children().Append(address);
            row.Children().Append(state);
            HostList().Items().Append(row);

            if (!selectedId.empty() && host.id == selectedId) restore = static_cast<int>(i);
        }

        HostsHeader().Text(m_hosts.empty()
            ? hstring(L"Searching the network…")
            : hstring(L"Found " + std::to_wstring(m_hosts.size()) +
                      (m_hosts.size() == 1 ? L" host" : L" hosts")));

        if (restore >= 0) {
            HostList().SelectedIndex(restore);
        } else if (m_selected < 0 && !m_hosts.empty()) {
            HostList().SelectedIndex(0);   // one host is the common case
        }
        UpdateButtons();
    }

    void MainPage::OnHostSelectionChanged(IInspectable const&, SelectionChangedEventArgs const&)
    {
        m_selected = HostList().SelectedIndex();
        UpdateButtons();
    }

    void MainPage::UpdateButtons()
    {
        const bool valid = m_selected >= 0 && m_selected < static_cast<int>(m_hosts.size());
        if (!valid) {
            PairButton().IsEnabled(false);
            StreamButton().IsEnabled(false);
            return;
        }
        auto const& host = m_hosts[m_selected];
        const auto stored = echo::LoadHostFingerprint(m_stateDir, host.address);

        // Pairing only needs an address. Streaming needs the fingerprint the
        // handshake earned, plus a relay the host advertised.
        PairButton().IsEnabled(!host.address.empty() && !m_streaming);
        StreamButton().IsEnabled(!stored.empty() && host.Usable() &&
                                 m_decoderReady && !m_streaming);
    }

    // ── Pair ────────────────────────────────────────────────────────────────

    void MainPage::OnPairClick(IInspectable const&, RoutedEventArgs const&)
    {
        if (m_selected < 0 || m_selected >= static_cast<int>(m_hosts.size())) return;
        auto const& host = m_hosts[m_selected];

        m_session->Close();
        m_pairing = true;

        const auto config = echo::BuildPairConfig(m_stateDir, host);
        std::wstring error;
        if (m_session->Pair(config, [this](std::string const& json) { OnSessionEvent(json); }, error)) {
            SetStatus(L"pairing with " + hstring(host.name) + L"…");
        } else {
            m_pairing = false;
            SetStatus(hstring(error));
        }
    }

    // ── Stream ──────────────────────────────────────────────────────────────

    void MainPage::OnStreamClick(IInspectable const&, RoutedEventArgs const&)
    {
        BeginStream();
    }

    void MainPage::BeginStream()
    {
        if (m_selected < 0 || m_selected >= static_cast<int>(m_hosts.size())) return;
        if (!m_decoderReady) { SetStatus(L"no decoder — cannot stream"); return; }

        auto const& host = m_hosts[m_selected];

        // The escape hatch: a hand-written echo.json overrides everything.
        // Only needed if discovery cannot see the relay fields; the native path
        // never writes this file.
        auto config = echo::LoadConfigOverride(m_stateDir);
        if (!config.empty()) {
            Append(L"config      using echo.json override");
        } else {
            const auto fingerprint = echo::LoadHostFingerprint(m_stateDir, host.address);
            if (fingerprint.empty()) { SetStatus(L"pair with this host first"); return; }
            if (!host.Usable())      { SetStatus(L"host advertised no relay — cannot stream"); return; }

            const std::string res = std::to_string(m_streamWidth) + "x" +
                                    std::to_string(m_streamHeight);
            Append(L"stream      " + to_hstring(res) + L" @ " +
                   std::to_wstring(m_streamFps) + L", app 5 (headless)");
            config = echo::BuildConnectConfig(m_stateDir, host, fingerprint,
                                              res, m_streamFps, kStreamBitrate,
                                              kAppVirtualDesktop);
        }

        m_session->Close();
        m_pairing = false;

        std::wstring error;
        if (m_session->Connect(config,
                               [this](std::string const& json) { OnSessionEvent(json); },
                               m_decoder.get(), error)) {
            SetStatus(L"connecting to " + hstring(host.name) + L"…");
        } else {
            SetStatus(hstring(error));
        }
    }

    void MainPage::OnStopClick(IInspectable const&, RoutedEventArgs const&)
    {
        if (m_session) m_session->Close();
        m_pairing = false;
        // Otherwise the last frame of the dead stream stays on the TV behind
        // the host picker, which reads as a frozen stream rather than a
        // finished one.
        if (m_renderer) m_renderer->ShowBackground();
        LeaveStreamingUi();
        SetStatus(L"disconnected");
    }

    // ── The overlay ─────────────────────────────────────────────────────────

    void MainPage::BuildOverlayChoices()
    {
        for (auto const& mode : kModes) {
            Button button;
            button.Content(box_value(hstring(mode.label)));
            button.FontSize(20);
            button.Padding(ThicknessHelper::FromLengths(22, 8, 22, 8));
            const uint32_t w = mode.width, h = mode.height;
            button.Click([this, w, h](auto&&...) { ApplyStreamMode(w, h, m_streamFps); });
            ResolutionRow().Children().Append(button);
        }
        for (uint32_t fps : kFrameRates) {
            Button button;
            button.Content(box_value(to_hstring(fps) + L" fps"));
            button.FontSize(20);
            button.Padding(ThicknessHelper::FromLengths(22, 8, 22, 8));
            button.Click([this, fps](auto&&...) {
                ApplyStreamMode(m_streamWidth, m_streamHeight, fps);
            });
            FrameRateRow().Children().Append(button);
        }
    }

    void MainPage::ApplyStreamMode(uint32_t width, uint32_t height, uint32_t fps)
    {
        // The same budget the startup path uses. Picking 4K and then 120 from
        // the overlay must not be able to reproduce the blank screen by hand.
        const uint32_t capped = DecodableFps(width, height, fps);
        if (capped != fps) {
            Append(L"display     " + to_hstring(fps) + L" fps is beyond this console at " +
                   to_hstring(width) + L"x" + to_hstring(height) + L" - using " +
                   to_hstring(capped));
        }
        fps = capped;

        m_streamWidth = width;
        m_streamHeight = height;
        m_streamFps = fps;

        if (!m_streaming || !m_session) {
            OverlaySubtitle().Text(L"saved — the next stream starts at " +
                                   to_hstring(width) + L"x" + to_hstring(height));
            return;
        }

        // Live. The host re-modes its virtual display in place and rebuilds its
        // encoder; the new geometry reaches this decoder as a stream change,
        // which the MFT answers by renegotiating its output type. Nothing
        // restarts — not the session, not the desktop, not what is running on it.
        if (m_session->SetDisplay(width, height, fps)) {
            OverlaySubtitle().Text(L"asking the host for " + to_hstring(width) + L"x" +
                                   to_hstring(height) + L" @ " + to_hstring(fps) + L" Hz…");
            Append(L"display     requested " + to_hstring(width) + L"x" + to_hstring(height) +
                   L"@" + to_hstring(fps) + L"Hz live");
        } else {
            OverlaySubtitle().Text(L"no live session to re-mode");
        }
    }

    void MainPage::ShowOverlay()
    {
        m_overlayOpen = true;
        // Park input FIRST. Otherwise the button press that dismisses this
        // panel also travels to the PC, and the A that opened a menu here opens
        // something over there too.
        if (m_input) m_input->SetForwarding(false);
        std::wstring subtitle = m_streaming
            ? L"streaming " + std::to_wstring(m_streamWidth) + L"x" +
              std::to_wstring(m_streamHeight) + L" @ " +
              std::to_wstring(m_streamFps) + L" fps"
            : std::wstring(L"not streaming");
        // The chord is the only way in or out of mouse mode, so the state has to
        // be visible somewhere or a user who toggled it by accident has no way
        // to find out why their controller stopped reaching the game.
        if (m_input && m_input->MouseMode()) {
            subtitle += L"   ·   MOUSE MODE (Menu+View to leave)";
        }
        OverlaySubtitle().Text(hstring(subtitle));
        OverlayRoot().Visibility(Visibility::Visible);
        ResumeButton().Focus(FocusState::Programmatic);
    }

    void MainPage::HideOverlay()
    {
        m_overlayOpen = false;
        OverlayRoot().Visibility(Visibility::Collapsed);
        DiagnosticsText().Visibility(Visibility::Collapsed);
        if (m_input) m_input->SetForwarding(m_streaming);
    }

    void MainPage::OnSettingsClick(IInspectable const&, RoutedEventArgs const&)
    {
        ShowOverlay();
    }

    void MainPage::OnResumeClick(IInspectable const&, RoutedEventArgs const&)
    {
        HideOverlay();
    }

    void MainPage::OnDiagnosticsClick(IInspectable const&, RoutedEventArgs const&)
    {
        const bool visible = DiagnosticsText().Visibility() == Visibility::Visible;
        DiagnosticsText().Visibility(visible ? Visibility::Collapsed : Visibility::Visible);
    }

    // ── Events ──────────────────────────────────────────────────────────────

    void MainPage::OnSessionEvent(std::string const& json)
    {
        const auto type = echo::JsonField(json, "type");

        // Surface every event: this is the same JSON the CLI and the Android
        // client print, so a failure here can be compared line for line with
        // the host's nova-service.log.
        Append(L"· " + to_hstring(json));

        if (type == "awaiting_consent") {
            const auto pin = to_hstring(echo::JsonField(json, "pin"));
            Dispatcher().RunAsync(CoreDispatcherPriority::Normal, [this, pin]() {
                PinText().Text(pin);
                PinPanel().Visibility(Visibility::Visible);
            });
            SetStatus(L"waiting for the PIN to be entered on the PC");
            return;
        }

        // `paired` and `verified` both carry the HOST's certificate
        // fingerprint — the value `host_fingerprint` must hold, and the only
        // trustworthy source for it. Storing it here is what makes STREAM work
        // on the next launch without pairing again.
        if (type == "paired" || type == "verified") {
            const auto fingerprint = echo::JsonField(json, "fingerprint");
            if (!fingerprint.empty() && m_selected >= 0 &&
                m_selected < static_cast<int>(m_hosts.size())) {
                auto const& host = m_hosts[m_selected];
                echo::SaveHostFingerprint(m_stateDir, host.address, host.name, fingerprint);
                Append(L"paired      host fingerprint stored");
            }
            return;
        }

        if (type == "error") {
            m_pairing = false;
            SetStatus(L"error: " + to_hstring(echo::JsonField(json, "message")));
            Dispatcher().RunAsync(CoreDispatcherPriority::Normal, [this]() {
                PinPanel().Visibility(Visibility::Collapsed);
                LeaveStreamingUi();
                UpdateButtons();
            });
            return;
        }

        // ── The handoff ─────────────────────────────────────────────────────
        // A pairing session ends with `closed` like any other. If it got as far
        // as storing a fingerprint, go straight into a stream rather than
        // making somebody press a second button for a step that always follows.
        if (type == "closed") {
            const bool wasPairing = m_pairing;
            m_pairing = false;
            Dispatcher().RunAsync(CoreDispatcherPriority::Normal, [this, wasPairing]() {
                PinPanel().Visibility(Visibility::Collapsed);
                UpdateButtons();
                if (wasPairing && StreamButton().IsEnabled()) {
                    SetStatus(L"paired — starting the stream");
                    BeginStream();
                } else if (!wasPairing) {
                    LeaveStreamingUi();
                    SetStatus(L"session closed");
                }
            });
            return;
        }

        // Anything that means video is on its way. `streaming` is the host's
        // grant; the UI switches then rather than on the first frame, so a
        // stream that connects but never decodes is still visibly distinct
        // from one that never connected.
        if (type == "streaming" || type == "granted") {
            Dispatcher().RunAsync(CoreDispatcherPriority::Normal, [this]() {
                EnterStreamingUi();
            });
            SetStatus(L"streaming");
        }
    }

    // ── UI modes ────────────────────────────────────────────────────────────

    void MainPage::EnterStreamingUi()
    {
        m_streaming = true;
        SetupRoot().Visibility(Visibility::Collapsed);
        HideOverlay();                       // also turns forwarding on
        UpdateButtons();
    }

    void MainPage::LeaveStreamingUi()
    {
        m_streaming = false;
        if (m_input) m_input->SetForwarding(false);
        m_overlayOpen = false;
        OverlayRoot().Visibility(Visibility::Collapsed);
        SetupRoot().Visibility(Visibility::Visible);
        NoVideoPanel().Visibility(Visibility::Collapsed);
        UpdateButtons();
    }

    void MainPage::StartStatsTimer()
    {
        // Each number is a different stage, so the FIRST zero names the culprit:
        // fed=0 is the network, decoded=0 with fed>0 is the decoder, drawn=0
        // with decoded>0 is the colour conversion.
        //
        // This used to be a strip pinned over the picture. It is now something
        // you ask for, from the overlay — the numbers are as useful as they ever
        // were and a stream should look like a stream.
        m_statsTimer = DispatcherTimer();
        m_statsTimer.Interval(std::chrono::seconds(1));
        m_statsTimer.Tick([this](auto&&...) {
            if (!m_renderer) return;
            const bool showing = DiagnosticsText().Visibility() == Visibility::Visible;

            const uint64_t fed     = m_session ? m_session->FramesFed() : 0;
            const uint64_t decoded = m_decoder ? m_decoder->DecodedFrames() : 0;
            const uint64_t drawn   = m_renderer->BlittedFrames();

            // `dropped` is the one to watch for lag. A slow climb is this client
            // refusing to fall behind and is exactly right; a number rising as
            // fast as `decoded` means the panel cannot show the frame rate
            // being negotiated, and the fix is to ask for fewer frames rather
            // than to throw half of them away.
            const uint64_t dropped = m_decoder ? m_decoder->DroppedFrames() : 0;
            std::wstring line =
                L"fed " + std::to_wstring(fed) +
                L"   decoded " + std::to_wstring(decoded) +
                L"   drawn " + std::to_wstring(drawn) +
                L"   dropped " + std::to_wstring(dropped);
            // `drawn` climbing while nothing is on screen used to be the only
            // clue that presenting had stopped, and it is not much of one.
            // `presented` is the one counter that answers "is the present loop
            // still alive" - which is exactly what two rounds of blank screen needed
            // and nobody could see.
            line += L"\npresented " + std::to_wstring(m_renderer->PresentedFrames()) +
                    L"   sync " + std::to_wstring(m_renderer->SyncInterval());
            if (const uint32_t hr = m_renderer->LastPresentError()) {
                wchar_t code[32]{};
                swprintf_s(code, L"  last error 0x%08X", hr);
                line += code;
            }
            if (showing) DiagnosticsText().Text(hstring(line));
            if (!m_streaming) StatusLine().Text(hstring(line));

            // ── Say why there is no picture ─────────────────────────────
            //
            // The first zero names the stage that is stuck, and the panel says
            // so in words rather than leaving somebody to infer it from three
            // counters. It disappears the moment a frame is drawn.
            if (m_streaming && drawn == 0) {
                std::wstring why;
                if (fed == 0) {
                    why = L"No frames are arriving.\n"
                          L"The session is up but the video path is carrying nothing.";
                } else if (decoded == 0) {
                    why = L"Frames are arriving but the decoder produces nothing.\n"
                          L"This console cannot decode the stream it was sent.\n"
                          L"Hold VIEW and pick a lower resolution or frame rate.";
                } else {
                    why = L"Decoding, but nothing reaches the screen.\n"
                          L"The colour conversion or the present path is stuck.";
                }
                NoVideoText().Text(hstring(why + L"\n\n" + line));
                NoVideoPanel().Visibility(Visibility::Visible);
            } else {
                NoVideoPanel().Visibility(Visibility::Collapsed);
            }
        });
        m_statsTimer.Start();
    }
}
