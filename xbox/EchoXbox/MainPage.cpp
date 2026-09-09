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
        // So the frame rate comes from a PIXEL RATE budget, and the display
        // refresh only ever caps it:
        //
        //     4K120  995M px/s  - over budget, refused
        //     4K60   498M px/s  - fine
        //     1440p120 442M    - fine
        //     1080p120 249M    - fine
        //
        // Deliberately a budget rather than a table of known-good modes: the
        // overlay lets a user pick any resolution, and every combination has to
        // land somewhere sensible without being enumerated here.
        //
        // ── 520M is an INFERENCE, and it is no longer the only word ─────────
        //
        // What those ten sessions established is that 4K120 produced nothing.
        // What they did NOT establish is why — a decode block that cannot do
        // the pixel rate and a Media Foundation transform that declines to try
        // are indistinguishable from a sofa, and they point at completely
        // different work. 520M is the number somebody drew through that one
        // data point.
        //
        // It stays as the default, because a guess that refuses a mode is
        // recoverable and one that accepts an undecodable mode is a black
        // screen. But it is now overwritten by `MF_VIDEO_MAX_MB_PER_SEC` when
        // the decoder declares one (`AdoptProbedBudget`), and it can be lifted
        // outright from the overlay for a deliberate test — so the question
        // can be settled with one sideload instead of another rebuild.
        //
        // `g_budgetSource` exists so the diagnostics panel can say WHERE the
        // ceiling came from. "Refused because a number said so" is only useful
        // when you can see which number, and who wrote it.
        uint64_t    g_maxDecodePixelRate = 520ull * 1000 * 1000;
        const wchar_t* g_budgetSource = L"inferred (ten blank 4K120 sessions)";
        bool        g_budgetUnlocked = false;

        // Highest frame rate this console should be asked to decode at a given
        // size, never above what the panel can show.
        uint32_t DecodableFps(uint32_t width, uint32_t height, uint32_t panelFps)
        {
            const uint64_t pixels =
                static_cast<uint64_t>(width) * static_cast<uint64_t>(height);
            if (pixels == 0) return 60;
            // Unlocked: the panel is the only cap. This is the test path, and
            // the failure it can produce is the known one — a decoder that
            // eats frames and produces nothing, which NoVideoPanel now names
            // in words rather than leaving as a blank screen.
            if (g_budgetUnlocked) return panelFps >= 100 ? 120u : 60u;
            const uint64_t affordable = g_maxDecodePixelRate / pixels;
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
        //
        // What the host will launch. The ids are Nova's, and the ORDER here is
        // a recommendation: Virtual Desktop first because it is the only entry
        // that gets this console its own 4K monitor, and Mirror second because
        // it is the one people ask for by name and then find surprising.
        //
        // The note on each is not decoration. "Mirror" and "Virtual Desktop"
        // sound like the same thing and behave completely differently — app 1
        // shows the PC's real screen at the PC's real resolution, and app 5
        // spawns a display sized to this television. Somebody picking blind
        // will choose wrong roughly half the time.
        struct AppChoice { uint32_t id; wchar_t const* label; wchar_t const* note; };
        constexpr std::array<AppChoice, 5> kApps{{
            { 5, L"Virtual Desktop", L"its own headless monitor, sized to this TV" },
            { 1, L"Mirror",          L"the PC's physical screen, at the PC's resolution" },
            { 2, L"Steam",           L"Big Picture on a headless display" },
            { 3, L"Xbox",            L"Game Bar on a headless display" },
            { 4, L"RetroArch",       L"headless" },
        }};

        // How close two presses of A must be to mean "start it now" rather than
        // "show me the apps". 400 ms is the platform's own double-tap window;
        // longer starts catching deliberate second presses, shorter is hard to
        // hit with a thumb.
        constexpr auto kDoublePress = std::chrono::milliseconds(400);

        struct ModeChoice { uint32_t width; uint32_t height; wchar_t const* label; };
        constexpr std::array<ModeChoice, 3> kModes{{
            { 3840, 2160, L"4K" },
            { 2560, 1440, L"1440p" },
            { 1920, 1080, L"1080p" },
        }};
        constexpr std::array<uint32_t, 2> kFrameRates{ 60, 120 };

        // One lookup for every brush built in code, so a colour is decided in
        // Ion.xaml and nowhere else. A missing key throws, which is the right
        // outcome: it is a typo, and it should be loud on the first row drawn
        // rather than a silently black piece of text.
        Windows::UI::Xaml::Media::Brush IonBrush(wchar_t const* key)
        {
            return Application::Current().Resources()
                .Lookup(box_value(hstring(key)))
                .as<Windows::UI::Xaml::Media::Brush>();
        }

        // Same idea for the button styles the overlay swaps at runtime.
        Windows::UI::Xaml::Style IonStyle(wchar_t const* key)
        {
            return Application::Current().Resources()
                .Lookup(box_value(hstring(key)))
                .as<Windows::UI::Xaml::Style>();
        }

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
        HookBackButton();
        StartupAsync();
    }

    // ── B, which the console thinks is Back ─────────────────────────────────
    //
    // Xbox maps B to system navigation. An unhandled BackRequested at the root
    // of an app's navigation stack means "leave the app", so pressing B while
    // streaming closed Echo outright — on a button most games bind to
    // something, which makes it roughly the worst possible key to lose.
    //
    // The pad path is untouched by this: B reaches the host through the 250 Hz
    // `Windows.Gaming.Input` poll, which is a completely separate mechanism
    // from XAML's back stack. So marking the event handled does not "forward"
    // anything — it only stops the SHELL from acting on a press the game is
    // already receiving.
    //
    // Deliberately NOT handled on the dashboard: B exiting from the host picker
    // is exactly what an Xbox user expects, and swallowing it there would leave
    // the app with no way out at all.
    void MainPage::HookBackButton()
    {
        auto manager = Windows::UI::Core::SystemNavigationManager::GetForCurrentView();
        if (!manager) return;

        m_backToken = manager.BackRequested(
            [this](IInspectable const&,
                   Windows::UI::Core::BackRequestedEventArgs const& args) {
                if (m_diagnosticsOpen) {
                    // Innermost screen first. Diagnostics can be opened from
                    // the overlay, so B has to unwind them in the order they
                    // were stacked or it closes the wrong one.
                    HideDiagnostics();
                    args.Handled(true);
                } else if (m_overlayOpen) {
                    // B closes the panel, which is what B means everywhere else
                    // on the console. Forwarding is parked while it is open, so
                    // this press was never going to reach the game anyway.
                    HideOverlay();
                    args.Handled(true);
                } else if (m_streaming) {
                    // Swallow it. The game already has it.
                    args.Handled(true);
                }
            });
    }

    MainPage::~MainPage()
    {
        // Order matters: the input threads call into the session, the session's
        // feed thread calls into the decoder, and the renderer's present thread
        // pulls from it. Stop producers first, working down the chain.
        // The handler captures `this`, and SystemNavigationManager is a view
        // singleton that outlives the page — so an un-revoked token is a call
        // into a destroyed MainPage on the next B press.
        if (m_backToken.value) {
            if (auto manager = Windows::UI::Core::SystemNavigationManager::GetForCurrentView()) {
                manager.BackRequested(m_backToken);
            }
            m_backToken = {};
        }

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
        // Kept, because this is the line that answers "what is the TV actually
        // being driven at" and the startup log scrolls away long before anyone
        // needs it. It belongs beside the counters, not in history.
        m_hdmiNote = hdmi.note;
        if (!hdmi.offered.empty()) {
            Append(L"            " + hstring(hdmi.offered));
            m_hdmiNote += L"\n            " + hdmi.offered;
        }

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
            m_outputHz = static_cast<uint32_t>(hdmi.refreshHz + 0.5);
            m_streamWidth = hdmi.width;
            m_streamHeight = hdmi.height;
            m_streamFps = DecodableFps(hdmi.width, hdmi.height, m_outputHz);
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
        // Ask the hardware what it can do BEFORE opening a session on it. The
        // answer replaces an inferred ceiling with a declared one where the
        // decoder is willing to declare anything — see DecoderProbe.
        m_probe = echo::HevcDecoder::Probe(m_renderer->Device());
        report += L"\n" + m_probe.Report();
        AdoptProbedBudget();

        // Kept so a decoder swap can rebuild at the same geometry without
        // re-querying the swap chain.
        m_backBufferWidth = facts.backBufferWidth;
        m_backBufferHeight = facts.backBufferHeight;

        report += L"\n" + StartDecoder(facts.backBufferWidth, facts.backBufferHeight);
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
            },
            // Also the pad thread, and it fires whichever way the mode was
            // toggled — the chord or the overlay button.
            [this](bool on) { OnMouseModeChanged(on); });

        std::wstring error;
        if (!m_input->Start(VideoPanel(), Window::Current().CoreWindow(), error)) {
            Append(L"input       " + hstring(error));
            return;
        }
        auto panel = VideoPanel();
        m_input->SetSurfaceSize(static_cast<float>(panel.ActualWidth()),
                                static_cast<float>(panel.ActualHeight()));
        Append(L"input       core-independent pointer + 250 Hz pad. Menu+View: tap = mouse mode, hold = overlay");
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

            // Ion, and the palette comes from the dictionary rather than from
            // literals here — the whole point of Theme.kt having one home is
            // that a colour is never typed twice. `IonBrush` is the lookup.
            //
            // Presence is deliberately TWO states here and three on Android.
            // Android probes a host's relay over TCP to tell "one hop away"
            // from "no route at all"; this client has no equivalent yet, and a
            // badge that claims a distinction it did not measure is worse than
            // one that admits it only knows what mDNS told it. So a discovered
            // host is `ONLINE // LAN` — which is exactly what a live mDNS
            // record means — and the third state waits for a real probe.
            const bool paired = !stored.empty();
            std::wstring badge = paired ? L"PAIRED" : L"NOT PAIRED";
            badge += L"   ONLINE // LAN";
            if (host.relayUrl.empty()) badge += L"   no relay advertised";

            StackPanel row;
            row.Margin(ThicknessHelper::FromLengths(4, 10, 4, 10));
            row.Spacing(2);

            TextBlock name;
            name.Text(hstring(host.name));
            name.FontSize(26);
            name.Foreground(IonBrush(L"IonText"));

            TextBlock address;
            address.Text(hstring(host.address));
            address.FontSize(17);
            address.FontFamily(Media::FontFamily(L"Consolas"));
            address.Foreground(IonBrush(L"IonTextDim"));

            // The badge line: a dot, then the state. Green is reserved for
            // "the network answered" and nothing else may use it — the same
            // rule Theme.kt states, and it is why a paired-but-unseen host
            // would not get one.
            StackPanel badgeRow;
            badgeRow.Orientation(Orientation::Horizontal);
            badgeRow.Spacing(8);
            badgeRow.Margin(ThicknessHelper::FromLengths(0, 4, 0, 0));

            Shapes::Ellipse dot;
            dot.Width(8);
            dot.Height(8);
            dot.VerticalAlignment(VerticalAlignment::Center);
            dot.Fill(IonBrush(L"IonMatrix"));

            TextBlock state;
            state.Text(hstring(badge));
            state.FontSize(15);
            state.FontFamily(Media::FontFamily(L"Consolas"));
            state.CharacterSpacing(80);
            state.VerticalAlignment(VerticalAlignment::Center);
            state.Foreground(IonBrush(paired ? L"IonAccent" : L"IonTextDim"));

            badgeRow.Children().Append(dot);
            badgeRow.Children().Append(state);

            row.Children().Append(name);
            row.Children().Append(address);
            row.Children().Append(badgeRow);
            HostList().Items().Append(row);

            if (!selectedId.empty() && host.id == selectedId) restore = static_cast<int>(i);
        }

        HostsHeader().Text(m_hosts.empty()
            ? hstring(L"SEARCHING THE NETWORK…")
            : hstring(std::to_wstring(m_hosts.size()) +
                      (m_hosts.size() == 1 ? L" HOST FOUND" : L" HOSTS FOUND")));

        if (restore >= 0) {
            HostList().SelectedIndex(restore);
        } else if (m_selected < 0 && !m_hosts.empty()) {
            HostList().SelectedIndex(0);   // one host is the common case
        }
        UpdateButtons();
    }

    void MainPage::OnHostSelectionChanged(IInspectable const&, SelectionChangedEventArgs const&)
    {
        const int previous = m_selected;
        m_selected = HostList().SelectedIndex();
        // Moving to a different host closes the app list. Leaving it open would
        // show one host's name above another host's apps, and pressing one
        // would then launch on a machine the user is not looking at.
        if (m_selected != previous) CollapseApps();
        UpdateButtons();
    }

    // ── A on a host: once expands, twice launches ───────────────────────────
    //
    // The two gestures are ordered so neither has to wait for the other. The
    // first press expands immediately — no double-press timer to run down
    // first, which is what makes a delayed-single-action UI feel broken. The
    // second press, if it lands inside `kDoublePress`, means the user already
    // knew what they wanted and starts the stream on the spot.
    //
    // A quick launch uses `m_selectedApp`, which is Virtual Desktop until the
    // user picks otherwise — the app that gets this console its own monitor,
    // and the only sensible default for a device with no screen of its own.
    void MainPage::OnHostInvoked(IInspectable const&, ItemClickEventArgs const& args)
    {
        uint32_t found = 0;
        if (!HostList().Items().IndexOf(args.ClickedItem(), found)) return;
        const int index = static_cast<int>(found);
        if (index >= static_cast<int>(m_hosts.size())) return;

        HostList().SelectedIndex(index);
        m_selected = index;

        const auto now = std::chrono::steady_clock::now();
        const bool doublePress = m_appsExpanded && index == m_lastInvoked &&
                                 (now - m_lastInvoke) < kDoublePress;
        m_lastInvoke = now;
        m_lastInvoked = index;

        if (doublePress) {
            // Only if it can actually go. An unpaired host answers the second
            // press with the reason rather than with silence.
            if (StreamButton().IsEnabled()) {
                SetStatus(L"starting…");
                BeginStream();
            } else {
                const auto stored = echo::LoadHostFingerprint(m_stateDir, m_hosts[index].address);
                SetStatus(stored.empty() ? L"not paired yet — press Pair first"
                                         : L"this host cannot stream yet");
            }
            return;
        }

        ExpandApps(index);
        UpdateButtons();
    }

    void MainPage::ExpandApps(int hostIndex)
    {
        if (hostIndex < 0 || hostIndex >= static_cast<int>(m_hosts.size())) return;
        auto const& host = m_hosts[hostIndex];

        AppRow().Children().Clear();
        for (auto const& app : kApps) {
            Button button;
            button.Content(box_value(hstring(app.label)));
            button.Style(IonStyle(app.id == m_selectedApp ? L"IonActiveButton" : L"IonButton"));
            const uint32_t id = app.id;
            wchar_t const* note = app.note;
            button.Click([this, id, note](auto&&...) {
                m_selectedApp = id;
                AppHint().Text(hstring(note));
                ExpandApps(m_selected);          // move the highlight
                if (StreamButton().IsEnabled()) {
                    SetStatus(L"starting…");
                    BeginStream();
                } else {
                    SetStatus(L"not paired yet — press Pair first");
                }
            });
            AppRow().Children().Append(button);
        }

        // The header names the host, because this panel is the one place where
        // pressing something reaches out to a specific machine.
        AppHeader().Text(hstring(L"LAUNCH ON " + host.name));
        for (auto const& app : kApps) {
            if (app.id == m_selectedApp) { AppHint().Text(hstring(app.note)); break; }
        }
        AppPanel().Visibility(Visibility::Visible);
        m_appsExpanded = true;
    }

    void MainPage::CollapseApps()
    {
        AppPanel().Visibility(Visibility::Collapsed);
        m_appsExpanded = false;
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
                   std::to_wstring(m_streamFps) + L", app " + std::to_wstring(m_selectedApp));
            config = echo::BuildConnectConfig(m_stateDir, host, fingerprint,
                                              res, m_streamFps, kStreamBitrate,
                                              m_selectedApp);
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
        // No local FontSize/Padding here, deliberately: a locally-set property
        // beats a Style setter in XAML, so a button carrying its own font size
        // would keep it when `RefreshModeButtons` swaps in IonActiveButton and
        // the selected state would come out subtly different from the rest.
        // Everything visual comes from the style.
        for (auto const& mode : kModes) {
            Button button;
            button.Content(box_value(hstring(mode.label)));
            button.Style(IonStyle(L"IonButton"));
            const uint32_t w = mode.width, h = mode.height;
            button.Click([this, w, h](auto&&...) { ApplyStreamMode(w, h, m_streamFps); });
            ResolutionRow().Children().Append(button);
        }
        for (uint32_t fps : kFrameRates) {
            Button button;
            button.Content(box_value(to_hstring(fps) + L" fps"));
            button.Style(IonStyle(L"IonButton"));
            button.Click([this, fps](auto&&...) {
                ApplyStreamMode(m_streamWidth, m_streamHeight, fps);
            });
            FrameRateRow().Children().Append(button);
        }
    }

    void MainPage::ApplyStreamMode(uint32_t width, uint32_t height, uint32_t fps)
    {
        // ── The panel rate ADVISES; it does not veto ────────────────────────
        //
        // Asking for more frames than the HDMI output can show is real waste —
        // double the decode load, half the time per frame, every second frame
        // discarded at the present stage — and that is worth saying out loud.
        //
        // But it is not worth refusing. A previous version of this made the
        // panel rate a hard cap and immediately took away a lever that was in
        // active use: with the output reading 60 Hz, the 120 button simply
        // stopped working, and the operator could no longer test the thing this
        // whole port exists for. `m_outputHz` is one reading from one API, and
        // that is far too thin a basis for overruling an explicit press.
        //
        // So: warn, and comply. The DECODE budget below is a different matter
        // and still binds — that one guards against a blank screen rather than
        // against inefficiency.
        const uint32_t panelHz = m_outputHz;
        const uint32_t capped = DecodableFps(width, height, fps);
        if (capped != fps) {
            Append(L"display     " + to_hstring(fps) + L" fps refused at " +
                   to_hstring(width) + L"x" + to_hstring(height) + L" - using " +
                   to_hstring(capped) + L"  (decode budget)");
        }
        // Separately from any cap: say when the request exceeds what the TV is
        // being driven at. This is the line that answers "why is 120 heavier
        // than it should be" without refusing anything.
        if (panelHz && fps > panelHz + 5) {
            Append(L"display     ⚠ asking for " + to_hstring(fps) +
                   L" fps while the HDMI output is running at " + to_hstring(panelHz) +
                   L" Hz — the console will decode every frame and show half of them");
        }
        fps = capped;

        m_streamWidth = width;
        m_streamHeight = height;
        m_streamFps = fps;

        // Before any of the early returns below: the highlight must move even
        // when the mode was only saved for next time, and even when the host
        // refused to re-mode. It reflects what this client will ask for.
        RefreshModeButtons();

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

    // ── Building whichever decoder is selected ──────────────────────────────
    //
    // FFmpeg first, because on this console Media Foundation offers the app
    // container no hardware decoder at all and what it falls back to is a
    // software MFT. If FFmpeg fails to start for any reason, MF is still a
    // working 4K60 picture, and a working picture beats a correct diagnosis.
    //
    // Both are kept for the whole session so the overlay can swap between them
    // on a button. That is not indulgence: §4b of the handoff is a list of
    // things only ever settled by comparing two behaviours on a television, and
    // this is the first change big enough to need that comparison since.
    std::wstring MainPage::StartDecoder(uint32_t width, uint32_t height)
    {
        // Tear down whatever is running FIRST. Two HEVC decoders holding
        // hardware surfaces on one device at once is not a state worth being
        // in, and the renderer must not be pulling from a decoder mid-swap.
        m_renderer->SetFrameSource(nullptr);
        m_decoderReady = false;
        if (m_decoder) { m_decoder->Shutdown(); m_decoder.reset(); }

        std::wstring report;
        std::wstring error;

        if (m_useFfmpeg) {
            auto ffmpeg = std::make_unique<echo::FfmpegHevcDecoder>();
            if (ffmpeg->Initialize(m_renderer->Device(), width, height, m_streamFps, error)) {
                m_decoder = std::move(ffmpeg);
            } else {
                report += L"decoder     FFmpeg refused: " + error + L"\n";
                report += L"            falling back to Media Foundation\n";
                m_useFfmpeg = false;
            }
        }

        if (!m_decoder) {
            auto mf = std::make_unique<echo::HevcDecoder>();
            if (mf->Initialize(m_renderer->Device(), width, height, m_streamFps, error)) {
                m_decoder = std::move(mf);
            } else {
                return report + L"decoder     FAILED: " + error;
            }
        }

        m_decoderReady = true;
        report += std::wstring(L"decoder     ") + m_decoder->Name() +
                  (m_decoder->IsHardware() ? L"  [hardware]"
                                           : L"  [SOFTWARE - will not keep up]");

        auto* decoder = m_decoder.get();
        auto* renderer = m_renderer.get();
        m_renderer->SetFrameSource(
            [decoder, renderer](com_ptr<ID3D11Texture2D>& texture, uint32_t& subresource) {
                if (!decoder->TryGetFrame(texture, subresource)) return false;
                // The CODED size, which is not the texture's size: a decoder
                // surface is allocated aligned (1088 rows for 1080), and
                // without this the processor scales the padding onto the
                // screen. Pushed per frame because a live re-mode changes it
                // mid-session and the decoder learns the new size before
                // anything else does.
                renderer->SetSourceSize(decoder->Width(), decoder->Height());
                return true;
            });

        // The ceiling is a property of the DECODER, not of the console.
        //
        // 520M px/s was drawn through one data point — ten blank 4K120 sessions
        // on Media Foundation — and the probe has since explained them: MF gave
        // the container a software decoder, and a software HEVC decoder was
        // never going to do 4K120. That number says nothing whatever about
        // D3D11VA, which is the layer the same probe found willing to hand out
        // a 4K HEVC decoder.
        //
        // So the budget is lifted for the FFmpeg path and restored for the MF
        // one, automatically, on every swap. Carrying MF's ceiling onto FFmpeg
        // would throttle the exact thing this port was built to unlock; leaving
        // it lifted on a fallback to MF would walk straight back into the blank
        // screen it was invented to prevent.
        ApplyDecoderBudget();
        return report;
    }

    void MainPage::ApplyDecoderBudget()
    {
        if (m_useFfmpeg) {
            g_budgetUnlocked = true;
            g_budgetSource = L"lifted — D3D11VA, not Media Foundation's software MFT";
        } else {
            g_budgetUnlocked = false;
            AdoptProbedBudget();      // back to declared-or-inferred
        }
        if (m_outputWidth && m_outputHeight) {
            m_streamFps = DecodableFps(m_outputWidth, m_outputHeight, m_outputHz);
        }
    }

    // Swap decoders mid-session. The stream is restarted deliberately: a
    // decoder change means a new DPB with no history, so the only frame the new
    // one can start from is a keyframe, and asking the host for a fresh session
    // is both simpler and more honest than hoping the next IDR arrives soon.
    void MainPage::OnDecoderToggleClick(IInspectable const&, RoutedEventArgs const&)
    {
        m_useFfmpeg = !m_useFfmpeg;
        const bool wasStreaming = m_streaming;

        if (wasStreaming && m_session) m_session->Close();

        Append(hstring(L"decoder     switching to " +
                       std::wstring(m_useFfmpeg ? L"FFmpeg/D3D11VA" : L"Media Foundation")));
        Append(hstring(StartDecoder(m_backBufferWidth, m_backBufferHeight)));

        RefreshOverlayState();
        if (wasStreaming) {
            SetStatus(L"restarting the stream on the new decoder…");
            BeginStream();
        }
    }

    // Believe the hardware over the inference, but only in the direction that
    // is safe to be wrong in.
    //
    // A decoder that declares MORE than 520M px/s has told us something we did
    // not know and the guess should yield to it. A decoder that declares LESS
    // is also believed — it is the authority on itself, and a lower ceiling
    // only ever refuses modes. What is NOT done here is inventing a number
    // when the decoder exposes none: silence is not a claim, and the 520M
    // guess stays exactly as visible a guess as it was.
    void MainPage::AdoptProbedBudget()
    {
        if (!m_probe.ran || m_probe.maxPixelRate == 0) return;

        g_maxDecodePixelRate = m_probe.maxPixelRate;
        g_budgetSource = L"declared by the decoder (MF_VIDEO_MAX_MB_PER_SEC)";

        // The startup path already picked a frame rate from the old budget, so
        // re-decide with the real one before anything is negotiated.
        if (m_outputWidth && m_outputHeight) {
            const uint32_t fps = DecodableFps(m_outputWidth, m_outputHeight, m_outputHz);
            if (fps != m_streamFps) {
                Append(L"decode      budget is " +
                       to_hstring(static_cast<uint64_t>(m_probe.maxPixelRate / 1000000)) +
                       L"M px/s from the hardware — " +
                       (fps > m_streamFps ? L"raising " : L"lowering ") +
                       to_hstring(m_streamFps) + L" to " + to_hstring(fps) + L" fps");
                m_streamFps = fps;
            }
        }
    }

    void MainPage::ShowOverlay()
    {
        m_overlayOpen = true;
        // Park input FIRST. Otherwise the button press that dismisses this
        // panel also travels to the PC, and the A that opened a menu here opens
        // something over there too.
        if (m_input) m_input->SetForwarding(false);
        RefreshOverlayState();
        OverlayRoot().Visibility(Visibility::Visible);
        ResumeButton().Focus(FocusState::Programmatic);
    }

    // Everything on the panel that describes state rather than offering a
    // choice. Called on open and again after anything the panel itself
    // changes, so a button never reports what it did before it did it.
    void MainPage::RefreshOverlayState()
    {
        const bool mouseMode = m_input && m_input->MouseMode();

        std::wstring subtitle = m_streaming
            ? L"streaming " + std::to_wstring(m_streamWidth) + L"x" +
              std::to_wstring(m_streamHeight) + L" @ " +
              std::to_wstring(m_streamFps) + L" fps"
            : std::wstring(L"not streaming");
        if (mouseMode) subtitle += L"   ·   MOUSE MODE";
        OverlaySubtitle().Text(hstring(subtitle));

        // The chord still works and is still the fast way; the button exists
        // because a chord is not discoverable and, until now, a user who hit
        // Menu+View by accident had no way to find out what had happened to
        // their controller. The label says which way the press goes.
        MouseModeButton().Content(box_value(
            mouseMode ? hstring(L"Mouse mode: ON") : hstring(L"Mouse mode: off")));
        MouseModeButton().Style(IonStyle(mouseMode ? L"IonActiveButton" : L"IonButton"));
        MouseModeHint().Text(mouseMode
            ? hstring(L"right stick moves the cursor · RT left-click · LT right-click\n"
                      L"tap Menu+View to leave · hold it to come back here")
            : hstring(L"tap Menu+View to toggle without opening this · hold it for the overlay"));

        BudgetButton().Content(box_value(
            g_budgetUnlocked ? hstring(L"Decode limit: OFF (test)")
                             : hstring(L"Decode limit: on")));
        BudgetButton().Style(IonStyle(g_budgetUnlocked ? L"IonActiveButton" : L"IonButton"));

        DecoderButton().Content(box_value(
            m_useFfmpeg ? hstring(L"Decoder: FFmpeg / D3D11VA")
                        : hstring(L"Decoder: Media Foundation")));
        DecoderButton().Style(IonStyle(m_useFfmpeg ? L"IonActiveButton" : L"IonButton"));
        DecoderHint().Text(m_decoder
            ? hstring(std::wstring(m_decoder->Name()) +
                      (m_decoder->IsHardware() ? L"  ·  hardware" : L"  ·  SOFTWARE") +
                      (m_useFfmpeg ? L"  ·  decode limit lifted"
                                   : L"  ·  decode limit enforced"))
            : hstring(L"no decoder"));

        RefreshModeButtons();
    }

    // Which resolution and which frame rate are actually in force.
    //
    // The buttons were previously write-only: pressing one changed the stream
    // and then looked exactly like the two it had not changed to, so the panel
    // could tell you what you *could* pick and never what you *had*.
    //
    // Read from `m_stream*` rather than from a "last pressed" flag, and that is
    // the point — `ApplyStreamMode` runs the request through `DecodableFps`, so
    // asking for 120 at 4K under an enforced budget lands on 60. The highlight
    // follows the mode that is RUNNING, so the cap becomes something you can
    // see happen instead of something that silently ignored you.
    void MainPage::RefreshModeButtons()
    {
        auto resolutions = ResolutionRow().Children();
        for (uint32_t i = 0; i < resolutions.Size() && i < kModes.size(); ++i) {
            if (auto button = resolutions.GetAt(i).try_as<Button>()) {
                const bool active = kModes[i].width == m_streamWidth &&
                                    kModes[i].height == m_streamHeight;
                button.Style(IonStyle(active ? L"IonActiveButton" : L"IonButton"));
            }
        }

        auto rates = FrameRateRow().Children();
        for (uint32_t i = 0; i < rates.Size() && i < kFrameRates.size(); ++i) {
            if (auto button = rates.GetAt(i).try_as<Button>()) {
                button.Style(IonStyle(kFrameRates[i] == m_streamFps ? L"IonActiveButton"
                                                                    : L"IonButton"));
            }
        }
    }

    // ── Mouse mode ──────────────────────────────────────────────────────────
    //
    // The button does NOT write the flag. It asks the pad loop to, and the pad
    // loop performs the transition through the identical code path the chord
    // uses — see InputBridge::RequestMouseMode.
    //
    // That indirection is load-bearing rather than fussy. Entering and leaving
    // the mode is not a bool assignment: it neutralises the pad on both edges
    // (a pad that merely goes quiet leaves the host holding Menu, which games
    // read as pause), it lifts whatever the triggers were holding down, and it
    // clears the sub-pixel accumulator. A UI thread that set the atomic
    // directly would skip all three, and the resulting bug — a click that
    // outlives the mode that made it — would look like a host-side input fault.
    void MainPage::OnMouseModeClick(IInspectable const&, RoutedEventArgs const&)
    {
        if (!m_input) return;
        m_input->RequestMouseMode(!m_input->MouseMode());
        // The pad loop applies it within one 4 ms tick and reports back through
        // OnMouseModeChanged, which is what refreshes this panel. Nothing is
        // updated here on the strength of having asked.
    }

    // Fired from the pad thread whichever way the mode was toggled — the chord
    // or the button — so this is the single place the UI learns about it.
    void MainPage::OnMouseModeChanged(bool on)
    {
        Dispatcher().RunAsync(CoreDispatcherPriority::Normal, [this, on]() {
            if (m_overlayOpen) RefreshOverlayState();
            // In the stream, with no overlay open, the toggle was silent: the
            // controller simply stopped reaching the game and nothing said
            // why. This is the smallest thing that fixes that — it says what
            // happened, it says how to undo it, and it gets out of the way.
            if (m_streaming && !m_overlayOpen) ShowModeToast(on);
        });
    }

    void MainPage::ShowModeToast(bool on)
    {
        ToastText().Text(on ? hstring(L"MOUSE MODE ON   ·   right stick drives the cursor")
                            : hstring(L"MOUSE MODE OFF   ·   controller back to the game"));
        ToastAccent().Fill(IonBrush(on ? L"IonAccent" : L"IonTextDim"));
        ToastRoot().Visibility(Visibility::Visible);

        // One timer, restarted. Toggling twice quickly must not leave the first
        // toast's expiry to hide the second one.
        if (!m_toastTimer) {
            m_toastTimer = DispatcherTimer();
            m_toastTimer.Interval(std::chrono::milliseconds(1800));
            m_toastTimer.Tick([this](auto&&...) {
                m_toastTimer.Stop();
                ToastRoot().Visibility(Visibility::Collapsed);
            });
        }
        m_toastTimer.Stop();
        m_toastTimer.Start();
    }

    // ── The decode limit ────────────────────────────────────────────────────
    //
    // Turning this off is how 4K120 gets tested at all, so it is deliberately
    // reachable without a rebuild — one sideload can answer the question in
    // both directions. It is equally deliberately NOT sticky: it lives for the
    // life of the process, because the failure it can produce is a blank
    // screen and nobody should meet that on a launch they did not ask for.
    void MainPage::OnBudgetClick(IInspectable const&, RoutedEventArgs const&)
    {
        g_budgetUnlocked = !g_budgetUnlocked;
        Append(g_budgetUnlocked
                   ? hstring(L"decode      limit OFF — the panel refresh is now the only cap.\n"
                             L"            If the picture goes black and 'decoded' stays 0, that\n"
                             L"            is the answer: this console cannot decode what it can show.")
                   : hstring(L"decode      limit back on"));
        RefreshOverlayState();
    }

    void MainPage::HideOverlay()
    {
        m_overlayOpen = false;
        OverlayRoot().Visibility(Visibility::Collapsed);
        // The diagnostics SCREEN is not touched here: it is not part of the
        // overlay any more, and closing the overlay behind it was exactly the
        // behaviour that made it vanish mid-stream.
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

    // ── Diagnostics, which is now a screen and not a drawer ─────────────────
    //
    // It used to be a ScrollViewer sharing the overlay panel with everything
    // else, and it kept vanishing during a stream. Two things did that, and a
    // screen removes both by construction rather than by tuning: it was in a
    // `*` row competing for whatever height was left inside a panel capped at
    // 920 px, and `EnterStreamingUi` collapses the overlay — so any session
    // event that re-fired took the diagnostics down with it, at exactly the
    // moment somebody would be reading them.
    //
    // The overlay is hidden rather than left underneath, so the stream is not
    // dimmed by a scrim nobody can see behind a full screen of text.
    void MainPage::ShowDiagnostics()
    {
        m_diagnosticsOpen = true;
        DiagnosticsText().Text(hstring(m_statsLine.empty() ? m_probe.Report() : m_statsLine));
        DiagnosticsScroller().ChangeView(nullptr, 0.0, nullptr, true);
        if (m_overlayOpen) OverlayRoot().Visibility(Visibility::Collapsed);
        DiagnosticsRoot().Visibility(Visibility::Visible);
        DiagnosticsCloseButton().Focus(FocusState::Programmatic);
    }

    void MainPage::HideDiagnostics()
    {
        m_diagnosticsOpen = false;
        DiagnosticsRoot().Visibility(Visibility::Collapsed);
        // Back to the overlay if that is where it was opened from. Input stays
        // parked the whole time, so nothing reached the PC while it was up.
        if (m_overlayOpen) {
            OverlayRoot().Visibility(Visibility::Visible);
            ResumeButton().Focus(FocusState::Programmatic);
        }
    }

    void MainPage::OnDiagnosticsClick(IInspectable const&, RoutedEventArgs const&)
    {
        ShowDiagnostics();
    }

    void MainPage::OnDiagnosticsCloseClick(IInspectable const&, RoutedEventArgs const&)
    {
        HideDiagnostics();
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
            const bool showing = m_diagnosticsOpen;

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
            // Decode errors and the keyframes they asked for. These are the two
            // numbers that were missing when a grey screen sat unexplained for
            // several minutes: the network counters were all healthy, and
            // nothing anywhere said the decoder had failed to produce a
            // picture.
            const uint64_t errors = m_decoder ? m_decoder->DecodeErrors() : 0;
            const uint64_t idrs   = m_session ? m_session->IdrRequests() : 0;
            line += L"\ndecode errors " + std::to_wstring(errors) +
                    L"   keyframes asked for " + std::to_wstring(idrs);
            if (m_decoder) {
                // The breakdown names WHICH failure, and the four mean quite
                // different things. `concealed` climbing on its own is the
                // silent one: the decoder produced a picture by filling in for
                // a reference it did not have, and said so only here.
                const auto detail = m_decoder->ErrorDetail();
                if (!detail.empty()) line += L"\n  " + detail;
            }

            // ── The receive path, from the shared core ──────────────────────
            //
            // The host log shows a relentless trickle of transit-loss
            // invalidations — one frame in roughly 25 never COMPLETING — while
            // every decoder counter reads zero. Both are true at once because
            // a frame that never completes never reaches the decoder, so no
            // decoder-side counter can ever see it. These are the numbers that
            // can: `dropped overflow` is the queue abandoning a frame as
            // stale, and `waiting keyframe` is the gate refusing one.
            if (m_session) {
                const auto stats = m_session->Stats();
                if (!stats.empty()) {
                    const auto field = [&stats](char const* key) {
                        const auto value = echo::JsonField(stats, key);
                        return value.empty() ? std::wstring(L"-")
                                             : std::wstring(to_hstring(value));
                    };
                    line += L"\nreceive   delivered " + field("frames_delivered") +
                            L"   dropped overflow " + field("frames_dropped_overflow") +
                            L"   waiting keyframe " + field("frames_dropped_waiting_keyframe");
                    line += L"\n          queue depth " + field("queue_depth") +
                            L"   frame age " + field("frame_age_ms") +
                            L" ms   worst " + field("worst_frame_age_ms") + L" ms";
                }
            }

            line += L"\npresented " + std::to_wstring(m_renderer->PresentedFrames()) +
                    L"   sync " + std::to_wstring(m_renderer->SyncInterval());
            if (const uint32_t hr = m_renderer->LastPresentError()) {
                wchar_t code[32]{};
                swprintf_s(code, L"  last error 0x%08X", hr);
                line += code;
            }
            // What this console says it can decode, and where the ceiling
            // currently in force came from. A frame rate that was refused is
            // only diagnosable next to the number that refused it.
            if (showing) {
                // What the TV is actually being driven at, which decides
                // whether asking for 120 is useful or just twice the work.
                line += L"\n\nhdmi        " + m_hdmiNote;
                if (m_outputHz) {
                    line += L"\noutput      " + std::to_wstring(m_outputWidth) + L"x" +
                            std::to_wstring(m_outputHeight) + L" @ " +
                            std::to_wstring(m_outputHz) + L" Hz   ·   asking for " +
                            std::to_wstring(m_streamFps) + L" fps";
                }
                line += L"\n\n" + m_probe.Report();
                line += L"\nbudget      " +
                        std::to_wstring(g_maxDecodePixelRate / 1000000) + L"M px/s, " +
                        g_budgetSource;
                if (g_budgetUnlocked) line += L"  — CURRENTLY OFF (test)";
            }
            // Kept whether or not the panel is open, so opening it shows the
            // current numbers at once. It used to write only while visible,
            // which left the box blank for up to a second after it appeared —
            // and a diagnostic that shows nothing when you first ask for it is
            // indistinguishable from one that is broken.
            m_statsLine = line;
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
                          L"Hold Menu+View and pick a lower resolution or frame rate.";
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
