package com.nova.echo

import android.content.Context
import android.os.Bundle
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.WindowManager
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
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

class MainActivity : ComponentActivity() {

    private lateinit var controller: EchoController

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // A stream is not a video the user scrubs; letting the screen sleep
        // mid-session is never what they meant.
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)

        controller = EchoController(filesDir.absolutePath).also { it.init() }

        setContent {
            MaterialTheme(colorScheme = darkColorScheme()) {
                Surface(color = Color.Black) { EchoScreen(controller) }
            }
        }
    }

    override fun onDestroy() {
        super.onDestroy()
        controller.stop()
    }
}

@Composable
private fun EchoScreen(controller: EchoController) {
    val state = controller.state

    // SurfaceView rather than TextureView: it gets a hardware overlay plane, so
    // decoded frames reach the display without a GPU composite step. On a
    // latency-sensitive stream that difference is the point.
    Box(Modifier.fillMaxSize()) {
        AndroidView(
            modifier = Modifier.fillMaxSize(),
            factory = { ctx ->
                SurfaceView(ctx).apply {
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

        if (!state.streaming) ControlPanel(controller, state)
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
