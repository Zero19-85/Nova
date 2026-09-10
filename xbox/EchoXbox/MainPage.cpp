#include "pch.h"
#include "MainPage.h"
#include "MainPage.g.cpp"

#include "App.h"
#include "EchoBridge.h"

#include <algorithm>
#include <array>
#include <cmath>
// quiet_NaN, which is how a FrameworkElement's Height says "Auto".
#include <limits>
#include <string>

using namespace winrt;
using namespace Windows::Foundation;
using namespace Windows::Storage;
using namespace Windows::System;
using namespace Windows::UI::Core;
using namespace Windows::UI::Xaml;
using namespace Windows::UI::Xaml::Controls;
using namespace Windows::UI::Xaml::Input;

namespace winrt::EchoXbox::implementation
{
    namespace
    {
        constexpr int32_t kTextBuffer = 2048;

        // What we ask the console to output. An Xbox left to itself treats a
        // UWP app as a 1080p media player - see RequestBestHdmiMode.
        constexpr uint32_t kDesiredWidth  = 3840;
        constexpr uint32_t kDesiredHeight = 2160;

        constexpr uint32_t kStreamBitrate = 40000;   // the host budget caps this

        // The space every panel in MainPage.xaml is authored in. See
        // ApplyChromeScale.
        constexpr double kDesignWidth  = 3840.0;
        constexpr double kDesignHeight = 2160.0;

        // ── The decode ceiling is GONE ──────────────────────────────────────
        //
        // There used to be a pixel-rate budget here - 520M px/s, drawn through
        // ten blank 4K120 sessions - and a `DecodableFps` that refused any mode
        // above it. It is deleted rather than defaulted off, and the reason it
        // can be is that the sessions it was inferred from have since been
        // explained: Media Foundation handed the app container a SOFTWARE HEVC
        // decoder, and a software decoder was never going to do 4K120. The
        // number described that decoder, not this console.
        //
        // FFmpeg/D3D11VA is the decoder now, 4K120 is live, and a ceiling that
        // exists only to refuse the thing the port was built to do is worse
        // than no ceiling at all. What remains is the panel rate, which advises
        // and does not veto - asking for more frames than the TV can show is
        // waste worth naming, and still the user's call.
        //
        // The decoder probe is untouched and still reported on the diagnostics
        // screen. It was always the honest half of this: a measurement, next to
        // the guess that used to overrule it.
        uint32_t StreamFpsFor(uint32_t panelFps)
        {
            return panelFps >= 100 ? 120u : 60u;
        }

        // App 5, "Virtual Desktop" - NOT app 1.
        //
        // `app_launcher::uses_virtual_display` early-returns false for app 1
        // (Desktop), which is why a Desktop session mirrors the physical
        // monitor no matter what `headless_for_all_apps` says. App 5 is the
        // headless desktop: it launches no process at all, and it is what makes
        // the Worker activate the Virtual Display Driver and hand this session
        // its own monitor at the size we asked for.
        constexpr uint32_t kVirtualDesktopApp = 5;

        // What the app strip offers, and Virtual Desktop is deliberately not on
        // it: a double tap of A launches that, and listing it here would turn
        // the fast path into one more equal option in a list of five.
        //
        // The note on each is not decoration. "Mirror" and "Virtual Desktop"
        // sound like the same thing and behave completely differently - app 1
        // shows the PC's real screen at the PC's real resolution, and app 5
        // spawns a display sized to this television. Somebody picking blind
        // will choose wrong roughly half the time, which is exactly why the
        // strip carries a description that follows the focus rect.
        struct AppChoice { uint32_t id; wchar_t const* label; wchar_t const* note; };
        constexpr std::array<AppChoice, 4> kQuickApps{{
            { 2, L"Steam",     L"Big Picture on its own headless display, sized to this TV." },
            { 3, L"Xbox",      L"Game Bar on its own headless display, sized to this TV." },
            { 4, L"RetroArch", L"RetroArch on its own headless display, sized to this TV." },
            { 1, L"Mirror",    L"Maintenance Mode: Clones the host PC's physical screen (Non-headless)." },
        }};

        // ── The two A gestures ──────────────────────────────────────────────
        //
        // 400 ms is the platform's own double-tap window: longer starts catching
        // deliberate second presses, shorter is hard to hit with a thumb. It is
        // also how long a single tap is withheld, because the two are the same
        // measurement seen from either end.
        //
        // There is no hold gesture. It moved to Start, which is where a console
        // user looks for a menu anyway.
        constexpr auto kDoubleTap = std::chrono::milliseconds(400);

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

        // Put the focus rect on the first thing in a menu that can take it.
        //
        // A console has no other cursor: a panel that opens with focus still on
        // whatever was behind it is a panel the user cannot reach, and one that
        // focuses a DISABLED button is one they cannot leave either, because a
        // disabled control is skipped by directional navigation and the rect
        // lands nowhere. So the first ENABLED control wins.
        void FocusFirstEnabled(std::initializer_list<Control> controls)
        {
            for (auto const& control : controls) {
                if (control && control.IsEnabled()) {
                    control.Focus(FocusState::Programmatic);
                    return;
                }
            }
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
        BuildAppMenu();
        HookBackButton();
        HookGestures();
        StartupAsync();
    }

    // ── The 4K design space ─────────────────────────────────────────────────
    //
    // ChromeRoot is a fixed 3840x2160 canvas, and this is what makes that fixed
    // size correct on a view of any size.
    //
    // The app asks for native pixels at launch (App::OnLaunched), and when the
    // console agrees the view IS 3840x2160 logical, the scale computed here is
    // 1.0, and every number in Ion.xaml lands on the pixel it names. When it
    // does not agree - a PC, an older console, a future policy change - the view
    // is 1920x1080 logical, the scale is 0.5, and the same layout renders at
    // exactly the proportions it would have had anyway.
    //
    // Measuring rather than trusting is the point. `TrySetDisableLayoutScaling`
    // returns a bool that says whether the request was accepted, and that is a
    // different question from what the view ended up being; a layout built on
    // the answer to the first question would be wrong in every case where they
    // disagree, and silently.
    //
    // The scale is uniform and the remainder is centred, so a view that is not
    // 16:9 letterboxes the chrome instead of stretching it. The video is not
    // affected either way - the SwapChainPanel is outside ChromeRoot precisely
    // so that it is sized by the window and scaled by the renderer.
    void MainPage::ApplyChromeScale()
    {
        const double width  = ActualWidth();
        const double height = ActualHeight();
        if (width <= 0.0 || height <= 0.0) return;

        const double scale = std::min(width / kDesignWidth, height / kDesignHeight);
        if (scale <= 0.0) return;

        ChromeScale().ScaleX(scale);
        ChromeScale().ScaleY(scale);

        // RenderTransform does not move layout, so the leftover has to be taken
        // out by hand. Without this a 21:9 display would pin the whole UI to the
        // left edge.
        const double left = std::max(0.0, (width  - kDesignWidth  * scale) / 2.0);
        const double top  = std::max(0.0, (height - kDesignHeight * scale) / 2.0);
        ChromeRoot().Margin(ThicknessHelper::FromLengths(left, top, 0, 0));
    }

    // ── A, and why it is intercepted at the page ────────────────────────────
    //
    // The dashboard has no Stream or Pair button any more; A carries two
    // gestures instead, and a tap has to be told apart from a double tap. That
    // cannot hang off a focused control, because the control that has focus is
    // a host row in a ListView and the gesture is about the screen rather than
    // about the row.
    //
    // `PreviewKeyDown` is the tunnelling half of XAML's key routing: it starts
    // at the root and walks DOWN to whatever has focus, so handling it here
    // happens before any control sees the press. Marking it handled is what
    // stops A from also activating things - and, just as importantly, NOT
    // marking it is what lets A work normally the moment a menu is open. That
    // one flag (`GesturesArmed`) is the whole arbitration.
    //
    // Both edges are owned or neither is: `m_aOwned` is what keeps a release
    // from reaching a control that took focus between the press and the
    // release.
    //
    // ── The hold is GONE, and Start carries the menu ────────────────────────
    //
    // Holding A used to open the context menu. It was the wrong button for it
    // twice over: a console user looking for settings or a way to disconnect
    // presses Start, and a third gesture on A meant every press had to survive
    // a hold clock before it could mean anything. Start had nothing else to do
    // on this screen, so the menu is simply there now.
    //
    // Removing it also removed a bug rather than just a keybinding — see the
    // note on the down edge below.
    void MainPage::HookGestures()
    {
        PreviewKeyDown({ this, &MainPage::OnChromeKeyDown });
        PreviewKeyUp({ this, &MainPage::OnChromeKeyUp });

        // The design space is only real once the page has been measured.
        Loaded([this](auto&&...) {
            ApplyChromeScale();
            // Put focus somewhere real on boot. The host list is empty for the
            // first second or two of a cold start, and `Focus` on an empty
            // ListView fails — so without the fallback the focus rect can rest
            // on nothing at all until discovery lands.
            //
            // The A gesture no longer depends on this at all — it comes off the
            // pad thread, which does not know what focus is. But the D-pad does:
            // with the rect resting nowhere, moving the stick highlighted
            // nothing and the list looked inert. `FocusHostList` is a no-op
            // while the list is empty, and `RefreshHostList` calls it again the
            // moment the first host lands.
            FocusHostList();
            Focus(FocusState::Programmatic);
        });
        SizeChanged([this](auto&&...) { ApplyChromeScale(); });

        m_singleTapTimer = DispatcherTimer();
        m_singleTapTimer.Interval(kDoubleTap);
        m_singleTapTimer.Tick([this](auto&&...) {
            m_singleTapTimer.Stop();
            FireSingleTap();
        });
    }

