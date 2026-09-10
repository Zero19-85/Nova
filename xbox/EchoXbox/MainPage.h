#pragma once
#include "MainPage.g.h"
#include "VideoRenderer.h"
#include "HevcDecoder.h"
#include "FfmpegHevcDecoder.h"
#include "EchoSession.h"
#include "HostDiscovery.h"
#include "InputBridge.h"

#include <memory>
#include <string>
#include <vector>

namespace winrt::EchoXbox::implementation
{
    struct MainPage : MainPageT<MainPage>
    {
        MainPage();
        ~MainPage();

        void OnHostSelectionChanged(IInspectable const&,
                                    Windows::UI::Xaml::Controls::SelectionChangedEventArgs const&);
        // Public because the XAML generates a call to it; the generated code is
        // not a friend of this class.
        void OnAppDrawerSizeChanged(IInspectable const&,
                                    Windows::UI::Xaml::SizeChangedEventArgs const&);

        // The Start menu.
        void OnActionStopClick(IInspectable const&, Windows::UI::Xaml::RoutedEventArgs const&);
        void OnActionStreamClick(IInspectable const&, Windows::UI::Xaml::RoutedEventArgs const&);
        void OnActionPairClick(IInspectable const&, Windows::UI::Xaml::RoutedEventArgs const&);

        // Settings, and what lives on it.
        void OnSettingsClick(IInspectable const&, Windows::UI::Xaml::RoutedEventArgs const&);
        void OnSettingsCloseClick(IInspectable const&, Windows::UI::Xaml::RoutedEventArgs const&);
        void OnMicLevelChanged(IInspectable const&,
                               Windows::UI::Xaml::Controls::Primitives::RangeBaseValueChangedEventArgs const&);
        void OnRescanClick(IInspectable const&, Windows::UI::Xaml::RoutedEventArgs const&);

        void OnDiagnosticsClick(IInspectable const&, Windows::UI::Xaml::RoutedEventArgs const&);
        void OnDiagnosticsCloseClick(IInspectable const&, Windows::UI::Xaml::RoutedEventArgs const&);

        // The in-stream overlay.
        void OnResumeClick(IInspectable const&, Windows::UI::Xaml::RoutedEventArgs const&);
        void OnLeaveStreamClick(IInspectable const&, Windows::UI::Xaml::RoutedEventArgs const&);
        void OnMouseModeClick(IInspectable const&, Windows::UI::Xaml::RoutedEventArgs const&);
        void OnDecoderToggleClick(IInspectable const&, Windows::UI::Xaml::RoutedEventArgs const&);

        // The safety catch.
        void OnEndStreamCancelClick(IInspectable const&, Windows::UI::Xaml::RoutedEventArgs const&);
        void OnEndStreamConfirmClick(IInspectable const&, Windows::UI::Xaml::RoutedEventArgs const&);

    private:
        // Probes, HDMI mode, renderer, decoder, input, discovery — in that
        // order, because each depends on the one before and a failure should
        // name itself rather than surfacing three steps later.
        Windows::Foundation::IAsyncAction StartupAsync();
        bool StartRenderer();                 // UI thread
        void StartInput();                    // UI thread
        void StartDiscovery();                // UI thread
        void StartStatsTimer();

        // ── The 4K design space ─────────────────────────────────────────────
        //
        // ChromeRoot is a fixed 3840x2160 canvas scaled to fit whatever logical
        // size the view actually has. See the note on the method.
        void ApplyChromeScale();              // UI thread

        // ── The dashboard's buttons, and where each one comes from ─────────
        //
        // They arrive by two different routes, and the split is not arbitrary:
        //
        //   Start  XAML. `OnChromeKeyDown` sees GamepadMenu through
        //          PreviewKeyDown, which tunnels from the root and so reaches
        //          the page before whatever has focus.
        //   A      the PAD THREAD, via `OnPadAccept`. XAML owns GamepadA for
        //          its own Accept handling and never routed it to the page at
        //          all — see the notes on OnChromeKeyDown and on
        //          InputBridge::SetSinks.
        void HookGestures();                  // UI thread, once
        void OnChromeKeyDown(IInspectable const&, Windows::UI::Xaml::Input::KeyRoutedEventArgs const&);
        void OnChromeKeyUp(IInspectable const&, Windows::UI::Xaml::Input::KeyRoutedEventArgs const&);
        // The A button, from InputBridge's 250 Hz poll. Called on the PAD
        // THREAD; hops to the dispatcher before touching anything.
        void OnPadAccept();                   // any thread
        // Put the focus rect on a host row, after layout has had a chance to
        // create one. See the note on the definition.
        void FocusHostList();                 // UI thread
        // Not const: it asks the PIN panel whether it is up, and the generated
        // XAML accessors are non-const.
        bool GesturesArmed();                 // UI thread
        void FireSingleTap();                 // UI thread
        void FireDoubleTap();                 // UI thread
        void CancelPendingTap();              // UI thread

