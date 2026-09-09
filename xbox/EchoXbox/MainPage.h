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
        void OnHostInvoked(IInspectable const&,
                           Windows::UI::Xaml::Controls::ItemClickEventArgs const&);
        void OnDiagnosticsCloseClick(IInspectable const&, Windows::UI::Xaml::RoutedEventArgs const&);
        void OnPairClick(IInspectable const&, Windows::UI::Xaml::RoutedEventArgs const&);
        void OnStreamClick(IInspectable const&, Windows::UI::Xaml::RoutedEventArgs const&);
        void OnRescanClick(IInspectable const&, Windows::UI::Xaml::RoutedEventArgs const&);
        void OnStopClick(IInspectable const&, Windows::UI::Xaml::RoutedEventArgs const&);
        void OnSettingsClick(IInspectable const&, Windows::UI::Xaml::RoutedEventArgs const&);
        void OnResumeClick(IInspectable const&, Windows::UI::Xaml::RoutedEventArgs const&);
        void OnDiagnosticsClick(IInspectable const&, Windows::UI::Xaml::RoutedEventArgs const&);
        void OnMouseModeClick(IInspectable const&, Windows::UI::Xaml::RoutedEventArgs const&);
        void OnBudgetClick(IInspectable const&, Windows::UI::Xaml::RoutedEventArgs const&);
        void OnDecoderToggleClick(IInspectable const&, Windows::UI::Xaml::RoutedEventArgs const&);

    private:
        // Probes, HDMI mode, renderer, decoder, input, discovery — in that
        // order, because each depends on the one before and a failure should
        // name itself rather than surfacing three steps later.
        Windows::Foundation::IAsyncAction StartupAsync();
        bool StartRenderer();                 // UI thread
        void StartInput();                    // UI thread
        void StartDiscovery();                // UI thread
        void StartStatsTimer();

        void RefreshHostList();               // UI thread
        void UpdateButtons();                 // UI thread
        void ExpandApps(int hostIndex);       // UI thread
        void CollapseApps();                  // UI thread
        void ShowDiagnostics();               // UI thread
        void HideDiagnostics();               // UI thread
        void EnterStreamingUi();              // UI thread
        void LeaveStreamingUi();              // UI thread

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

        // Replaces the inferred decode ceiling with the declared one, when the
        // decoder declares anything at all.
        void AdoptProbedBudget();             // UI thread

        // Builds whichever decoder `m_useFfmpeg` selects, wires it to the
        // renderer, and sets the decode budget that belongs to it.
        std::wstring StartDecoder(uint32_t width, uint32_t height);
        void ApplyDecoderBudget();            // UI thread

        void Append(hstring const& line);     // any thread
        void SetStatus(hstring const& text);  // any thread

        void OnSessionEvent(std::string const& json);
        void BeginStream();                   // UI thread; used by button AND handoff

        std::wstring m_log;
        std::string  m_stateDir;

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

        // The app-expansion state, and the double-press clock behind it.
        bool m_appsExpanded = false;
        int  m_lastInvoked = -1;
        std::chrono::steady_clock::time_point m_lastInvoke{};

        // Which app a stream launches. Virtual Desktop (5) by default: it is
        // the only one that gets this console its own monitor at the size we
        // asked for, and app 1 deliberately does not — see kApps.
        uint32_t m_selectedApp = 5;

        // What the console is really putting on the wire to the TV, read back
        // from HdmiDisplayInformation rather than assumed from what we asked
        // for. Zero means we never got an answer (a PC, not a console).
        uint32_t m_outputWidth = 0;
        uint32_t m_outputHeight = 0;
        uint32_t m_outputHz = 0;

        // What the hardware says it can decode, read once at startup. Kept
        // because the diagnostics panel shows it and because AdoptProbedBudget
        // needs it after the startup path has already guessed a frame rate.
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
    };
}

namespace winrt::EchoXbox::factory_implementation
{
    struct MainPage : MainPageT<MainPage, implementation::MainPage>
    {
    };
}