    // Armed only on the bare dashboard. Every panel below is a place where A
    // means "press the thing I am pointing at", and taking that away would leave
    // the user looking at a menu they cannot operate.
    //
    // The app drawer is deliberately NOT in this list. It is part of the
    // dashboard rather than something covering it, and A has to keep reaching
    // the gesture while it is open — that is what closes it again. Which of the
    // two a press means is settled by `FocusInsideDrawer`, not by this flag.
    bool MainPage::GesturesArmed()
    {
        return !m_streaming && !m_overlayOpen && !m_actionMenuOpen &&
               !m_settingsOpen && !m_diagnosticsOpen && !m_endStreamOpen &&
               PinPanel().Visibility() == Visibility::Collapsed;
    }

    void MainPage::OnChromeKeyDown(IInspectable const&, KeyRoutedEventArgs const& args)
    {
        const auto key = args.Key();

        // ── Start: the menu ─────────────────────────────────────────────────
        //
        // Stop Stream, Stream, Pair and Settings. This is where a console user
        // looks for them, and it is the button the two-step teardown is
        // documented against: press Start, choose End Stream.
        //
        // Only on the dashboard. In a stream Start belongs to the game - it is
        // the pause button in most of them - and it reaches the host through the
        // pad thread, which is a completely separate path from XAML's keys, so
        // not handling it here is what forwards it.
        if (key == VirtualKey::GamepadMenu) {
            // A menu button toggles its menu. Pressing Start again to get rid of
            // what Start put on screen is the reflex on every console, and
            // leaving it to B alone means the button that opened the panel does
            // nothing while the panel is up.
            if (m_actionMenuOpen) {
                args.Handled(true);
                if (!args.KeyStatus().WasKeyDown) HideActionMenu();
                return;
            }
            if (!GesturesArmed()) return;
            args.Handled(true);
            if (args.KeyStatus().WasKeyDown) return;   // auto-repeat
            ShowActionMenu();
            return;
        }

        // ── A is NOT handled here, and this branch only counts it ───────────
        //
        // Two builds tried to drive the dashboard's A from this intercept and
        // it never fired once on the console — with focus on a host row, with
        // focus on the page, with the press marked handled and with it left
        // alone. GamepadMenu arrived through this same function every time,
        // which is what ruled out the intercept, the arming condition and the
        // focus rect together.
        //
        // A is the console's Accept button and XAML consumes it to invoke
        // whatever has focus; it is not ours to intercept from here. The
        // gesture now comes off the 250 Hz pad thread instead — see
        // `OnPadAccept` and the note on `InputBridge::SetSinks`.
        //
        // The counter stays because it answers a question that cost two round
        // trips: whether XAML delivers this key to the page at all. It is
        // expected to read zero forever. If it ever starts climbing, this
        // platform behaviour changed and the note above needs revisiting.
        if (key == VirtualKey::GamepadA) ++m_aXamlEdges;
    }

    // ── A, from the pad thread ──────────────────────────────────────────────
    //
    // Fired on the rising edge of the A button by `InputBridge`'s poll loop,
    // which means this arrives on the PAD THREAD. Everything below touches XAML
    // — a timer, a panel, a stream — so the first thing it does is hop to the
    // dispatcher, exactly like the overlay chord sink does.
    //
    // Getting that wrong is a silent failure rather than a crash: DispatcherTimer
    // methods called off the UI thread throw RPC_E_WRONG_THREAD inside a noexcept
    // pad loop, and the visible result is a gesture that does nothing at all.
    void MainPage::OnPadAccept()
    {
        Dispatcher().RunAsync(CoreDispatcherPriority::Normal, [this]() {
            ++m_aPadEdges;
            if (!GesturesArmed()) return;

            // Focus inside the drawer means this press is aimed at a tab, not
            // at the drawer itself. XAML is already delivering it there — its
            // copy of A was never suppressed — so the gesture stands aside
            // rather than launching Virtual Desktop out from under a user who
            // was choosing Steam.
            if (FocusInsideDrawer()) return;

            // ── Both gestures resolve on the PRESS ─────────────────────────
            //
            // A first press arms the timer; a second inside the window IS the
            // double tap and is acted on immediately. Nothing waits for a
            // release — the pad loop reports presses, and a release-based
            // design was what killed this gesture on the previous build.
            const auto now = std::chrono::steady_clock::now();
            const bool second = m_lastTapAt.time_since_epoch().count() != 0 &&
                                (now - m_lastTapAt) < kDoubleTap;
            if (second) {
                CancelPendingTap();
                m_lastTapAt = {};
                FireDoubleTap();
                return;
            }

            // A first press commits to nothing yet. Same delay-rather-than-
            // retract rule the Menu+View chord follows: a first tap is
            // indistinguishable from the first half of a double tap, and
            // retraction cannot work — by the time we know, the strip is open,
            // and closing it again is a second visible event nobody asked for.
            m_lastTapAt = now;
            m_singleTapTimer.Stop();
            m_singleTapTimer.Start();
        });
    }

    // Counts XAML-delivered A releases, and nothing else. Kept for the same
    // reason the down counter is: it says whether this key reaches the page at
    // all, which is the fork two builds were stuck on. Deliberately never
    // swallowed — nothing waits for it, and eating it would break the Click
    // that menu buttons fire on release.
    void MainPage::OnChromeKeyUp(IInspectable const&, KeyRoutedEventArgs const& args)
    {
        if (args.Key() == VirtualKey::GamepadA) ++m_aXamlUpEdges;
    }

    void MainPage::CancelPendingTap()
    {
        m_singleTapTimer.Stop();
    }

    // ── Getting the focus rect onto a host row ──────────────────────────────
    //
    // `HostList().Focus()` right after appending items does not work, and the
    // reported workaround is the proof: opening the Start menu and backing out
    // put the highlight on the host every time. That path calls the SAME
    // Focus() — the only difference is that it runs a second later.
    //
    // The reason is container realisation. A ListView creates the visual for a
    // row lazily, during layout; until that has happened there is no
    // ListViewItem to receive focus, and Focus() on the list itself has nothing
    // to hand it to and returns false. Appending an item and focusing in the
    // same tick is asking for a visual that does not exist yet.
    //
    // So: force the layout pass, then focus the CONTAINER rather than the list.
    // The posted retry covers the case where realisation is deferred anyway —
    // once, at Low priority, which runs after layout and render.
    void MainPage::FocusHostList()
    {
        if (m_streaming || m_hosts.empty()) return;

        auto tryFocus = [this]() -> bool {
            if (m_hosts.empty()) return false;
            HostList().UpdateLayout();
            const int index = HostList().SelectedIndex() >= 0 ? HostList().SelectedIndex() : 0;
            if (auto container = HostList().ContainerFromIndex(index).try_as<Control>()) {
                if (container.Focus(FocusState::Programmatic)) return true;
            }
            return HostList().Focus(FocusState::Programmatic);
        };

        if (tryFocus()) return;
        Dispatcher().RunAsync(CoreDispatcherPriority::Low,
                              [this, tryFocus]() { tryFocus(); });
    }

    // One tap: the app drawer, open or closed.
    void MainPage::FireSingleTap()
    {
        if (!GesturesArmed()) return;

        // Closing never needs a host, a pairing or anything else — it is the
        // undo of the press that opened it, and it has to work in every state
        // the drawer can be in.
        if (m_appDrawerOpen) { HideAppDrawer(); return; }

        // Same reasoning as the double tap: with one host on the network there
        // is nothing to disambiguate, and the drawer should name the machine it
        // is about to launch on rather than say "LAUNCH" at nobody.
        if (m_selected < 0 && !m_hosts.empty()) {
            HostList().SelectedIndex(0);
            m_selected = HostList().SelectedIndex();
        }

        // Unpaired hosts do not get a drawer. Every entry in it launches
        // something, and launching needs the fingerprint a PIN handshake earns —
        // so the strip would be four buttons that all answer with the same
        // refusal. Say the one useful thing instead, and say where to go.
        if (!SelectedHostPaired()) {
            ShowToast(L"NOT PAIRED   ::   press Start", L"IonAmber");
            return;
        }

        ShowAppDrawer();
    }

