package com.nova.echo

import android.content.Context
import android.hardware.input.InputManager
import android.util.Log
import android.view.InputDevice
import android.view.InputEvent
import android.view.KeyEvent
import android.view.MotionEvent

/**
 * Physical game controllers → GameStream controller snapshots.
 *
 * ## Android gives events; the wire wants state
 *
 * `NV_MULTI_CONTROLLER_PACKET` is a **complete snapshot**: every packet carries
 * all fifteen buttons, both triggers and all four axes, and the host applies it
 * wholesale to a ViGEm virtual pad. Android hands out a [KeyEvent] for one
 * button or a [MotionEvent] carrying whichever axes happened to move. Nothing
 * bridges those two shapes except somebody holding the running state, and that
 * is this class. Building a packet from a single event would release everything
 * else the user is holding, every time they pressed anything.
 *
 * So: one [Pad] per attached controller, mutated by each event, and the whole
 * thing re-sent whenever it differs from what was last sent.
 *
 * ## Why source-gating is not a detail
 *
 * [Keycodes] maps `KEYCODE_DPAD_UP` to VK 0x26 — an arrow key — and
 * `MainActivity.dispatchKeyEvent` consumes every key it sees. Before this class
 * existed, a controller's d-pad therefore typed arrow keys at the host, while
 * `KEYCODE_BUTTON_A` and friends went nowhere at all because [Keycodes] has no
 * entry for them. Splitting the two apart by **keycode** cannot work: a real
 * keyboard's arrow keys are the same keycodes and must keep reaching
 * [Keycodes].
 *
 * The split is by device, then by event source ([padDevice]):
 *
 * - **The device** must declare `SOURCE_GAMEPAD` or `SOURCE_JOYSTICK`. A plain
 *   keyboard declares neither, so its arrows are never mistaken for a d-pad.
 * - **The event** must arrive from a gamepad, joystick or dpad source. Some
 *   controllers also expose a keyboard source for media keys, and those belong
 *   to [Keycodes] like any other key.
 *
 * `SOURCE_DPAD` alone is accepted only because the device test already ran:
 * many pads report their d-pad with that source and nothing else, and plenty of
 * ordinary keyboards report it too.
 *
 * ## Controllers that are not Xbox controllers
 *
 * Everything reaching [Pad] is in the Xbox/XInput layout the host expects,
 * because the host does no translation — GameStream's `buttonFlags` *are*
 * XInput's bits. Getting other pads into that shape is [Layout]'s job, and it
 * does it by asking each device what it has rather than by looking it up in a
 * table of vendor IDs. The reasoning is on [Layout]; the short version is that
 * a vendor table only ever contains hardware someone has physically owned,
 * while a probe handles a pad nobody has seen before.
 *
 * Each arriving controller logs the layout it resolved to, under `EchoPad`.
 * That line is the starting point for any report of a pad behaving oddly.
 */
