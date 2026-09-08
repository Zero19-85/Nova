// Windows.Gaming.Input → XInput bits, and why this translation exists.
//
// Nova's `NV_MULTI_CONTROLLER_PACKET` carries XInput's `XINPUT_GAMEPAD` button
// bits unchanged — the host hands them straight to ViGEm with no table at all,
// so the mapping below is the only place the two vocabularies meet.
//
// `Windows.Gaming.Input::GamepadButtons` is a different enum with different
// values. It is NOT a drop-in for the wire format, and because both are
// 16-bit flags a wrong entry compiles, runs, and produces a controller that
// works *almost* right — the expensive kind of wrong.
//
// Two names differ from what a controller's label says, which is the usual
// source of a transposed pair:
//   Menu ("hamburger", ☰) is XInput START
//   View ("two rectangles", ⧉) is XInput BACK
//
// Start + Select is the host's controller-mouse-mode chord (`gamepad_mouse.rs`),
// so on this platform it is **Menu + View**. Nothing here implements it: the
// host owns that toggle deliberately, on the packet every client already sends.
// Send the chord and it works.
#pragma once

#include <cstdint>
#include <winrt/Windows.Gaming.Input.h>

namespace echo {

// XINPUT_GAMEPAD_* — GameStream's low 16 buttonFlags are bit-for-bit these.
enum XInputBits : uint16_t {
    kDpadUp        = 0x0001,
    kDpadDown      = 0x0002,
    kDpadLeft      = 0x0004,
    kDpadRight     = 0x0008,
    kStart         = 0x0010,  // Menu
    kBack          = 0x0020,  // View
    kLeftThumb     = 0x0040,
    kRightThumb    = 0x0080,
    kLeftShoulder  = 0x0100,
    kRightShoulder = 0x0200,
    kA             = 0x1000,
    kB             = 0x2000,
    kX             = 0x4000,
    kY             = 0x8000,
};

// Translate one reading's buttons. Paddles are deliberately unmapped: XInput has
// no bits for them, so there is nothing truthful to send.
inline uint16_t ToXInputButtons(winrt::Windows::Gaming::Input::GamepadButtons b) {
    using winrt::Windows::Gaming::Input::GamepadButtons;
    const auto has = [b](GamepadButtons f) {
        return (b & f) == f;
    };
    uint16_t out = 0;
    if (has(GamepadButtons::DPadUp))          out |= kDpadUp;
    if (has(GamepadButtons::DPadDown))        out |= kDpadDown;
    if (has(GamepadButtons::DPadLeft))        out |= kDpadLeft;
    if (has(GamepadButtons::DPadRight))       out |= kDpadRight;
    if (has(GamepadButtons::Menu))            out |= kStart;
    if (has(GamepadButtons::View))            out |= kBack;
    if (has(GamepadButtons::LeftThumbstick))  out |= kLeftThumb;
    if (has(GamepadButtons::RightThumbstick)) out |= kRightThumb;
    if (has(GamepadButtons::LeftShoulder))    out |= kLeftShoulder;
    if (has(GamepadButtons::RightShoulder))   out |= kRightShoulder;
    if (has(GamepadButtons::A))               out |= kA;
    if (has(GamepadButtons::B))               out |= kB;
    if (has(GamepadButtons::X))               out |= kX;
    if (has(GamepadButtons::Y))               out |= kY;
    return out;
}

// Sticks: WinRT gives -1.0..1.0 with **Y positive up**, which is XInput's own
// convention — so unlike the Android client there is no negation here. A sign
// flip reads as inverted look, a preference someone forgot to expose, rather
// than as a defect. That is exactly why it is worth a comment.
inline int16_t ToAxis(double v) {
    const double scaled = v * 32767.0;
    if (scaled >= 32767.0) return 32767;
    if (scaled <= -32768.0) return -32768;
    return static_cast<int16_t>(scaled);
}

// Triggers: 0.0..1.0 → 0..255.
inline uint8_t ToTrigger(double v) {
    const double scaled = v * 255.0;
    if (scaled >= 255.0) return 255;
    if (scaled <= 0.0) return 0;
    return static_cast<uint8_t>(scaled);
}

}  // namespace echo