    // Two taps: the app that needs no choosing. Virtual Desktop is the reason
    // this client exists - it is the only entry that gets this console its own
    // monitor at the size of this television - so it is the gesture with no
    // menu in front of it.
    //
    // An unpaired host answers the same gesture with the pairing flow rather
    // than with a refusal. Pairing hands straight off to a stream when it
    // succeeds (see OnSessionEvent), so the gesture means the same thing in both
    // states: "put this console on that PC".
    // ── A double tap LAUNCHES. It never pairs. ──────────────────────────────
    //
    // It used to fall through to the pairing flow when the host was not paired
    // yet, on the reasoning that both gestures mean "put this console on that
    // PC". That is now deliberately not the design: pairing lives in the Start
    // menu and nowhere else.
    //
    // The reason is what the gesture IS. A double tap is the fastest thing on
    // this screen and the one a thumb produces by accident, and pairing is not
    // a fast action — it opens a session, puts a PIN on the television and asks
    // somebody to walk to another room. Reaching that by mashing A is a worse
    // failure than not reaching it at all. A deliberate menu is the right home
    // for it, and the toast below is what points there.
    void MainPage::FireDoubleTap()
    {
        if (!GesturesArmed()) return;

        // ── Nothing selected is not a reason to refuse ──────────────────────
        //
        // Selection follows the focus rect, so it is only set once the list has
        // been touched or a refresh has put it there. A user who boots the app,
        // sees one host appear and immediately double-taps has selected nothing
        // at all. With one host on the network there is no ambiguity to resolve,
        // so resolve it rather than answering "no host selected".
        if (m_selected < 0 && !m_hosts.empty()) {
            HostList().SelectedIndex(0);
            m_selected = HostList().SelectedIndex();
        }

        // Unpaired is answered the same way a single tap answers it, so the two
        // gestures never disagree about what state the console is in.
        if (!SelectedHostPaired()) {
            ShowToast(L"NOT PAIRED   ::   press Start", L"IonAmber");
            SetStatus(L"not paired - press Start, then Pair");
            return;
        }

        // Launching supersedes the drawer: it was a way to choose an app, and
        // an app has just been chosen.
        HideAppDrawer();

        m_selectedApp = kVirtualDesktopApp;

        if (CanStream()) {
            SetStatus(L"starting Virtual Desktop...");
            BeginStream();
            return;
        }

        // ── Every refusal is SEEN ───────────────────────────────────────────
        //
        // The gesture used to decline on the status line alone — small text,
        // top left, on a screen somebody is looking at the middle of. From
        // three metres that is indistinguishable from a dead button, which is
        // precisely how it was reported.
        std::wstring why;
        if (m_hosts.empty()) {
            why = L"NO HOST FOUND   ::   still searching the network";
        } else if (m_selected < 0) {
            why = L"NO HOST SELECTED   ::   move to one with the stick";
        } else if (!m_decoderReady) {
            why = L"NO DECODER   ::   cannot stream";
        } else if (!m_hosts[m_selected].Usable()) {
            why = L"NO RELAY   ::   this host advertised none";
        } else {
            why = L"CANNOT START   ::   see Diagnostics";
        }
        ShowToast(why, L"IonAmber");
        SetStatus(hstring(why));
    }

    // ── B, which the console thinks is Back ─────────────────────────────────
    //
    // Xbox maps B to system navigation. An unhandled BackRequested at the root
    // of an app's navigation stack means "leave the app", so pressing B while
    // streaming closed Echo outright - on a button most games bind to
    // something, which makes it roughly the worst possible key to lose.
    //
    // The pad path is untouched by this: B reaches the host through the 250 Hz
    // `Windows.Gaming.Input` poll, which is a completely separate mechanism
    // from XAML's back stack. So marking the event handled does not "forward"
    // anything - it only stops the SHELL from acting on a press the game is
    // already receiving.
    //
    // The order below is the order the panels were stacked in, innermost first.
    // Getting it wrong closes the wrong screen, and the confirmation is the one
    // that must unwind first: it can be raised from two places and it is the
    // only panel in the app whose other button is destructive.
    //
    // Deliberately NOT handled on the bare dashboard: B leaving the app from the
    // host picker is exactly what an Xbox user expects, and swallowing it there
    // would leave the app with no way out at all.
    void MainPage::HookBackButton()
    {
        auto manager = Windows::UI::Core::SystemNavigationManager::GetForCurrentView();
        if (!manager) return;

        m_backToken = manager.BackRequested(
            [this](IInspectable const&,
                   Windows::UI::Core::BackRequestedEventArgs const& args) {
                if (PinPanel().Visibility() == Visibility::Visible) {
                    // The PIN screen disarms every gesture on the dashboard, so
                    // while it is up A and Start do nothing by design. That is
                    // correct right until the pairing attempt stops progressing
                    // — a PIN nobody types, a host that goes away mid-handshake
                    // — at which point the screen is a dead end with no way back
                    // to a client that cannot pair. B is the way out.
                    PinPanel().Visibility(Visibility::Collapsed);
                    m_pairing = false;
                    if (m_session) m_session->Close();
                    SetStatus(L"pairing cancelled");
                    UpdateHostState();
                    args.Handled(true);
                } else if (m_endStreamOpen) {
                    HideEndStream();
                    args.Handled(true);
                } else if (m_diagnosticsOpen) {
                    HideDiagnostics();
                    args.Handled(true);
                } else if (m_settingsOpen) {
                    HideSettings();
                    args.Handled(true);
                } else if (m_appDrawerOpen) {
                    HideAppDrawer();
                    args.Handled(true);
                } else if (m_actionMenuOpen) {
                    HideActionMenu();
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
        // singleton that outlives the page - so an un-revoked token is a call
        // into a destroyed MainPage on the next B press.
        if (m_backToken.value) {
            if (auto manager = Windows::UI::Core::SystemNavigationManager::GetForCurrentView()) {
                manager.BackRequested(m_backToken);
            }
            m_backToken = {};
        }

        // A running DispatcherTimer keeps firing into a lambda that captured
        // `this`. Stopping them is not tidiness; a tick after this point is a
        // call into a destroyed page.
        if (m_singleTapTimer) m_singleTapTimer.Stop();
        if (m_statsTimer)     m_statsTimer.Stop();
        if (m_toastTimer)     m_toastTimer.Stop();

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

        // The escape hatch, read once. A hand-written echo.json overrides
        // discovery entirely; the native path never writes it.
        m_configOverride = echo::LoadConfigOverride(m_stateDir);

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
                SetStatus(L"bridge self-test failed - cannot continue");
                co_return;
            }
        }

        // 2. Take the display back from the shell. Must happen BEFORE the swap
        //    chain exists: the request is what makes the composition scale
        //    report 2.0, and the swap chain is sized from that.
        SetStatus(L"requesting 4K output...");
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

        // 3. What we ask the HOST to encode is the console's real output size -
        //    read back from the console, not inferred from the request above and
        //    NOT taken from the swap chain.
        //
        //    The swap chain was the wrong source and it is why the stream was
        //    stuck at 1080p: an Xbox composes a XAML app's surface at 1920x1080
        //    whatever the TV is doing, so sizing the request from the back
        //    buffer asked a 4K console for a 1080p desktop and then upscaled it.
        //    The virtual monitor the host spawns should be the size of the
        //    SCREEN, and the renderer scales into whatever surface XAML gives
        //    us - which it was already doing for free.
        if (hdmi.width >= 1280 && hdmi.height >= 720) {
            m_outputWidth = hdmi.width;
            m_outputHeight = hdmi.height;
            m_outputHz = static_cast<uint32_t>(hdmi.refreshHz + 0.5);
            m_streamWidth = hdmi.width;
            m_streamHeight = hdmi.height;
            m_streamFps = StreamFpsFor(m_outputHz);
            wchar_t line[160]{};
            swprintf_s(line, L"output      %ux%u @ %.0f Hz - asking the host for this",
                       hdmi.width, hdmi.height, hdmi.refreshHz);
            Append(hstring(line));
        } else {
            Append(L"output      unknown - falling back to the swap chain's size");
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
        // leave the GPU. Sized to the back buffer as a first guess; the decoder
        // renegotiates to whatever the stream actually carries - which is also
        // what makes a live resolution change work at all.
        //
        // The probe is a measurement kept for the diagnostics screen. It used to
        // also set a ceiling that refused modes; it no longer does, and the note
        // where that lived says why.
        m_probe = echo::HevcDecoder::Probe(m_renderer->Device());
        report += L"\n" + m_probe.Report();

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
            // toggled - the chord or the overlay button.
            [this](bool on) { OnMouseModeChanged(on); },
            // A, on the dashboard. The pad thread is the ONLY source that
            // reaches this page: XAML consumes GamepadA for its own Accept
            // handling and never routed it here. See OnPadAccept.
            [this] { OnPadAccept(); });

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
            SetStatus(L"discovery unavailable - see the log");
            return;
        }
        SetStatus(L"searching for Nova on the network...");
    }