        void RefreshHostList();               // UI thread
        void UpdateHostState();               // UI thread

        // ── The app drawer ─────────────────────────────────────────────────
        //
        // Inline, in its own layout row under the host card — not a pop-up. It
        // animates its own Height, so the card above genuinely gives up the
        // space rather than being covered. Only ever open for a PAIRED host.
        void BuildAppMenu();                  // UI thread
        void ShowAppDrawer();                 // UI thread
        void HideAppDrawer();                 // UI thread
        void AnimateDrawer(double from, double to, bool openWhenDone);   // UI thread
        // True when the focus rect is on something inside the drawer, in which
        // case A belongs to that button and not to the gesture.
        bool FocusInsideDrawer();             // UI thread
        // Whether the selected host has a stored fingerprint. The drawer, and
        // both A gestures, are gated on it.
        bool SelectedHostPaired() const;      // UI thread

        // The Start menu, and settings.
        void ShowActionMenu();                // UI thread
        void HideActionMenu();                // UI thread
        void ShowSettings();                  // UI thread
        void HideSettings();                  // UI thread
        void ShowDiagnostics();               // UI thread
        void HideDiagnostics();               // UI thread
        void EnterStreamingUi();              // UI thread
        void LeaveStreamingUi();              // UI thread

        // ── Two-step teardown ───────────────────────────────────────────────
        //
        // `DetachStream` walks away and lets the host hold its display for the
        // grace period; `ShowEndStream`/`HardEndSession` is the only path that
        // hands the monitor back. Keeping them apart is the whole feature — see
        // the notes in the .cpp.
        void DetachStream();                  // UI thread
        void ShowEndStream();                 // UI thread
        void HideEndStream();                 // UI thread
        Windows::Foundation::IAsyncAction HardEndSession();   // UI thread; finishes on a pool thread
        void RefreshHeldBadge();              // UI thread

        // The overlay is the ONLY chrome a running stream ever shows, and it
        // exists only while it is open. Opening it parks input forwarding, so
        // the controller drives the panel rather than the PC.
        void HookBackButton();                // UI thread, once
        void BuildOverlayChoices();           // UI thread, once
        void ShowOverlay();                   // UI thread
        void HideOverlay();                   // UI thread
        void RefreshOverlayState();           // UI thread
        void RefreshModeButtons();            // UI thread
        void ApplyStreamMode(uint32_t width, uint32_t height, uint32_t fps);

        // Mouse mode is owned by the pad thread; these are the two ends of the
        // conversation with it. The button asks, the sink reports.
        void OnMouseModeChanged(bool on);     // any thread
        void ShowModeToast(bool on);          // UI thread

        // The centred, self-expiring banner. Anything that has to be seen from
        // three metres goes here rather than to the status line, which is small
        // text in a corner nobody is looking at. `accentKey` is an Ion brush
        // name — IonAmber for a refusal.
        void ShowToast(std::wstring const& text, wchar_t const* accentKey);  // UI thread

        // Builds whichever decoder `m_useFfmpeg` selects and wires it to the
        // renderer.
        std::wstring StartDecoder(uint32_t width, uint32_t height);

        void Append(hstring const& line);     // any thread
        void SetStatus(hstring const& text);  // any thread

        void OnSessionEvent(std::string const& json);
        void BeginStream();                   // UI thread
        void BeginPairing();                  // UI thread
        bool CanStream() const;               // UI thread
        bool CanPair() const;                 // UI thread

        std::wstring m_log;
        std::string  m_stateDir;

        // The `echo.json` escape hatch, read once at startup. Cached rather than
        // re-read: it is consulted by `CanStream`, which runs on every host-list
        // refresh, and a file read per mDNS TXT update is a lot of I/O to answer
        // a question whose answer cannot change while the app is running.
        std::string  m_configOverride;

        std::unique_ptr<echo::VideoRenderer> m_renderer;
        // Either implementation, behind one interface. Which one is running is
        // `m_useFfmpeg`, and the overlay can change it mid-session.
        std::unique_ptr<echo::VideoDecoder>  m_decoder;
        bool m_useFfmpeg = true;
        uint32_t m_backBufferWidth = 0;
        uint32_t m_backBufferHeight = 0;
        std::unique_ptr<echo::EchoSession>   m_session;
        std::unique_ptr<echo::HostDiscovery> m_discovery;
        std::unique_ptr<echo::InputBridge>   m_input;

