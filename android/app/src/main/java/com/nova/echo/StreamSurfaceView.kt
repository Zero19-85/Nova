package com.nova.echo

import android.content.Context
import android.hardware.input.InputManager
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.text.InputType
import android.view.InputDevice
import android.view.KeyCharacterMap
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.PointerIcon
import android.view.RoundedCorner
import android.view.SurfaceView
import android.view.View
import android.view.ViewConfiguration
import android.view.WindowInsets
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputMethodManager

/**
 * The streaming surface, and the app's pointer input source.
 *
 * Modelled directly on `moonlight-android`'s `Game.java` and
 * `AndroidNativePointerCaptureProvider`, because three rounds of live failures
 * here all came from guessing at Android's input semantics instead of reading
 * the one implementation known to work. The three that mattered:
 *
 * 1. **A captured mouse reports its deltas in `AXIS_X`/`AXIS_Y`, not
 *    `AXIS_RELATIVE_X/Y`.** Under capture the source becomes
 *    `SOURCE_MOUSE_RELATIVE` and the ordinary X/Y axes carry relative motion;
 *    `AXIS_RELATIVE_*` is the *uncaptured* spelling. Reading the wrong one
 *    yields zeroes, so capture appeared to do nothing at all.
 * 2. **Motion events are batched, and the batch must be summed.** Android
 *    delivers several samples in one event via `getHistoricalAxisValue`.
 *    Reading only the current value throws most of the movement away, which
 *    feels exactly like a mouse with a very low polling rate.
 * 3. **Input is buffered to VBlank unless you opt out.**
 *    `requestUnbufferedDispatch` is what removes that latency; Moonlight's own
 *    comment calls it "artificially increasing input latency while streaming".
 *
 * A fourth, for the picture: hiding the pointer icon
 * ([PointerIcon.TYPE_NULL]) works even where capture is unavailable, and
 * removing Android's cursor is what stops the compositor drawing over the
 * video — which is what made the stream look washed out whenever the mouse
 * was touched.
 */
class StreamSurfaceView(context: Context) : SurfaceView(context), InputManager.InputDeviceListener {

    /** Set once a session is live; input before that has nowhere to go. */
    var controller: EchoController? = null

    /**
     * Master switch. When false nothing is forwarded at all.
     *
     * A misbehaving input path is worse than no input path: it drags windows
     * around on a machine the user is also sitting at. Being able to kill it
     * without ending the stream is not a nicety.
     */
    var inputEnabled: Boolean = true

    /**
     * Whether touches drive the host pointer.
     *
     * Off by default, and that default is load-bearing: in Android desktop mode
     * the phone screen is a trackpad for the *local* session, so forwarding
     * those touches dragged host windows open (live 2026-08-15).
     */
    var touchAsPointer: Boolean = false

    /**
     * Whether touches are forwarded as **native Windows touch contacts** rather
     * than as a synthesised mouse.
     *
     * Outranks [touchAsPointer] when both are set, because it is the more
     * specific answer to the same question. The host injects real
     * `POINTER_TOUCH_INFO` contacts, so Windows switches to touch-sized hit
     * targets, edge swipes work, press-and-hold becomes a right-click, and
     * multi-touch reaches applications — none of which a moved-and-clicked
     * cursor can do.
     *
     * Off by default for the same reason [touchAsPointer] is: a phone in the
     * hand is not always a pointing device, and the mode that touches someone's
     * live desktop should be one they asked for.
     */
    var absoluteTouch: Boolean = false

    /** Reports whether pointer capture is actually held — not merely requested. */
    var onCaptureChanged: ((Boolean) -> Unit)? = null

    /** Last input source seen, for tracing stray input to a device. */
    var onDiagnostic: ((String) -> Unit)? = null

    /** Why the most recent capture attempt did not take. */
    var onCaptureDiagnosis: ((String) -> Unit)? = null

    private var captureWanted = false
    private var lastReported = 0
    private var mouseEvents = 0L
    private var rateWindow = 0L
    private var rateSince = 0L
    private var eventsPerSecond = 0
    private var peakEventsPerSecond = 0
    private var lastBatchSamples = 0
    private var peakBatchSamples = 0
    private val handler = Handler(Looper.getMainLooper())
    private val inputManager = context.getSystemService(InputManager::class.java)

    /**
     * Reports whether the phone's own keyboard is on screen.
     *
     * The UI needs this for the same reason it needs [onCaptureChanged]: a
     * captured mouse cannot dismiss a keyboard it can no longer point at, so
     * capture is handed back while the IME is up.
     */
    var onKeyboardVisibility: ((Boolean) -> Unit)? = null

    private val imm = context.getSystemService(InputMethodManager::class.java)
    private var keyboardShown = false

    // ── Three-finger tap state ──────────────────────────────────────────────
    // Reset on ACTION_DOWN, judged on ACTION_UP. See [trackTouchGesture].
    private var gestureMaxPointers = 0
    private var gestureRejected = false
    private var gestureStartMs = 0L
    private val gestureDownX = FloatArray(MAX_TRACKED_POINTERS)
    private val gestureDownY = FloatArray(MAX_TRACKED_POINTERS)
    /** Whether the touch-as-pointer path currently holds the host's left button. */
    private var touchButtonDown = false
    private val tapSlop = ViewConfiguration.get(context).scaledTouchSlop * SLOP_MULTIPLIER
    /** Single-finger travel that ends the press-hold window. See [beginPendingPress]. */
    private val dragSlop = ViewConfiguration.get(context).scaledTouchSlop
    /** True once the three-finger gesture has already acted on this touch stream. */
    private var gestureConsumed = false

    // ── Deferred press ──────────────────────────────────────────────────────
    // The state trap that keeps a three-finger keyboard gesture from clicking
    // the host on its way in. See [beginPendingPress] for the whole rationale.
    private var pressPending = false
    private var pressX = 0f
    private var pressY = 0f
    private val pressFlush = Runnable { flushPendingPress() }

    // ── Absolute touch state ────────────────────────────────────────────────
    // Which contacts the host currently believes are down, and where each one
    // was last reported. The host reassembles frames from these transitions, so
    // this side owes it a consistent story: no update for a contact it never
    // saw land, and no contact left down when the gesture ends.
    private val contactDown = BooleanArray(MAX_TRACKED_POINTERS)
    private val contactX = IntArray(MAX_TRACKED_POINTERS)
    private val contactY = IntArray(MAX_TRACKED_POINTERS)

    // The same trap as the deferred press, for contacts. Held transitions have
    // not been transmitted, so they are absent from [contactDown] — that array
    // is what the HOST believes, and nothing may enter it before it goes out on
    // the wire. See [beginHeldTouch].
    private var touchHeld = false
    private val heldTouch = ArrayList<HeldContact>(32)
    private val touchFlush = Runnable { flushHeldTouch() }

    /** One buffered contact transition, in wire units, awaiting a verdict. */
    private class HeldContact(val id: Int, val event: Int, val x: Int, val y: Int)

    /**
     * Fraction of each axis treated as unreachable glass, per edge.
     *
     * On a phone with aggressively curved edges the outermost band of the panel
     * cannot be touched accurately — the finger is on a surface angled away from
     * it — so a 1:1 mapping puts the host's screen corners somewhere the user
     * physically cannot reach. That is where Windows keeps everything that
     * matters: the clock, the show-desktop strip, every window's close button.
     *
     * So the mapping stretches: the central `1 - 2*inset` of the panel spans the
     * host's full extent, and a touch just inside the curve reports the very
     * edge. Calibrated from the platform's own corner radius where it will say
     * ([calibrateCornerInset]); [DEFAULT_CORNER_INSET] otherwise, which is the
     * ~98% box that a Pixel-class curve works out to anyway.
     */
    private var cornerInsetX = DEFAULT_CORNER_INSET
    private var cornerInsetY = DEFAULT_CORNER_INSET

    init {
        isFocusable = true
        isFocusableInTouchMode = true
        requestFocus()

        // Suppress the default focus highlight.
        //
        // Since API 26 a focusable View draws a translucent highlight drawable
        // whenever it holds focus outside touch mode — and connecting a mouse is
        // exactly what takes Android out of touch mode. Over a video surface
        // that renders as the whole picture looking washed out or "selected",
        // appearing the moment the mouse is used and vanishing when the screen
        // is touched. Reported live 2026-08-16 as "the screen brightens when the
        // mouse is enabled"; it was never a colour-space or compositing problem.
        //
        // This view has to be focusable — pointer capture is granted only to a
        // focused view — so the highlight is what goes, not the focus.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            defaultFocusHighlightEnabled = false
        }