    void MainPage::OnRescanClick(IInspectable const&, RoutedEventArgs const&)
    {
        if (m_discovery) m_discovery->Stop();
        HostList().Items().Clear();
        m_hosts.clear();
        m_selected = -1;
        UpdateHostState();
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

        // Whether this refresh is the moment the list stops being empty. The
        // dashboard has no buttons left, so the host rows are the only focusable
        // thing on it — and a console with the focus rect nowhere has no cursor
        // at all until the user guesses that a direction press will summon one.
        const bool wasEmpty = m_hosts.empty();

        m_hosts = m_discovery->Hosts();
        HostList().Items().Clear();

        int restore = -1;
        for (size_t i = 0; i < m_hosts.size(); ++i) {
            auto const& host = m_hosts[i];
            const auto stored = echo::LoadHostFingerprint(m_stateDir, host.address);

            // Ion, and the palette comes from the dictionary rather than from
            // literals here - the whole point of one theme file is that a
            // colour is never typed twice. `IonBrush` is the lookup.
            //
            // Presence is deliberately TWO states here and three on Android.
            // Android probes a host's relay over TCP to tell "one hop away"
            // from "no route at all"; this client has no equivalent yet, and a
            // badge that claims a distinction it did not measure is worse than
            // one that admits it only knows what mDNS told it. So a discovered
            // host is `ONLINE // LAN` - which is exactly what a live mDNS
            // record means - and the third state waits for a real probe.
            const bool paired = !stored.empty();
            std::wstring badge = paired ? L"PAIRED" : L"NOT PAIRED";
            badge += L"   ONLINE // LAN";
            if (host.relayUrl.empty()) badge += L"   no relay advertised";

            StackPanel row;
            row.Margin(ThicknessHelper::FromLengths(6, 16, 6, 16));
            row.Spacing(4);

            TextBlock name;
            name.Text(hstring(host.name));
            name.FontSize(42);
            name.Foreground(IonBrush(L"IonText"));

            TextBlock address;
            address.Text(hstring(host.address));
            address.FontSize(27);
            address.FontFamily(Media::FontFamily(L"Consolas"));
            address.Foreground(IonBrush(L"IonTextDim"));

            // The badge line: a dot, then the state. Green is reserved for
            // "the network answered" and nothing else may use it - which is
            // why a paired-but-unseen host would not get one.
            StackPanel badgeRow;
            badgeRow.Orientation(Orientation::Horizontal);
            badgeRow.Spacing(13);
            badgeRow.Margin(ThicknessHelper::FromLengths(0, 6, 0, 0));

            Shapes::Ellipse dot;
            dot.Width(13);
            dot.Height(13);
            dot.VerticalAlignment(VerticalAlignment::Center);
            dot.Fill(IonBrush(L"IonMatrix"));

            TextBlock state;
            state.Text(hstring(badge));
            state.FontSize(24);
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
            ? hstring(L"SEARCHING THE NETWORK...")
            : hstring(std::to_wstring(m_hosts.size()) +
                      (m_hosts.size() == 1 ? L" HOST FOUND" : L" HOSTS FOUND")));

        if (restore >= 0) {
            HostList().SelectedIndex(restore);
        } else if (m_selected < 0 && !m_hosts.empty()) {
            HostList().SelectedIndex(0);   // one host is the common case
        }

        // Only on the empty-to-populated edge. Doing it on every refresh would
        // yank the focus rect back to the list each time an mDNS TXT record
        // updated, which is often and at no notice.
        if (wasEmpty && !m_hosts.empty() && !m_streaming) {
            FocusHostList();
        }
        UpdateHostState();
    }

    void MainPage::OnHostSelectionChanged(IInspectable const&, SelectionChangedEventArgs const&)
    {
        const int previous = m_selected;
        m_selected = HostList().SelectedIndex();

        // The drawer belongs to ONE host — its header names it and its buttons
        // launch on it. Moving to another host while it is open would leave one
        // machine's name above another machine's launch buttons, which is the
        // kind of mistake that only shows up after something has started on the
        // wrong PC. Re-title it for a paired host; close it for anything else.
        if (m_appDrawerOpen && m_selected != previous) {
            if (SelectedHostPaired() && m_selected >= 0 &&
                m_selected < static_cast<int>(m_hosts.size())) {
                AppMenuHeader().Text(hstring(L"LAUNCH ON " + m_hosts[m_selected].name));
            } else {
                HideAppDrawer();
            }
        }

        UpdateHostState();
    }

    // What the dashboard says about the selected host. There are no buttons to
    // enable any more, so this is a status line and a badge rather than a set of
    // IsEnabled flags - the gestures answer for themselves when they cannot run,
    // which is the only moment the reason is worth reading.
    void MainPage::UpdateHostState()
    {
        RefreshHeldBadge();

        if (m_streaming || m_ending) return;

        // A held session outranks anything about the selected host. It is the
        // one piece of state on this screen that costs the PC something while it
        // lasts, so it stays on the status line until it is resolved rather than
        // being overwritten by the next list refresh.
        if (m_hostHolding) {
            SetStatus(L"the host is still holding its display  -  press Start, then Stop Stream");
            return;
        }

        const bool valid = m_selected >= 0 && m_selected < static_cast<int>(m_hosts.size());
        if (!valid) {
            SetStatus(m_hosts.empty() ? L"searching for Nova on the network..."
                                      : L"pick a host");
            return;
        }

        auto const& host = m_hosts[m_selected];
        const auto stored = echo::LoadHostFingerprint(m_stateDir, host.address);
        if (stored.empty()) {
            SetStatus(hstring(host.name + L"  -  not paired yet. Press A twice to pair and stream."));
        } else if (!host.Usable()) {
            SetStatus(hstring(host.name + L"  -  no relay advertised, cannot stream"));
        } else {
            SetStatus(hstring(host.name + L"  -  ready"));
        }
    }

    // Pairing needs an address. Streaming needs the fingerprint the handshake
    // earned plus a relay the host advertised - or an echo.json that supplies
    // both by hand.
    bool MainPage::CanStream() const
    {
        if (m_streaming || !m_decoderReady) return false;
        if (!m_configOverride.empty()) return true;
        if (m_selected < 0 || m_selected >= static_cast<int>(m_hosts.size())) return false;
        auto const& host = m_hosts[m_selected];
        return !echo::LoadHostFingerprint(m_stateDir, host.address).empty() && host.Usable();
    }

    bool MainPage::CanPair() const
    {
        if (m_streaming) return false;
        if (m_selected < 0 || m_selected >= static_cast<int>(m_hosts.size())) return false;
        return !m_hosts[m_selected].address.empty();
    }

    // ── The app strip: one tap of A ─────────────────────────────────────────

    void MainPage::BuildAppMenu()
    {
        AppMenuRow().Children().Clear();
        for (auto const& app : kQuickApps) {
            Button button;
            button.Content(box_value(hstring(app.label)));
            button.Style(IonStyle(app.id == m_selectedApp ? L"IonActiveButton" : L"IonButton"));

            const uint32_t id = app.id;
            wchar_t const* note = app.note;

            // The hover description. GotFocus rather than PointerEntered,
            // because on a console there is no pointer: the focus rect IS the
            // hover, and it is moved with the stick.
            button.GotFocus([this, note](auto&&...) {
                AppMenuHint().Text(hstring(note));
            });
            button.Click([this, id, note](auto&&...) {
                m_selectedApp = id;
                AppMenuHint().Text(hstring(note));
                HideAppDrawer();
                if (CanStream()) {
                    SetStatus(L"starting...");
                    BeginStream();
                    return;
                }
                // The strip launches; it does not pair, for the same reason the
                // double tap does not. Pairing has one home and it is the Start
                // menu — see FireDoubleTap.
                const bool unpaired =
                    m_selected < 0 || m_selected >= static_cast<int>(m_hosts.size()) ||
                    echo::LoadHostFingerprint(m_stateDir,
                                              m_hosts[m_selected].address).empty();
                const std::wstring why = unpaired
                    ? std::wstring(L"NOT PAIRED   ::   press Start, then Pair")
                    : std::wstring(L"CANNOT START   ::   see Diagnostics");
                ShowToast(why, L"IonAmber");
                SetStatus(hstring(why));
            });
            AppMenuRow().Children().Append(button);
        }
    }

