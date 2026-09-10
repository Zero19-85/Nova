#pragma once
#include "App.xaml.g.h"

namespace winrt::EchoXbox::implementation
{
    struct App : AppT<App>
    {
        App();
        void OnLaunched(Windows::ApplicationModel::Activation::LaunchActivatedEventArgs const&);

        // Whether the console agreed to hand this app native pixels rather than
        // a 200%-scaled 1080p view. Static because it is a property of the
        // process, asked once at launch and read later by the diagnostics
        // screen; MainPage does not act on it — it measures the view it was
        // actually given — but "the request was refused" and "the request
        // worked and something else undid it" look identical without it.
        static bool NativeScaling() noexcept;

    private:
        void OnSuspending(IInspectable const&, Windows::ApplicationModel::SuspendingEventArgs const&);
        void OnResuming(IInspectable const&, IInspectable const&);
        void OnNavigationFailed(IInspectable const&,
                                Windows::UI::Xaml::Navigation::NavigationFailedEventArgs const&);
    };
}