class ControllerHandler(
    context: Context,
    /**
     * Where snapshots go, resolved per event rather than held.
     *
     * The session outlives any one Activity and can end underneath a pad that
     * is still being held, so a cached reference here would be a stale handle
     * exactly when a controller is most likely to still be sending.
     */
    private val sink: () -> EchoController?,
) {

    private val inputManager = context.getSystemService(Context.INPUT_SERVICE) as? InputManager

    /** `deviceId` → the slot the host has a virtual pad in. */
    private val slots = HashMap<Int, Pad>()

    /**
     * Its own listener rather than sharing [StreamSurfaceView]'s.
     *
     * That one is registered only while pointer capture is wanted and is
     * deliberately unregistered before capture is released, because releasing
     * capture itself fires a device-changed callback. A controller unplugged
     * while the mouse is not captured still has to be unplugged host-side, so
     * this needs a lifetime of its own and must not perturb that one.
     */
    private val listener = object : InputManager.InputDeviceListener {
        override fun onInputDeviceAdded(deviceId: Int) = Unit // claimed on first event
        override fun onInputDeviceRemoved(deviceId: Int) = release(deviceId)
        override fun onInputDeviceChanged(deviceId: Int) = Unit
    }

    fun start() {
        inputManager?.registerInputDeviceListener(listener, null)
    }

    fun stop() {
        inputManager?.unregisterInputDeviceListener(listener)
    }

    // ── Events ──────────────────────────────────────────────────────────────

    /**
     * Analog sticks, triggers and the hat-switch d-pad.
     *
     * Returns whether the event was a controller's. A `false` here must fall
     * through to the pointer path — joystick and mouse motion arrive at the same
     * callback.
     */
    fun onMotion(event: MotionEvent): Boolean {
        if (event.actionMasked != MotionEvent.ACTION_MOVE) return false
        val device = padDevice(event) ?: return false
        val pad = padFor(device) ?: return false

        // Historical samples are ignored, deliberately, and this is the opposite
        // of the rule for a mouse. Mouse deltas ACCUMULATE, so dropping a
        // sample loses distance; a stick reports absolute position, so every
        // older sample in the batch is a place the stick has already left. Only
        // the newest is true.
        val layout = pad.layout
        pad.leftStickX = layout.leftX.stick(event, invert = false)
        pad.leftStickY = layout.leftY.stick(event, invert = true)
        pad.rightStickX = layout.rightX.stick(event, invert = false)
        pad.rightStickY = layout.rightY.stick(event, invert = true)

        if (layout.combinedTrigger != null) {
            // One axis carrying both: negative is left, positive is right.
            val v = layout.combinedTrigger.raw(event)
            pad.leftTrigger = if (v < 0) (-v * 255).toInt().coerceIn(0, 255) else 0
            pad.rightTrigger = if (v > 0) (v * 255).toInt().coerceIn(0, 255) else 0
        } else {
            pad.leftTrigger = layout.leftTrigger.trigger(event)
            pad.rightTrigger = layout.rightTrigger.trigger(event)
        }

        // The d-pad reaches us one of two ways depending on the controller:
        // hat axes here, `KEYCODE_DPAD_*` in [onKey].
        //
        // **Guarded on the device actually having a hat**, and that guard is
        // load-bearing rather than an optimisation. `getAxisValue` returns 0 for
        // an axis the device does not have, which is indistinguishable from a
        // centred hat — so on a key-reporting pad this block would clear the
        // d-pad bits on every stick sample. Holding a direction while moving the
        // stick, which is most of how anyone plays, would drop the direction.
        if (layout.hasHat) {
            val hatX = layout.hatX.raw(event)
            val hatY = layout.hatY.raw(event)
            pad.setButton(DPAD_LEFT, hatX < -HAT_THRESHOLD)
            pad.setButton(DPAD_RIGHT, hatX > HAT_THRESHOLD)
            pad.setButton(DPAD_UP, hatY < -HAT_THRESHOLD)
            pad.setButton(DPAD_DOWN, hatY > HAT_THRESHOLD)
        }

        flush(pad)
        return true
    }

    /**
     * Face buttons, shoulders, thumbstick clicks, start/select and the d-pad on
     * controllers that report it as keys.
     *
     * Returns whether the event was a controller's. A `false` must fall through
     * to the keyboard path, or a real keyboard stops working.
     */
    fun onKey(event: KeyEvent): Boolean {
        val device = padDevice(event) ?: return false

        // Claim the event whatever it turns out to be. It came from a
        // controller, so letting it fall through would hand it to [Keycodes] —
        // which is the d-pad-types-arrow-keys bug in its general form.
        val pad = padFor(device) ?: return true
        val down = event.action == KeyEvent.ACTION_DOWN

        // Auto-repeat is discarded: the state is already "held", so a repeat
        // carries no new information and only costs a packet.
        if (down && event.repeatCount > 0) return true

        when (val mapped = pad.layout.buttons[event.keyCode]) {
            null -> return true // a controller key with no XInput counterpart
            // Some controllers report the analog triggers as buttons only. Full
            // deflection is the best available answer; a pad that reports both
            // overwrites this from its axes on the next motion event.
            TRIGGER_LEFT -> pad.leftTrigger = if (down) 255 else 0
            TRIGGER_RIGHT -> pad.rightTrigger = if (down) 255 else 0
            else -> pad.setButton(mapped, down)
        }

        flush(pad)
        return true
    }

    // ── Lifecycle ───────────────────────────────────────────────────────────

    /**
     * Let go of every controller and unplug every virtual pad.
     *
     * Called by `EchoController.releaseAllInput`, which is what runs whenever
     * input stops mid-gesture — the session ending, the app backgrounding,
     * input being switched off. The *native* `INPUT_RELEASE_ALL` cannot do this
     * job: it is a fixed list of modifiers and mouse buttons, chosen so it needs
     * no state to be correct, and which slots are live is exactly the state it
     * refuses to carry.
     *
     * Two packets per pad, in this order: a neutral snapshot while the pad is
     * still plugged, then the same snapshot with the slot's bit cleared. The
     * first is what actually releases a held button — the host skips the state
     * update entirely on the packet that unplugs — so collapsing these into one
     * would leave a game seeing a trigger held at the instant the pad vanished.
     */
    fun releaseAll() {
        if (slots.isEmpty()) return
        val c = sink()
        for (pad in slots.values) {
            pad.neutralise()
            c?.gamepad(pad.slot, mask(), pad)
            c?.gamepad(pad.slot, mask() and (1 shl pad.slot).inv(), pad)
        }
        slots.clear()
    }

    /** Forget local state without telling the host — the session is already gone. */
    fun forget() = slots.clear()

    // ── Slots ───────────────────────────────────────────────────────────────

    /**
     * The pad for a device, claiming a slot on first sight.
     *
     * Claimed lazily rather than on `onInputDeviceAdded` because Android
     * reports plenty of devices that declare a joystick source and are not
     * controllers — some phones' own sensors among them. A device that has
     * actually sent controller input is the only evidence worth allocating a
     * host-side virtual pad for.
     *
     * Returns `null` when all four slots are taken; the host ignores packets
     * beyond its `MAX_PADS` anyway, so a fifth controller is silently inert
     * rather than stealing another player's slot.
     */
    private fun padFor(device: InputDevice): Pad? {
        slots[device.id]?.let { return it }

        val free = (0 until EchoNative.MAX_GAMEPAD_SLOTS).firstOrNull { slot ->
            slots.values.none { it.slot == slot }
        } ?: return null

        val layout = Layout.resolve(device)
        val pad = Pad(free, layout)
        slots[device.id] = pad

        // The one line that turns "my pad is weird" into something fixable.
        // Every quirk below was found by reading what a device actually
        // reports, so a device that still behaves oddly should be diagnosed the
        // same way rather than guessed at.
        Log.i(TAG, "slot $free ← ${device.name} [$layout]")

        // Announce arrival with a neutral snapshot: the set mask bit is what
        // plugs the virtual pad in host-side, and sending the state along with
        // it means the pad arrives centred rather than at whatever the previous
        // occupant of the slot left behind.
        sink()?.gamepad(pad.slot, mask(), pad)
        return pad
    }

    /** A controller was unplugged: unplug its virtual counterpart. */
    private fun release(deviceId: Int) {
        val pad = slots.remove(deviceId) ?: return
        val c = sink()
        pad.neutralise()
        // Same two-step as [releaseAll], and for the same reason: the neutral
        // state has to land while the pad is still plugged in.
        c?.gamepad(pad.slot, mask() or (1 shl pad.slot), pad)
        c?.gamepad(pad.slot, mask(), pad)
    }

    /** Which slots currently hold a controller. */
    private fun mask(): Int = slots.values.fold(0) { acc, pad -> acc or (1 shl pad.slot) }

    /** Send the snapshot, but only if it says something new. */
    private fun flush(pad: Pad) {
        if (!pad.changed()) return
        sink()?.gamepad(pad.slot, mask(), pad)
        pad.commit()
    }

    /**
     * The controller behind an event, or `null` if this is not controller input.
     *
     * Both halves matter — see the class doc. The device test keeps a keyboard's
     * arrow keys out; the source test keeps a controller's own media keys in
     * [Keycodes] where they belong.
     */
    private fun padDevice(event: InputEvent): InputDevice? {
        val device = event.device ?: return null
        val isPad = device.supportsSource(InputDevice.SOURCE_GAMEPAD) ||
            device.supportsSource(InputDevice.SOURCE_JOYSTICK)
        if (!isPad) return null

        val fromPad = event.isFromSource(InputDevice.SOURCE_GAMEPAD) ||
            event.isFromSource(InputDevice.SOURCE_JOYSTICK) ||
            event.isFromSource(InputDevice.SOURCE_DPAD)
        return if (fromPad) device else null
    }

    /**
     * One axis, with the range the device reports for it.
     *
     * Ranges are read once and kept, rather than queried per event: they cannot
     * change while a device is attached, and `getMotionRange` on the hot path
     * was a lookup per axis per sample.
     *
     * **Normalising against the reported range is itself a quirk fix.** Most
     * pads report sticks as `-1..1` and triggers as `0..1`, but not all: some
     * report triggers as `-1..1` resting at `-1` (so a released trigger would
     * read as fully pressed if assumed to rest at zero), and cheap pads
     * sometimes report raw `0..255`. Deriving everything from `min`/`max`
     * handles all three without needing to know which is which.
     */
    class Axis(
        private val axis: Int,
        private val min: Float,
        max: Float,
        private val flat: Float,
    ) {
        private val span = max - min
        private val centre = (min + max) / 2f
        private val half = span / 2f

        /** Raw value normalised to `-1..1` about the resting centre. */
        fun raw(event: MotionEvent): Float {
            if (half <= 0f) return 0f
            val v = event.getAxisValue(axis) - centre
            val usable = half - flat
            if (usable <= 0f) return 0f
            val magnitude = kotlin.math.abs(v)
            if (magnitude <= flat) return 0f
            val scaled = (magnitude - flat) / usable
            return (if (v < 0) -scaled else scaled).coerceIn(-1f, 1f)
        }

        /** A stick axis as a full-range `Short`, optionally inverted. */
        fun stick(event: MotionEvent, invert: Boolean): Int {
            val v = if (invert) -raw(event) else raw(event)
            // Short.MIN_VALUE is one further from centre than MAX; scaling by
            // MAX keeps the resting point exactly at zero, which matters more
            // than reaching the last unit of deflection.
            return (v * Short.MAX_VALUE).toInt().coerceIn(-32768, 32767)
        }

        /** A trigger as `0..255`, measured from its own resting end. */
        fun trigger(event: MotionEvent): Int {
            if (span <= 0f) return 0
            val v = event.getAxisValue(axis) - min
            val usable = span - flat
            if (usable <= 0f || v <= flat) return 0
            return (((v - flat) / usable) * 255f).toInt().coerceIn(0, 255)
        }
    }

    /** A trigger axis the device does not have. Always reads as released. */
    private object NoAxis {
        val instance = Axis(MotionEvent.AXIS_GENERIC_1, 0f, 0f, 0f)
    }

    /**
     * How one controller model differs from the Xbox layout, resolved once from
     * what the device reports about itself.
     *
     * ## Why this probes rather than keying off vendor IDs
     *
     * A table of vendor and product IDs is the obvious shape for this and it is
     * the wrong one. It is wrong on the merits — Android already normalises
     * most HID controllers, so the same physical pad reports differently
     * depending on Android version, connection (USB vs Bluetooth), and whether
     * a vendor driver claimed it — and it is wrong in practice, because such a
     * table can only ever contain hardware someone has physically tested. Every
     * pad not in it falls back to a guess.
     *
     * Probing inverts that. `getMotionRange` and `hasKeys` are the device
     * answering questions about itself, so an unknown controller that reports
     * honestly is handled correctly the first time it is plugged in. The three
     * differences below are the ones that actually occur:
     *
     * 1. **Right stick on `RX`/`RY` instead of `Z`/`RZ`.** Both spellings are
     *    common. This is also what frees `Z` to be a trigger axis — see below.
     * 2. **Triggers.** `LTRIGGER`/`RTRIGGER` is the usual spelling,
     *    `BRAKE`/`GAS` the driving-wheel one, and some stacks report *both*
     *    triggers combined on a single `Z` axis with left negative and right
     *    positive. The combined case is detectable precisely because the right
     *    stick landed on `RX`/`RY`.
     * 3. **Generic HID numbering.** A pad the platform has no profile for
     *    reports `KEYCODE_BUTTON_1..16` rather than `BUTTON_A`/`B`/`X`/`Y`.
     *    `hasKeys` says which, so this is a question with an answer rather than
     *    a guess. This is the common third-party-pad case.
     *
     * [VENDORS] exists only to put a readable name in the log line. It
     * deliberately changes no behaviour: naming a device is not the same as
     * knowing something about it that probing does not.
     */
    class Layout(
        val leftX: Axis,
        val leftY: Axis,
        val rightX: Axis,
        val rightY: Axis,
        val leftTrigger: Axis,
        val rightTrigger: Axis,
        /** Set when one axis carries both triggers; then the two above are unused. */
        val combinedTrigger: Axis?,
        val hatX: Axis,
        val hatY: Axis,
        /** Whether the device reports a hat at all — see the guard in [onMotion]. */
        val hasHat: Boolean,
        val buttons: Map<Int, Int>,
        private val summary: String,
    ) {
        override fun toString(): String = summary

        companion object {
            fun resolve(device: InputDevice): Layout {
                // Both spellings of the right stick. RX/RY wins when present,
                // which is also the signal that Z may be a trigger axis.
                val hasRxRy = has(device, MotionEvent.AXIS_RX) && has(device, MotionEvent.AXIS_RY)
                val rightX = if (hasRxRy) MotionEvent.AXIS_RX else MotionEvent.AXIS_Z
                val rightY = if (hasRxRy) MotionEvent.AXIS_RY else MotionEvent.AXIS_RZ

                val left = firstPresent(device, MotionEvent.AXIS_LTRIGGER, MotionEvent.AXIS_BRAKE)
                val right = firstPresent(
                    device,
                    MotionEvent.AXIS_RTRIGGER,
                    MotionEvent.AXIS_GAS,
                    MotionEvent.AXIS_THROTTLE,
                )

                // No trigger axis of either spelling, but Z exists and the
                // sticks did not take it: Z is carrying both triggers, left
                // negative and right positive.
                val combined = if (left == null && right == null && hasRxRy &&
                    has(device, MotionEvent.AXIS_Z)
                ) MotionEvent.AXIS_Z else null

                // `hasKeys` is the device saying whether it has a face button
                // by that name. A pad with no profile reports the numbered
                // generic set instead.
                val generic = !device.hasKeys(KeyEvent.KEYCODE_BUTTON_A)[0]
                val buttons = if (generic) GENERIC_BUTTONS else BUTTONS

                val hasHat = has(device, MotionEvent.AXIS_HAT_X) ||
                    has(device, MotionEvent.AXIS_HAT_Y)

                val summary = buildString {
                    append(VENDORS[device.vendorId] ?: "vendor 0x%04x".format(device.vendorId))
                    append(if (hasRxRy) " rstick=RX/RY" else " rstick=Z/RZ")
                    append(
                        when {
                            combined != null -> " triggers=combined(Z)"
                            left != null || right != null -> " triggers=axes"
                            else -> " triggers=buttons"
                        }
                    )
                    append(if (hasHat) " dpad=hat" else " dpad=keys")
                    if (generic) append(" buttons=generic-HID")
                }

                return Layout(
                    leftX = axisOf(device, MotionEvent.AXIS_X),
                    leftY = axisOf(device, MotionEvent.AXIS_Y),
                    rightX = axisOf(device, rightX),
                    rightY = axisOf(device, rightY),
                    leftTrigger = left?.let { axisOf(device, it) } ?: NoAxis.instance,
                    rightTrigger = right?.let { axisOf(device, it) } ?: NoAxis.instance,
                    combinedTrigger = combined?.let { axisOf(device, it) },
                    hatX = axisOf(device, MotionEvent.AXIS_HAT_X),
                    hatY = axisOf(device, MotionEvent.AXIS_HAT_Y),
                    hasHat = hasHat,
                    buttons = buttons,
                    summary = summary,
                )
            }

            private fun has(device: InputDevice, axis: Int): Boolean =
                device.getMotionRange(axis, InputDevice.SOURCE_JOYSTICK) != null

            private fun firstPresent(device: InputDevice, vararg axes: Int): Int? =
                axes.firstOrNull { has(device, it) }

            /**
             * An axis with the device's own reported range and dead zone.
             *
             * `flat` is the manufacturer's statement of how far the axis wanders
             * at rest. Without it a worn stick never reads exactly zero, the
             * snapshot changes on every sample, and a controller nobody is
             * touching becomes the loudest thing on the uplink.
             *
             * A missing range yields a zero-span axis, which always reads as
             * centred — an axis the device does not have must be silent, not an
             * error.
             */
            private fun axisOf(device: InputDevice, axis: Int): Axis {
                val range = device.getMotionRange(axis, InputDevice.SOURCE_JOYSTICK)
                    ?: return Axis(axis, 0f, 0f, 0f)
                return Axis(axis, range.min, range.max, range.flat)
            }
        }
    }

    /**
     * One controller's running state, plus what was last sent.
     *
     * The two are compared rather than the state being sent unconditionally:
     * Android delivers a motion event for any axis that moves, and a stick held
     * still off-centre keeps producing them. Sending only differences turns a
     * held stick from a continuous packet stream into one packet.
     */
    class Pad(val slot: Int, val layout: Layout) {
        var buttons: Int = 0
        var leftTrigger: Int = 0
        var rightTrigger: Int = 0
        var leftStickX: Int = 0
        var leftStickY: Int = 0
        var rightStickX: Int = 0
        var rightStickY: Int = 0

        private var sent: Long = -1L

        fun setButton(bit: Int, down: Boolean) {
            buttons = if (down) buttons or bit else buttons and bit.inv()
        }

        fun neutralise() {
            buttons = 0
            leftTrigger = 0
            rightTrigger = 0
            leftStickX = 0
            leftStickY = 0
            rightStickX = 0
            rightStickY = 0
        }

        fun changed(): Boolean = fingerprint() != sent

        fun commit() { sent = fingerprint() }

        /**
         * Every field in one `Long`, for change detection only.
         *
         * Never sent anywhere and never unpacked — the wire packet is built
         * from the fields themselves by `echo_client::input::gamepad`. This is
         * the one place packing is safe, because a collision costs a skipped
         * duplicate packet rather than a wrong value, and the fields are
         * re-read from the struct regardless.
         */
        private fun fingerprint(): Long =
            (buttons.toLong() and 0xFFFF) or
                ((leftTrigger.toLong() and 0xFF) shl 16) or
                ((rightTrigger.toLong() and 0xFF) shl 24) or
                ((leftStickX.toLong() and 0xFFFF) shl 32) or
                ((leftStickY.toLong() and 0xFFFF) shl 40) or
                ((rightStickX.toLong() and 0xFFFF) shl 48) or
                ((rightStickY.toLong() and 0xFFFF) shl 56)
    }

    companion object {
        private const val TAG = "EchoPad"

        /**
         * Sentinels for the two analog triggers in a button map.
         *
         * Negative so they cannot collide with an XInput bit — the map's values
         * are otherwise the wire format itself, and a sentinel that overlapped
         * one would press a real button.
         */
        private const val TRIGGER_LEFT = -1
        private const val TRIGGER_RIGHT = -2

        /**
         * XInput `XINPUT_GAMEPAD_*` bits.
         *
         * GameStream's low 16 `buttonFlags` are bit-for-bit identical to these,
         * which is why the host hands them to ViGEm with no translation table
         * at all. These constants are therefore the wire format, not a local
         * convention — do not renumber them.
         */
        private const val DPAD_UP = 0x0001
        private const val DPAD_DOWN = 0x0002
        private const val DPAD_LEFT = 0x0004
        private const val DPAD_RIGHT = 0x0008
        private const val START = 0x0010
        private const val BACK = 0x0020
        private const val LEFT_THUMB = 0x0040
        private const val RIGHT_THUMB = 0x0080
        private const val LEFT_SHOULDER = 0x0100
        private const val RIGHT_SHOULDER = 0x0200
        /** Undocumented in XInput but universally honoured, and what ViGEm emits. */
        private const val GUIDE = 0x0400
        private const val A = 0x1000
        private const val B = 0x2000
        private const val X = 0x4000
        private const val Y = 0x8000

        /**
         * How far a hat axis must travel to count as pressed. Hats are
         * three-valued (-1, 0, 1) in practice, so anything clear of zero works;
         * the threshold guards against a hat reported as a noisy analog axis.
         */
        private const val HAT_THRESHOLD = 0.5f

        /**
         * `KEYCODE_BUTTON_*` → XInput bit.
         *
         * `KEYCODE_BACK` is deliberately absent. Many controllers send it for
         * their select button, but Back is also the user's only way out of a
         * fullscreen stream once the pointer is captured, and
         * `MainActivity.dispatchKeyEvent` hands it to the system before this
         * class is ever consulted. Select therefore comes from
         * `KEYCODE_BUTTON_SELECT` only — a pad that sends nothing else loses
         * its select button, which is a far better failure than a user with no
         * way to leave the stream.
         */
        private val BUTTONS: Map<Int, Int> = mapOf(
            KeyEvent.KEYCODE_BUTTON_A to A,
            KeyEvent.KEYCODE_BUTTON_B to B,
            KeyEvent.KEYCODE_BUTTON_X to X,
            KeyEvent.KEYCODE_BUTTON_Y to Y,
            KeyEvent.KEYCODE_BUTTON_L1 to LEFT_SHOULDER,
            KeyEvent.KEYCODE_BUTTON_R1 to RIGHT_SHOULDER,
            KeyEvent.KEYCODE_BUTTON_THUMBL to LEFT_THUMB,
            KeyEvent.KEYCODE_BUTTON_THUMBR to RIGHT_THUMB,
            KeyEvent.KEYCODE_BUTTON_START to START,
            KeyEvent.KEYCODE_BUTTON_SELECT to BACK,
            KeyEvent.KEYCODE_BUTTON_MODE to GUIDE,
            KeyEvent.KEYCODE_DPAD_UP to DPAD_UP,
            KeyEvent.KEYCODE_DPAD_DOWN to DPAD_DOWN,
            KeyEvent.KEYCODE_DPAD_LEFT to DPAD_LEFT,
            KeyEvent.KEYCODE_DPAD_RIGHT to DPAD_RIGHT,
            KeyEvent.KEYCODE_BUTTON_L2 to TRIGGER_LEFT,
            KeyEvent.KEYCODE_BUTTON_R2 to TRIGGER_RIGHT,
        )

        /**
         * The generic HID gamepad set, used when a device reports no
         * `KEYCODE_BUTTON_A` — i.e. Android has no profile for it and fell back
         * to numbering the buttons in HID report order.
         *
         * The order below is the conventional one for HID gamepads and is what
         * `moonlight-android` assumes for the same case: face buttons first in
         * A/B/X/Y positions, then shoulders, then triggers, then select/start,
         * then the stick clicks. A pad whose firmware orders its report
         * differently will land buttons in the wrong places — but it will do so
         * *consistently*, and the log line naming `buttons=generic-HID` is what
         * points at this map when that happens.
         *
         * The d-pad entries are shared with [BUTTONS]: a generic pad still
         * reports its hat through either `AXIS_HAT_*` or `KEYCODE_DPAD_*`, both
         * of which are already handled.
         */
        private val GENERIC_BUTTONS: Map<Int, Int> = mapOf(
            KeyEvent.KEYCODE_BUTTON_1 to A,
            KeyEvent.KEYCODE_BUTTON_2 to B,
            KeyEvent.KEYCODE_BUTTON_3 to X,
            KeyEvent.KEYCODE_BUTTON_4 to Y,
            KeyEvent.KEYCODE_BUTTON_5 to LEFT_SHOULDER,
            KeyEvent.KEYCODE_BUTTON_6 to RIGHT_SHOULDER,
            KeyEvent.KEYCODE_BUTTON_7 to TRIGGER_LEFT,
            KeyEvent.KEYCODE_BUTTON_8 to TRIGGER_RIGHT,
            KeyEvent.KEYCODE_BUTTON_9 to BACK,
            KeyEvent.KEYCODE_BUTTON_10 to START,
            KeyEvent.KEYCODE_BUTTON_11 to LEFT_THUMB,
            KeyEvent.KEYCODE_BUTTON_12 to RIGHT_THUMB,
            KeyEvent.KEYCODE_BUTTON_13 to GUIDE,
            KeyEvent.KEYCODE_DPAD_UP to DPAD_UP,
            KeyEvent.KEYCODE_DPAD_DOWN to DPAD_DOWN,
            KeyEvent.KEYCODE_DPAD_LEFT to DPAD_LEFT,
            KeyEvent.KEYCODE_DPAD_RIGHT to DPAD_RIGHT,
        )

        /**
         * USB vendor IDs, for the log line only.
         *
         * **Deliberately affects no behaviour.** Every actual difference is
         * resolved by asking the device what it has — see [Layout]. Naming a
         * vendor is useful when reading a log and is not evidence about layout:
         * the same vendor ships pads that report differently, and the same pad
         * reports differently over USB and Bluetooth.
         */
        private val VENDORS: Map<Int, String> = mapOf(
            0x045E to "Microsoft",
            0x054C to "Sony",
            0x057E to "Nintendo",
            0x28DE to "Valve",
            0x2DC8 to "8BitDo",
            0x046D to "Logitech",
            0x1532 to "Razer",
        )
    }
}