    // Whether the selected host has completed a PIN handshake. The drawer and
    // both A gestures hang off this, so it is one function rather than the same
    // three-line lookup written out at each site.
    bool MainPage::SelectedHostPaired() const
    {
        if (m_selected < 0 || m_selected >= static_cast<int>(m_hosts.size())) return false;
        return !echo::LoadHostFingerprint(m_stateDir, m_hosts[m_selected].address).empty();
    }

    void MainPage::ShowAppDrawer()
    {
        // Never for an unpaired host. The callers check too — this is the
        // backstop, because the drawer's whole contents are actions that cannot
        // succeed without a fingerprint, and offering them anyway is how a menu
        // teaches somebody that the app is broken.
        if (!SelectedHostPaired()) return;
        m_appDrawerOpen = true;

        // The highlight marks what a plain Stream would launch, which is not
        // necessarily anything in this strip — Virtual Desktop is the default
        // and is deliberately absent. So nothing is highlighted in that case,
        // and that is honest rather than a missing state.
        auto children = AppMenuRow().Children();
        for (uint32_t i = 0; i < children.Size() && i < kQuickApps.size(); ++i) {
            if (auto button = children.GetAt(i).try_as<Button>()) {
                button.Style(IonStyle(kQuickApps[i].id == m_selectedApp ? L"IonActiveButton"
                                                                       : L"IonButton"));
            }
        }

        if (m_selected >= 0 && m_selected < static_cast<int>(m_hosts.size())) {
            AppMenuHeader().Text(hstring(L"LAUNCH ON " + m_hosts[m_selected].name));
        } else {
            AppMenuHeader().Text(L"LAUNCH");
        }
        AppMenuHint().Text(hstring(kQuickApps[0].note));

        // ── Measure through real layout, then animate to it ─────────────────
        //
        // Setting Height to Auto and forcing a layout pass is what produces the
        // natural height; measuring the content by hand gets the wrapping of
        // the hint line wrong, because that depends on the width the row
        // actually gets. Nothing is drawn between these lines — rendering
        // happens after this handler returns — so the jump to full size and
        // back to zero is never on screen.
        AppDrawer().Visibility(Visibility::Visible);
        AppDrawer().Height(std::numeric_limits<double>::quiet_NaN());
        AppDrawer().UpdateLayout();
        const double target = AppDrawer().ActualHeight();
        AppDrawer().Height(0);

        AnimateDrawer(0.0, target, true);

        // Focus deliberately STAYS on the host list. That is what lets a second
        // A close the drawer again: with focus inside it, A belongs to the tab
        // it is resting on. Down moves into the strip when the user wants it.
    }

    void MainPage::HideAppDrawer()
    {
        if (!m_appDrawerOpen) return;
        m_appDrawerOpen = false;
        AnimateDrawer(AppDrawer().ActualHeight(), 0.0, false);
        if (!m_streaming) FocusHostList();
    }

    // The accordion itself.
    //
    // `EnableDependentAnimation` is not optional: Height is a layout property,
    // and XAML refuses to animate those off the composition thread unless the
    // animation says it knows it is asking for per-frame layout. Without it
    // this silently does nothing at all — the panel simply appears at its final
    // size, which looks like the animation code was never called.
    void MainPage::AnimateDrawer(double from, double to, bool openWhenDone)
    {
        namespace anim = Windows::UI::Xaml::Media::Animation;

        // One storyboard at a time. Two of them driving the same Height leaves
        // the drawer stuck at whichever value lost the race.
        if (m_drawerStory) m_drawerStory.Stop();

        anim::DoubleAnimation slide;
        slide.From(from);
        slide.To(to);
        // `Duration` lives in Windows::UI::Xaml, not in the Animation namespace
        // its only users are in.
        slide.Duration(Windows::UI::Xaml::Duration{
            Windows::Foundation::TimeSpan{ std::chrono::milliseconds(160) } });
        slide.EnableDependentAnimation(true);

        anim::CubicEase ease;
        ease.EasingMode(anim::EasingMode::EaseOut);
        slide.EasingFunction(ease);

        anim::Storyboard story;
        story.Children().Append(slide);
        anim::Storyboard::SetTarget(slide, AppDrawer());
        anim::Storyboard::SetTargetProperty(slide, L"Height");

        story.Completed([this, openWhenDone](auto&&...) {
            if (openWhenDone) {
                // Back to Auto so the drawer follows its own content afterwards
                // — a host name of a different length, or a hint line that wraps
                // to two rows, must not be clipped by a height measured once.
                AppDrawer().Height(std::numeric_limits<double>::quiet_NaN());
            } else {
                AppDrawer().Height(0);
                AppDrawer().Visibility(Visibility::Collapsed);
            }
        });

        m_drawerStory = story;
        story.Begin();
    }

    // A Grid does not clip its children, so without this the card would spill
    // over the hint bar for the whole 160 ms of a collapse. Driven from
    // SizeChanged so it tracks every frame the animation produces.
    void MainPage::OnAppDrawerSizeChanged(IInspectable const&,
                                          SizeChangedEventArgs const& args)
    {
        Windows::Foundation::Rect rect{ 0.0f, 0.0f,
                                        args.NewSize().Width, args.NewSize().Height };
        AppDrawerClip().Rect(rect);
    }

    // Where the focus rect is, relative to the drawer.
    //
    // This is the arbitration between the gesture and the buttons: A on the
    // host list toggles the drawer, and A on a tab inside it launches that app.
    // Both are the same physical press arriving on the same pad thread, so the
    // only thing that can tell them apart is what the press is pointing AT.
    bool MainPage::FocusInsideDrawer()
    {
        auto focused = Windows::UI::Xaml::Input::FocusManager::GetFocusedElement()
                           .try_as<Windows::UI::Xaml::DependencyObject>();
        auto drawer = AppDrawer().as<Windows::UI::Xaml::DependencyObject>();
        while (focused) {
            if (focused == drawer) return true;
            focused = Windows::UI::Xaml::Media::VisualTreeHelper::GetParent(focused);
        }
        return false;
    }

    // ── The action menu: Start ──────────────────────────────────────────────

    void MainPage::ShowActionMenu()
    {
        m_actionMenuOpen = true;

        // ── Pair must never be greyed out just because nothing has focus ────
        //
        // This menu is now the ONLY way to pair, which makes `CanPair` the
        // gate on the whole feature rather than one route to it. `CanPair`
        // wants a selected host, selection follows the focus rect, and on a
        // freshly-booted console that has not been touched there is no
        // selection — so the sole entry point to pairing would open with its
        // Pair button disabled, and a disabled control cannot even take focus
        // on a gamepad. The button would be visible, greyed, and unreachable.
        //
        // Same resolution as the taps: with hosts on the list there is nothing
        // ambiguous about which one, so pick the first rather than refuse.
        if (m_selected < 0 && !m_hosts.empty()) {
            HostList().SelectedIndex(0);
            m_selected = HostList().SelectedIndex();
        }

        const bool something = (m_session && m_session->IsOpen()) || m_hostHolding;
        ActionStopButton().IsEnabled(something && !m_ending);
        ActionStreamButton().IsEnabled(CanStream());
        ActionPairButton().IsEnabled(CanPair());

        std::wstring subtitle;
        if (m_hostHolding) {
            subtitle = L"the host is still holding its display for " +
                       (m_heldHostName.empty() ? std::wstring(L"this console") : m_heldHostName);
        } else if (m_selected >= 0 && m_selected < static_cast<int>(m_hosts.size())) {
            subtitle = m_hosts[m_selected].name;
        } else {
            subtitle = L"no host selected";
        }
        ActionMenuSubtitle().Text(hstring(subtitle));

        ActionMenuRoot().Visibility(Visibility::Visible);
        FocusFirstEnabled({ ActionStopButton(), ActionStreamButton(),
                            ActionPairButton(), ActionSettingsButton() });
    }

    void MainPage::HideActionMenu()
    {
        m_actionMenuOpen = false;
        ActionMenuRoot().Visibility(Visibility::Collapsed);
        if (!m_streaming) FocusHostList();
    }

    void MainPage::OnActionStopClick(IInspectable const&, RoutedEventArgs const&)
    {
        HideActionMenu();
        ShowEndStream();
    }

