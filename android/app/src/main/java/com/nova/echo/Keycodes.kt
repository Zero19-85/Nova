package com.nova.echo

import android.view.KeyEvent

/**
 * Android key codes to Windows virtual-key codes.
 *
 * ## Why a table and not arithmetic
 *
 * The two spaces are unrelated. Android's codes are ordered by when Android
 * needed them; Windows' are a mix of ASCII for letters and digits and arbitrary
 * assignments for everything else. The letter and digit ranges do map
 * arithmetically and are handled that way; everything else is a lookup, because
 * a clever formula covering the rest does not exist.
 *
 * ## What this deliberately does not do
 *
 * No layout translation. The host receives a *key*, not a character, and
 * applies the PC's own keyboard layout — which is what you want, because the
 * remote machine's layout is the one the remote applications believe in. A
 * Bluetooth keyboard set to a different layout than the PC will therefore
 * produce the PC's characters, exactly as a physically attached keyboard would.
 */
object Keycodes {

    /** Modifier bits, matching GameStream's mask (moonlight-android `KeyboardPacket`). */
    const val MOD_SHIFT = 0x01
    const val MOD_CTRL = 0x02
    const val MOD_ALT = 0x04
    const val MOD_META = 0x08

    /**
     * The Windows virtual-key code for an Android key code, or 0 if there is no
     * sensible mapping — callers must not send 0, since the host would mask it
     * to a meaningless key.
     */
    fun toWindows(androidKeyCode: Int): Int = when (androidKeyCode) {
        // Letters and digits map arithmetically: Windows uses ASCII for both.
        in KeyEvent.KEYCODE_A..KeyEvent.KEYCODE_Z ->
            0x41 + (androidKeyCode - KeyEvent.KEYCODE_A)
        in KeyEvent.KEYCODE_0..KeyEvent.KEYCODE_9 ->
            0x30 + (androidKeyCode - KeyEvent.KEYCODE_0)
        in KeyEvent.KEYCODE_F1..KeyEvent.KEYCODE_F12 ->
            0x70 + (androidKeyCode - KeyEvent.KEYCODE_F1)
        in KeyEvent.KEYCODE_NUMPAD_0..KeyEvent.KEYCODE_NUMPAD_9 ->
            0x60 + (androidKeyCode - KeyEvent.KEYCODE_NUMPAD_0)

        else -> OTHERS[androidKeyCode] ?: 0
    }

    /** Current modifier mask from a live event's meta state. */
    fun modifiers(metaState: Int): Int {
        var mask = 0
        if (metaState and KeyEvent.META_SHIFT_ON != 0) mask = mask or MOD_SHIFT
        if (metaState and KeyEvent.META_CTRL_ON != 0) mask = mask or MOD_CTRL
        if (metaState and KeyEvent.META_ALT_ON != 0) mask = mask or MOD_ALT
        if (metaState and KeyEvent.META_META_ON != 0) mask = mask or MOD_META
        return mask
    }

    private val OTHERS = mapOf(
        KeyEvent.KEYCODE_DEL to 0x08,           // VK_BACK
        KeyEvent.KEYCODE_TAB to 0x09,
        KeyEvent.KEYCODE_ENTER to 0x0D,
        KeyEvent.KEYCODE_NUMPAD_ENTER to 0x0D,
        KeyEvent.KEYCODE_SHIFT_LEFT to 0xA0,
        KeyEvent.KEYCODE_SHIFT_RIGHT to 0xA1,
        KeyEvent.KEYCODE_CTRL_LEFT to 0xA2,
        KeyEvent.KEYCODE_CTRL_RIGHT to 0xA3,
        KeyEvent.KEYCODE_ALT_LEFT to 0xA4,
        KeyEvent.KEYCODE_ALT_RIGHT to 0xA5,
        KeyEvent.KEYCODE_META_LEFT to 0x5B,     // VK_LWIN
        KeyEvent.KEYCODE_META_RIGHT to 0x5C,
        KeyEvent.KEYCODE_CAPS_LOCK to 0x14,
        KeyEvent.KEYCODE_ESCAPE to 0x1B,
        KeyEvent.KEYCODE_SPACE to 0x20,
        KeyEvent.KEYCODE_PAGE_UP to 0x21,
        KeyEvent.KEYCODE_PAGE_DOWN to 0x22,
        KeyEvent.KEYCODE_MOVE_END to 0x23,
        KeyEvent.KEYCODE_MOVE_HOME to 0x24,
        KeyEvent.KEYCODE_DPAD_LEFT to 0x25,
        KeyEvent.KEYCODE_DPAD_UP to 0x26,
        KeyEvent.KEYCODE_DPAD_RIGHT to 0x27,
        KeyEvent.KEYCODE_DPAD_DOWN to 0x28,
        KeyEvent.KEYCODE_INSERT to 0x2D,
        KeyEvent.KEYCODE_FORWARD_DEL to 0x2E,   // VK_DELETE
        KeyEvent.KEYCODE_NUMPAD_MULTIPLY to 0x6A,
        KeyEvent.KEYCODE_NUMPAD_ADD to 0x6B,
        KeyEvent.KEYCODE_NUMPAD_SUBTRACT to 0x6D,
        KeyEvent.KEYCODE_NUMPAD_DOT to 0x6E,
        KeyEvent.KEYCODE_NUMPAD_DIVIDE to 0x6F,
        KeyEvent.KEYCODE_NUM_LOCK to 0x90,
        KeyEvent.KEYCODE_SCROLL_LOCK to 0x91,
        KeyEvent.KEYCODE_SEMICOLON to 0xBA,
        KeyEvent.KEYCODE_EQUALS to 0xBB,
        KeyEvent.KEYCODE_COMMA to 0xBC,
        KeyEvent.KEYCODE_MINUS to 0xBD,
        KeyEvent.KEYCODE_PERIOD to 0xBE,
        KeyEvent.KEYCODE_SLASH to 0xBF,
        KeyEvent.KEYCODE_GRAVE to 0xC0,
        KeyEvent.KEYCODE_LEFT_BRACKET to 0xDB,
        KeyEvent.KEYCODE_BACKSLASH to 0xDC,
        KeyEvent.KEYCODE_RIGHT_BRACKET to 0xDD,
        KeyEvent.KEYCODE_APOSTROPHE to 0xDE,
    )
}
