package com.nova.echo

import android.content.pm.PackageManager
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.selection.selectable
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import kotlinx.coroutines.launch

// ── Shared building blocks ──────────────────────────────────────────────────
//
// Every control in both sheets is built from these four, so the palette is
// applied once rather than at forty call sites. Material's defaults are the
// wrong colours for this theme in a way that is very hard to fix piecemeal —
// one missed `colors =` argument and a purple thumb appears on a black sheet.

@Composable
private fun SheetTitle(text: String) {
    Text(text, style = MaterialTheme.typography.titleSmall, color = TextDim)
}

@Composable
private fun IonField(
    value: String,
    onValueChange: (String) -> Unit,
    label: String,
    numeric: Boolean = false,
    modifier: Modifier = Modifier,
) {
    OutlinedTextField(
        value = value,
        onValueChange = onValueChange,
        label = { Text(label, style = Telemetry) },
        singleLine = true,
        modifier = modifier.fillMaxWidth(),
        textStyle = TelemetryStrong.copy(fontSize = 13.sp, fontWeight = FontWeight.Normal),
        keyboardOptions = androidx.compose.foundation.text.KeyboardOptions(
            keyboardType = if (numeric) KeyboardType.Number else KeyboardType.Text,
        ),
        colors = OutlinedTextFieldDefaults.colors(
            focusedBorderColor = Ion,
            unfocusedBorderColor = Edge,
            focusedLabelColor = Ion,
            unfocusedLabelColor = TextDim,
            cursorColor = Ion,
            focusedTextColor = Text,
            unfocusedTextColor = Text,
            focusedContainerColor = Color.Transparent,
            unfocusedContainerColor = Color.Transparent,
        ),
    )
}

@Composable
private fun IonSwitch(label: String, sub: String?, checked: Boolean, onChange: (Boolean) -> Unit) {
    Row(
        Modifier.fillMaxWidth().padding(vertical = 4.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.SpaceBetween,
    ) {
        Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(2.dp)) {
            Text(label, color = Text, fontSize = 14.sp)
            sub?.let { Text(it, style = Telemetry) }
        }
        Switch(
            checked = checked,
            onCheckedChange = onChange,
            colors = SwitchDefaults.colors(
                checkedThumbColor = Void,
                checkedTrackColor = Ion,
                checkedBorderColor = Ion,
                uncheckedThumbColor = TextDim,
                uncheckedTrackColor = Carbon,
                uncheckedBorderColor = Edge,
            ),
        )
    }
}

/**
 * A selectable chip.
 *
 * Hand-rolled rather than `FilterChip`, whose border helper changed signature
 * between Material3 releases — this app pins a BOM, but a chip that stops
 * compiling on the next bump is a poor trade for a rounded rectangle with a
 * border on it.
 */
@Composable
private fun ChoiceChip(label: String, selected: Boolean, onClick: () -> Unit) {
    Box(
        Modifier
            .background(if (selected) Ion.copy(alpha = 0.12f) else Color.Transparent, RoundedCornerShape(4.dp))
            .border(1.dp, if (selected) Ion else Edge, RoundedCornerShape(4.dp))
            .clickable(onClick = onClick)
            .padding(horizontal = 12.dp, vertical = 7.dp)
    ) {
        Text(label, style = TelemetryStrong.copy(color = if (selected) Ion else TextDim, fontSize = 12.sp))
    }
}

@Composable
private fun SheetButton(label: String, accent: Color = Ion, enabled: Boolean = true, onClick: () -> Unit) {
    OutlinedButton(
        onClick = onClick,
        enabled = enabled,
        modifier = Modifier.fillMaxWidth(),
        shape = RoundedCornerShape(4.dp),
        border = androidx.compose.foundation.BorderStroke(1.dp, if (enabled) accent.copy(alpha = 0.6f) else Edge),
        colors = ButtonDefaults.outlinedButtonColors(
            contentColor = accent,
            disabledContentColor = TextDim,
        ),
    ) {
        Text(label, style = TelemetryStrong.copy(color = if (enabled) accent else TextDim))
    }
}

// ── Global settings ─────────────────────────────────────────────────────────

