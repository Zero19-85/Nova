#pragma once
#include "MainPage.g.h"
#include "VideoRenderer.h"
#include "HevcDecoder.h"
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
        void OnPairClick(IInspectable const&, Windows::UI::Xaml::RoutedEventArgs const&);
        void OnStreamClick(IInspectable const&, Windows::UI::Xaml::RoutedEventArgs const&);
        void OnRescanClick(IInspectable const&, Windows::UI::Xaml::RoutedEventArgs const&);
        void OnStopClick(IInspectable const&, Windows::UI::Xaml::RoutedEventArgs const&);
        void OnSettingsClick(IInspectable const&, Windows::UI::Xaml::RoutedEventArgs const&);
        void OnResumeClick(IInspectable const&, Windows::UI::Xaml::RoutedEventArgs const&);
        void OnDiagnosticsClick(IInspectable const&, Windows::UI::Xaml::RoutedEventArgs const&);

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
        void EnterStreamingUi();              // UI thread
        void LeaveStreamingUi();              // UI thread

        // The overlay is the ONLY chrome a running stream ever shows, and it
        // exists only while it is open. Opening it parks input forwarding, so
        // the controller drives the panel rather than the PC.
        void BuildOverlayChoices();           // UI thread, once
        void ShowOverlay();                   // UI thread
        void HideOverlay();                   // UI thread
        void ApplyStreamMode(uint32_t width, uint32_t height, uint32_t fps);

        void Append(hstring const& line);     // any thread
        void SetStatus(hstring const& text);  // any thread

        void OnSessionEvent(std::string const& json);
        void BeginStream();                   // UI thread; used by button AND handoff

        std::wstring m_log;
        std::string  m_stateDir;

        std::unique_ptr<echo::VideoRenderer> m_renderer;
        std::unique_ptr<echo::HevcDecoder>   m_decoder;
        std::unique_ptr<echo::EchoSession>   m_session;
        std::unique_ptr<echo::HostDiscovery> m_discovery;
        std::unique_ptr<echo::InputBridge>   m_input;

        std::vector<echo::DiscoveredHost> m_hosts;   // parallel to HostList items
        int  m_selected = -1;
        bool m_decoderReady = false;
        bool m_streaming = false;
        bool m_overlayOpen = false;

        // What the console is really putting on the wire to the TV, read back
        // from HdmiDisplayInformation rather than assumed from what we asked
        // for. Zero means we never got an answer (a PC, not a console).
        uint32_t m_outputWidth = 0;
        uint32_t m_outputHeight = 0;

        // What we ask the host to encode. Starts at the console's real output
        // size and is whatever the overlay last chose after that.
        uint32_t m_streamWidth = 1920;
        uint32_t m_streamHeight = 1080;
        uint32_t m_streamFps = 60;

        // True while a pairing session is open, so the `paired` event knows to
        // hand straight off to a stream instead of just reporting success.
        bool m_pairing = false;

        Windows::UI::Xaml::DispatcherTimer m_statsTimer{ nullptr };
    };
}

namespace winrt::EchoXbox::factory_implementation
{
    struct MainPage : MainPageT<MainPage, implementation::MainPage>
    {
    };
}
