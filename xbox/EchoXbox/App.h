#pragma once
#include "App.xaml.g.h"

namespace winrt::EchoXbox::implementation
{
    struct App : AppT<App>
    {
        App();
        void OnLaunched(Windows::ApplicationModel::Activation::LaunchActivatedEventArgs const&);

    private:
        void OnSuspending(IInspectable const&, Windows::ApplicationModel::SuspendingEventArgs const&);
        void OnResuming(IInspectable const&, IInspectable const&);
        void OnNavigationFailed(IInspectable const&,
                                Windows::UI::Xaml::Navigation::NavigationFailedEventArgs const&);
    };
}
