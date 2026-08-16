package com.nova.echo

import android.content.Context
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.view.KeyEvent
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.WindowManager
import androidx.activity.ComponentActivity
import androidx.activity.compose.BackHandler
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.viewinterop.AndroidView
import org.json.JSONObject

class MainActivity : ComponentActivity() {

    private lateinit var controller: EchoController

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // A stream is not a video the user scrubs; letting the screen sleep
        // mid-session is never what they meant.
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)

        // API 33+ hides a foreground service's notification without this. The
        // service itself still runs either way, so the request is best-effort
        // and never gates streaming on an answer.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            checkSelfPermission(android.Manifest.permission.POST_NOTIFICATIONS) !=
                PackageManager.PERMISSION_GRANTED
        ) {
            requestPermissions(arrayOf(android.Manifest.permission.POST_NOTIFICATIONS), 1)
        }

        controller = EchoController(applicationContext).also { it.init() }

        setContent {
            MaterialTheme(colorScheme = darkColorScheme()) {
                Surface(color = Color.Black) { EchoScreen(controller) }
            }
        }
    }

    override fun onPause() {
        super.onPause()
        // Leaving the app mid-keystroke means the key-up never sends. The
        // session keeps running (the foreground service sees to that), so
        // without this the host is left holding a key indefinitely.
        controller.releaseAllInput()
    }

    override fun onDestroy() {
        super.onDestroy()
        controller.stop()
    }

    /**
     * Keyboard input is taken here rather than in the SurfaceView.
     *
     * A Compose `AndroidView` does not reliably hold focus — Compose owns the
     * focus system, and an embedded View can sit unfocused for a whole session.
     * Key events then never reach it and Android handles them as local
     * shortcuts, which is why typing did nothing and stray keys opened the
     * phone's own menus. `dispatchKeyEvent` sees every key the window receives,
     * before focus is consulted at all.
     */
    override fun dispatchKeyEvent(event: KeyEvent): Boolean {
        if (!controller.state.streaming || !controller.inputEnabled) {
            return super.dispatchKeyEvent(event)
        }
        // Left with the system: Back is how the user escapes a fullscreen
        // stream, and volume belongs to whatever is playing locally.
        when (event.keyCode) {
            KeyEvent.KEYCODE_BACK,
            KeyEvent.KEYCODE_VOLUME_UP,
            KeyEvent.KEYCODE_VOLUME_DOWN,
            KeyEvent.KEYCODE_VOLUME_MUTE,
            KeyEvent.KEYCODE_POWER -> return super.dispatchKeyEvent(event)
        }

        when (event.action) {
            KeyEvent.ACTION_DOWN -> {
                // Android's auto-repeat is discarded: Windows generates its own
                // from the held key, so forwarding both doubles the rate.
                if (event.repeatCount == 0) {
                    controller.key(event.keyCode, true, event.metaState)
                }
            }
            KeyEvent.ACTION_UP -> controller.key(event.keyCode, false, event.metaState)
        }
        // Consumed either way. Letting an unmapped key fall through is what let
        // the phone act on keys meant for the PC.
        return true
    }
}