        // Captured events arrive through their own callback, but they mean the
        // same thing, so they go to the same handler.
        setOnCapturedPointerListener { _, event -> handlePointer(event, captured = true) }
    }

    /**
     * Ask for unbuffered input — and do it here, not in the constructor.
     *
     * `requestUnbufferedDispatch` forwards the request up the view hierarchy to
     * the ViewRootImpl. A view with no parent has nothing to forward to, so the
     * call is silently discarded. This view is built inside Compose's
     * `AndroidView` factory, where it has no parent yet, so the request made in
     * `init` never reached the framework at all.
     *
     * The cost was exact and measurable: input stayed buffered to VBlank, so a
     * 1000 Hz mouse was delivered at the display's refresh rate. The host
     * logged **40–62 relative packets per second** during continuous movement —
     * one per frame — which is why the pointer moved in visible jumps rather
     * than lagging smoothly. (Moonlight makes the same call in `onCreate` on a
     * view already inflated into its layout, so its request does propagate;
     * that difference is invisible unless you know to look for the parent.)
     *
     * `SOURCE_CLASS_TRACKBALL` is not a typo: a mouse under pointer capture is
     * classified there, so omitting it would leave the captured path buffered.
     */
    override fun onAttachedToWindow() {
        super.onAttachedToWindow()

        // Kill the focus highlight on every ancestor too, not just here.
        //
        // The translucent "selected" wash over the video appears when a mouse is
        // used and disappears on touch, which points at focus rather than at
        // colour: attaching a mouse takes Android out of touch mode, and a
        // focused view outside touch mode draws a highlight drawable. Disabling
        // it on this view alone was not enough, because the view sits inside
        // Compose's `AndroidView` container, and the container is a focusable
        // ViewGroup that draws its own.
        stripAncestorDecorations()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            requestUnbufferedDispatch(
                InputDevice.SOURCE_CLASS_BUTTON or      // keyboards
                    InputDevice.SOURCE_CLASS_JOYSTICK or   // gamepads
                    InputDevice.SOURCE_CLASS_POINTER or    // touchscreens, uncaptured mice
                    InputDevice.SOURCE_CLASS_POSITION or   // touchpads
                    InputDevice.SOURCE_CLASS_TRACKBALL     // mice under pointer capture
            )
        }
    }

    // ── Capture ─────────────────────────────────────────────────────────────

    /**
     * Hide Android's cursor and take the mouse.
     *
     * The icon is hidden unconditionally, because it works on devices where
     * capture does not (DeX, ChromeOS) and because a visible cursor is what
     * forces the compositor to draw over the video.
     */
    fun captureMouse() {
        captureWanted = true
        setPointerIcon(PointerIcon.getSystemIcon(context, PointerIcon.TYPE_NULL))
        inputManager?.registerInputDeviceListener(this, null)
        attemptCapture("enabling capture")
    }

    fun releaseMouse() {
        captureWanted = false
        // Unregister BEFORE releasing: releasing capture can itself fire an
        // onInputDeviceChanged for touchpad-bearing devices, which would
        // immediately re-request capture.
        inputManager?.unregisterInputDeviceListener(this)
        releasePointerCapture()
        setPointerIcon(null)
    }

    /**
     * Never enter the hovered state.
     *
     * `View.onHoverEvent`'s default implementation calls `setHovered(true)`,
     * which is a drawable state change — and any background or foreground with
     * a hovered state then paints over the video. It is the other half of the
     * "mouse makes the picture look selected" behaviour, since hover is exactly
     * what a mouse produces and a finger does not.
     */
    /**
     * Clear focus highlighting and foreground drawables on this view and every
     * ancestor up to the root.
     *
     * The wash appears only when this view holds focus outside touch mode, which
     * is the exact condition under which Android paints a focus highlight — but
     * it survived disabling that on this view alone, because the view sits
     * inside Compose's `AndroidView` container, itself a focusable ViewGroup
     * that decorates independently.
     */
    private fun stripAncestorDecorations() {
        var node: Any? = this
        var depth = 0
        while (node is android.view.View) {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                node.defaultFocusHighlightEnabled = false
            }
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
                node.foreground = null
            }
            // Ancestor BACKGROUNDS matter here, contrary to the usual intuition
            // that a background renders harmlessly behind things.
            //
            // A SurfaceView does not draw into the window; it gets its own layer
            // underneath and punches a transparent hole in the window above it.
            // Anything an ancestor paints — including a background with a
            // `state_focused` entry in its state list — is therefore drawn in
            // the window layer *over* that hole, i.e. over the video. That is
            // the shape of the reported symptom exactly: a translucent film
            // across the whole picture that appears when the view takes focus
            // outside touch mode and clears the instant the screen is touched.
            //
            // Skipped for this view itself (depth 0) and for the DecorView
            // (recognised by its parent not being a View — its background is the
            // window's own and clearing it would be a visible regression).
            if (depth > 0 && node.parent is android.view.View) {
                node.background = null
            }
            node = node.parent
            depth++
        }
    }

    /**
     * Draw nothing on top of the video, ever.
     *
     * `onDrawForeground` is the single pass that paints a View's scrollbars, its
     * foreground drawable, **and** the default focus highlight. Suppressing the
     * highlight by flag and clearing the foreground both failed to remove the
     * white wash, and each of those was a guess about *which* of them was
     * responsible. Overriding the pass itself removes the question: nothing this
     * view owns can paint over the picture, whatever state it is in.
     *
     * Safe because this view has no scrollbars and no foreground it wants — it
     * is a video surface, and the only correct thing to draw on top of it is
     * nothing.
     */
    override fun onDrawForeground(canvas: android.graphics.Canvas) {
        // Deliberately empty; see above.
    }

    /**
     * Re-strip ancestor decorations whenever focus changes.
     *
     * The walk in [onAttachedToWindow] runs once, but a Compose container can
     * set a foreground *after* attach — and focus change is exactly the moment
     * the wash appears, so it is the right moment to re-assert.
     */
    override fun onFocusChanged(gainFocus: Boolean, direction: Int, previous: android.graphics.Rect?) {
        super.onFocusChanged(gainFocus, direction, previous)
        if (gainFocus) stripAncestorDecorations()
    }

    override fun onHoverChanged(hovered: Boolean) {
        // Deliberately not calling super: nothing here should ever render a
        // hover state over a video surface.
    }

    override fun onPointerCaptureChange(hasCapture: Boolean) {
        super.onPointerCaptureChange(hasCapture)
        remainderX = 0f
        remainderY = 0f
        onCaptureChanged?.invoke(hasCapture)
    }

    /**
     * Ask for the pointer, and say what happened.
     *
     * Every precondition Android checks is silent when it fails:
     * `requestPointerCapture` returns void, logs nothing an app can see, and
     * leaves no way to distinguish "refused" from "never asked". Three rounds
     * of this were spent unable to tell which — so each precondition is
     * evaluated separately here and reported by name.
     */
    private fun attemptCapture(trigger: String) {
        if (!captureWanted) return

        val devices = InputDevice.getDeviceIds().toList().mapNotNull { InputDevice.getDevice(it) }
        val pointers = devices.filter {
            it.supportsSource(InputDevice.SOURCE_MOUSE) ||
                it.supportsSource(InputDevice.SOURCE_MOUSE_RELATIVE) ||
                it.supportsSource(InputDevice.SOURCE_TOUCHPAD)
        }
        if (!hasCaptureCompatibleDevice()) {
            onCaptureDiagnosis?.invoke(
                if (pointers.isEmpty()) {
                    "no mouse or touchpad is attached — connect one, then tap the video"
                } else {
                    // Every pointer also claims to be a touchscreen, so the
                    // touchscreen filter rejected all of them.
                    "the only pointer devices also report as touchscreens " +
                        "(${pointers.joinToString { it.name }}) — capture is skipped for those"
                }
            )
            return
        }

        val focused = isFocused || requestFocus()
        if (!focused) {
            onCaptureDiagnosis?.invoke(
                "the video view cannot take focus ($trigger) — Android refuses capture " +
                    "to an unfocused view"
            )
            return
        }
        if (!hasWindowFocus()) {
            onCaptureDiagnosis?.invoke("the window does not have focus ($trigger) — retrying on tap")
            return
        }

        requestPointerCapture()

        // Verify by polling rather than trusting the callback.
        //
        // `onPointerCaptureChange` is dispatched to the window's *focused*
        // view. If focus moves between the request and the grant — and under
        // Compose it can, because Compose's focus owner runs its own pass over
        // the embedded view — the capture is granted but the callback lands
        // somewhere else, and the app concludes it was refused. `hasPointerCapture()`
        // asks the framework directly and cannot be missed that way.
        //
        // This is why the previous round could not distinguish "Android refused"
        // from "Android granted and we never heard": both look like silence.
        handler.postDelayed({
            val held = hasPointerCapture()
            if (held) {
                // Announce it ourselves; the callback evidently is not coming.
                onCaptureChanged?.invoke(true)
                onCaptureDiagnosis?.invoke("held (granted via $trigger)")
            } else {
                onCaptureDiagnosis?.invoke(
                    "Android declined the request from $trigger. The view and window " +
                        "both had focus and a mouse was present, so this is the OS's " +
                        "own policy — check for a system \"mouse (Games)\" or pointer-" +
                        "capture permission for Echo. Relative mouse input still works " +
                        "without it."
                )
            }
        }, CAPTURE_VERIFY_MS)
    }

    /**
     * Capture is dropped whenever the window loses focus and is not restored
     * automatically.
     *
     * The delay is required, not defensive: requesting immediately on regaining
     * focus hits "requestPointerCapture called for a window that has no focus"
     * and silently fails. Moonlight uses 500 ms for the same reason.
     */
    override fun onWindowFocusChanged(hasWindowFocus: Boolean) {
        super.onWindowFocusChanged(hasWindowFocus)
        if (!hasWindowFocus || !captureWanted) return
        handler.postDelayed({ attemptCapture("window focus") }, RECAPTURE_DELAY_MS)
    }

    /**
     * Whether any attached device justifies capture.
     *
     * Touchscreens are skipped deliberately: some devices report a touchpad as
     * `SOURCE_TOUCHSCREEN or SOURCE_MOUSE`, and capturing for those breaks
     * stylus and touch input.
     */
    fun hasCaptureCompatibleDevice(): Boolean = InputDevice.getDeviceIds().any { id ->
        val device = InputDevice.getDevice(id) ?: return@any false
        if (device.supportsSource(InputDevice.SOURCE_TOUCHSCREEN)) return@any false
        device.supportsSource(InputDevice.SOURCE_MOUSE) ||
            device.supportsSource(InputDevice.SOURCE_MOUSE_RELATIVE) ||
            device.supportsSource(InputDevice.SOURCE_TOUCHPAD)
    }

    override fun onInputDeviceAdded(deviceId: Int) {
        if (captureWanted && !hasPointerCapture()) {
            attemptCapture("device attached")
        }
    }

    override fun onInputDeviceRemoved(deviceId: Int) {
        if (hasPointerCapture() && !hasCaptureCompatibleDevice()) {
            releasePointerCapture()
        }
    }

    override fun onInputDeviceChanged(deviceId: Int) {
        // Remove+add is sufficient. Careful: this can fire as a *result* of
        // requestPointerCapture(), because trackpads gain SOURCE_MOUSE_RELATIVE
        // when captured.
        onInputDeviceRemoved(deviceId)
        onInputDeviceAdded(deviceId)
    }

    // ── Pointer ─────────────────────────────────────────────────────────────

    override fun onCapturedPointerEvent(event: MotionEvent): Boolean =
        handlePointer(event, captured = true) || super.onCapturedPointerEvent(event)

    /**
     * Joystick motion and uncaptured pointer motion arrive at the same
     * callback, so the controller path is offered the event first.
     *
     * [ControllerHandler.onMotion] returns false for anything that is not a
     * gamepad's `ACTION_MOVE`, so a mouse falls straight through to
     * [handlePointer] as before. Order matters only because a joystick event
     * carries `AXIS_X`/`AXIS_Y` too, and the pointer path would happily read a
     * stick as a cursor.
     */
    override fun onGenericMotionEvent(event: MotionEvent): Boolean =
        (active() != null && controller?.gamepads?.onMotion(event) == true) ||
            handlePointer(event, captured = false) ||
            super.onGenericMotionEvent(event)

    /**
     * An **uncaptured** mouse arrives here, and only here.
     *
     * `View.dispatchGenericMotionEvent` sends `ACTION_HOVER_ENTER/MOVE/EXIT`
     * to `dispatchHoverEvent` → [onHoverEvent]; `onGenericMotionEvent` sees
     * scroll and joystick events but never hover. Without this override the
     * entire uncaptured-mouse branch of [handlePointer] was unreachable, so
     * moving the mouse produced *nothing* — the overlay's "last input source"
     * stayed on whatever had last touched the screen, which is exactly how this
     * was found (live 2026-08-16).
     *
     * Captured mice do not come through here; they arrive via
     * [onCapturedPointerEvent]. So this path exists precisely for the case
     * where capture was refused, which is the case that has to keep working.
     */
    override fun onHoverEvent(event: MotionEvent): Boolean =
        handlePointer(event, captured = false) || super.onHoverEvent(event)

    /**
     * One path for every pointer event, captured or not.
     *
     * Relative motion is preferred whenever the event carries it; otherwise a
     * mouse falls back to absolute position within this view.
     */
    private fun handlePointer(event: MotionEvent, captured: Boolean): Boolean {
        val c = active() ?: return false
        report(event)

        // `captured` comes from *which callback delivered this*, which is
        // authoritative in a way `event.source` is not. Captured events have
        // been observed arriving with `source == 0` (SOURCE_UNKNOWN), and every
        // branch below keys off the source — so those events matched nothing
        // and were dropped, throwing away motion on the very path that is
        // supposed to be the good one.
        if (captured || eventHasRelativeAxes(event)) {
            emitRelativeSamples(event, captured, c)
        } else if (isMouse(event)) {
            // Capture was refused, so this is an ordinary hovering mouse. Prefer
            // its RELATIVE axes anyway.
            //
            // `AXIS_RELATIVE_X/Y` are populated on uncaptured mouse events from
            // API 24 onward, and using them avoids the absolute path's real
            // defect: absolute positions describe *Android's* cursor, which has
            // already been through the OS's own acceleration and is clamped to
            // the screen. Once that cursor reaches an edge it stops reporting
            // movement no matter how far the hand keeps going, so a long swipe
            // silently loses everything past the boundary.
            //
            // Relative deltas have no such ceiling. They still stop at the edge
            // (Android has nowhere left to move its cursor) — only real pointer
            // capture removes that limit — but everything before the edge is
            // reported faithfully instead of being squeezed through a mapping.
            val moved = hasRelativeMotion(event)
            if (moved) {
                emitRelativeSamples(event, captured = false, c = c)
            } else if (event.actionMasked == MotionEvent.ACTION_HOVER_MOVE) {
                // No relative axes on this event — fall back to position.
                sendAbsolute(event, c)
            }
        } else {
            return false // touch and joysticks are not handled here
        }

        when (event.actionMasked) {
            MotionEvent.ACTION_BUTTON_PRESS -> buttonOf(event)?.let { c.mouseButton(it, true) }
            MotionEvent.ACTION_BUTTON_RELEASE -> buttonOf(event)?.let { c.mouseButton(it, false) }
            MotionEvent.ACTION_SCROLL -> {
                // Android reports notches as a float; Windows wants WHEEL_DELTA.
                val amount = (event.getAxisValue(MotionEvent.AXIS_VSCROLL) * WHEEL_DELTA).toInt()
                if (amount != 0) c.scroll(amount)
            }
        }
        return true
    }

    /**
     * `SOURCE_MOUSE_RELATIVE` is how a mouse appears once this view has
     * capture. A touchpad carries relative axes only while captured.
     */
    private fun eventHasRelativeAxes(event: MotionEvent): Boolean {
        val source = event.source
        return (source == InputDevice.SOURCE_MOUSE_RELATIVE &&
            event.getToolType(0) == MotionEvent.TOOL_TYPE_MOUSE) ||
            (source == InputDevice.SOURCE_TOUCHPAD && hasPointerCapture())
    }

    /**
     * A relative delta, including every batched sample, with the sub-pixel
     * remainder carried to the next event.
     *
     * Three traps in a dozen lines.
     *
     * The **axis differs by source**: a captured mouse puts relative motion in
     * `AXIS_X`/`AXIS_Y` and everything else uses `AXIS_RELATIVE_X/Y`. This is
     * the opposite of the intuitive reading and matches
     * `AndroidNativePointerCaptureProvider.getRelativeAxisX` exactly; reading
     * `AXIS_RELATIVE_*` under capture yields zeroes, i.e. a dead mouse.
     *
     * The event carries a **history** of samples that must be summed, or most
     * of the motion is discarded.
     *
     * And the axes are **floats**. With unbuffered dispatch a high-polling-rate
     * mouse delivers many events whose delta is a fraction of a pixel, so
     * truncating each one independently throws away a large share of slow and
     * medium movement — the pointer travels a shorter distance than the hand
     * did, which reads as a mouse that is "slow" rather than one that is
     * laggy. Keeping the remainder makes the mapping exact over any number of
     * events. (Moonlight truncates per event here; this is one of the few
     * places Echo should not copy it.)
     */
    /**
     * Whether an uncaptured mouse event carries usable relative axes at all.
     *
     * Checked across the batch, not just the current sample: a batched event can
     * report zero on its newest sample while its history holds the movement.
     */
    private fun hasRelativeMotion(event: MotionEvent): Boolean {
        if (event.getAxisValue(MotionEvent.AXIS_RELATIVE_X) != 0f ||
            event.getAxisValue(MotionEvent.AXIS_RELATIVE_Y) != 0f
        ) return true
        for (i in 0 until event.historySize) {
            if (event.getHistoricalAxisValue(MotionEvent.AXIS_RELATIVE_X, i) != 0f ||
                event.getHistoricalAxisValue(MotionEvent.AXIS_RELATIVE_Y, i) != 0f
            ) return true
        }
        return false
    }

    private fun emitRelativeSamples(event: MotionEvent, captured: Boolean, c: EchoController) {
        val relative = captured || event.source == InputDevice.SOURCE_MOUSE_RELATIVE
        val xAxis = if (relative) MotionEvent.AXIS_X else MotionEvent.AXIS_RELATIVE_X
        val yAxis = if (relative) MotionEvent.AXIS_Y else MotionEvent.AXIS_RELATIVE_Y

        // Every batched sample becomes its own movement.
        //
        // This is the same mistake as summing deltas in `coalesce`, one layer
        // earlier and better hidden. Android batches motion samples into a
        // single MotionEvent — the extras are reachable only through
        // `getHistoricalAxisValue` — and adding them together preserved the
        // total distance while collapsing the whole batch into ONE host
        // `SendInput`. The pointer therefore advanced once per delivered event
        // no matter how many samples the mouse had actually produced, which is
        // why the host kept logging a rate near the display's refresh rate
        // (120 Hz on this phone) instead of the mouse's, and why motion arrived
        // as hops.
        //
        // Emitting per sample restores the real cadence and costs 14 bytes each,
        // in the same datagram. Note this also makes the app correct whether or
        // not `requestUnbufferedDispatch` is honoured: if it is, batches are
        // size 1 and this loop does nothing extra.
        // Recorded so the overlay can show how many samples Android is packing
        // into one event — the multiplier between the delivered event rate and
        // the mouse's real report rate.
        lastBatchSamples = event.historySize + 1
        if (lastBatchSamples > peakBatchSamples) peakBatchSamples = lastBatchSamples
        for (i in 0 until event.historySize) {
            emitMove(
                event.getHistoricalAxisValue(xAxis, i),
                event.getHistoricalAxisValue(yAxis, i),
                c,
            )
        }
        emitMove(event.getAxisValue(xAxis), event.getAxisValue(yAxis), c)
    }

    /**
     * Send one sample, carrying the sub-pixel remainder.
     *
     * The axes are floats and a fast mouse reports many samples below one pixel;
     * truncating each independently would discard a large share of slow and
     * medium movement. The remainder makes the mapping exact across any number
     * of samples.
     */
    private fun emitMove(fx: Float, fy: Float, c: EchoController) {
        val x = fx + remainderX
        val y = fy + remainderY
        // toInt() truncates toward zero, so the remainder keeps the sign of the
        // motion and negative movement accumulates as accurately as positive.
        val dx = x.toInt()
        val dy = y.toInt()
        remainderX = x - dx
        remainderY = y - dy
        if (dx != 0 || dy != 0) c.mouseMove(dx, dy)
    }

    /**
     * Sub-pixel motion not yet reported, per axis.
     *
     * Reset whenever capture changes, because the remainder describes a gesture
     * in progress and carrying it across a capture boundary would apply a
     * fraction of the old device's movement to the new one.
     */
    private var remainderX = 0f
    private var remainderY = 0f

    /**
     * Touch, and an uncaptured mouse's drags.
     *
     * A finger is inherently absolute — there is no previous position when a
     * touch begins — so this moves the pointer to the touch and then presses.
     */
    override fun onTouchEvent(event: MotionEvent): Boolean {
        // A tap is the most reliable moment to take the pointer.
        //
        // `requestPointerCapture` silently fails unless this view has focus AND
        // its window has focus, and neither is guaranteed here: Compose owns
        // the focus system, and an `AndroidView`-embedded SurfaceView can sit
        // unfocused for an entire session — the same root cause that forced
        // keyboard input up to `Activity.dispatchKeyEvent`. Capture cannot use
        // that escape hatch, because it is defined in terms of focus.
        //
        // Inside a touch handler both conditions are guaranteed true by
        // construction: the window is focused because the user is touching it,
        // and requesting focus here actually takes it. Retrying on every tap
        // also gives the user something to *do* when the overlay reports
        // capture was refused, rather than only an explanation.
        if (event.actionMasked == MotionEvent.ACTION_DOWN && !hasPointerCapture()) {
            attemptCapture("tap")
        }

        // Gesture tracking runs ABOVE the `active()` gate below, deliberately.
        // The three-finger tap is a gesture on *this phone* — it summons the
        // phone's own keyboard — so it must keep working when input forwarding
        // is switched off, and before a session has been granted. Gating it on
        // a live controller would make the one control that rescues a stuck
        // session unavailable in exactly the states worth rescuing.
        trackTouchGesture(event)

        val c = active() ?: return super.onTouchEvent(event)
        report(event)

        if (event.actionMasked == MotionEvent.ACTION_DOWN &&
            Build.VERSION.SDK_INT >= Build.VERSION_CODES.LOLLIPOP
        ) {
            // Still required even with the source-mask call in init.
            requestUnbufferedDispatch(event)
        }

        if (!event.isFromSource(InputDevice.SOURCE_TOUCHSCREEN)) {
            return super.onTouchEvent(event)
        }

        // Absolute touch outranks the pointer mode: it is the more specific
        // answer to the same question, and running both would put a mouse click
        // and a touch contact on the host for one finger.
        return when {
            absoluteTouch -> { handleAbsoluteTouch(event, c); true }
            touchAsPointer -> { handlePointerTouch(event, c); true }
            else -> super.onTouchEvent(event)
        }
    }

    // ── Touch as a mouse: the deferred press ────────────────────────────────

    /**
     * Drive the host's cursor from a finger, with the button held back.
     *
     * A finger is inherently absolute — there is no previous position when a
     * touch begins — so this moves the pointer to the touch and then presses.
     * The delay before pressing is the whole point; see [beginPendingPress].
     */
    private fun handlePointerTouch(event: MotionEvent, c: EchoController) {
        // A multi-finger gesture is never pointer input.
        //
        // Judged on the high-water mark rather than the live pointer count, so
        // the tail of the gesture — the fingers lifting one at a time, ending
        // with a single-pointer ACTION_UP — is suppressed too. Without that the
        // last finger up reads as an ordinary tap and clicks the host at
        // whatever the keyboard gesture happened to be over.
        //
        // Cancelling the pending press is what makes a *fast* gesture safe, and
        // releasing is what makes a slow one safe: three fingers that land
        // inside the hold window never press at all, and three that take longer
        // than that press and are released here rather than left holding a
        // button down on a machine the user is also sitting at.
        if (gestureMaxPointers > 1) {
            cancelPendingPress()
            releaseTouchButton(c)
            return
        }

        when (event.actionMasked) {
            MotionEvent.ACTION_DOWN -> {
                // The cursor moves immediately; only the *button* waits. Moving
                // costs nothing if this turns out to be a keyboard gesture — a
                // cursor that travelled is not an action — and doing it now is
                // what keeps a drag's first pixel free of the hold window.
                sendTouchAsPointer(event.x, event.y, c)
                beginPendingPress(event)
            }
            MotionEvent.ACTION_MOVE -> {
                // Condition C: real travel means this is a drag or a pan, not a
                // gesture in progress. Press now rather than waiting out the
                // window, so the host sees the button down before the motion it
                // belongs to. Flushed *before* the samples below for exactly
                // that reason: the press lands where the finger went down.
                if (pressPending && travelledPastSlop(event)) flushPendingPress()
                // Historical samples matter here too: a fast drag delivers
                // several positions per event.
                for (i in 0 until event.historySize) {
                    sendTouchAsPointer(event.getHistoricalX(i), event.getHistoricalY(i), c)
                }
                sendTouchAsPointer(event.x, event.y, c)
            }
            MotionEvent.ACTION_UP -> {
                sendTouchAsPointer(event.x, event.y, c)
                // A tap quicker than the hold window is still a tap. Flushing
                // here is what stops the trap eating short taps outright —
                // Condition B's timer is the *slow* path, not the only one.
                flushPendingPress()
                releaseTouchButton(c)
            }
            MotionEvent.ACTION_CANCEL -> {
                // Android took the gesture away. Nothing was a click.
                cancelPendingPress()
                releaseTouchButton(c)
            }
        }
    }

    /**
     * Arm the press trap: buffer the button-down for [PRESS_HOLD_MS] instead of
     * sending it.
     *
     * **The problem it solves.** The three-finger keyboard gesture is a gesture
     * on *this phone*, but its first finger is indistinguishable from a tap
     * meant for the host — and by the time the second finger lands, the press
     * has already gone out. The old code released that button as soon as it saw
     * a second pointer, which kept the host from being left with a button held
     * but still delivered a complete down-then-up: a stray click, on whatever
     * the user happened to be over, every time they asked for the keyboard.
     * Releasing early cannot fix that, because the click has already happened.
     *
     * So the press is *withheld* rather than undone, and one of three things
     * ends the wait:
     *
     *  - **A third finger** ([trackTouchGesture]) — the payload is dropped and
     *    the keyboard opens. The host never learns the screen was touched.
     *  - **The timer** ([PRESS_HOLD_MS]) — an ordinary press-and-hold, flushed.
     *  - **Travel past [dragSlop]** — a drag or a pan, flushed at once so the
     *    hold window costs a moving finger nothing.
     *
     * Only the first of those adds any latency a user can perceive, and it is
     * the case where the click is not wanted at all.
     */
    private fun beginPendingPress(event: MotionEvent) {
        cancelPendingPress()
        pressPending = true
        pressX = event.x
        pressY = event.y
        handler.postDelayed(pressFlush, PRESS_HOLD_MS)
    }

    /** Whether the finger has moved far enough to be a drag rather than a tap. */
    private fun travelledPastSlop(event: MotionEvent): Boolean =
        Math.abs(event.x - pressX) > dragSlop || Math.abs(event.y - pressY) > dragSlop

    /**
     * Send the buffered press. Idempotent, and a no-op if the trap already
     * closed — every exit from the gesture calls it or [cancelPendingPress], and
     * the two must be safe to call in either order.
     */
    private fun flushPendingPress() {
        if (!pressPending) return
        pressPending = false
        handler.removeCallbacks(pressFlush)
        // Re-checked rather than captured when the trap was armed: input can be
        // switched off, or the session can end, inside the hold window. A press
        // sent to a controller that has gone away is at best discarded and at
        // worst a button the host is left holding.
        val c = active() ?: return
        c.mouseButton(BUTTON_LEFT, true)
        touchButtonDown = true
    }

    /** Drop the buffered press without ever sending it. */
    private fun cancelPendingPress() {
        if (!pressPending) return
        pressPending = false
        handler.removeCallbacks(pressFlush)
    }

    /** Let go of the host's left button, if this view is the one holding it. */
    private fun releaseTouchButton(c: EchoController) {
        cancelPendingPress()
        if (!touchButtonDown) return
        touchButtonDown = false
        c.mouseButton(BUTTON_LEFT, false)
    }

    // ── Touch as touch: native Windows contacts ─────────────────────────────

    /**
     * Forward the whole touch stream as native contacts.
     *
     * Gated by the same 60 ms trap as the deferred press, and it has to be.
     *
     * **Retraction is not enough, and this is the lesson.** The first version
     * forwarded contacts immediately and cancelled them once a third finger
     * arrived, on the reasoning that a contact is not a button — there is no
     * click to withhold, so nothing irreversible had happened yet. That was
     * wrong about Windows. A `POINTER_TOUCH_INFO` contact that goes down and is
     * then cancelled is not a no-op: the desktop has already had a finger put on
     * it, and it reacts — focus moves, a press-and-hold timer starts, a control
     * under the contact lights up. The cancel undoes the *contact*, not what the
     * contact caused. Reported live 2026-08-24 as the taskbar responding to a
     * keyboard gesture.
     *
     * So a contact is withheld exactly like a press: nothing reaches the host
     * until the gesture has declared itself. The retraction below is still here,
     * because it is what covers a third finger arriving *after* the window
     * closed — by then the contacts really have gone out and cancelling them is
     * the best that can be done.
     *
     * The cost is unchanged: three simultaneous fingers are reserved by the
     * client and never reach Windows. Two are not, so pinch-zoom and two-finger
     * scroll work as they would on a real touchscreen.
     */
    private fun handleAbsoluteTouch(event: MotionEvent, c: EchoController) {
        if (gestureMaxPointers >= THREE_FINGERS) {
            // Only reachable once the window has closed — inside it,
            // [trackTouchGesture] consumes the gesture and drops the buffer, so
            // there is nothing here to retract.
            dropHeldTouch()
            cancelAllContacts(c)
            return
        }

        when (event.actionMasked) {
            MotionEvent.ACTION_DOWN -> {
                // The first finger arms the trap. Everything the gesture
                // produces from here — including the second finger and any
                // movement — is buffered until the timer, a lift, or a third
                // finger says what it was.
                beginHeldTouch()
                val i = event.actionIndex
                recordContact(c, event.getPointerId(i), EchoNative.TOUCH_DOWN, event.getX(i), event.getY(i))
            }
            MotionEvent.ACTION_POINTER_DOWN -> {
                val i = event.actionIndex
                recordContact(c, event.getPointerId(i), EchoNative.TOUCH_DOWN, event.getX(i), event.getY(i))
            }
            MotionEvent.ACTION_MOVE -> {
                // Only ACTION_MOVE carries history, and one historical sample
                // holds a position for *every* pointer — the batch is a moment
                // in time, not a moment for one finger. Iterating pointers
                // inside samples (not the reverse) is what keeps a two-finger
                // gesture's contacts in step with each other.
                for (h in 0 until event.historySize) {
                    for (i in 0 until event.pointerCount) {
                        recordContact(
                            c,
                            event.getPointerId(i),
                            EchoNative.TOUCH_UPDATE,
                            event.getHistoricalX(i, h),
                            event.getHistoricalY(i, h),
                        )
                    }
                }
                for (i in 0 until event.pointerCount) {
                    recordContact(c, event.getPointerId(i), EchoNative.TOUCH_UPDATE, event.getX(i), event.getY(i))
                }
            }
            MotionEvent.ACTION_POINTER_UP -> {
                val i = event.actionIndex
                recordContact(c, event.getPointerId(i), EchoNative.TOUCH_UP, event.getX(i), event.getY(i))
            }
            MotionEvent.ACTION_UP -> {
                val i = event.actionIndex
                recordContact(c, event.getPointerId(i), EchoNative.TOUCH_UP, event.getX(i), event.getY(i))
                // The last finger is off the glass, so the gesture has said
                // everything it is going to say — and it was not a three-finger
                // tap. Flushing here rather than waiting out the remaining
                // milliseconds is what keeps a quick tap feeling like a tap;
                // without it the whole gesture would arrive at the host after
                // the finger had already left, which reads as lag on the one
                // interaction most sensitive to it.
                flushHeldTouch()
            }
            MotionEvent.ACTION_CANCEL -> {
                // Android took the gesture away. Nothing buffered ever happened,
                // and anything already sent is retracted.
                dropHeldTouch()
                cancelAllContacts(c)
            }
        }
    }

    /**
     * Arm the contact trap for one gesture.
     *
     * Deliberately the same [PRESS_HOLD_MS] the deferred press uses. The two
     * traps are answering the identical question — "is this the start of a
     * keyboard gesture?" — and a person's hand does not roll onto the glass at
     * different speeds depending on which mode the app is in. Two constants here
     * would be two things to tune and one of them would end up wrong.
     */
    private fun beginHeldTouch() {
        dropHeldTouch()
        touchHeld = true
        handler.postDelayed(touchFlush, PRESS_HOLD_MS)
    }

    /**
     * Buffer a transition, or send it if the trap has already closed.
     *
     * The single funnel for the absolute path, so there is no route to the wire
     * that forgets to check. Coordinates are normalised *here*, on the way into
     * the buffer, rather than at flush time: the transform depends on the view's
     * size and corner calibration, and both can change between a contact being
     * held and being released — a rotation inside the window would otherwise
     * replay the gesture through the new geometry and land it somewhere the
     * finger never was.
     */
    private fun recordContact(c: EchoController, id: Int, event: Int, x: Float, y: Float) {
        if (id < 0 || id >= MAX_TRACKED_POINTERS) return
        if (!touchHeld) {
            sendContact(c, id, event, x, y)
            return
        }
        if (heldTouch.size >= MAX_HELD_CONTACTS) {
            // A digitiser producing this much inside 60 ms is not a three-finger
            // tap by any reading, so the gesture is resolved in favour of the
            // host rather than growing a buffer without bound. Flushing (not
            // dropping) is the safe direction: the worst case is the leak this
            // trap exists to prevent, and the alternative is discarding input
            // the user really made.
            flushHeldTouch()
            sendContact(c, id, event, x, y)
            return
        }
        heldTouch.add(
            HeldContact(id, event, normalize(x, width, cornerInsetX), normalize(y, height, cornerInsetY))
        )
    }

    /**
     * Release the buffer to the host, in order, and resume real-time forwarding.
     *
     * Replayed through [sendContact]'s bookkeeping rather than written straight
     * to the wire, so [contactDown] is populated by the same rules a live
     * gesture uses — that array is the host's view, and a contact must not
     * appear in it until the moment its `down` actually leaves.
     */
    private fun flushHeldTouch() {
        if (!touchHeld) return
        touchHeld = false
        handler.removeCallbacks(touchFlush)
        // Re-read rather than captured when the trap was armed: input can be
        // switched off, or the session can end, inside the window.
        val c = active()
        if (c == null) {
            heldTouch.clear()
            return
        }
        for (h in heldTouch) sendContactAt(c, h.id, h.event, h.x, h.y)
        heldTouch.clear()
    }

    /** Discard the buffer. Nothing in it was ever transmitted, so nothing is owed. */
    private fun dropHeldTouch() {
        if (!touchHeld && heldTouch.isEmpty()) return
        touchHeld = false
        handler.removeCallbacks(touchFlush)
        heldTouch.clear()
    }

    /**
     * Send one contact transition, keeping this side's story consistent.
     *
     * The host reassembles per-contact transitions into whole frames, so it
     * trusts the sequence: an update for a contact it never saw land is dropped,
     * and a lift it never hears about is a finger left pressed on the desktop.
     * [contactDown] is what guarantees neither happens — Android is well-behaved
     * here, but the gesture retraction above and a session ending mid-drag both
     * produce sequences the raw `MotionEvent` stream does not.
     */
    private fun sendContact(c: EchoController, id: Int, event: Int, x: Float, y: Float) =
        sendContactAt(c, id, event, normalize(x, width, cornerInsetX), normalize(y, height, cornerInsetY))

    /**
     * The wire, in [EchoNative.TOUCH_REF] units.
     *
     * Split from [sendContact] so a buffered transition can be replayed at the
     * coordinates it was recorded with — see [recordContact] for why the
     * transform has to happen when the finger is read, not when it is released.
     */
    private fun sendContactAt(c: EchoController, id: Int, event: Int, rx: Int, ry: Int) {
        // Ids beyond the host's synthetic device are a finger it cannot carry.
        if (id < 0 || id >= MAX_TRACKED_POINTERS) return

        when (event) {
            EchoNative.TOUCH_DOWN -> {
                if (contactDown[id]) return // already down; a second press would be a second tap
                contactDown[id] = true
            }
            EchoNative.TOUCH_UPDATE -> if (!contactDown[id]) return
            else -> if (!contactDown[id]) return else contactDown[id] = false
        }

        contactX[id] = rx
        contactY[id] = ry
        c.touch(id, event, rx, ry)
    }

    /**
     * Retract every live contact, at the position it was last reported.
     *
     * At its last position rather than the event's, deliberately: sending a
     * cancel from somewhere the finger never was moves the contact before
     * lifting it, and a contact that jumps across the screen and then lifts is
     * a flick — which is a gesture Windows acts on.
     */
    private fun cancelAllContacts(c: EchoController) {
        for (id in contactDown.indices) {
            if (!contactDown[id]) continue
            contactDown[id] = false
            c.touch(id, EchoNative.TOUCH_CANCEL, contactX[id], contactY[id])
        }
    }

    /**
     * Give up every input the host might think is held.
     *
     * Called when forwarding stops for a reason the touch stream never sees —
     * the view detaching, input being switched off, the session ending. Android
     * sends no `ACTION_CANCEL` for any of those, so without this the host keeps
     * a button pressed or a finger down for the rest of the session.
     */
    fun releaseAllTouchInput() {
        cancelPendingPress()
        // Dropped, not flushed. Forwarding is stopping, so buffered contacts
        // must not be delivered on the way out — and they are owed nothing,
        // because the host never heard about them.
        dropHeldTouch()
        val c = controller ?: run {
            // No controller to tell: forget the state anyway, or a later session
            // would inherit contacts it never received a press for.
            contactDown.fill(false)
            touchButtonDown = false
            return
        }
        cancelAllContacts(c)
        releaseTouchButton(c)
    }

    override fun onDetachedFromWindow() {
        releaseAllTouchInput()
        super.onDetachedFromWindow()
    }

    // ── Three-finger tap → the phone's keyboard ─────────────────────────────

    /**
     * Track the touch stream well enough to recognise one gesture: three
     * fingers down together, and up again without moving.
     *
     * Deliberately a small state machine rather than a `GestureDetector`.
     * Android's detector describes the *first* pointer — tap, scroll, fling,
     * long-press — and has no notion of "exactly three fingers, none of which
     * travelled". Every implementation built on top of it ends up tracking the
     * same three facts this does, with a second event consumer inserted into
     * the app's lowest-latency input path to get them.
     *
     * The three facts, and why each one is needed:
     *
     *  - **The high-water mark of simultaneous pointers.** Not the live count:
     *    fingers do not land or lift together, so the count passes through 1
     *    and 2 on the way in and again on the way out. Requiring it to *reach*
     *    exactly three accepts a real three-finger tap and rejects both a
     *    two-finger one and a palm, which lands four or more.
     *  - **Whether anything moved.** A pinch, a two-finger scroll and a drag
     *    all begin identically to this gesture and are told apart only by
     *    travel. Measured per pointer against where that pointer landed.
     *  - **How long it took.** Three fingers resting on the screen while
     *    reading is not a tap, and neither is a hand put down to steady the
     *    phone.
     *
     * A rejected gesture stays rejected until the next `ACTION_DOWN`, so one
     * disqualifying sample cannot be undone by the fingers coming back to
     * where they started.
     */
    private fun trackTouchGesture(event: MotionEvent) {
        when (event.actionMasked) {
            MotionEvent.ACTION_DOWN -> {
                gestureMaxPointers = 1
                gestureRejected = false
                gestureConsumed = false
                gestureStartMs = event.eventTime
                rememberPointerStart(event, 0)
            }
            MotionEvent.ACTION_POINTER_DOWN -> {
                gestureMaxPointers = maxOf(gestureMaxPointers, event.pointerCount)
                if (gestureMaxPointers > THREE_FINGERS) gestureRejected = true
                rememberPointerStart(event, event.actionIndex)

                // Condition A: the third finger landed while the host's input is
                // still buffered, so the gesture can be answered *now* and the
                // payload dropped rather than merely undone after the fact.
                //
                // Both traps are checked, because both modes withhold something
                // and for the same reason. Pointer mode holds a left-click;
                // absolute mode holds the contacts themselves — a cancelled
                // contact is not a contact that never happened, since the
                // desktop has already reacted to the finger by the time the
                // cancel lands.
                //
                // Gated on a trap still being armed, which is what keeps this
                // from competing with the ACTION_UP judgment below. Inside the
                // window nothing has reached the host and the intent is
                // unambiguous; outside it the input has already gone out, so the
                // slower, stricter test — no travel, quick enough — decides, and
                // the mode's own handler undoes what it left behind. The two
                // never both fire: [gestureConsumed] is the interlock.
                //
                // It follows that with both touch modes OFF no trap is ever
                // armed and this arm never fires, so the gesture keeps exactly
                // the behaviour it had before any of this. That is deliberate —
                // there is nothing to prevent leaking when nothing is being
                // sent, and the tap-on-release path is the one with live hours
                // behind it.
                if (!gestureRejected && (pressPending || touchHeld) &&
                    event.pointerCount >= THREE_FINGERS
                ) {
                    cancelPendingPress()
                    dropHeldTouch()
                    gestureConsumed = true
                    toggleSoftKeyboard()
                }
            }
            MotionEvent.ACTION_MOVE -> {
                if (gestureRejected) return
                for (i in 0 until event.pointerCount) {
                    val id = event.getPointerId(i)
                    if (id >= MAX_TRACKED_POINTERS) continue
                    if (Math.abs(event.getX(i) - gestureDownX[id]) > tapSlop ||
                        Math.abs(event.getY(i) - gestureDownY[id]) > tapSlop
                    ) {
                        gestureRejected = true
                        return
                    }
                }
            }
            MotionEvent.ACTION_UP -> {
                // The last finger of the gesture. Everything it is judged on is
                // known by now, which is why the decision is taken here rather
                // than when the third finger lands: a tap is not a tap until it
                // has been let go of without moving.
                val quick = event.eventTime - gestureStartMs <= THREE_FINGER_TAP_TIMEOUT_MS
                if (!gestureConsumed && !gestureRejected &&
                    gestureMaxPointers == THREE_FINGERS && quick
                ) {
                    toggleSoftKeyboard()
                }
            }
            MotionEvent.ACTION_CANCEL -> gestureRejected = true
        }
    }

    /** Record where a pointer landed, so [trackTouchGesture] can measure travel. */
    private fun rememberPointerStart(event: MotionEvent, index: Int) {
        val id = event.getPointerId(index)
        // Pointer ids are small and dense in practice, but the array is not
        // Android's to size — an id beyond it is a device this gesture will not
        // try to interpret rather than an index to crash on.
        if (id >= MAX_TRACKED_POINTERS) {
            gestureRejected = true
            return
        }
        gestureDownX[id] = event.getX(index)
        gestureDownY[id] = event.getY(index)
    }

    // ── Soft keyboard ───────────────────────────────────────────────────────

    /**
     * Show the phone's own keyboard over the stream, or dismiss it.
     *
     * The IME is requested by this view because Android grants it to a focused
     * *editor*, and this view is the only thing in the hierarchy claiming to be
     * one — see [onCheckIsTextEditor]. Asking from the Activity would need a
     * view to name anyway, and Compose owns no editor here.
     *
     * Reliable because it is reached from a touch handler: `showSoftInput` is
     * refused for a view whose window is not focused, and a window being
     * touched is focused by construction. That is the same property the
     * capture-on-tap retry above depends on.
     */
    fun toggleSoftKeyboard() {
        val visible = isSoftKeyboardVisible()
        // Reconcile before deciding. The platform is the authority, and the UI
        // can be behind it: [onApplyWindowInsets] only fires if the insets
        // dispatch reaches this view through Compose's container, whereas
        // `rootWindowInsets` is a direct question to the ViewRootImpl and always
        // answers. Without this line a dismissal the app never saw would leave
        // the mouse released for the rest of the session.
        setKeyboardShown(visible)
        if (visible) hideSoftKeyboard() else showSoftKeyboard()
    }

    fun showSoftKeyboard() {
        val manager = imm ?: return
        if (!isFocused) requestFocus()
        setKeyboardShown(true)
        if (!manager.showSoftInput(this, InputMethodManager.SHOW_IMPLICIT)) {
            // Refused, and the ordinary reason is that focus has not settled:
            // `requestFocus` above runs through Compose's focus owner, which
            // can complete after this frame. One retry on the next loop — not a
            // retry storm, because a second failure is a real refusal and
            // asking again would not change it.
            handler.post { manager.showSoftInput(this, InputMethodManager.SHOW_IMPLICIT) }
        }
    }

    fun hideSoftKeyboard() {
        setKeyboardShown(false)
        imm?.hideSoftInputFromWindow(windowToken, 0)
    }

    /**
     * Whether the keyboard is actually on screen.
     *
     * Asked of the window insets wherever the platform will answer (API 30+),
     * falling back to the remembered flag below that. The user can dismiss the
     * IME with the system back gesture and nothing informs the app, so a flag
     * on its own ends up inverted for the rest of the session — and the symptom
     * of that is a three-finger tap that "stops working", which is the gesture
     * taking the blame for a stale boolean.
     */
    fun isSoftKeyboardVisible(): Boolean =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            rootWindowInsets?.isVisible(WindowInsets.Type.ime()) ?: keyboardShown
        } else {
            keyboardShown
        }

    private fun setKeyboardShown(shown: Boolean) {
        if (keyboardShown == shown) return
        keyboardShown = shown
        onKeyboardVisibility?.invoke(shown)
    }

    /**
     * The authoritative answer, where the platform provides one.
     *
     * Covers every dismissal the app did not initiate — the back gesture, the
     * IME's own hide key, a configuration change — so the UI's idea of the
     * keyboard cannot drift from what is on screen.
     */
    override fun onApplyWindowInsets(insets: WindowInsets): WindowInsets {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            setKeyboardShown(insets.isVisible(WindowInsets.Type.ime()))
        }
        return super.onApplyWindowInsets(insets)
    }

    /**
     * Claim to be a text editor, so an IME can be summoned at all.
     *
     * Nothing is edited here: the keystrokes go to the PC and this view keeps
     * no text. But Android will not open a keyboard for a view that answers
     * false, whatever [showSoftKeyboard] asks for.
     */
    override fun onCheckIsTextEditor(): Boolean = true

    override fun onCreateInputConnection(outAttrs: EditorInfo): InputConnection {
        // TYPE_NULL is the whole trick. It tells the IME there is no text field
        // behind this editor, and a keyboard in that mode delivers raw key
        // events instead of composing words — which is exactly what this app
        // forwards. Under any real input type the keys would arrive as
        // committed text with autocorrect applied, and nothing would reach the
        // PC until a word ended.
        outAttrs.inputType = InputType.TYPE_NULL
        outAttrs.imeOptions = EditorInfo.IME_ACTION_NONE or
            // Landscape IMEs otherwise open a full-screen "extract" editor: a
            // text box covering the whole display. This activity is locked to
            // landscape, so without these two the keyboard hides the very thing
            // being typed into.
            EditorInfo.IME_FLAG_NO_EXTRACT_UI or
            EditorInfo.IME_FLAG_NO_FULLSCREEN or
            // None of this is text this phone should learn from. It is going to
            // another machine, and may well be a password on it.
            EditorInfo.IME_FLAG_NO_PERSONALIZED_LEARNING
        return StreamInputConnection(this)
    }

    // ── Physical keyboard ───────────────────────────────────────────────────

    // Keys are taken by MainActivity.dispatchKeyEvent, which sees them before
    // focus is consulted — Compose can leave this view unfocused for a whole
    // session. These overrides only stop the system acting on keys locally if
    // one arrives here anyway.

    override fun onKeyDown(keyCode: Int, event: KeyEvent): Boolean =
        if (active() != null && !passToSystem(keyCode)) true else super.onKeyDown(keyCode, event)

    override fun onKeyUp(keyCode: Int, event: KeyEvent): Boolean =
        if (active() != null && !passToSystem(keyCode)) true else super.onKeyUp(keyCode, event)

    private fun passToSystem(keyCode: Int): Boolean = when (keyCode) {
        KeyEvent.KEYCODE_BACK,
        KeyEvent.KEYCODE_VOLUME_UP,
        KeyEvent.KEYCODE_VOLUME_DOWN,
        KeyEvent.KEYCODE_VOLUME_MUTE,
        KeyEvent.KEYCODE_POWER -> true
        else -> false
    }

    // ── Helpers ─────────────────────────────────────────────────────────────

    private fun active(): EchoController? = if (inputEnabled) controller else null

    private fun sendAbsolute(event: MotionEvent, c: EchoController) =
        sendAbsoluteAt(event.x, event.y, c)

    /**
     * A raw 1:1 absolute position — for a **mouse**, not a finger.
     *
     * Deliberately not corner-corrected. A mouse pointer is drawn on the screen
     * and can be walked into any pixel of it, curved edge or not, so stretching
     * its coordinates would trade a problem it does not have for imprecision it
     * would feel everywhere. [sendTouchAsPointer] is the finger's counterpart.
     */
    private fun sendAbsoluteAt(x: Float, y: Float, c: EchoController) {
        if (width <= 0 || height <= 0) return
        // Clamped: a drag can leave the view's bounds, and a fraction above 1.0
        // would put the host's cursor off-screen.
        c.mouseAbsolute(
            x.toInt().coerceIn(0, width),
            y.toInt().coerceIn(0, height),
            width,
            height,
        )
    }

    // ── Corner compensation ────────────────────────────────────────────────

    /**
     * A finger's absolute position, corner-corrected.
     *
     * Sent in [EchoNative.TOUCH_REF] units rather than view pixels, so the
     * numbers the host divides are the transformed ones and there is nothing on
     * that side tempting anyone to divide by the panel's real size instead.
     */
    private fun sendTouchAsPointer(x: Float, y: Float, c: EchoController) {
        if (width <= 0 || height <= 0) return
        c.mouseAbsolute(
            normalize(x, width, cornerInsetX),
            normalize(y, height, cornerInsetY),
            EchoNative.TOUCH_REF,
            EchoNative.TOUCH_REF,
        )
    }

    /**
     * Map one axis of a touch into the host's full extent.
     *
     *     f = ((v / size) - inset) / (1 - 2 * inset)      clamped to 0..1
     *     out = round(f * TOUCH_REF)
     *
     * The central `1 - 2*inset` of the panel becomes the host's whole axis, so a
     * touch at `inset` reports 0 and one at `1 - inset` reports the far edge.
     * Everything outside that band — the curve itself, and a drag that leaves
     * the view — pins at the rail rather than being refused: the user is
     * unambiguously asking for the edge, and clamping is how they get it.
     *
     * Clamping *after* the stretch, never before, is what makes the band reach
     * the rails at all. Clamping the raw fraction first would map the panel's
     * true edge to 1.0 and leave the correction with nothing to do.
     */
    private fun normalize(v: Float, size: Int, inset: Float): Int {
        if (size <= 0) return 0
        val span = 1f - 2f * inset
        if (span <= 0f) return 0 // a nonsensical inset; refuse rather than divide
        val f = ((v / size) - inset) / span
        return (f.coerceIn(0f, 1f) * EchoNative.TOUCH_REF).toInt()
    }

    /**
     * Measure the unreachable band from the panel's own corner radius.
     *
     * Preferred over a constant because the constant is a guess about one
     * phone and this is the platform's own answer for whichever phone is
     * running. `RoundedCorner` arrived in API 31 and reports nothing on a
     * square-cornered display, both of which fall back to
     * [DEFAULT_CORNER_INSET].
     *
     * **The factor.** For a corner of radius `r` the arc's closest approach to
     * the notional square corner is `r·(1 − 1/√2)` on each axis — about `0.29r`
     * — and that is the width of the band a finger cannot land in. On a
     * Pixel-class panel it works out at roughly 1% of the long axis, which is
     * where [DEFAULT_CORNER_INSET] comes from: the derivation and the guess
     * agree, and the derivation adapts.
     *
     * Bounded by [MAX_CORNER_INSET] because an implausible radius — a foldable's
     * inner display, a future device reporting something odd — would otherwise
     * stretch the mapping far enough to make the middle of the screen unusable,
     * and a slightly unreachable corner is much the smaller failure.
     */
    private fun calibrateCornerInset() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.S) return
        val insets = rootWindowInsets ?: return
        // listOf, not intArrayOf: a primitive array has no `mapNotNull`, and the
        // four positions are read once per size change.
        val radius = listOf(
            RoundedCorner.POSITION_TOP_LEFT,
            RoundedCorner.POSITION_TOP_RIGHT,
            RoundedCorner.POSITION_BOTTOM_LEFT,
            RoundedCorner.POSITION_BOTTOM_RIGHT,
        ).mapNotNull { insets.getRoundedCorner(it)?.radius }.maxOrNull() ?: return
        if (radius <= 0 || width <= 0 || height <= 0) return

        val band = radius * CORNER_REACH_FACTOR
        cornerInsetX = (band / width).coerceIn(0f, MAX_CORNER_INSET)
        cornerInsetY = (band / height).coerceIn(0f, MAX_CORNER_INSET)
    }

    /**
     * Recalibrate on every size change — rotation, a fold, multi-window.
     *
     * The radius is a fixed number of pixels, so the *fraction* of the axis it
     * occupies changes whenever the axis does. Calibrating once would leave the
     * long edge over-corrected and the short edge under-corrected after a
     * rotation, which reads as a screen that is subtly miscalibrated in one
     * orientation only — a symptom nobody would trace back to here.
     */
    override fun onSizeChanged(w: Int, h: Int, oldW: Int, oldH: Int) {
        super.onSizeChanged(w, h, oldW, oldH)
        calibrateCornerInset()
    }

    private fun buttonOf(event: MotionEvent): Int? = when (event.actionButton) {
        MotionEvent.BUTTON_PRIMARY -> BUTTON_LEFT
        MotionEvent.BUTTON_TERTIARY -> BUTTON_MIDDLE
        MotionEvent.BUTTON_SECONDARY -> BUTTON_RIGHT
        MotionEvent.BUTTON_BACK -> BUTTON_X1
        MotionEvent.BUTTON_FORWARD -> BUTTON_X2
        else -> null
    }

    private fun isMouse(event: MotionEvent): Boolean =
        event.isFromSource(InputDevice.SOURCE_MOUSE) ||
            event.isFromSource(InputDevice.SOURCE_MOUSE_RELATIVE)

    /**
     * Report a source once per kind, so stray input can be traced to a device.
     *
     * Also carries a running count of mouse events, because the source alone is
     * ambiguous: it names only the *most recent* device, so a tap on the overlay
     * makes it read "touchscreen" whether the mouse has been delivering
     * thousands of events or none at all. The count distinguishes "the mouse is
     * working and you touched the screen last" from "the mouse is not reaching
     * this view", which is precisely the confusion that hid the missing
     * `onHoverEvent` override for three rounds.
     */
    private fun report(event: MotionEvent) {
        val mouse = isMouse(event) || event.source == InputDevice.SOURCE_MOUSE_RELATIVE
        if (mouse) {
            mouseEvents++
            // Delivery rate, which is the number that distinguishes the two
            // remaining explanations for a pointer that feels under-sampled.
            // A gaming mouse reports at 125–1000 Hz. If this reads near the
            // display's refresh rate instead, input is still being buffered to
            // VBlank; if it reads in the hundreds, Android is delivering
            // properly and anything left is downstream.
            rateWindow++
            val now = android.os.SystemClock.uptimeMillis()
            if (now - rateSince >= 1000) {
                eventsPerSecond = (rateWindow * 1000L / (now - rateSince)).toInt()
                // The live rate is unreadable: the panel that displays it
                // releases capture to open, so by the time it can be seen the
                // mouse has nothing to report and it reads near zero. The peak
                // survives that, and it is the figure that actually answers
                // "how fast is Android delivering when the mouse is moving".
                if (eventsPerSecond > peakEventsPerSecond) peakEventsPerSecond = eventsPerSecond
                // Once a second, and only while input is flowing — cheap, and
                // it puts these numbers in the host log where they can be read
                // next to the host's own.
                EchoNative.nativeReportUiState(
                    peakEventsPerSecond,
                    peakBatchSamples,
                    hasPointerCapture(),
                )
                rateWindow = 0
                rateSince = now
            }
        }
        val source = event.source
        // Refresh on a source change, and periodically while a mouse is moving
        // so the counters stay live instead of frozen at the last switch.
        if (source == lastReported && !(mouse && mouseEvents % 30L == 0L)) return
        lastReported = source
        onDiagnostic?.invoke(
            // `touch mode` is reported because the white wash tracks it exactly:
            // touching clears it, the mouse brings it back. If the film is ever
            // seen while this says `touch=true`, the cause is NOT focus
            // highlighting and the compositing theory is back in play.
            "mouse PEAK $peakEventsPerSecond/s x$peakBatchSamples, " +
                "touch=${isInTouchMode} focus=${isFocused}, last: " + when {
                source == InputDevice.SOURCE_MOUSE_RELATIVE -> "mouse (captured)"
                event.isFromSource(InputDevice.SOURCE_MOUSE) -> "mouse"
                event.isFromSource(InputDevice.SOURCE_TOUCHPAD) -> "touchpad"
                event.isFromSource(InputDevice.SOURCE_TOUCHSCREEN) -> "touchscreen"
                event.isFromSource(InputDevice.SOURCE_JOYSTICK) -> "gamepad"
                event.isFromSource(InputDevice.SOURCE_STYLUS) -> "stylus"
                else -> "source 0x${Integer.toHexString(source)}"
            }
        )
    }

    private companion object {
        /** Fingers that mean "give me the keyboard". */
        const val THREE_FINGERS = 3
        /**
         * How many pointer ids the tap tracker will follow. Android hands out
         * small, dense ids and no phone reports ten fingers, so this is a bound
         * rather than a limit anyone reaches.
         *
         * It is also the ceiling on forwarded touch contacts, and it must not
         * exceed `MAX_CONTACTS` in the host's `touch.rs` — the synthetic
         * touchscreen is created for that many, and it refuses any frame with
         * more. Ten on both sides; change neither alone.
         */
        const val MAX_TRACKED_POINTERS = 10
        /**
         * How long the whole three-finger tap may take.
         *
         * Generous next to `ViewConfiguration.getTapTimeout()` (100 ms), which
         * describes ONE finger. Three fingers never land or lift in the same
         * millisecond — the hand rolls as they arrive — and a 100 ms budget
         * rejects most genuine attempts. Long enough to be reachable, short
         * enough that resting a hand on the screen is not a tap.
         */
        const val THREE_FINGER_TAP_TIMEOUT_MS = 500L
        /**
         * Slop multiplier for the tap, over the single-finger `scaledTouchSlop`.
         *
         * Three fingers wobble far more than one, for the same reason they do
         * not land together: the hand rotates slightly as it comes down. At 1x
         * the tap is rejected by movement that no user would call movement.
         */
        const val SLOP_MULTIPLIER = 3
        /**
         * How long a single finger's press is withheld from the host.
         *
         * The budget for the second and third fingers of a keyboard gesture to
         * arrive. Long enough that a hand rolling onto the glass lands inside it
         * — the same fact that makes [THREE_FINGER_TAP_TIMEOUT_MS] generous —
         * and short enough to sit under the ~100 ms at which a delay stops being
         * "instant" to a person. It is also strictly cheaper than it looks: a
         * drag or a pan leaves through [travelledPastSlop] on its first moving
         * sample, so the only gesture that ever waits the full window is a
         * stationary press, which is a press-and-hold anyway.
         */
        const val PRESS_HOLD_MS = 60L
        /**
         * Ceiling on buffered contact transitions before the trap resolves early.
         *
         * A bound, not a limit anyone reaches: three fingers on a 1000 Hz
         * digitiser produce well under this inside [PRESS_HOLD_MS]. It exists so
         * a pathological event storm cannot grow the buffer without end while
         * the timer is pending.
         */
        const val MAX_HELD_CONTACTS = 256
        /**
         * Unreachable band per edge when the platform will not name a radius —
         * a 98% bounding box. See [calibrateCornerInset] for where it comes
         * from and why the measured value normally supersedes it.
         */
        const val DEFAULT_CORNER_INSET = 0.01f
        /** Ceiling on a measured inset; see [calibrateCornerInset]. */
        const val MAX_CORNER_INSET = 0.04f
        /**
         * Band width as a fraction of the corner radius: `1 − 1/√2`, the arc's
         * closest approach to the corner it replaces.
         */
        const val CORNER_REACH_FACTOR = 0.293f
        // Matching the host's NV_MOUSE_BUTTON_PACKET values.
        const val BUTTON_LEFT = 1
        const val BUTTON_MIDDLE = 2
        const val BUTTON_RIGHT = 3
        const val BUTTON_X1 = 4
        const val BUTTON_X2 = 5
        const val WHEEL_DELTA = 120
        /** Android rejects a capture request made too soon after regaining focus. */
        const val RECAPTURE_DELAY_MS = 500L
        /** Grace period before asking the framework whether capture actually took. */
        const val CAPTURE_VERIFY_MS = 400L
    }
}