/**
 * The gear icon.
 *
 * Everything here is global rather than per-host, which is the line that
 * decides what belongs in this sheet: a codec is a property of *this phone's
 * decoder*, an address is a property of *that machine*. Per-host settings live
 * behind a long press on the card instead.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun SettingsSheet(
    settings: EchoSettings,
    controller: EchoController,
    micEnabled: Boolean,
    micActive: Boolean,
    micProblem: String?,
    onDismiss: () -> Unit,
) {
    val prefs = settings.prefsState
    val context = LocalContext.current

    // Asked for on first use rather than at launch: a microphone prompt before
    // the user has asked for a microphone is the kind of thing people decline
    // reflexively, and a declined permission is much harder to recover from
    // than an un-asked one.
    val micPermission = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestPermission(),
    ) { granted -> controller.onMicPermissionResult(granted) }

    ModalBottomSheet(
        onDismissRequest = onDismiss,
        // Straight to full height. This activity is locked to landscape, where a
        // half-expanded sheet is a letterbox: three controls visible and the
        // rest behind a drag most people do not know is there.
        sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true),
        containerColor = Carbon,
        contentColor = Text,
        dragHandle = { BottomSheetDefaults.DragHandle(color = Edge) },
    ) {
        Column(
            Modifier
                .fillMaxWidth()
                .padding(horizontal = 20.dp)
                .padding(bottom = 28.dp)
                .verticalScroll(rememberScrollState()),
            verticalArrangement = Arrangement.spacedBy(14.dp),
        ) {
            Text("SETTINGS", style = MaterialTheme.typography.titleSmall, color = Ion)

            // ── Video ───────────────────────────────────────────────────────
            SheetTitle("VIDEO CODEC")
            EchoSettings.CODECS.forEach { (id, label, note) ->
                Row(
                    Modifier
                        .fillMaxWidth()
                        .selectable(
                            selected = prefs.codec == id,
                            onClick = { settings.edit { it.copy(codec = id) } },
                        )
                        .padding(vertical = 2.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    RadioButton(
                        selected = prefs.codec == id,
                        onClick = { settings.edit { it.copy(codec = id) } },
                        colors = RadioButtonDefaults.colors(selectedColor = Ion, unselectedColor = TextDim),
                    )
                    Column(Modifier.padding(start = 4.dp)) {
                        Text(label, color = if (prefs.codec == id) Ion else Text, fontSize = 14.sp)
                        Text(note, style = Telemetry)
                    }
                }
            }

            HorizontalDivider(color = Edge)

            SheetTitle("RESOLUTION")
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                EchoSettings.RESOLUTIONS.forEach { res ->
                    ChoiceChip(
                        label = res.substringAfter('x') + "P",
                        selected = prefs.resolution == res,
                        onClick = { settings.selectResolution(res) },
                    )
                }
            }

            SheetTitle("FRAMERATE")
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                EchoSettings.FRAMERATES.forEach { fps ->
                    ChoiceChip(
                        label = "${fps}FPS",
                        selected = prefs.fps == fps,
                        onClick = { settings.selectFps(fps) },
                    )
                }
            }

            SheetTitle("TARGET BITRATE")
            val recommended = EchoSettings.recommendedBitrateKbps(prefs.resolution, prefs.fps)
            Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                Text("${prefs.bitrateKbps / 1000} MBPS", style = TelemetryStrong.copy(fontSize = 20.sp, color = Ion))
                // Says whether this figure is the one the app chose or one the
                // user did. Without it, a snapped value and a deliberate
                // override look identical, and the next person to change the
                // resolution is surprised when "their" number moves.
                if (prefs.bitrateKbps != recommended) {
                    Badge("MANUAL // AUTO ${recommended / 1000}", Amber)
                } else {
                    Badge("AUTO", Matrix)
                }
            }
            Slider(
                value = (prefs.bitrateKbps / 1000).toFloat(),
                onValueChange = { settings.edit { p -> p.copy(bitrateKbps = it.toInt() * 1000) } },
                valueRange = EchoSettings.MIN_MBPS.toFloat()..EchoSettings.MAX_MBPS.toFloat(),
                // One stop per Mbps. Continuous would suggest a precision the
                // host does not honour anyway — it applies its own ceiling from
                // the negotiated resolution and framerate.
                steps = EchoSettings.MAX_MBPS - EchoSettings.MIN_MBPS - 1,
                colors = SliderDefaults.colors(
                    thumbColor = Ion,
                    activeTrackColor = Ion,
                    inactiveTrackColor = Edge,
                ),
            )
            Text(
                "Set automatically from the resolution and framerate, and re-set " +
                    "whenever either changes — drag to override. Still an opening bid: " +
                    "Nova caps it from what it actually negotiates, and the grant is " +
                    "what the decoder is configured from.",
                style = Telemetry,
            )

            HorizontalDivider(color = Edge)

            // ── Audio ───────────────────────────────────────────────────────
            SheetTitle("AUDIO")
            IonSwitch(
                label = "Microphone passthrough",
                sub = when {
                    !micEnabled -> "The phone microphone becomes the PC microphone."
                    micActive -> "Capturing — the PC hears this phone."
                    else -> micProblem ?: "Starts with the next session."
                },
                // Read from the controller, which owns the microphone and
                // persists the intent itself. Mirroring it into two places is
                // how a switch ends up disagreeing with the thing it controls.
                checked = micEnabled,
            ) { wanted ->
                val granted = context.checkSelfPermission(android.Manifest.permission.RECORD_AUDIO) ==
                    PackageManager.PERMISSION_GRANTED
                when {
                    !wanted -> controller.setMicEnabled(false)
                    granted -> controller.setMicEnabled(true)
                    else -> {
                        // Intent recorded first, so capture starts by itself the
                        // moment the permission callback returns — otherwise the
                        // user has to flip the switch twice.
                        controller.setMicEnabled(true)
                        micPermission.launch(android.Manifest.permission.RECORD_AUDIO)
                    }
                }
            }

            HorizontalDivider(color = Edge)

            // ── Identity ────────────────────────────────────────────────────
            SheetTitle("THIS DEVICE")
            IonField(
                value = prefs.deviceName,
                onValueChange = { name -> settings.edit { it.copy(deviceName = name) } },
                label = "Name shown on the PC when pairing",
            )

            HorizontalDivider(color = Edge)

            SheetTitle("DIAGNOSTICS")
            IonSwitch(
                label = "Show raw fingerprints & hashes",
                sub = "64-character certificate hashes, relay URLs and the event log.",
                checked = prefs.showTelemetry,
            ) { on -> settings.edit { it.copy(showTelemetry = on) } }
        }
    }
}

// ── Per-host configuration ──────────────────────────────────────────────────

/**
 * The long-press sheet: everything that is true of one machine.
 *
 * Sections expand in place rather than opening further sheets. A bottom sheet
 * stacked on a bottom sheet has no obvious way back on Android, and every
 * action here is two fields and a button.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun HostSheet(
    host: KnownHost,
    presence: Presence,
    store: HostStore,
    controller: EchoController,
    showTelemetry: Boolean,
    onDismiss: () -> Unit,
) {
    var panel by remember { mutableStateOf("") }

    ModalBottomSheet(
        onDismissRequest = onDismiss,
        // Straight to full height. This activity is locked to landscape, where a
        // half-expanded sheet is a letterbox: three controls visible and the
        // rest behind a drag most people do not know is there.
        sheetState = rememberModalBottomSheetState(skipPartiallyExpanded = true),
        containerColor = Carbon,
        contentColor = Text,
        dragHandle = { BottomSheetDefaults.DragHandle(color = Edge) },
    ) {
        Column(
            Modifier
                .fillMaxWidth()
                .padding(horizontal = 20.dp)
                .padding(bottom = 28.dp)
                .verticalScroll(rememberScrollState()),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Row(
                Modifier.fillMaxWidth(),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.SpaceBetween,
            ) {
                Text(host.displayName, color = Text, fontSize = 18.sp, fontWeight = FontWeight.SemiBold)
                Badge(presence.label(), presence.accent())
            }
            if (showTelemetry && host.fingerprint.isNotBlank()) {
                Text("FP ${host.fingerprint}", style = Telemetry)
            }

            HorizontalDivider(color = Edge)

            SheetButton("EDIT ENDPOINTS") { panel = if (panel == "endpoints") "" else "endpoints" }
            if (panel == "endpoints") EndpointEditor(host, store)

            SheetButton("RENAME HOST") { panel = if (panel == "rename") "" else "rename" }
            if (panel == "rename") RenameEditor(host, store)

            SheetButton("DIAGNOSTICS // PING") { panel = if (panel == "diag") "" else "diag" }
            if (panel == "diag") DiagnosticsPanel(host)

            // Host-scoped and genuinely needed: a session outlives the app, so a
            // phone that was swiped away leaves Nova holding the display for the
            // grace period with no client left to ask for it back. It needs an
            // authenticated tunnel and nothing else — no media socket, no keys,
            // no grant — which is why it works with no session of our own.
            SheetButton("END SESSION ON HOST", accent = Amber, enabled = host.streamable) {
                controller.releaseHostSession(host)
                onDismiss()
            }

            HorizontalDivider(color = Edge)

            if (panel == "forget") {
                Text(
                    "Forgetting deletes the stored identity for this host. Pairing again " +
                        "means walking to the PC for a new PIN.",
                    style = Telemetry.copy(color = Amber),
                )
                Row(horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                    Box(Modifier.weight(1f)) {
                        SheetButton("CONFIRM FORGET", accent = Crimson) {
                            store.forget(host.id)
                            onDismiss()
                        }
                    }
                    Box(Modifier.weight(1f)) { SheetButton("CANCEL", accent = TextDim) { panel = "" } }
                }
            } else {
                SheetButton("FORGET / UNPAIR", accent = Crimson) { panel = "forget" }
            }

            // Kept out of the way at the bottom: useful, never the point.
            Text(
                "Last seen " + if (host.lastSeenMs == 0L) "never"
                else "${(System.currentTimeMillis() - host.lastSeenMs) / 60_000} min ago",
                style = Telemetry,
            )
        }
    }}

@Composable
private fun EndpointEditor(host: KnownHost, store: HostStore) {
    // Seeded from the host and keyed on its id, so switching cards reseeds and
    // an edit in progress is not carried onto a different machine.
    var address by remember(host.id) { mutableStateOf(host.lanAddress ?: "") }
    var port by remember(host.id) { mutableStateOf(host.port.toString()) }
    var wan by remember(host.id) { mutableStateOf(host.wanEndpoint ?: "") }
    var relay by remember(host.id) { mutableStateOf(host.relayUrl ?: "") }
    var pin by remember(host.id) { mutableStateOf(host.relayPin ?: "") }
    var saved by remember(host.id) { mutableStateOf(false) }

    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
        IonField(address, { address = it; saved = false }, "Host IPv4 on the LAN")
        IonField(port, { port = it.filter(Char::isDigit); saved = false }, "Echo control port", numeric = true)
        IonField(wan, { wan = it; saved = false }, "WAN address or domain (optional)")
        IonField(relay, { relay = it; saved = false }, "Relay signalling URL")
        IonField(pin, { pin = it.trim(); saved = false }, "Relay certificate fingerprint")
        Text(
            "The relay pair is what makes a host reachable from outside this " +
                "network, and both halves are required — the fingerprint is what " +
                "authenticates the relay, so a URL without it cannot be dialled.",
            style = Telemetry,
        )
        SheetButton(if (saved) "SAVED" else "SAVE ENDPOINTS", accent = if (saved) Matrix else Ion) {
            store.update(host.id) {
                it.copy(
                    lanAddress = address.trim().takeIf(String::isNotBlank),
                    port = port.toIntOrNull() ?: KnownHost.DEFAULT_PORT,
                    wanEndpoint = wan.trim().takeIf(String::isNotBlank),
                    relayUrl = relay.trim().takeIf(String::isNotBlank),
                    relayPin = pin.trim().takeIf(String::isNotBlank),
                )
            }
            saved = true
        }
    }
}

@Composable
private fun RenameEditor(host: KnownHost, store: HostStore) {
    var alias by remember(host.id) { mutableStateOf(host.alias ?: "") }
    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
        IonField(alias, { alias = it }, "Alias — blank restores \"${host.name}\"")
        SheetButton("SAVE NAME") {
            store.update(host.id) { it.copy(alias = alias.trim().takeIf(String::isNotBlank)) }
        }
    }
}

@Composable
private fun DiagnosticsPanel(host: KnownHost) {
    val scope = rememberCoroutineScope()
    val filesDir = LocalContext.current.filesDir.absolutePath
    var lan by remember(host.id) { mutableStateOf<ProbeResult?>(null) }
    var relay by remember(host.id) { mutableStateOf<ProbeResult?>(null) }
    var registration by remember(host.id) { mutableStateOf<RelayStatus?>(null) }
    var running by remember(host.id) { mutableStateOf(false) }

    fun run() {
        if (running) return
        running = true
        scope.launch {
            lan = host.lanAddress?.let { Probe.tcp(it, host.port) }
            relay = Probe.relay(host.relayUrl)
            // Three questions, not two, and the third is the one that decides
            // whether tapping the card will work: the relay being up says
            // nothing about whether Nova is announcing to it.
            registration = Probe.hostRegistered(host, filesDir)
            running = false
        }
    }

    // Probed on open rather than on a button, because opening this panel IS the
    // question. The button is for asking again.
    LaunchedEffect(host.id) { run() }

    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
        ProbeLine("LAN   ${host.lanAddress ?: "not set"}:${host.port}", lan, running)
        ProbeLine("RELAY " + (Probe.relayEndpoint(host.relayUrl)?.let { "${it.first}:${it.second}" } ?: "not set"), relay, running)
        Row(
            Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.SpaceBetween,
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text("HOST REGISTERED WITH RELAY", style = Telemetry, modifier = Modifier.weight(1f))
            when {
                registration == null && running -> Text("…", style = Telemetry.copy(color = Ion))
                registration == null -> Text("—", style = Telemetry)
                registration!!.registered -> Badge("YES", Matrix)
                else -> Badge("NO", Amber)
            }
        }
        registration?.takeIf { !it.registered && it.detail.isNotBlank() }?.let {
            Text(it.detail, style = Telemetry.copy(color = Amber))
        }
        registration?.candidates?.takeIf { it.isNotEmpty() }?.let {
            // The addresses the host is announcing. This is the answer to "do I
            // need to fill in a manual WAN endpoint?" — normally no, because
            // STUN discovers the public address and the host publishes it here
            // without anyone typing anything.
            Text("HOST CANDIDATES: ${it.joinToString(", ")}", style = Telemetry)
        }
        Text(
            "TCP connect time, not ICMP — Android gives an app no raw sockets. It " +
                "measures the path to the port, which is the question a card that " +
                "says OFFLINE is really asking.",
            style = Telemetry,
        )
        SheetButton(if (running) "PROBING…" else "PROBE AGAIN", enabled = !running) { run() }
    }
}

@Composable
private fun ProbeLine(label: String, result: ProbeResult?, running: Boolean) {
    Row(
        Modifier.fillMaxWidth(),
        horizontalArrangement = Arrangement.SpaceBetween,
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(label, style = Telemetry, modifier = Modifier.weight(1f))
        when {
            result == null && running -> Text("…", style = Telemetry.copy(color = Ion))
            result == null -> Text("—", style = Telemetry)
            result.reachable -> Badge("${result.millis} MS", if (result.millis <= 20) Matrix else Ion)
            else -> Badge("NO ANSWER", Crimson)
        }
    }
}

/** Add a host that mDNS cannot see — a different subnet, or a VPN. */
@Composable
fun AddHostDialog(onDismiss: () -> Unit, onAdd: (String, String, Int) -> Unit) {
    var name by remember { mutableStateOf("") }
    var address by remember { mutableStateOf("") }
    var port by remember { mutableStateOf(KnownHost.DEFAULT_PORT.toString()) }

    AlertDialog(
        onDismissRequest = onDismiss,
        containerColor = Carbon,
        titleContentColor = Ion,
        textContentColor = Text,
        title = { Text("ADD HOST", style = MaterialTheme.typography.titleSmall) },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(10.dp)) {
                IonField(name, { name = it }, "Name")
                IonField(address, { address = it }, "IPv4 address")
                IonField(port, { port = it.filter(Char::isDigit) }, "Port", numeric = true)
                Text(
                    "Pair over the LAN first; the relay can be filled in afterwards " +
                        "from the card.",
                    style = Telemetry,
                )
            }
        },
        confirmButton = {
            TextButton(
                onClick = { onAdd(name.trim(), address.trim(), port.toIntOrNull() ?: KnownHost.DEFAULT_PORT) },
                enabled = address.isNotBlank(),
            ) { Text("ADD", style = TelemetryStrong.copy(color = if (address.isNotBlank()) Ion else TextDim)) }
        },
        dismissButton = {
            TextButton(onClick = onDismiss) { Text("CANCEL", style = TelemetryStrong.copy(color = TextDim)) }
        },
    )
}