    void MainPage::OnActionStreamClick(IInspectable const&, RoutedEventArgs const&)
    {
        HideActionMenu();
        SetStatus(L"starting...");
        BeginStream();
    }

    void MainPage::OnActionPairClick(IInspectable const&, RoutedEventArgs const&)
    {
        HideActionMenu();
        BeginPairing();
    }

    // ── Settings ────────────────────────────────────────────────────────────

    void MainPage::OnSettingsClick(IInspectable const&, RoutedEventArgs const&)
    {
        ShowSettings();
    }

    void MainPage::ShowSettings()
    {
        m_settingsOpen = true;
        // The action menu is closed rather than left underneath: settings is a
        // screen, and a menu showing through it would suggest B returns there
        // when it returns to the dashboard.
        if (m_actionMenuOpen) {
            m_actionMenuOpen = false;
            ActionMenuRoot().Visibility(Visibility::Collapsed);
        }
        SettingsRoot().Visibility(Visibility::Visible);
        SettingsCloseButton().Focus(FocusState::Programmatic);
    }

    void MainPage::HideSettings()
    {
        m_settingsOpen = false;
        SettingsRoot().Visibility(Visibility::Collapsed);
        if (!m_streaming) FocusHostList();
    }

    void MainPage::OnSettingsCloseClick(IInspectable const&, RoutedEventArgs const&)
    {
        HideSettings();
    }

    // The microphone level, which currently travels nowhere.
    //
    // The control is here ahead of the feature deliberately, and it is a real
    // control rather than a disabled one: on a console a disabled control cannot
    // take focus, so it is invisible to the only input device there is, and the
    // point of putting it in early is to settle the layout and the navigation
    // order before the audio path arrives. What it is NOT is a lie - the panel
    // says in words that nothing is captured into it yet.
    //
    // When the capture side lands, the uplink it needs already exists on the
    // bridge (`echo_send_mic`, one raw Opus packet per call).
    void MainPage::OnMicLevelChanged(IInspectable const&,
                                     Primitives::RangeBaseValueChangedEventArgs const& args)
    {
        m_micLevel = static_cast<uint32_t>(std::lround(args.NewValue()));
        MicValueText().Text(m_micLevel == 0 ? hstring(L"off")
                                            : hstring(std::to_wstring(m_micLevel) + L"%"));
    }

    // ── Pair ────────────────────────────────────────────────────────────────

    // The one place pairing starts. Reached only from the Start menu.
    void MainPage::BeginPairing()
    {
        if (!CanPair()) {
            ShowToast(L"NO HOST SELECTED   ::   nothing to pair with", L"IonAmber");
            SetStatus(L"no host selected");
            return;
        }
        if (!m_session) {
            // Only reachable if the renderer failed at startup, which already
            // said so. Saying it again here beats dereferencing nothing.
            ShowToast(L"NOT READY   ::   see Diagnostics", L"IonAmber");
            return;
        }
        auto const& host = m_hosts[m_selected];

        m_session->Close();
        m_pairing = true;

        const auto config = echo::BuildPairConfig(m_stateDir, host);
        std::wstring error;
        if (m_session->Pair(config, [this](std::string const& json) { OnSessionEvent(json); }, error)) {
            SetStatus(L"pairing with " + hstring(host.name) + L"...");
            Append(L"pair        opening a pairing session with " + hstring(host.address));
        } else {
            // A refusal here is the end of the only pairing route in the app, so
            // it is said out loud and written to the log. It used to land on the
            // status line alone, which is where a failure goes to be missed.
            m_pairing = false;
            ShowToast(L"PAIRING REFUSED   ::   see Diagnostics", L"IonAmber");
            SetStatus(hstring(error));
            Append(L"pair        refused: " + hstring(error));
        }
    }

    // ── Stream ──────────────────────────────────────────────────────────────

    void MainPage::BeginStream()
    {
        if (!m_decoderReady) { SetStatus(L"no decoder - cannot stream"); return; }

        std::string config = m_configOverride;
        std::wstring hostName;
        if (!config.empty()) {
            Append(L"config      using echo.json override");
            hostName = L"the configured host";
        } else {
            if (m_selected < 0 || m_selected >= static_cast<int>(m_hosts.size())) {
                SetStatus(L"no host selected");
                return;
            }
            auto const& host = m_hosts[m_selected];
            hostName = host.name;

            const auto fingerprint = echo::LoadHostFingerprint(m_stateDir, host.address);
            if (fingerprint.empty()) { SetStatus(L"pair with this host first"); return; }
            if (!host.Usable())      { SetStatus(L"host advertised no relay - cannot stream"); return; }

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
            // Remembered for the teardown, not for the stream: `echo_release`
            // needs a config and there is no session left to ask by the time
            // anybody wants one. Stored on the way IN because a detached client
            // has nothing else to reconstruct it from.
            m_heldConfig = config;
            m_heldHostName = hostName;
            // A new session supersedes whatever was being held - the host cannot
            // be holding two, and the one it holds now is this.
            m_hostHolding = false;
            RefreshHeldBadge();
            SetStatus(L"connecting to " + hstring(hostName) + L"...");
        } else {
            SetStatus(hstring(error));
        }
    }

    // ── Two-step teardown ───────────────────────────────────────────────────
    //
    // The whole point is that these are two different acts and only one of them
    // is destructive.
    //
    // LEAVING a stream is common: somebody wants the console back for a minute,
    // or the app is being put down mid-session. The host holds the virtual
    // display, the desktop arrangement and whatever is running on it for its
    // detach grace period, so coming back is instant and nothing is lost. That
    // costs the PC a monitor it is not currently showing to anyone, which is
    // exactly the trade a user makes when they walk away for a moment.
    //
    // ENDING it is rarer and cannot be undone from here: the host tears the
    // session down, drops the virtual display, and puts the PC's own monitors
    // back. Doing that by accident - from the same button that means "hide this
    // panel" - is how somebody loses a desktop full of windows, so it is behind
    // a button nothing else uses and a confirmation nothing else raises.
    //
    // Which is why `Detach` had to exist at all: before this, closing the
    // session sent the host a goodbye, and every way out of a stream was the
    // destructive one.
    void MainPage::DetachStream()
    {
        if (m_session) m_session->Detach();
        m_pairing = false;

        m_hostHolding = !m_heldConfig.empty();

        // Otherwise the last frame of the stream stays on the TV behind the
        // dashboard, which reads as a frozen stream rather than a finished one.
        if (m_renderer) m_renderer->ShowBackground();
        LeaveStreamingUi();
        Append(L"session     left the stream; the host keeps its display for the grace period");
        SetStatus(m_hostHolding
            ? hstring(L"left the stream - the host is still holding its display. Start: End Stream.")
            : hstring(L"left the stream"));
    }

    void MainPage::ShowEndStream()
    {
        m_endStreamOpen = true;

        const bool live = m_session && m_session->IsOpen();
        std::wstring body;
        if (live) {
            body = L"This tells the host to stop streaming and hand its display back.\n"
                   L"The virtual monitor goes away and the PC's own screens return.";
        } else if (m_hostHolding) {
            body = L"The host is still holding a display for this console"
                   + (m_heldHostName.empty() ? std::wstring() : L" on " + m_heldHostName) +
                   L".\nEnding the session releases it and gives the PC its screens back.\n\n"
                   L"Leave it held if you are coming back shortly - reconnecting resumes\n"
                   L"into the same desktop with the same windows on it.";
        } else {
            body = L"Nothing is running and nothing is being held.";
        }
        EndStreamText().Text(hstring(body));

        EndStreamConfirmButton().IsEnabled((live || m_hostHolding) && !m_ending);
        EndStreamRoot().Visibility(Visibility::Visible);

        // The cancel button takes focus, not the destructive one. On a console
        // the focus rect is where the next A press lands, and this panel is
        // raised by a button a thumb can hit on the way to something else.
        EndStreamCancelButton().Focus(FocusState::Programmatic);
    }

    void MainPage::HideEndStream()
    {
        m_endStreamOpen = false;
        EndStreamRoot().Visibility(Visibility::Collapsed);
        if (!m_streaming) FocusHostList();
    }

    void MainPage::OnEndStreamCancelClick(IInspectable const&, RoutedEventArgs const&)
    {
        HideEndStream();
    }

    void MainPage::OnEndStreamConfirmClick(IInspectable const&, RoutedEventArgs const&)
    {
        HideEndStream();
        HardEndSession();
    }