@Composable
private fun EchoScreen(controller: EchoController) {
    val state = controller.state

    // SurfaceView rather than TextureView: it gets a hardware overlay plane, so
    // decoded frames reach the display without a GPU composite step. On a
    // latency-sensitive stream that difference is the point.
    // Kept so the streaming overlay can hand the mouse back and forth.
    var view by remember { mutableStateOf<StreamSurfaceView?>(null) }
    var controlsVisible by rememberSaveable { mutableStateOf(false) }
    var inputEnabled by rememberSaveable { mutableStateOf(true) }
    var touchAsPointer by rememberSaveable { mutableStateOf(false) }
    // Pointer capture is now opt-in. Grabbing it automatically hid the cursor
    // and swallowed the touchscreen, which left no way to reach the controls.
    var captureMouse by rememberSaveable { mutableStateOf(false) }
    var lastSource by remember { mutableStateOf("none") }
    // What the framework actually did, not what we asked for.
    var captureHeld by remember { mutableStateOf(false) }
    var captureWhy by remember { mutableStateOf("") }
    var captureEverHeld by remember { mutableStateOf(false) }

    // Requested on first use rather than at launch: a microphone prompt before
    // the user has asked for a microphone is the kind of thing people decline
    // reflexively, and a declined permission is much harder to recover from
    // than an un-asked one.
    val context = LocalContext.current
    val micPermission = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestPermission(),
    ) { granted -> controller.onMicPermissionResult(granted) }

    Box(Modifier.fillMaxSize()) {
        AndroidView(
            modifier = Modifier.fillMaxSize(),
            factory = { ctx ->
                StreamSurfaceView(ctx).apply {
                    controller.let { this.controller = it }
                    onDiagnostic = { lastSource = it }
                    onCaptureChanged = {
                        captureHeld = it
                        // Remembered because the panel that displays this
                        // releases capture to open, so the live value is always
                        // false by the time it can be read. "It has worked at
                        // least once" is the fact worth surviving that.
                        if (it) captureEverHeld = true
                    }
                    onCaptureDiagnosis = { captureWhy = it }
                    view = this
                    holder.addCallback(object : SurfaceHolder.Callback {
                        override fun surfaceCreated(h: SurfaceHolder) {
                            controller.surface = h.surface
                        }
                        override fun surfaceChanged(h: SurfaceHolder, f: Int, w: Int, ht: Int) {}
                        override fun surfaceDestroyed(h: SurfaceHolder) {
                            controller.surface = null
                        }
                    })
                }
            },
        )

        // Push the toggles down whenever they change. The controller carries
        // the master switch because the Activity's key dispatch reads it too.
        LaunchedEffect(view, inputEnabled, touchAsPointer) {
            controller.inputEnabled = inputEnabled
            view?.inputEnabled = inputEnabled
            view?.touchAsPointer = touchAsPointer
        }

        // Turn capture on by itself when a mouse is actually attached. Leaving
        // it to a switch the user has to find means the default experience is
        // two cursors and a washed-out picture (Android's cursor overlay knocks
        // the video off its hardware overlay plane, which changes how the frame
        // is composited).
        LaunchedEffect(state.streaming, view) {
            // Retried rather than checked once: a Bluetooth mouse can finish
            // enumerating seconds after the stream starts, and a single check at
            // the wrong moment left capture switched off for the whole session
            // with no indication why.
            repeat(20) {
                if (!state.streaming) return@LaunchedEffect
                if (view?.hasCaptureCompatibleDevice() == true) {
                    captureMouse = true
                    return@LaunchedEffect
                }
                kotlinx.coroutines.delay(500)
            }
        }

        // Capture only when asked, and never while the controls are open — a
        // captured pointer cannot click them.
        LaunchedEffect(state.streaming, view, controlsVisible, captureMouse) {
            val v = view ?: return@LaunchedEffect
            if (state.streaming && captureMouse && !controlsVisible) v.captureMouse()
            else v.releaseMouse()
        }

        // Opening the controls stops input mid-gesture, so anything held has to
        // be let go or it stays held on the host.
        LaunchedEffect(controlsVisible, inputEnabled) {
            if (controlsVisible || !inputEnabled) controller.releaseAllInput()
        }

        if (state.streaming) {
            // Back is the natural "get me out" gesture, and it is the only one
            // available once the pointer is captured and the panel is hidden.
            BackHandler(enabled = true) {
                if (controlsVisible) controller.stop() else controlsVisible = true
            }
            StreamOverlay(
                visible = controlsVisible,
                status = state.status,
                lastSource = lastSource,
                captureHeld = captureHeld,
                captureWhy = captureWhy,
                captureEverHeld = captureEverHeld,
                inputEnabled = inputEnabled,
                touchAsPointer = touchAsPointer,
                captureMouse = captureMouse,
                micEnabled = state.micEnabled,
                syncEnabled = state.syncEnabled,
                videoDelayMs = state.videoDelayMs,
                onSyncEnabled = { controller.setSyncEnabled(it) },
                micActive = state.micActive,
                micProblem = state.micProblem,
                onMicEnabled = { wanted ->
                    val granted = context.checkSelfPermission(android.Manifest.permission.RECORD_AUDIO) ==
                        PackageManager.PERMISSION_GRANTED
                    when {
                        !wanted -> controller.setMicEnabled(false)
                        // Record the intent first, so the capture starts by
                        // itself the moment the permission callback returns —
                        // otherwise the user has to flip the switch twice.
                        granted -> controller.setMicEnabled(true)
                        else -> {
                            controller.setMicEnabled(true)
                            micPermission.launch(android.Manifest.permission.RECORD_AUDIO)
                        }
                    }
                },
                onInputEnabled = { inputEnabled = it },
                onTouchAsPointer = { touchAsPointer = it },
                onCaptureMouse = { captureMouse = it },
                onToggle = { controlsVisible = !controlsVisible },
                onStop = { controlsVisible = false; controller.stop() },
                stats = { controller.stats() },
            )
        } else {
            ControlPanel(controller, state)
        }
    }

    // Leaving the stream must release the pointer, or the mouse is stuck.
    LaunchedEffect(state.streaming) { if (!state.streaming) controlsVisible = false }
}

