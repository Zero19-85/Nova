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
    namespace
    {
        // Set once in OnLaunched, read by the diagnostics screen. A plain bool
        // rather than an atomic: it is written before the first frame exists and
        // only ever read from the UI thread afterwards.
        bool g_nativeScaling = false;
    }

    bool App::NativeScaling() noexcept { return g_nativeScaling; }

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

        // ── Native pixels instead of 200% ───────────────────────────────────
        //
        // A console hands a XAML app a 1920x1080 LOGICAL view and composes it at
        // 200%, on the reasonable assumption that an app is read from a sofa and
        // would otherwise be a wall of small text. The cost is that every size
        // in the app is doubled before it reaches the panel: a 24 px label is 48
        // real pixels, a 1 px hairline is 2, and a 4K screen is being used to
        // display a 1080p layout. That is the "everything looks blown up"
        // this call fixes.
        //
        // `TrySetDisableLayoutScaling` is the documented opt-out, and it is the
        // scaling equivalent of the bounds-mode call above: both trade a default
        // that protects a careless app for control this one actually wants. With
        // it, the view is 3840x2160 logical at 100% and XAML lays out in real
        // pixels — which is the space `Ion.xaml`'s type ramp is authored in.
        //
        // The return value is not ignored and not fatal. It is false on a PC and
        // on any console that declines, and `MainPage::ApplyChromeScale` handles
        // that by measuring the view it actually got rather than trusting this
        // to have worked. Nothing downstream needs to know which happened.
        g_nativeScaling = ApplicationViewScaling::TrySetDisableLayoutScaling(true);

        Frame rootFrame{ nullptr };
        auto content = Window::Current().Content();
        if (content) {
            rootFrame = content.try_as<Frame>();
        }

        if (rootFrame == nullptr) {
            rootFrame = Frame();
            // The Frame is a full-screen rectangle between the CoreWindow and
            // the Page, and its default ground is the theme's page brush — a
            // lifted chrome near-black, not #000000. Stated here rather than
            // left to the theme override in Ion.xaml because this one is the
            // root of the visual tree: if anything above the Page is going to
            // put light on a dimming zone, it is this.
            rootFrame.Background(
                Media::SolidColorBrush(Windows::UI::Colors::Black()));
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