/**
 * A keyboard that types keys, not text.
 *
 * `BaseInputConnection` with no editable buffer is already most of the way
 * there: with `TYPE_NULL` most IMEs deliver `KeyEvent`s through `sendKeyEvent`,
 * and the base implementation dispatches those into the view root — where
 * `MainActivity.dispatchKeyEvent` picks them up exactly like a physical key, so
 * the soft keyboard and a Bluetooth one travel the same path to the host.
 *
 * The overrides below cover the three things an IME still does its own way. All
 * three are turned back into key events rather than handled as text, because a
 * key is what the host understands: `EchoController.key` sends a Windows
 * virtual-key code, and there is no path for a character.
 */
private class StreamInputConnection(view: View) : BaseInputConnection(view, false) {

    private val keyMap = KeyCharacterMap.load(KeyCharacterMap.VIRTUAL_KEYBOARD)

    /**
     * Text the IME committed rather than keyed — gesture typing, autocorrect, a
     * tapped suggestion, and every keystroke on some third-party keyboards.
     *
     * `getEvents` returns null for anything the virtual keyboard layout cannot
     * produce (emoji, most non-Latin scripts). Those fall through to the base
     * implementation, which buffers them where nobody reads them — the honest
     * outcome, since the host is being sent keystrokes and there is no
     * keystroke for an emoji.
     */
    override fun commitText(text: CharSequence?, newCursorPosition: Int): Boolean {
        val chars = text?.toString()?.toCharArray() ?: return false
        val events = keyMap.getEvents(chars) ?: return super.commitText(text, newCursorPosition)
        for (event in events) sendKeyEvent(event)
        return true
    }

    /**
     * Backspace and delete.
     *
     * IMEs remove characters by asking the editor to delete around the cursor,
     * not by sending KEYCODE_DEL — so without this the delete key is silently
     * inert, which is a confusing thing for a keyboard to be.
     */
    override fun deleteSurroundingText(beforeLength: Int, afterLength: Int): Boolean {
        repeat(beforeLength) { tap(KeyEvent.KEYCODE_DEL) }
        repeat(afterLength) { tap(KeyEvent.KEYCODE_FORWARD_DEL) }
        return true
    }

    /** The IME's action key. Go, Send, Search and Done all mean Enter here. */
    override fun performEditorAction(editorAction: Int): Boolean {
        tap(KeyEvent.KEYCODE_ENTER)
        return true
    }

    private fun tap(keyCode: Int) {
        sendKeyEvent(KeyEvent(KeyEvent.ACTION_DOWN, keyCode))
        sendKeyEvent(KeyEvent(KeyEvent.ACTION_UP, keyCode))
    }
}
