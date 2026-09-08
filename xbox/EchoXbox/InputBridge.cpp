#include "pch.h"
#include "InputBridge.h"
#include "GamepadInput.h"

#include <winrt/Windows.Devices.Input.h>
#include <winrt/Windows.Foundation.Collections.h>
#include <winrt/Windows.Gaming.Input.h>
#include <winrt/Windows.UI.Input.h>

#include <cmath>

using namespace winrt;
using namespace Windows::Foundation;
using namespace Windows::Gaming::Input;
using namespace Windows::System::Threading;
using namespace Windows::UI::Core;
using namespace Windows::UI::Input;
using namespace Windows::UI::Xaml::Controls;

namespace echo {
namespace {

// `echo_send_input` kinds.
constexpr int32_t kMouseRelative = 1;
constexpr int32_t kMouseAbsolute = 2;
constexpr int32_t kMouseButton   = 3;
constexpr int32_t kScroll        = 4;
constexpr int32_t kKey           = 5;
constexpr int32_t kReleaseAll    = 6;

// `input::MouseButton` — 1 left, 2 middle, 3 right, 4/5 the side buttons.
constexpr int32_t kLeft = 1, kMiddle = 2, kRight = 3, kX1 = 4, kX2 = 5;

// One wheel detent. GameStream counts clicks, and WinRT reports the same 120
// units per detent that Win32 does.
constexpr int32_t kWheelDelta = 120;

// How often the pad is sampled. 250 Hz is XInput's own polling convention: fast
// enough that no human-speed tap falls between two samples, slow enough that the
// thread is asleep almost all the time.
constexpr auto kPadInterval = std::chrono::microseconds(4000);

// A snapshot that is not at rest is re-sent this often even when unchanged.
// Input rides an unreliable datagram channel and each packet is a whole state,
// so this bounds how long a lost one can leave the host holding a stale stick.
constexpr auto kPadKeepAlive = std::chrono::milliseconds(50);

// How long View must be held before it means "show me the overlay" rather than
// "press View on the PC".
constexpr auto kOverlayHold = std::chrono::milliseconds(700);

// ── Controller mouse mode ───────────────────────────────────────────────────
//
// Deliberately the same numbers as `gamepad_mouse.rs`: the feature already
// exists host-side, users will meet both, and a stick that moves at a
// different speed depending on which client is in their hands is a bug they
// will never be able to describe.

// Stick deflection below this is treated as centred. Sticks rest a little off
// centre and wear worse with age; without this the cursor drifts on its own,
// which is the single most irritating way for this to fail.
constexpr float kDeadzone = 0.13f;

// Cursor speed at full deflection, in pixels per second — about two and a half
// seconds to cross a 4K width.
constexpr float kMaxSpeedPxS = 1600.0f;

// Applied to the deflection after the deadzone. Above 1 it buys precision
// where a stick is weakest: small pushes stay slow, full deflection still
// reaches kMaxSpeedPxS. A linear response makes fine positioning impossible.
constexpr float kResponseCurve = 2.0f;

// Trigger travel past which a trigger counts as a click.
constexpr int32_t kTriggerThreshold = 96;

// One axis, deadzoned and curved, as -1.0 .. 1.0.
float Deflection(int32_t axis) noexcept {
    const float value = static_cast<float>(axis) / 32767.0f;
    const float magnitude = std::fabs(value);
    if (magnitude < kDeadzone) return 0.0f;
    // Rescale so the deadzone edge is 0 and full deflection is still 1 —
    // otherwise the stick would jump to 13% speed the moment it left centre.
    const float scaled = (magnitude - kDeadzone) / (1.0f - kDeadzone);
    return std::copysign(std::pow(scaled, kResponseCurve), value);
}

// A precise sleep. The default Windows timer tick is ~15.6 ms — over three pad
// intervals — so a plain sleep would sample the controller at 64 Hz while
// claiming 250, and nothing would report the difference.
void PreciseSleep(std::chrono::microseconds duration) noexcept {
    static thread_local handle timer{
        CreateWaitableTimerExW(nullptr, nullptr,
                               CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS)
    };
    if (timer) {
        LARGE_INTEGER due{};
        due.QuadPart = -static_cast<LONGLONG>(duration.count() * 10);   // 100 ns units
        if (SetWaitableTimer(timer.get(), &due, 0, nullptr, nullptr, FALSE)) {
            WaitForSingleObjectEx(timer.get(), INFINITE, FALSE);
            return;
        }
    }
    Sleep(static_cast<DWORD>(duration.count() / 1000));
}

}  // namespace

InputBridge::~InputBridge() { Stop(); }

void InputBridge::SetSinks(InputSink input, PadSink pad, Gesture overlay) noexcept {
    std::lock_guard<std::mutex> guard(m_sinkLock);
    m_input = std::move(input);
    m_pad = std::move(pad);
    m_overlay = std::move(overlay);
}

void InputBridge::SetSurfaceSize(float width, float height) noexcept {
    if (width > 0) {
        m_surfaceWidth.store(static_cast<uint32_t>(std::lround(width)),
                             std::memory_order_relaxed);
    }
    if (height > 0) {
        m_surfaceHeight.store(static_cast<uint32_t>(std::lround(height)),
                              std::memory_order_relaxed);
    }
}

void InputBridge::Emit(InputEvent const& event) noexcept {
    if (!m_forwarding.load(std::memory_order_acquire)) return;
    InputSink sink;
    {
        std::lock_guard<std::mutex> guard(m_sinkLock);
        sink = m_input;
    }
    if (sink) sink(event);
}

void InputBridge::SetForwarding(bool on) noexcept {
    const bool was = m_forwarding.exchange(on, std::memory_order_acq_rel);
    if (was == on || on) return;

    // Turning forwarding OFF: whatever the host believes is held, it is not any
    // more. `release-all` is one packet that lifts every key and button. This is
    // the same reason the host neutralises the pad on its mouse-mode toggle
    // edges rather than merely stopping — input that simply STOPS leaves the
    // last state standing, and the last state may well be "held".
    InputSink sink;
    PadSink pad;
    {
        std::lock_guard<std::mutex> guard(m_sinkLock);
        sink = m_input;
        pad = m_pad;
    }
    if (sink) sink(InputEvent{ kReleaseAll, 0, 0, 0, 0 });
    if (pad) pad(0, 1, PadState{});
    m_buttonsDown.store(0, std::memory_order_relaxed);
}

// ── Start / stop ────────────────────────────────────────────────────────────

bool InputBridge::Start(SwapChainPanel const& panel, CoreWindow const& window,
                        std::wstring& error) noexcept {
    try {
        m_running.store(true, std::memory_order_release);
        // MANUAL reset: Stop can be called twice (explicitly, then again from the
        // destructor), and an auto-reset event would make the second call sit out
        // the whole timeout waiting for a thread that finished long ago.
        m_pointerDone.attach(CreateEventExW(nullptr, nullptr, CREATE_EVENT_MANUAL_RESET,
                                            EVENT_MODIFY_STATE | SYNCHRONIZE));

        // The pointer half. Everything inside this work item runs on the thread
        // the pool hands us, and `CreateCoreIndependentInputSource` binds the
        // source to THAT thread — so creation, the handlers and the pump all
        // have to be in here, in this order. Wiring it from `Start` would bind
        // it right back to the UI thread this exists to escape.
        //
        // `WorkItemPriority::High` is the high-priority half of the design: this
        // thread does almost nothing, and it must never wait behind the decoder
        // or the renderer for its slice.
        m_pointerWorker = ThreadPool::RunAsync(
            [this, panel](IAsyncAction const&) {
                try {
                    auto source = panel.CreateCoreIndependentInputSource(
                        CoreInputDeviceTypes::Mouse | CoreInputDeviceTypes::Touch |
                        CoreInputDeviceTypes::Pen);
                    if (!source) {
                        if (m_pointerDone) SetEvent(m_pointerDone.get());
                        return;
                    }
                    {
                        std::lock_guard<std::mutex> guard(m_pointerLock);
                        m_pointer = source;
                    }

                    source.PointerMoved([this](auto&&, PointerEventArgs const& args) {
                        const auto pos = args.CurrentPoint().Position();
                        // Absolute, not relative, and deliberately: this is a
                        // desktop on a television, so where the pointer sits on
                        // the panel IS where it belongs on the remote screen.
                        // Relative deltas accumulate rounding and drift away
                        // from the cursor the user can actually see.
                        Emit(InputEvent{
                            kMouseAbsolute,
                            static_cast<int32_t>(std::lround(pos.X)),
                            static_cast<int32_t>(std::lround(pos.Y)),
                            static_cast<int32_t>(m_surfaceWidth.load(std::memory_order_relaxed)),
                            static_cast<int32_t>(m_surfaceHeight.load(std::memory_order_relaxed)),
                        });
                    });

                    source.PointerPressed([this](auto&&, PointerEventArgs const& args) {
                        const auto props = args.CurrentPoint().Properties();
                        int32_t button = 0;
                        if (props.IsLeftButtonPressed())        button = kLeft;
                        else if (props.IsRightButtonPressed())  button = kRight;
                        else if (props.IsMiddleButtonPressed()) button = kMiddle;
                        else if (props.IsXButton1Pressed())     button = kX1;
                        else if (props.IsXButton2Pressed())     button = kX2;
                        if (button == 0) return;
                        m_buttonsDown.fetch_or(1u << button, std::memory_order_relaxed);
                        Emit(InputEvent{ kMouseButton, button, 1, 0, 0 });
                    });

                    // A release cannot read its own button off the properties —
                    // by the time it fires they all report "not pressed". The
                    // set this bridge believes is down is the only record of
                    // which one it was, so the release is the difference.
                    source.PointerReleased([this](auto&&, PointerEventArgs const& args) {
                        const auto props = args.CurrentPoint().Properties();
                        uint32_t still = 0;
                        if (props.IsLeftButtonPressed())   still |= 1u << kLeft;
                        if (props.IsRightButtonPressed())  still |= 1u << kRight;
                        if (props.IsMiddleButtonPressed()) still |= 1u << kMiddle;
                        if (props.IsXButton1Pressed())     still |= 1u << kX1;
                        if (props.IsXButton2Pressed())     still |= 1u << kX2;

                        const uint32_t before =
                            m_buttonsDown.exchange(still, std::memory_order_relaxed);
                        for (int32_t button = kLeft; button <= kX2; ++button) {
                            const uint32_t bit = 1u << button;
                            if ((before & bit) && !(still & bit)) {
                                Emit(InputEvent{ kMouseButton, button, 0, 0, 0 });
                            }
                        }
                    });

                    source.PointerWheelChanged([this](auto&&, PointerEventArgs const& args) {
                        const auto props = args.CurrentPoint().Properties();
                        if (props.IsHorizontalMouseWheel()) return;   // no wire kind for it
                        const int32_t clicks = props.MouseWheelDelta() / kWheelDelta;
                        if (clicks != 0) Emit(InputEvent{ kScroll, clicks, 0, 0, 0 });
                    });

                    // Blocks until StopProcessEvents. This call IS the thread.
                    source.Dispatcher().ProcessEvents(
                        CoreProcessEventsOption::ProcessUntilQuit);
                } catch (...) {
                    // A console with no mouse attached still has a controller.
                    // Losing the pointer source must not take the pad with it.
                }
                if (m_pointerDone) SetEvent(m_pointerDone.get());
            },
            WorkItemPriority::High, WorkItemOptions::TimeSliced);

        m_padThread = std::thread([this] { PadLoop(); });

        // The keyboard stays on the UI thread on purpose. It is the one input
        // whose latency nobody can feel — a keystroke is a discrete event that
        // has already waited on a human — and `CoreWindow` is the only place
        // UWP reports key state at all.
        if (window) {
            window.KeyDown([this](auto&&, KeyEventArgs const& args) {
                Emit(InputEvent{ kKey, static_cast<int32_t>(args.VirtualKey()), 1, 0, 0 });
            });
            window.KeyUp([this](auto&&, KeyEventArgs const& args) {
                Emit(InputEvent{ kKey, static_cast<int32_t>(args.VirtualKey()), 0, 0, 0 });
            });
        }
        return true;
    } catch (hresult_error const& e) {
        error = std::wstring(L"input bridge: ") + e.message().c_str();
        return false;
    } catch (...) {
        error = L"input bridge failed to start";
        return false;
    }
}

void InputBridge::Stop() noexcept {
    m_running.store(false, std::memory_order_release);
    m_forwarding.store(false, std::memory_order_release);
    if (m_padThread.joinable()) m_padThread.join();

    CoreIndependentInputSource source{ nullptr };
    {
        std::lock_guard<std::mutex> guard(m_pointerLock);
        source = m_pointer;
    }
    try {
        if (source && source.Dispatcher()) source.Dispatcher().StopProcessEvents();
    } catch (...) {
    }
    // Wait for the pool thread to leave ProcessEvents before returning. The
    // handlers capture `this`, and the caller is usually a destructor — so
    // returning early here is a use-after-free, not an untidy shutdown. Bounded
    // rather than infinite: a hung input thread must not take the app down with
    // it on the way out.
    if (m_pointerDone) WaitForSingleObjectEx(m_pointerDone.get(), 2000, FALSE);
    {
        std::lock_guard<std::mutex> guard(m_pointerLock);
        m_pointer = nullptr;
    }
    m_pointerWorker = nullptr;
}

// ── The controller ──────────────────────────────────────────────────────────
//
// ## Controller mouse mode lives HERE, and the chord never leaves this thread
//
// Nova implements this host-side too (`gamepad_mouse.rs`), on the packet every
// client sends, and that is normally the right place for it: one implementation
// serves Moonlight on any platform. It is reachable from Echo — an Echo pad
// datagram ends up in `input::handle_input_packet`, whose gamepad arm gives
// `gamepad_mouse::intercept` first refusal on every frame.
//
// So the chord is SWALLOWED here — stripped from the snapshot until both
// buttons are released — and that is not tidiness. If the chord reached the
// host, both implementations would toggle on the same press: two cursor drivers
// integrating the same stick, at roughly double speed, with the host also
// swallowing the pad so nothing could be turned off again. Exactly one of them
// may see the chord, and on this client it is this one.
//
// Doing it client-side buys something real. The host's driver integrates a
// stick position it learns from arriving packets; this loop integrates one it
// sampled 4 ms ago, and the resulting cursor motion never crosses the network
// at all — only the pixels it moved do.

void InputBridge::PadLoop() noexcept {
    SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);