    // The only path in the app that hands the host's monitor back.
    //
    // Two cases, and they need different calls. A LIVE session ends by closing
    // it: `echo_close` waits for the session's own goodbye to reach the host,
    // which is the fast path and the one that needs no network work of its own.
    // A DETACHED one has no handle left, so it takes `echo_release` - a single
    // blocking request-response that opens a path, says stop, and ends. Ending a
    // session never needed a session, and the version that started one just to
    // stop it raced itself and took several presses to land.
    //
    // Both block, so both run on a pool thread. That matters more than it looks:
    // `echo_close` can take a second and a half waiting for the goodbye, and on
    // the UI thread that is a frozen screen at the exact moment somebody is
    // watching for confirmation that their monitor is coming back.
    IAsyncAction MainPage::HardEndSession()
    {
        auto lifetime = get_strong();
        if (m_ending) co_return;
        m_ending = true;

        const bool live = m_session && m_session->IsOpen();
        const std::string config = m_heldConfig;

        SetStatus(L"ending the session...");
        if (live) {
            if (m_renderer) m_renderer->ShowBackground();
            LeaveStreamingUi();
        }

        auto* session = m_session.get();
        co_await winrt::resume_background();

        std::wstring outcome;
        bool released = false;
        if (live) {
            // Closing IS the goodbye, and it cannot fail in a way this side can
            // see: the handle is gone either way.
            session->Close();
            released = true;
            outcome = L"the host was told to stop";
        } else if (!config.empty()) {
            std::array<char, kTextBuffer> buffer{};
            const int32_t code = echo_release(config.c_str(), buffer.data(), kTextBuffer);
            released = code >= 0;
            outcome = std::wstring(Describe(buffer, code));
        } else {
            released = true;
            outcome = L"nothing to end";
        }

        co_await winrt::resume_foreground(Dispatcher());
        m_ending = false;
        m_pairing = false;

        // Only on success. A release that failed - the host asleep, the relay
        // unreachable, the grace period already expired into something else -
        // leaves the display exactly as held as it was, and clearing the flag
        // would take away the badge and the confirmation behind Start, which are
        // the only two things pointing at the problem. The user would be left
        // with a monitor they cannot get back and no sign that anything is
        // outstanding.
        if (released) m_hostHolding = false;
        RefreshHeldBadge();
        Append(L"session     end: " + hstring(outcome));
        SetStatus(released ? hstring(L"session ended - " + outcome)
                           : hstring(L"could not end the session - " + outcome +
                                     L". Press Start, then Stop Stream, to try again."));
        UpdateHostState();
    }

