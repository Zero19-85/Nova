// Precompiled header.
//
// C++/WinRT projection headers are large and every translation unit needs the
// same handful, so they live here rather than being included per-file. Adding a
// `winrt/…` include to a .cpp instead of here compiles, but slowly.
#pragma once

#include <windows.h>
#include <unknwn.h>
#include <restrictederrorinfo.h>
#include <hstring.h>

// Turn HRESULT failures from the projection into C++ exceptions rather than
// silent E_FAIL returns. Every winrt call below can throw; that is the design.
#include <winrt/Windows.Foundation.h>
#include <winrt/Windows.Foundation.Collections.h>
#include <winrt/Windows.ApplicationModel.h>
#include <winrt/Windows.ApplicationModel.Activation.h>
#include <winrt/Windows.ApplicationModel.Core.h>
#include <winrt/Windows.Storage.h>
#include <winrt/Windows.UI.Core.h>
#include <winrt/Windows.UI.Xaml.h>
#include <winrt/Windows.UI.Xaml.Controls.h>
#include <winrt/Windows.UI.Xaml.Controls.Primitives.h>
#include <winrt/Windows.UI.Xaml.Data.h>
#include <winrt/Windows.UI.Xaml.Interop.h>
#include <winrt/Windows.UI.Xaml.Markup.h>
#include <winrt/Windows.UI.Xaml.Media.h>
// Storyboard/DoubleAnimation, for the app drawer's accordion. The drawer
// animates a LAYOUT property, so it needs EnableDependentAnimation — see
// MainPage::AnimateDrawer.
#include <winrt/Windows.UI.Xaml.Media.Animation.h>
// KeyRoutedEventArgs, for the A-button gesture handler on MainPage. The whole
// dashboard is driven from PreviewKeyDown/Up now, so this is not optional.
#include <winrt/Windows.UI.Xaml.Input.h>
// VirtualKey — GamepadA and GamepadMenu specifically.
#include <winrt/Windows.System.h>
// The toast's accent bar and the dashboard's identity rule are Rectangles, and
// MainPage.g.h projects them — without this the generated header does not
// compile, and the error names the generated file rather than this omission.
#include <winrt/Windows.UI.Xaml.Shapes.h>
#include <winrt/Windows.UI.Xaml.Navigation.h>
#include <winrt/Windows.UI.ViewManagement.h>
#include <winrt/Windows.Gaming.Input.h>
#include <winrt/Windows.Devices.Enumeration.h>
#include <winrt/Windows.Data.Json.h>
#include <winrt/Windows.UI.h>

#include <string>
#include <vector>
#include <chrono>