        std::vector<echo::DiscoveredHost> m_hosts;   // parallel to HostList items
        int  m_selected = -1;
        bool m_decoderReady = false;
        bool m_streaming = false;
        bool m_overlayOpen = false;
        bool m_diagnosticsOpen = false;
        bool m_appDrawerOpen = false;
        bool m_actionMenuOpen = false;
        bool m_settingsOpen = false;
        bool m_endStreamOpen = false;

        // ── A-gesture state ────────────────────────────────────────────────
        //
        // Both gestures are decided on the PRESS, and the press comes off the
        // PAD THREAD rather than from XAML — see OnPadAccept. Nothing waits for
        // a release.
        //
        // The counters are diagnostics, on the Diagnostics screen. `pad`
        // climbing while `xaml` stays at zero is the normal, expected reading
        // and it is the evidence for why this gesture is wired the way it is.
        // Having both on screen is what turns "the button is dead" into a
        // one-glance answer about WHICH path is dead.
        std::chrono::steady_clock::time_point m_lastTapAt{};
        uint32_t m_aPadEdges = 0;
        uint32_t m_aXamlEdges = 0;
        uint32_t m_aXamlUpEdges = 0;
        Windows::UI::Xaml::DispatcherTimer m_singleTapTimer{ nullptr };

        // Which app a stream launches. Virtual Desktop (5) by default: it is
        // the only one that gets this console its own monitor at the size we
        // asked for, and app 1 deliberately does not — see kQuickApps and kVirtualDesktopApp.
        uint32_t m_selectedApp = 5;

        // ── What the host may still be holding ─────────────────────────────
        //
        // Set when this client walks away from a live stream without telling
        // the host to stop. The host keeps the virtual display for its detach
        // grace period, and until somebody confirms End Stream that monitor is
        // still spoken for. The config is kept because `echo_release` needs one
        // and there is no session left to ask.
        bool m_hostHolding = false;
        std::string  m_heldConfig;
        std::wstring m_heldHostName;
        bool m_ending = false;                // an End Stream RPC is in flight

        // Microphone passthrough, 0-100. Remembered here and sent nowhere; the
        // capture path does not exist yet. See the note on OnMicLevelChanged.
        uint32_t m_micLevel = 0;

        // What the console is really putting on the wire to the TV, read back
        // from HdmiDisplayInformation rather than assumed from what we asked
        // for. Zero means we never got an answer (a PC, not a console).
        uint32_t m_outputWidth = 0;
        uint32_t m_outputHeight = 0;
        uint32_t m_outputHz = 0;

        // What the hardware says it can decode, read once at startup. Kept
        // because the diagnostics panel shows it — it is information now, not a
        // ceiling: see the note where the decode clamp used to be.
        echo::DecoderProbe m_probe;

        // What RequestBestHdmiMode reported, kept for the diagnostics screen.
        std::wstring m_hdmiNote;

        // What we ask the host to encode. Starts at the console's real output
        // size and is whatever the overlay last chose after that.
        uint32_t m_streamWidth = 1920;
        uint32_t m_streamHeight = 1080;
        uint32_t m_streamFps = 60;

        // True while a pairing session is open, so the `paired` event knows to
        // hand straight off to a stream instead of just reporting success.
        bool m_pairing = false;

        // Revoked in the destructor: SystemNavigationManager is a per-view
        // singleton that outlives this page, and the handler captures `this`.
        event_token m_backToken{};

        // The stats line is built every tick whether or not the panel is
        // showing, so opening it can fill it immediately rather than leaving a
        // blank box until the next second ticks over.
        std::wstring m_statsLine;

        Windows::UI::Xaml::DispatcherTimer m_statsTimer{ nullptr };
        // Built on first use and restarted rather than replaced, so toggling
        // mouse mode twice quickly cannot have the first toast's expiry hide
        // the second one.
        Windows::UI::Xaml::DispatcherTimer m_toastTimer{ nullptr };

        // The drawer's accordion. Held so a second press mid-animation stops
        // the first one rather than fighting it — two storyboards driving the
        // same Height is how a panel ends up stuck at an arbitrary size.
        Windows::UI::Xaml::Media::Animation::Storyboard m_drawerStory{ nullptr };
    };
}

namespace winrt::EchoXbox::factory_implementation
{
    struct MainPage : MainPageT<MainPage, implementation::MainPage>
    {
    };
}
