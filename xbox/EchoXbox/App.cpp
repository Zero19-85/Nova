#include "pch.h"
#include "App.h"
#include "MainPage.h"

using namespace winrt;
using namespace Windows::ApplicationModel;
using namespace Windows::ApplicationModel::Activation;
using namespace Windows::Foundation;
using namespace Windows::UI::Xaml;
using namespace Windows::UI::Xaml::Controls;
using namespace Windows::UI::Xaml::Navigation;
using namespace Windows::UI::ViewManagement;

namespace winrt::EchoXbox::implementation
{
    App::App()
    {
        InitializeComponent();

        // ── Kill the console's own cursor ──────────────────────────────
        //
        // An Xbox puts every XAML app into "mouse mode" by default: it draws a
        // system cursor and steers it with the LEFT STICK, converting the pad
        // into pointer input before the app sees any of it. For a launcher that
        // is a kindness. For a client that forwards a controller to a PC it is a
        // disaster in three ways at once — the left stick never reaches the
        // game, there are two cursors on the screen (the console's and the
        // host's), and the console's pointer emulation adds its own latency to
        // input that was already travelling over a network.
        //
        // `WhenRequested` says: give me raw gamepad input, and only synthesise a
        // pointer for a page that explicitly asks. No page here ever asks, so
        // the console's cursor never appears. A real USB mouse is unaffected —
        // this governs the EMULATED pointer, not physical pointer devices, so
        // `CoreIndependentInputSource` still receives a real mouse.
        //
        // Set here rather than in OnLaunched: the mode has to be decided before
        // the first view exists, and OnLaunched is already too late.
        RequiresPointerMode(ApplicationRequiresPointerMode::WhenRequested);

#if defined _DEBUG && !defined DISABLE_XAML_GENERATED_BREAK_ON_UNHANDLED_EXCEPTION
        UnhandledException([](IInspectable const&, UnhandledExceptionEventArgs const& e) {
            if (IsDebuggerPresent()) {
                auto errorMessage = e.Message();
                __debugbreak();
            }
        });
#endif
    }

    void App::OnLaunched(LaunchActivatedEventArgs const& e)
    {
        // ── The Xbox full-bleed opt-in ──────────────────────────────────────
        //
        // XAML on a console defaults to the TV-safe area: a ~48px inset on every
        // edge, so the app never draws where an old CRT would have overscanned.
        // For a dashboard that is correct and for a video stream it is not — a
        // 1080p picture in the safe area is scaled down and re-scaled by the TV,
        // which is both softer and letterboxed by a black frame the user cannot
        // explain.
        //
        // `UseCoreWindow` is the opt-out, and it is a deliberate promise: from
        // here on, keeping important UI away from the very edge is our job.
        ApplicationView::GetForCurrentView().SetDesiredBoundsMode(
            ApplicationViewBoundsMode::UseCoreWindow);

        Frame rootFrame{ nullptr };
        auto content = Window::Current().Content();
        if (content) {
            rootFrame = content.try_as<Frame>();
        }

        if (rootFrame == nullptr) {
            rootFrame = Frame();
            rootFrame.NavigationFailed({ this, &App::OnNavigationFailed });
            Window::Current().Content(rootFrame);
        }

        if (rootFrame.Content() == nullptr) {
            rootFrame.Navigate(xaml_typename<EchoXbox::MainPage>(), box_value(e.Arguments()));
        }

        // A console suspends the app whenever the user presses the Xbox button
        // and opens something else. That is not a crash and not a disconnect —
        // but it does stop our threads, so the session must be told.
        Suspending({ this, &App::OnSuspending });
        Resuming({ this, &App::OnResuming });

        Window::Current().Activate();
    }

    void App::OnSuspending(IInspectable const&, SuspendingEventArgs const&)
    {
        // Milestone 2 hangs the session teardown here. Worth stating early: a
        // suspend is NOT the same as a user ending a session. The host holds a
        // detached session for its grace period precisely so a resume can walk
        // back into it, so this must not send a goodbye — `echo_close` does, and
        // that is the wrong call here.
    }

    void App::OnResuming(IInspectable const&, IInspectable const&)
    {
        // Milestone 2: call echo_network_changed(handle). Coming back from
        // suspend is the console's version of a phone changing networks — the
        // sockets are stale even when the Wi-Fi never moved.
    }

    void App::OnNavigationFailed(IInspectable const&, NavigationFailedEventArgs const& e)
    {
        throw hresult_error(E_FAIL, hstring(L"Failed to load Page ") + e.SourcePageType().Name);
    }
}
