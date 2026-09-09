// Mouse, keyboard and controller, from the console to the host.
//
// ── Why this is a separate thread, and not the UI thread ────────────────────
//
// XAML's pointer events are delivered by the UI thread's dispatcher, so they
// queue behind layout, animation, and every `RunAsync` the app posts. On a page
// that is doing nothing that costs little; on one presenting 60 frames a second
// it is a variable few milliseconds added to every mouse move, which is exactly
// the "floaty" feel a cursor gets when its latency is not constant.
//
// `SwapChainPanel::CreateCoreIndependentInputSource` exists for this. It hands
// raw pointer input to a thread of the app's choosing, bypassing the XAML input
// pipeline entirely, and it is the same mechanism DirectX titles use. It MUST be
// created on the thread that will service it, and that thread must then pump its
// dispatcher — which is why the whole thing lives inside one work item rather
// than being wired up from `Start`.
//
// The gamepad is a second thread for a different reason: `Windows.Gaming.Input`
// is polled, not pushed. There is no event to wait on, so someone has to ask,
// and asking on a frame boundary would sample the pad at the frame rate.
//
// ── Controller mouse mode IS here ───────────────────────────────────────────
//
// This note used to say the opposite — that mouse mode lived host-side in
// `gamepad_mouse.rs` and that this file "must not implement it, only avoid
// swallowing the chord". That was true of an earlier draft and is now exactly
// backwards; `PadLoop` owns it and the chord is deliberately swallowed here.
// The reasoning is at the top of the mouse-mode section in the .cpp, and the
// short version is that exactly one implementation may see Menu+View: if both
// did, two cursor drivers would integrate the same stick at double speed with
// the host also swallowing the pad, so nothing could turn it off again.
#pragma once

#include <cstdint>
#include <atomic>
#include <functional>
#include <mutex>
#include <string>
#include <thread>

#include <winrt/Windows.UI.Core.h>
#include <winrt/Windows.UI.Xaml.Controls.h>
#include <winrt/Windows.System.Threading.h>

namespace echo {

// One event, already reduced to what `echo_send_input` takes. The kinds are
// its table: 1 mouse-rel, 2 mouse-abs, 3 button, 4 scroll, 5 key, 6 release-all.
struct InputEvent {
    int32_t kind = 0;
    int32_t a = 0, b = 0, c = 0, d = 0;
};

// One controller snapshot, in XInput's vocabulary (see GamepadInput.h).
struct PadState {
    int32_t buttons = 0;
    int32_t leftTrigger = 0, rightTrigger = 0;
    int32_t leftX = 0, leftY = 0, rightX = 0, rightY = 0;

    bool operator==(PadState const& o) const noexcept {
        return buttons == o.buttons && leftTrigger == o.leftTrigger &&
               rightTrigger == o.rightTrigger && leftX == o.leftX && leftY == o.leftY &&
               rightX == o.rightX && rightY == o.rightY;
    }
    // "At rest" decides whether a snapshot is re-sent as insurance. Input rides
    // an unreliable datagram channel, so a lost "stick returned to centre" would
    // otherwise leave the host holding a direction forever.
    bool AtRest() const noexcept { return *this == PadState{}; }
};

class InputBridge {
public:
    using InputSink = std::function<void(InputEvent const&)>;
    using PadSink   = std::function<void(int32_t slot, int32_t activeMask, PadState const&)>;
    using Gesture   = std::function<void()>;
    // Called from the pad thread on every mouse-mode transition, whichever way
    // it was caused. The UI has no other way to learn: before this, the chord
    // toggled silently and a user who hit it by accident just watched their
    // controller stop reaching the game.
    using ModeSink  = std::function<void(bool on)>;

    ~InputBridge();

    // UI thread. Starts the pointer work item and the pad thread; returns false
    // with a reason only if the input source could not be created at all.
    bool Start(winrt::Windows::UI::Xaml::Controls::SwapChainPanel const& panel,
               winrt::Windows::UI::Core::CoreWindow const& window,
               std::wstring& error) noexcept;
    void Stop() noexcept;

    void SetSinks(InputSink input, PadSink pad, Gesture overlay,
                  ModeSink mouseMode = nullptr) noexcept;

    // The panel's logical size, pushed from the UI thread because the input
    // thread cannot read `ActualWidth`. Absolute mouse positions are sent in
    // this space and the host maps them onto its capture rect.
    void SetSurfaceSize(float width, float height) noexcept;

    // False parks everything: no forwarding at all, and the pad state is
    // released first so the host is not left holding a button.
    void SetForwarding(bool on) noexcept;

    // Menu+View has put the right stick on the host's cursor.
    bool MouseMode() const noexcept { return m_mouseMode.load(std::memory_order_acquire); }

    // Ask for the mode to change. Deliberately a REQUEST and not a setter.
    //
    // Entering and leaving is not a bool assignment: both edges neutralise the
    // pad (a pad that merely goes quiet leaves the host holding Menu, and games
    // read Menu as pause), leaving lifts whatever the triggers were holding
    // down, and both clear the sub-pixel accumulator. All three live in the pad
    // loop, which is the only thread that may touch that state — so the flag is
    // set THERE, through the same code path the chord takes, and this call just
    // posts the intent. It is honoured within one 4 ms tick and answered on the
    // ModeSink; nothing should assume it took effect because it was asked for.
    void RequestMouseMode(bool on) noexcept;

private:
    void PadLoop() noexcept;
    void Emit(InputEvent const& event) noexcept;

    InputSink m_input;
    PadSink   m_pad;
    Gesture   m_overlay;
    ModeSink  m_mouseModeSink;
    std::mutex m_sinkLock;

    // Written by the pool thread, read by whoever calls Stop, so it is guarded.
    // `m_pointerDone` is how Stop knows the pool thread has actually left
    // `ProcessEvents` — without it, teardown races a thread still using `this`.
    winrt::Windows::UI::Core::CoreIndependentInputSource m_pointer{ nullptr };
    std::mutex m_pointerLock;
    winrt::handle m_pointerDone;
    winrt::Windows::Foundation::IAsyncAction m_pointerWorker{ nullptr };
    std::thread m_padThread;

    std::atomic<bool> m_running{ false };
    std::atomic<bool> m_forwarding{ false };
    std::atomic<uint32_t> m_surfaceWidth{ 1920 };
    std::atomic<uint32_t> m_surfaceHeight{ 1080 };
    // Which mouse buttons this bridge believes are down, so `SetForwarding(false)`
    // can lift them rather than stranding a drag on the host.
    std::atomic<uint32_t> m_buttonsDown{ 0 };
    // Controller mouse mode, toggled by Menu+View inside the pad loop.
    std::atomic<bool> m_mouseMode{ false };
    // A pending request from outside the pad loop. -1 = nothing asked for;
    // 0/1 = the state somebody wants. Consumed by the loop, which is what
    // makes an overlay press take the identical path the chord does.
    std::atomic<int32_t> m_mouseModeRequest{ -1 };
};

}  // namespace echo