/**
 * The only way out of a live stream.
 *
 * Before this existed the control panel — which holds Stop — was hidden for the
 * whole session, so quitting was impossible from the UI and the stream simply
 * carried on. Deliberately minimal: a small always-present handle that reveals
 * a Stop button, rather than a bar that covers the picture.
 */
@Composable
private fun BoxScope.StreamOverlay(
    visible: Boolean,
    status: String,
    lastSource: String,
    captureHeld: Boolean,
    captureWhy: String,
    captureEverHeld: Boolean,
    inputEnabled: Boolean,
    touchAsPointer: Boolean,
    captureMouse: Boolean,
    micEnabled: Boolean,
    syncEnabled: Boolean,
    videoDelayMs: Int,
    onSyncEnabled: (Boolean) -> Unit,
    micActive: Boolean,
    micProblem: String?,
    onMicEnabled: (Boolean) -> Unit,
    onInputEnabled: (Boolean) -> Unit,
    onTouchAsPointer: (Boolean) -> Unit,
    onCaptureMouse: (Boolean) -> Unit,
    onToggle: () -> Unit,
    onStop: () -> Unit,
    stats: () -> String,
) {
    if (!visible) {
        TextButton(onClick = onToggle, modifier = Modifier.align(Alignment.TopEnd)) {
            Text("☰", color = Color.White.copy(alpha = 0.55f), fontSize = 22.sp)
        }
        return
    }

    Surface(
        modifier = Modifier.align(Alignment.TopCenter).padding(12.dp),
        color = Color.Black.copy(alpha = 0.9f),
        shape = MaterialTheme.shapes.medium,
    ) {
        Column(
            Modifier.padding(horizontal = 16.dp, vertical = 12.dp),
            verticalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            Row(
                horizontalArrangement = Arrangement.spacedBy(12.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text(status, color = Color.White, fontSize = 13.sp)
                Button(onClick = onStop) { Text("Stop") }
                TextButton(onClick = onToggle) { Text("Resume") }
            }

            OverlayToggle("Send input to PC", inputEnabled, onInputEnabled)
            OverlayToggle("Capture mouse (games)", captureMouse, onCaptureMouse)
            OverlayToggle("Touch moves PC pointer", touchAsPointer, onTouchAsPointer)
            OverlayToggle("Microphone → PC", micEnabled, onMicEnabled)
            OverlayToggle("A/V sync (adds input lag)", syncEnabled, onSyncEnabled)

            // The trade, said on screen rather than buried in a doc. Audio sits
            // at a device-imposed floor that cannot be lowered, so sync is
            // reached by holding video back to meet it — which is the right
            // call while watching something and the wrong one while playing.
            if (syncEnabled) {
                Text(
                    if (videoDelayMs > 0) "video held ${videoDelayMs}ms to match audio"
                    else "measuring audio latency…",
                    color = Color.White.copy(alpha = 0.65f),
                    fontSize = 11.sp,
                )
            }

            // Switched on but not capturing. Said plainly, because the only
            // other evidence is the absence of a system microphone indicator —
            // and "my mic doesn't work" with nothing on screen to explain it is
            // exactly the report that costs an evening to diagnose remotely.
            if (micEnabled && !micActive) {
                Text(
                    micProblem ?: "microphone starting…",
                    color = Color.White.copy(alpha = 0.65f),
                    fontSize = 11.sp,
                )
            }

            // Whether capture actually engaged, not whether it was requested.
            // Two cursors with no explanation is what this line exists to end.
            // Capture is deliberately released while this panel is open — a
            // captured pointer cannot click these controls. Saying so matters:
            // the panel is the only place the state is visible, so it always
            // reads "off" here, and without this note that looks like a failure
            // rather than the intended behaviour.
            if (captureMouse) {
                Text(
                    (if (captureEverHeld) "capture WORKS — it has been granted this session.\n"
                     else "capture has not been granted yet this session.\n") +
                        "It is released while this panel is open (a grabbed mouse " +
                        "can't tap these controls) — close it to grab the mouse.\n" +
                        "last attempt: " + captureWhy.ifEmpty { "none yet" },
                    color = Color.White.copy(alpha = 0.6f),
                    fontSize = 11.sp,
                )
            } else {
                Text(
                    "capture is off — the mouse works, but stops at the screen edges.",
                    color = Color.White.copy(alpha = 0.6f),
                    fontSize = 11.sp,
                )
            }

            // Names the device the last event came from. When something moves
            // that nobody touched, this says what to blame.
            Text(
                "capture: ${if (captureHeld) "held" else "off"}   " +
                    "last input source: $lastSource",
                color = Color.White.copy(alpha = 0.6f),
                fontSize = 11.sp,
                fontFamily = FontFamily.Monospace,
            )

            // Live pipeline latency. The one number that separates "the host
            // got my input late" from "the host reacted instantly and I saw it
            // late" — two faults that feel identical while streaming, and which
            // three rounds of guessing failed to tell apart.
            var pipeline by remember { mutableStateOf("") }
            LaunchedEffect(Unit) {
                while (true) {
                    pipeline = runCatching {
                        val j = JSONObject(stats())
                        "video ${j.optInt("frame_age_ms")}ms (worst ${j.optInt("worst_frame_age_ms")}ms)  " +
                            "drops ${j.optInt("frames_dropped_overflow")}\n" +
                            "NETWORK round trip ${j.optInt("rtt_ms")}ms (best ${j.optInt("rtt_best_ms")}ms)\n" +
                            "input batch ${j.optInt("input_batch")} (worst ${j.optInt("input_batch_worst")})"
                    }.getOrDefault("")
                    kotlinx.coroutines.delay(500)
                }
            }
            if (pipeline.isNotEmpty()) {
                Text(
                    pipeline,
                    color = Color.White.copy(alpha = 0.6f),
                    fontSize = 11.sp,
                    fontFamily = FontFamily.Monospace,
                )
            }
        }
    }
}

@Composable
private fun OverlayToggle(label: String, checked: Boolean, onChange: (Boolean) -> Unit) {
    Row(
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Switch(checked = checked, onCheckedChange = onChange)
        Text(label, color = Color.White, fontSize = 12.sp)
    }
}

/**
 * A text field backed by SharedPreferences.
 *
 * `rememberSaveable` alone survives rotation but not a process restart, and
 * during bring-up the app gets killed and relaunched constantly. Retyping a
 * 64-character fingerprint on a phone keyboard each time is not a reasonable
 * thing to ask of anyone.
 */
@Composable
private fun rememberPref(key: String, default: String): MutableState<String> {
    val context = LocalContext.current
    val prefs = remember { context.getSharedPreferences("echo", Context.MODE_PRIVATE) }
    val state = rememberSaveable { mutableStateOf(prefs.getString(key, default) ?: default) }
    LaunchedEffect(state.value) { prefs.edit().putString(key, state.value).apply() }
    return state
}

@Composable
private fun ControlPanel(controller: EchoController, state: UiState) {
    var host by rememberPref("host", "10.0.0.205")
    var deviceName by rememberPref("device_name", "Echo Android")
    var relayUrl by rememberPref("relay_url", "https://10.0.0.205:8443/v1/signal")
    var relayPin by rememberPref("relay_pin", "")
    var hostFp by rememberPref("host_fp", "")

    // Once pairing succeeds the fingerprint is known; carry it into the stream
    // field so nobody has to copy 64 hex characters by hand.
    LaunchedEffect(state.hostFingerprint) {
        state.hostFingerprint?.let { if (it.isNotBlank()) hostFp = it }
    }

    Column(
        Modifier
            .fillMaxSize()
            .padding(20.dp)
            .verticalScroll(rememberScrollState()),
        verticalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        Text("Echo", style = MaterialTheme.typography.headlineMedium)
        Text(state.status, style = MaterialTheme.typography.bodyLarge)

        state.pin?.let { pin ->
            Card(Modifier.fillMaxWidth()) {
                Column(
                    Modifier.fillMaxWidth().padding(16.dp),
                    horizontalAlignment = Alignment.CenterHorizontally,
                ) {
                    Text("Type this PIN into Nova on the PC", textAlign = TextAlign.Center)
                    Text(pin, fontSize = 48.sp, fontFamily = FontFamily.Monospace)
                }
            }
        }

        state.error?.let {
            Text(it, color = MaterialTheme.colorScheme.error)
        }

        // Named arguments throughout: OutlinedTextField has a TextFieldValue
        // overload, and positional args let the compiler pick it, at which point
        // the lambda's `it` no longer resolves.
        OutlinedTextField(
            value = host,
            onValueChange = { host = it },
            label = { Text("Host LAN address") },
            singleLine = true,
        )
        OutlinedTextField(
            value = deviceName,
            onValueChange = { deviceName = it },
            label = { Text("This device's name") },
            singleLine = true,
        )
        Button(onClick = { controller.pair(host, deviceName) }, Modifier.fillMaxWidth()) {
            Text("Pair (LAN only)")
        }

        HorizontalDivider(Modifier.padding(vertical = 6.dp))

        OutlinedTextField(
            value = relayUrl,
            onValueChange = { relayUrl = it },
            label = { Text("Relay URL") },
            singleLine = true,
        )
        OutlinedTextField(
            value = relayPin,
            onValueChange = { relayPin = it },
            label = { Text("Relay fingerprint") },
            singleLine = true,
        )
        OutlinedTextField(
            value = hostFp,
            onValueChange = { hostFp = it },
            label = { Text("Nova fingerprint") },
            singleLine = true,
        )
        // A disabled button that does not say why is a dead end, and this one
        // has two independent preconditions. Naming the missing ones turns
        // "nothing happens" into an instruction.
        val missing = buildList {
            if (hostFp.isBlank()) add("Nova's fingerprint — pair first")
            if (relayPin.isBlank()) add("the relay's fingerprint — nova-relay prints it at startup")
        }
        if (missing.isNotEmpty()) {
            Text(
                "Stream needs ${missing.joinToString(" and ")}.",
                color = MaterialTheme.colorScheme.secondary,
                fontSize = 12.sp,
            )
        }
        Button(
            onClick = { controller.connect(relayUrl, relayPin, hostFp) },
            enabled = missing.isEmpty(),
            modifier = Modifier.fillMaxWidth(),
        ) { Text("Stream") }

        OutlinedButton(onClick = { controller.stop() }, Modifier.fillMaxWidth()) { Text("Stop") }

        Text(
            "This device: ${state.myFingerprint}",
            fontSize = 11.sp,
            fontFamily = FontFamily.Monospace,
        )

        if (state.log.isNotEmpty()) {
            HorizontalDivider(Modifier.padding(vertical = 6.dp))
            state.log.forEach {
                Text(it, fontSize = 11.sp, fontFamily = FontFamily.Monospace)
            }
        }
    }
}