    PadState last{};
    bool everSent = false;
    auto lastSent = std::chrono::steady_clock::now();
    auto lastTick = lastSent;

    // The View-hold gesture.
    bool viewDown = false;
    bool viewSwallowed = false;
    std::chrono::steady_clock::time_point viewSince{};

    // Menu+View.
    bool chordHeld = false;
    bool chordSwallow = false;

    // Cursor integration. Sub-pixel remainder is carried between ticks: at
    // 250 Hz, `SendInput` moving in whole pixels means anything under 250 px/s
    // rounds to zero every tick and the cursor does not move at all — removing
    // precisely the slow, careful movement a stick is worst at.
    float accumX = 0.0f, accumY = 0.0f;
    bool leftHeld = false, rightHeld = false;

    const auto sendPad = [this](int32_t activeMask, PadState const& snapshot) {
        PadSink sink;
        {
            std::lock_guard<std::mutex> guard(m_sinkLock);
            sink = m_pad;
        }
        if (sink) sink(0, activeMask, snapshot);
    };

    while (m_running.load(std::memory_order_acquire)) {
        PreciseSleep(kPadInterval);

        PadState state{};
        bool present = false;
        try {
            auto pads = Gamepad::Gamepads();
            if (pads.Size() > 0) {
                present = true;
                const auto reading = pads.GetAt(0).GetCurrentReading();
                state.buttons      = ToXInputButtons(reading.Buttons);
                state.leftTrigger  = ToTrigger(reading.LeftTrigger);
                state.rightTrigger = ToTrigger(reading.RightTrigger);
                state.leftX        = ToAxis(reading.LeftThumbstickX);
                state.leftY        = ToAxis(reading.LeftThumbstickY);
                state.rightX       = ToAxis(reading.RightThumbstickX);
                state.rightY       = ToAxis(reading.RightThumbstickY);
            }
        } catch (...) {
            continue;   // a pad disconnecting mid-read is not an error
        }
        if (!present) continue;

        const auto now = std::chrono::steady_clock::now();
        const float dt = std::chrono::duration<float>(now - lastTick).count();
        lastTick = now;

        // ── Menu + View ─────────────────────────────────────────────────────
        const bool menuNow  = (state.buttons & kStart) != 0;
        const bool viewNow  = (state.buttons & kBack) != 0;
        const bool chordNow = menuNow && viewNow;

        if (chordNow && !chordHeld) {
            const bool entering = !m_mouseMode.load(std::memory_order_acquire);
            m_mouseMode.store(entering, std::memory_order_release);
            chordSwallow = true;

            // Neutralise the pad on BOTH toggle edges. Not "stop sending" — a
            // pad that merely goes quiet leaves the host holding whatever it
            // last saw, and what it last saw has Menu held. Games read Menu as
            // pause, so the mode would open a pause menu going in and another
            // coming out.
            if (m_forwarding.load(std::memory_order_acquire)) sendPad(1, PadState{});
            everSent = false;
            last = PadState{};

            // Leaving: lift anything the triggers were holding down, or a click
            // outlives the mode that made it.
            if (!entering) {
                if (leftHeld)  Emit(InputEvent{ kMouseButton, kLeft, 0, 0, 0 });
                if (rightHeld) Emit(InputEvent{ kMouseButton, kRight, 0, 0, 0 });
            }
            leftHeld = rightHeld = false;
            accumX = accumY = 0.0f;

            // A chord is not a View press. Abandon any hold in progress so the
            // gesture cannot fire a stray View on the way out.
            viewDown = false;
            viewSwallowed = false;
        }
        chordHeld = chordNow;

        // Held past the toggle, or half-released: keep both bits off the wire
        // until the user has let go of both. See the header note above — this
        // is what keeps the host's implementation out of the picture.
        if (chordSwallow) {
            if (!menuNow && !viewNow) {
                chordSwallow = false;
            } else {
                state.buttons &= ~static_cast<int32_t>(kStart | kBack);
            }
        }

        // ── The overlay gesture ─────────────────────────────────────────────
        //
        // View is WITHHELD while it might still turn into a hold, exactly like
        // the Android client's 60 ms press trap: the first moment of a gesture
        // is indistinguishable from the start of an ordinary press, so the input
        // is delayed rather than sent and then retracted. Retraction cannot
        // work — the host has already pressed the button.
        //
        // Skipped entirely while the chord is in play: Menu+View is a chord, not
        // a View press that happens to overlap one.
        if (!chordSwallow && !chordNow) {
            if (viewNow && !viewDown) {
                viewDown = true;
                viewSwallowed = false;
                viewSince = now;
            } else if (!viewNow && viewDown) {
                viewDown = false;
                if (!viewSwallowed) {
                    // A short press. It was never sent, so send it now as one
                    // complete snapshot with View set; the next pass reports it
                    // released, because by then it genuinely is.
                    PadState tap = state;
                    tap.buttons |= kBack;
                    if (m_forwarding.load(std::memory_order_acquire) &&
                        !m_mouseMode.load(std::memory_order_acquire)) {
                        sendPad(1, tap);
                    }
                }
                viewSwallowed = false;
            }

            if (viewDown) {
                state.buttons &= ~static_cast<int32_t>(kBack);   // pending the verdict
                if (!viewSwallowed && (now - viewSince) >= kOverlayHold) {
                    viewSwallowed = true;
                    Gesture overlay;
                    {
                        std::lock_guard<std::mutex> guard(m_sinkLock);
                        overlay = m_overlay;
                    }
                    if (overlay) overlay();
                }
            }
        }

        if (!m_forwarding.load(std::memory_order_acquire)) {
            // Parked: keep tracking the gestures — that is how the overlay is
            // opened and dismissed — but send nothing. Forgetting `last` here is
            // what makes the first snapshot after un-parking a full one.
            everSent = false;
            last = PadState{};
            continue;
        }

        // ── Mouse mode: the stick drives the host's cursor ───────────────────
        if (m_mouseMode.load(std::memory_order_acquire)) {
            // The RIGHT stick, matching `gamepad_mouse.rs` — the same gesture
            // should mean the same thing whichever client a user picks up.
            const float dx = Deflection(state.rightX);
            const float dy = Deflection(state.rightY);
            if (dx != 0.0f || dy != 0.0f) {
                accumX += dx * kMaxSpeedPxS * dt;
                // XInput reports Y positive UP; screens count it down.
                accumY -= dy * kMaxSpeedPxS * dt;
            } else {
                // Nothing to carry: a remainder held across a release would
                // twitch the cursor when the stick next moved.
                accumX = accumY = 0.0f;
            }
            const int32_t stepX = static_cast<int32_t>(accumX);
            const int32_t stepY = static_cast<int32_t>(accumY);
            if (stepX != 0 || stepY != 0) {
                accumX -= static_cast<float>(stepX);
                accumY -= static_cast<float>(stepY);
                Emit(InputEvent{ kMouseRelative, stepX, stepY, 0, 0 });
            }

            // Triggers click. One threshold and no hysteresis: hysteresis on a
            // click only adds ways to leave a button stuck down.
            const bool wantLeft  = state.rightTrigger >= kTriggerThreshold;
            const bool wantRight = state.leftTrigger >= kTriggerThreshold;
            if (wantLeft != leftHeld) {
                leftHeld = wantLeft;
                Emit(InputEvent{ kMouseButton, kLeft, wantLeft ? 1 : 0, 0, 0 });
            }
            if (wantRight != rightHeld) {
                rightHeld = wantRight;
                Emit(InputEvent{ kMouseButton, kRight, wantRight ? 1 : 0, 0, 0 });
            }

            // The game sees nothing while the pad is driving a cursor. The
            // neutral snapshot sent at the toggle edge is the last thing the
            // host heard, so it is holding nothing.
            continue;
        }

        // ── Ordinary forwarding ─────────────────────────────────────────────
        const bool changed = !everSent || !(state == last);
        const bool due = !state.AtRest() && (now - lastSent) >= kPadKeepAlive;
        if (!changed && !due) continue;

        // active_mask bit 0: slot 0 stays plugged in host-side for as long as a
        // pad is present here. Clearing the bit is how the virtual pad unplugs.
        sendPad(1, state);
        last = state;
        everSent = true;
        lastSent = now;
    }

    // The thread is going away. Do not leave a direction held on the host.
    if (leftHeld)  Emit(InputEvent{ kMouseButton, kLeft, 0, 0, 0 });
    if (rightHeld) Emit(InputEvent{ kMouseButton, kRight, 0, 0, 0 });
    sendPad(0, PadState{});
}

}  // namespace echo
