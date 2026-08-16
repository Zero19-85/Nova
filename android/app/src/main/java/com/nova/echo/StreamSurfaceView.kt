package com.nova.echo

import android.content.Context
import android.hardware.input.InputManager
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.view.InputDevice
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.PointerIcon
import android.view.SurfaceView

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

    override fun onGenericMotionEvent(event: MotionEvent): Boolean =
        handlePointer(event, captured = false) || super.onGenericMotionEvent(event)

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

        val c = active() ?: return super.onTouchEvent(event)
        report(event)

        if (event.actionMasked == MotionEvent.ACTION_DOWN &&
            Build.VERSION.SDK_INT >= Build.VERSION_CODES.LOLLIPOP
        ) {
            // Still required even with the source-mask call in init.
            requestUnbufferedDispatch(event)
        }

        if (!touchAsPointer || !event.isFromSource(InputDevice.SOURCE_TOUCHSCREEN)) {
            return super.onTouchEvent(event)
        }

        when (event.actionMasked) {
            MotionEvent.ACTION_DOWN -> {
                sendAbsolute(event, c)
                c.mouseButton(BUTTON_LEFT, true)
            }
            MotionEvent.ACTION_MOVE -> {
                // Historical samples matter here too: a fast drag delivers
                // several positions per event.
                for (i in 0 until event.historySize) {
                    sendAbsoluteAt(event.getHistoricalX(i), event.getHistoricalY(i), c)
                }
                sendAbsolute(event, c)
            }
            MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> {
                sendAbsolute(event, c)
                c.mouseButton(BUTTON_LEFT, false)
            }
        }
        return true
    }

    // ── Keyboard ────────────────────────────────────────────────────────────
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