    void MainPage::RefreshHeldBadge()
    {
        HeldBadge().Visibility((m_hostHolding && !m_streaming) ? Visibility::Visible
                                                               : Visibility::Collapsed);
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
        // Asking for more frames than the HDMI output can show is real waste -
        // double the decode load, half the time per frame, every second frame
        // discarded at the present stage - and that is worth saying out loud.
        //
        // But it is not worth refusing. A previous version of this made the
        // panel rate a hard cap and immediately took away a lever that was in
        // active use: with the output reading 60 Hz, the 120 button simply
        // stopped working, and the operator could no longer test the thing this
        // whole port exists for. `m_outputHz` is one reading from one API, and
        // that is far too thin a basis for overruling an explicit press.
        //
        // The decode budget that used to bind here is gone entirely; see the
        // note at the top of this file.
        const uint32_t panelHz = m_outputHz;
        if (panelHz && fps > panelHz + 5) {
            Append(L"display     asking for " + to_hstring(fps) +
                   L" fps while the HDMI output is running at " + to_hstring(panelHz) +
                   L" Hz - the console will decode every frame and show half of them");
        }

        m_streamWidth = width;
        m_streamHeight = height;
        m_streamFps = fps;

        // Before any of the early returns below: the highlight must move even
        // when the mode was only saved for next time, and even when the host
        // refused to re-mode. It reflects what this client will ask for.
        RefreshModeButtons();

        if (!m_streaming || !m_session) {
            OverlaySubtitle().Text(L"saved - the next stream starts at " +
                                   to_hstring(width) + L"x" + to_hstring(height));
            return;
        }

        // Live. The host re-modes its virtual display in place and rebuilds its
        // encoder; the new geometry reaches this decoder as a stream change,
        // which it answers by renegotiating its output type. Nothing restarts -
        // not the session, not the desktop, not what is running on it.
        if (m_session->SetDisplay(width, height, fps)) {
            OverlaySubtitle().Text(L"asking the host for " + to_hstring(width) + L"x" +
                                   to_hstring(height) + L" @ " + to_hstring(fps) + L" Hz...");
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
    // on a button. That is not indulgence: the handoff is a list of things only
    // ever settled by comparing two behaviours on a television.
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

        return report;
    }

    // Swap decoders mid-session. The stream is restarted deliberately: a
    // decoder change means a new DPB with no history, so the only frame the new
    // one can start from is a keyframe, and asking the host for a fresh session
    // is both simpler and more honest than hoping the next IDR arrives soon.
    void MainPage::OnDecoderToggleClick(IInspectable const&, RoutedEventArgs const&)
    {
        m_useFfmpeg = !m_useFfmpeg;
        const bool wasStreaming = m_streaming;

        // Closed rather than detached: this ends a session in order to start
        // another one immediately, so leaving the host holding a display for a
        // session that is being replaced would strand it for the grace period.
        if (wasStreaming && m_session) m_session->Close();

        Append(hstring(L"decoder     switching to " +
                       std::wstring(m_useFfmpeg ? L"FFmpeg/D3D11VA" : L"Media Foundation")));
        Append(hstring(StartDecoder(m_backBufferWidth, m_backBufferHeight)));

        RefreshOverlayState();
        if (wasStreaming) {
            SetStatus(L"restarting the stream on the new decoder...");
            BeginStream();
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
        if (mouseMode) subtitle += L"   ::   MOUSE MODE";
        OverlaySubtitle().Text(hstring(subtitle));

        // The chord still works and is still the fast way; the button exists
        // because a chord is not discoverable and, until it existed, a user who
        // hit Menu+View by accident had no way to find out what had happened to
        // their controller. The label says which way the press goes.
        MouseModeButton().Content(box_value(
            mouseMode ? hstring(L"Mouse mode: ON") : hstring(L"Mouse mode: off")));
        MouseModeButton().Style(IonStyle(mouseMode ? L"IonActiveButton" : L"IonButton"));
        MouseModeHint().Text(mouseMode
            ? hstring(L"right stick moves the cursor, RT left-click, LT right-click\n"
                      L"tap Menu+View to leave, hold it to come back here")
            : hstring(L"tap Menu+View to toggle without opening this, hold it for the overlay"));

        DecoderButton().Content(box_value(
            m_useFfmpeg ? hstring(L"Decoder: FFmpeg / D3D11VA")
                        : hstring(L"Decoder: Media Foundation")));
        DecoderButton().Style(IonStyle(m_useFfmpeg ? L"IonActiveButton" : L"IonButton"));
        DecoderHint().Text(m_decoder
            ? hstring(std::wstring(m_decoder->Name()) +
                      (m_decoder->IsHardware() ? L"  ::  hardware" : L"  ::  SOFTWARE"))
            : hstring(L"no decoder"));

        RefreshModeButtons();
    }

    // Which resolution and which frame rate are actually in force.
    //
    // The buttons were previously write-only: pressing one changed the stream
    // and then looked exactly like the two it had not changed to, so the panel
    // could tell you what you *could* pick and never what you *had*.
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
    // uses - see InputBridge::RequestMouseMode.
    //
    // That indirection is load-bearing rather than fussy. Entering and leaving
    // the mode is not a bool assignment: it neutralises the pad on both edges
    // (a pad that merely goes quiet leaves the host holding Menu, which games
    // read as pause), it lifts whatever the triggers were holding down, and it
    // clears the sub-pixel accumulator. A UI thread that set the atomic
    // directly would skip all three, and the resulting bug - a click that
    // outlives the mode that made it - would look like a host-side input fault.
    void MainPage::OnMouseModeClick(IInspectable const&, RoutedEventArgs const&)
    {
        if (!m_input) return;
        m_input->RequestMouseMode(!m_input->MouseMode());
        // The pad loop applies it within one tick and reports back through
        // OnMouseModeChanged, which is what refreshes this panel. Nothing is
        // updated here on the strength of having asked.
    }

    // Fired from the pad thread whichever way the mode was toggled - the chord
    // or the button - so this is the single place the UI learns about it.
    void MainPage::OnMouseModeChanged(bool on)
    {
        Dispatcher().RunAsync(CoreDispatcherPriority::Normal, [this, on]() {
            if (m_overlayOpen) RefreshOverlayState();
            // In the stream, with no overlay open, the toggle was silent: the
            // controller simply stopped reaching the game and nothing said
            // why. This is the smallest thing that fixes that - it says what
            // happened, it says how to undo it, and it gets out of the way.
            if (m_streaming && !m_overlayOpen) ShowModeToast(on);
        });
    }

    // One toast, used by anything that has to say something the user is not
    // looking at the corner of the screen for. The accent bar carries the
    // register: the palette's yellow for a choice the user made, amber for a
    // refusal, dim for something switching off.
    void MainPage::ShowToast(std::wstring const& text, wchar_t const* accentKey)
    {
        ToastText().Text(hstring(text));
        ToastAccent().Fill(IonBrush(accentKey));
        ToastRoot().Visibility(Visibility::Visible);

        // One timer, restarted. Two toasts in quick succession must not leave
        // the first one's expiry to hide the second.
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

    void MainPage::ShowModeToast(bool on)
    {
        ShowToast(on ? L"MOUSE MODE ON   ::   right stick drives the cursor"
                     : L"MOUSE MODE OFF   ::   controller back to the game",
                  on ? L"IonAccent" : L"IonTextDim");
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

    void MainPage::OnResumeClick(IInspectable const&, RoutedEventArgs const&)
    {
        HideOverlay();
    }

    // "Leave the stream", which is NOT "end it". See the note on DetachStream.
    void MainPage::OnLeaveStreamClick(IInspectable const&, RoutedEventArgs const&)
    {
        HideOverlay();
        DetachStream();
    }

    // ── Diagnostics, which is a screen and not a drawer ─────────────────────
    //
    // It used to be a ScrollViewer sharing the overlay panel with everything
    // else, and it kept vanishing during a stream. Two things did that, and a
    // screen removes both by construction rather than by tuning: it was in a
    // `*` row competing for whatever height was left inside a capped panel, and
    // `EnterStreamingUi` collapses the overlay - so any session event that
    // re-fired took the diagnostics down with it, at exactly the moment somebody
    // would be reading them.
    //
    // Everything the dashboard used to print into its status line is here now.
    // A running counter on the front screen is not a status; it is a diagnostic
    // that happened to be in the way, and the one thing a dashboard should say
    // is which host is selected and whether it can be streamed to.
    void MainPage::ShowDiagnostics()
    {
        m_diagnosticsOpen = true;
        DiagnosticsText().Text(hstring(m_statsLine.empty() ? m_probe.Report() : m_statsLine));
        DiagnosticsScroller().ChangeView(nullptr, 0.0, nullptr, true);
        if (m_overlayOpen) OverlayRoot().Visibility(Visibility::Collapsed);
        if (m_settingsOpen) SettingsRoot().Visibility(Visibility::Collapsed);
        DiagnosticsRoot().Visibility(Visibility::Visible);
        DiagnosticsCloseButton().Focus(FocusState::Programmatic);
    }

    void MainPage::HideDiagnostics()
    {
        m_diagnosticsOpen = false;
        DiagnosticsRoot().Visibility(Visibility::Collapsed);
        // Back to whichever screen it was opened from, in the order they can
        // stack. Input stays parked the whole time, so nothing reached the PC
        // while it was up.
        if (m_settingsOpen) {
            SettingsRoot().Visibility(Visibility::Visible);
            SettingsCloseButton().Focus(FocusState::Programmatic);
        } else if (m_overlayOpen) {
            OverlayRoot().Visibility(Visibility::Visible);
            ResumeButton().Focus(FocusState::Programmatic);
        } else {
            FocusHostList();
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
        Append(L"- " + to_hstring(json));

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
        // fingerprint - the value `host_fingerprint` must hold, and the only
        // trustworthy source for it. Storing it here is what makes a stream work
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
                UpdateHostState();
            });
            return;
        }

        // ── Pairing ENDS at paired ──────────────────────────────────────────
        //
        // A pairing session ends with `closed` like any other. This used to
        // hand straight off into a stream on the reasoning that streaming is
        // what always follows — but pairing and streaming are two decisions,
        // and running them together means a user who wanted to set the console
        // up now owns a live session, a virtual display on the PC, and a
        // teardown to perform before they can do anything else. Pairing is
        // setup. It finishes by being finished.
        //
        // The host list is rebuilt rather than just re-labelled: every row's
        // badge is built from `LoadHostFingerprint` at construction, so the
        // PAIRED state only appears if the rows are made again.
        if (type == "closed") {
            const bool wasPairing = m_pairing;
            m_pairing = false;
            Dispatcher().RunAsync(CoreDispatcherPriority::Normal, [this, wasPairing]() {
                PinPanel().Visibility(Visibility::Collapsed);
                if (!wasPairing) {
                    LeaveStreamingUi();
                    UpdateHostState();
                    return;
                }

                // `closed` fires whether the handshake succeeded or not, so
                // which it was is read from what pairing actually left behind
                // rather than assumed from having reached this point.
                RefreshHostList();
                if (SelectedHostPaired()) {
                    ShowToast(L"PAIRED   ::   press A for apps", L"IonAccent");
                    SetStatus(L"paired - press A for apps, A A for Virtual Desktop");
                } else {
                    ShowToast(L"PAIRING DID NOT COMPLETE", L"IonAmber");
                    SetStatus(L"pairing did not complete - see Diagnostics");
                }
                FocusHostList();
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
        // A live session supersedes a held one - it IS the held one now.
        m_hostHolding = false;
        RefreshHeldBadge();
        SetupRoot().Visibility(Visibility::Collapsed);
        // Every dashboard pop-up belongs to a screen that is no longer visible.
        HideAppDrawer();
        HideActionMenu();
        HideOverlay();                       // also turns forwarding on
    }

    void MainPage::LeaveStreamingUi()
    {
        m_streaming = false;
        if (m_input) m_input->SetForwarding(false);
        m_overlayOpen = false;
        OverlayRoot().Visibility(Visibility::Collapsed);
        SetupRoot().Visibility(Visibility::Visible);
        NoVideoPanel().Visibility(Visibility::Collapsed);
        RefreshHeldBadge();
        FocusHostList();
    }

    void MainPage::StartStatsTimer()
    {
        // Each number is a different stage, so the FIRST zero names the culprit:
        // fed=0 is the network, decoded=0 with fed>0 is the decoder, drawn=0
        // with decoded>0 is the colour conversion.
        //
        // None of it is on the dashboard any more. It is built every tick
        // regardless so that opening the diagnostics screen shows the current
        // numbers at once - a diagnostic that shows nothing when you first ask
        // for it is indistinguishable from one that is broken.
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
            // Decode errors and the keyframes they asked for. These are the two
            // numbers that were missing when a grey screen sat unexplained for
            // several minutes: the network counters were all healthy, and
            // nothing anywhere said the decoder had failed to produce a picture.
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
            // invalidations - one frame in roughly 25 never COMPLETING - while
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
            if (showing) {
                // What the TV is actually being driven at, which decides
                // whether asking for 120 is useful or just twice the work.
                line += L"\n\nhdmi        " + m_hdmiNote;
                if (m_outputHz) {
                    line += L"\noutput      " + std::to_wstring(m_outputWidth) + L"x" +
                            std::to_wstring(m_outputHeight) + L" @ " +
                            std::to_wstring(m_outputHz) + L" Hz   ::   asking for " +
                            std::to_wstring(m_streamFps) + L" fps";
                }
                // How the chrome is being scaled, and whether the console
                // agreed to it. Two different facts: the request can be
                // accepted and the view still not be what was expected, and
                // without both a wrong-sized UI has no explanation at all.
                wchar_t scale[160]{};
                swprintf_s(scale,
                           L"\nui          %.0fx%.0f logical, chrome scale %.3f, native request %s",
                           ActualWidth(), ActualHeight(), ChromeScale().ScaleX(),
                           App::NativeScaling() ? L"accepted" : L"refused");
                line += scale;
                // The A button, counted on both paths it could arrive by.
                //
                // `pad` climbing while `xaml` stays at zero is the EXPECTED
                // reading, and it is the measurement this whole gesture is
                // wired around: the console consumes GamepadA for its own
                // Accept handling and never routes it to the page. If `pad`
                // does not climb while A is being pressed, the pad thread is
                // not running or forwarding is on. If `xaml` starts climbing,
                // the platform changed.
                wchar_t edges[160]{};
                swprintf_s(edges,
                           L"\ngesture     A pad %u   ::   A xaml %u down / %u up   ::   %s",
                           m_aPadEdges, m_aXamlEdges, m_aXamlUpEdges,
                           GesturesArmed() ? L"armed" : L"parked");
                line += edges;
                line += L"\n\n" + m_probe.Report();
                // Reported, never enforced. The clamp this used to feed is gone;
                // see the note at the top of this file.
                line += L"\ndecode      the probe above is information only - no ceiling is applied";
            }
            m_statsLine = line;
            if (showing) DiagnosticsText().Text(hstring(line));

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
