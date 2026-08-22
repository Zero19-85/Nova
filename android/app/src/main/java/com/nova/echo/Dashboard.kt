package com.nova.echo

import androidx.compose.animation.animateColorAsState
import androidx.compose.animation.animateContentSize
import androidx.compose.animation.core.tween
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.indication
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.interaction.PressInteraction
import androidx.compose.foundation.interaction.collectIsPressedAsState
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.semantics.onClick
import androidx.compose.ui.semantics.onLongClick
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import kotlinx.coroutines.launch

/**
 * Where a host is answering from *right now*.
 *
 * Deliberately three states rather than a boolean, because "offline" was doing
 * two jobs and neither well. A host that is not on this Wi-Fi may still be one
 * relay hop away and perfectly streamable; a host with no relay at all is not.
 * The badge says which, so tapping a card is never a guess.
 */
enum class Presence {
    Lan,
    Wan,
    /** The manually configured WAN endpoint is the one carrying media. */
    DirectWan,
    /**
     * Not on this network, but the relay says the host is announcing right now
     * — so tapping will reach it over the internet.
     *
     * The state that fixes the misleading badge: a host on the far side of a
     * relay was reading OFFLINE, which told the user not to bother with a
     * machine that was perfectly reachable.
     */
    RelayReady,
    Cached,
}

fun Presence.label() = when (this) {
    Presence.Lan -> "ONLINE // LAN"
    Presence.Wan -> "ONLINE // WAN_PUNCH"
    Presence.DirectWan -> "ONLINE // DIRECT_WAN"
    Presence.RelayReady -> "ONLINE // RELAY_READY"
    Presence.Cached -> "OFFLINE // CACHED"
}

fun Presence.accent() = when (this) {
    // Green is reserved for "the network answered on the local segment", which
    // is the only case where latency is not at the mercy of the internet.
    Presence.Lan -> Matrix
    Presence.Wan, Presence.DirectWan, Presence.RelayReady -> Ion
    Presence.Cached -> TextDim
}

/**
 * The transport the engine reported, as a badge state.
 *
 * The string comes from `session::Transport::as_str` on the Rust side and is
 * API between the two. An unrecognised value falls back to [Presence.Wan]
 * rather than to `Cached`: whatever it is, a path is open, and a card reading
 * OFFLINE beside a live picture is the one answer that is certainly wrong.
 */
fun transportPresence(transport: String): Presence = when (transport) {
    "lan" -> Presence.Lan
    "direct_wan" -> Presence.DirectWan
    else -> Presence.Wan
}

/**
 * What a session should open INTO, chosen before it starts.
 *
 * [appId] is Nova's own `app_launcher::APP_ID_*` value and is wire API between
 * the two halves — the host routes on it twice: once to decide whether the
 * session is headless (`uses_virtual_display`: 2/3/4/5 always are) and once to
 * decide what to spawn. Label and id therefore live in one place. A mode whose
 * id has drifted is not a dead button, it is a session that opens on the wrong
 * desktop, which is far harder to recognise as a bug.
 *
 * [Mirror] is app 1 (Desktop) — the one mode that shows the physical primary
 * rather than a virtual display, which is why it earns a button beside the
 * three launchers.
 */
enum class LaunchMode(val label: String, val appId: Int) {
    Steam("STEAM", 2),
    Xbox("XBOX", 3),
    RetroArch("RETROARCH", 4),
    Mirror("MIRROR", 1),
}

/**
 * What a double-tap opens: app 5, Virtual Desktop.
 *
 * A top-level constant rather than a fifth [LaunchMode], because
 * `LaunchMode.entries` IS the launch row — adding it there would draw a fifth
 * button for the mode whose whole point is that it needs no button.
 */
const val QUICK_START_APP_ID = 5

/**
 * The dashboard: what Echo shows when nothing is streaming.
 *
 * It renders [HostStore], not [HostDiscovery]. That inversion is the fix for
 * the vanishing-host bug — mDNS is now one *source of evidence* folded into a
 * persistent list, rather than the list itself, so a host survives Nova
 * restarting, the phone moving to cellular, and the app being killed.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun EchoDashboard(controller: EchoController, state: UiState) {
    val context = LocalContext.current
    val store = remember { HostStore.of(context) }
    val settings = remember { EchoSettings.of(context) }
    val prefs = settings.prefsState
    val known by store.hosts.collectAsState()

    // Browse only while this screen is up. During a session there is nothing to
    // choose, and a browse that outlives its UI is multicast traffic and a
    // wakelock nobody asked for.
    val discovery = remember { HostDiscovery(context) }
    DisposableEffect(discovery) {
        discovery.start()
        onDispose { discovery.stop() }
    }
    val found by discovery.hosts.collectAsState()
    val browsing by discovery.browsing.collectAsState()

    // Every sighting is folded into the store, so the card outlives the record
    // that produced it.
    LaunchedEffect(found) { found.forEach { store.observed(it) } }

    // A known host counts as on the LAN when a live record matches its identity,
    // or — before pairing has supplied one — the address it answers on.
    val onLan: Set<String> = remember(found, known) {
        known.filter { host ->
            found.any { seen ->
                (host.fingerprint.isNotBlank() && seen.fingerprint.equals(host.fingerprint, true)) ||
                    (host.lanAddress != null && seen.address == host.lanAddress)
            }
        }.map { it.id }.toSet()
    }

    // Reachability for hosts that are NOT on this LAN. Without this a cached
    // card could only ever say "offline", which is the one thing it often is
    // not: the host is usually up and announcing to its relay.
    //
    // The question asked is "is this host registered with its relay", not "is
    // the relay up". A relay running beside a switched-off Nova would answer a
    // TCP probe perfectly and put a live badge on a dead machine.
    var wan by remember { mutableStateOf(mapOf<String, RelayStatus>()) }
    var probeNonce by remember { mutableIntStateOf(0) }
    val filesDir = remember { context.filesDir.absolutePath }
    LaunchedEffect(known.map { it.id to it.relayUrl }, onLan, probeNonce) {
        // Sequential rather than parallel: this is background curiosity, not
        // something anyone is waiting on, and a burst of simultaneous TLS
        // handshakes on a phone radio is the kind of thing that wakes it up for
        // no reason.
        known.filter { it.id !in onLan && it.hasRelay }.forEach { host ->
            wan = wan + (host.id to Probe.hostRegistered(host, filesDir))
        }
    }

    // Which host the live session belongs to, so its badge can report the route
    // the engine actually took rather than what a probe guessed a moment ago.
    var activeHostId by remember { mutableStateOf<String?>(null) }
    LaunchedEffect(state.connected) { if (!state.connected) activeHostId = null }

    // Which card has its launch row open. Hoisted out of the card so that at
    // most ONE is ever open — an accordion is the right shape for a list of
    // machines, since two open panels are two identical sets of buttons with
    // nothing saying which belongs to which. It also has to live above the
    // card because card-local state is lost every time the store re-emits,
    // which it does on every mDNS sighting.
    var expandedHostId by remember { mutableStateOf<String?>(null) }

    // A session starting is the end of choosing. Collapsing here also means the
    // panel is not still sitting open behind the stream when it ends.
    LaunchedEffect(state.connected) { if (state.connected) expandedHostId = null }

    fun presence(host: KnownHost): Presence = when {
        // A live path outranks every probe. The engine classifies from the peer
        // the punch latched, which is the only source that can distinguish a
        // relay-signalled session that landed on the LAN from one that did not
        // — and no probe from here can tell those apart.
        host.id == activeHostId && state.transport != null -> transportPresence(state.transport)
        host.id in onLan -> Presence.Lan
        wan[host.id]?.registered == true -> Presence.RelayReady
        else -> Presence.Cached
    }

    // Which host a pairing handshake belongs to. The fingerprint arrives on an
    // event with nothing in it identifying the card that started the flow, so
    // the association has to be remembered here.
    var pairingId by remember { mutableStateOf<String?>(null) }
    LaunchedEffect(state.hostFingerprint) {
        val fingerprint = state.hostFingerprint
        val id = pairingId
        if (!fingerprint.isNullOrBlank() && id != null) {
            store.paired(id, fingerprint)
            pairingId = null
        }
    }

    var settingsOpen by remember { mutableStateOf(false) }
    var sheetHostId by remember { mutableStateOf<String?>(null) }
    var addOpen by remember { mutableStateOf(false) }
    val snackbar = remember { SnackbarHostState() }
    val scope = rememberCoroutineScope()

    fun say(message: String) {
        scope.launch { snackbar.showSnackbar(message) }
    }

    /**
     * Start a session on [host], opening into [appId].
     *
     * The unpaired and unroutable branches come first because they are not
     * failures of the app id — no app id is reachable on a host with no route,
     * and saying so is more use than a session that cannot open.
     */
    fun activate(host: KnownHost, appId: Int = LaunchMode.Mirror.appId) {
        when {
            !host.paired -> {
                val address = host.lanAddress
                if (address.isNullOrBlank()) {
                    say("No address to pair with — hold the card to set one.")
                } else {
                    pairingId = host.id
                    controller.pair(address, prefs.deviceName)
                }
            }
            // A relay is only needed to reach a host that is NOT on this
            // network. With a LAN address the cascade has a route to try.
            !host.streamable -> say("No route to this host — hold the card to set an address or a relay.")
            else -> {
                activeHostId = host.id
                controller.connect(host, prefs, appId)
            }
        }
    }

    /**
     * A launch button was pressed: stream, opening into that mode's app.
     *
     * The panel closes on its own when the session starts — the collapse is
     * driven by `state.connected`, not from here, so a connect that never
     * lands leaves the row open with the other three modes still in reach.
     */
    fun launch(host: KnownHost, mode: LaunchMode) = activate(host, mode.appId)

    /**
     * Double-tap: straight into a stream, no panel, no choice.
     *
     * App 5 is Virtual Desktop, the mode that exists precisely to be the one
     * you take without thinking about it — which is why it is the gesture with
     * no menu in front of it.
     */
    fun quickStart(host: KnownHost) = activate(host, QUICK_START_APP_ID)

    Scaffold(
        containerColor = Void,
        snackbarHost = { SnackbarHost(snackbar) },
        topBar = {
            TopAppBar(
                // Borderless: no elevation, no divider, no container tint. The
                // bar is a place to put the logo and the gear, not a surface.
                colors = TopAppBarDefaults.topAppBarColors(
                    containerColor = Color.Transparent,
                    scrolledContainerColor = Color.Transparent,
                ),
                title = {
                    Text(
                        "ECHO",
                        style = MaterialTheme.typography.headlineMedium,
                        color = Ion,
                        fontSize = 22.sp,
                    )
                },
                actions = {
                    IconButton(onClick = { settingsOpen = true }) {
                        Icon(Icons.Filled.Settings, contentDescription = "Settings", tint = TextDim)
                    }
                },
            )
        },
    ) { padding ->
        Column(
            Modifier
                .padding(padding)
                .fillMaxSize()
                .padding(horizontal = 20.dp)
                .verticalScroll(rememberScrollState()),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            StatusLine(state)

            state.pin?.let { pin -> PinCard(pin) }

            state.error?.let { message ->
                Text(message, color = Crimson, style = Telemetry.copy(color = Crimson, fontSize = 12.sp))
            }

            SectionHeader(
                title = if (known.isEmpty()) "NO HOSTS" else "HOSTS",
                trailing = if (browsing) "SCANNING" else "IDLE",
                trailingAccent = if (browsing) Ion else TextDim,
                onAction = {
                    discovery.restart()
                    probeNonce++
                },
                actionLabel = "RESCAN",
            )

            if (known.isEmpty()) {
                EmptyState(browsing)
            } else {
                known.forEach { host ->
                    // A host that cannot stream yet keeps the old meaning of a
                    // tap. Expanding into four launch buttons that would all
                    // fail is a worse answer than starting the pairing the card
                    // is already telling the user to start.
                    val expandable = host.paired && host.streamable
                    HostCard(
                        host = host,
                        presence = presence(host),
                        relay = wan[host.id],
                        showTelemetry = prefs.showTelemetry,
                        busy = state.connected && !state.streaming,
                        expandable = expandable,
                        expanded = expandable && expandedHostId == host.id,
                        onTap = {
                            if (expandable) {
                                expandedHostId = if (expandedHostId == host.id) null else host.id
                            } else {
                                activate(host)
                            }
                        },
                        onDoubleTap = { quickStart(host) },
                        onLongPress = { sheetHostId = host.id },
                        onLaunch = { mode -> launch(host, mode) },
                    )
                }
            }

            TextButton(onClick = { addOpen = true }) {
                Text("+ ADD HOST MANUALLY", style = TelemetryStrong.copy(color = Ion))
            }

            HorizontalDivider(color = Edge)

            // Identity and the raw event log are the two things that made this
            // screen read as a test harness. Both are still one switch away.
            if (prefs.showTelemetry) {
                Text("THIS DEVICE", style = MaterialTheme.typography.labelSmall, color = TextDim)
                Text(state.myFingerprint, style = Telemetry)
                if (state.log.isNotEmpty()) {
                    Text("EVENT LOG", style = MaterialTheme.typography.labelSmall, color = TextDim)
                    state.log.forEach { Text(it, style = Telemetry) }
                }
            }

            Spacer(Modifier.height(24.dp))
        }
    }

    if (settingsOpen) {
        SettingsSheet(
            settings = settings,
            controller = controller,
            micEnabled = state.micEnabled,
            micActive = state.micActive,
            micProblem = state.micProblem,
            onDismiss = { settingsOpen = false },
        )
    }

    sheetHostId?.let { id ->
        // Resolved from the live list rather than captured, so an edit made in
        // the sheet is reflected by the sheet itself. A host forgotten from
        // inside it simply closes it.
        val host = known.firstOrNull { it.id == id }
        if (host == null) {
            sheetHostId = null
        } else {
            HostSheet(
                host = host,
                presence = presence(host),
                store = store,
                controller = controller,
                showTelemetry = prefs.showTelemetry,
                onDismiss = { sheetHostId = null },
            )
        }
    }

    if (addOpen) {
        AddHostDialog(
            onDismiss = { addOpen = false },
            onAdd = { name, address, port ->
                sheetHostId = store.addManual(name, address, port)
                addOpen = false
            },
        )
    }
}

@Composable
private fun StatusLine(state: UiState) {
    Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
        val live = state.connected || state.streaming
        Box(
            Modifier
                .size(7.dp)
                .background(if (live) Matrix else TextDim, RoundedCornerShape(4.dp))
        )
        Text(state.status.uppercase(), style = TelemetryStrong)
    }
}

@Composable
private fun PinCard(pin: String) {
    Card(
        Modifier.fillMaxWidth(),
        colors = CardDefaults.cardColors(containerColor = Carbon),
        border = BorderStroke(1.dp, Ion),
        shape = RoundedCornerShape(6.dp),
    ) {
        Column(
            Modifier.fillMaxWidth().padding(16.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            Text("TYPE THIS PIN INTO NOVA", style = MaterialTheme.typography.labelSmall, color = TextDim)
            Text(
                pin,
                style = TelemetryStrong.copy(fontSize = 44.sp, color = Ion, letterSpacing = 8.sp),
                textAlign = TextAlign.Center,
            )
        }
    }
}

@Composable
private fun SectionHeader(
    title: String,
    trailing: String,
    trailingAccent: Color,
    actionLabel: String,
    onAction: () -> Unit,
) {
    Row(
        Modifier.fillMaxWidth(),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.SpaceBetween,
    ) {
        Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(10.dp)) {
            Text(title, style = MaterialTheme.typography.titleSmall, color = Text)
            Text(trailing, style = Telemetry.copy(color = trailingAccent))
        }
        TextButton(onClick = onAction) {
            Text(actionLabel, style = TelemetryStrong.copy(color = Ion))
        }
    }
}

@Composable
private fun EmptyState(browsing: Boolean) {
    Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
        Text(
            if (browsing) "Listening for Nova on this network…"
            else "Nothing answered on this network.",
            style = Telemetry.copy(fontSize = 12.sp),
        )
        Text(
            "Check the phone and the PC are on the same Wi-Fi and that Nova is running. " +
                "A host added by hand stays on this screen even when it cannot be seen.",
            style = Telemetry,
        )
    }
}

/**
 * One machine.
 *
 * Three gestures, routed by one [detectTapGestures] rather than by
 * `combinedClickable`: tap opens the launch row, double-tap skips it and
 * streams, hold configures. The card IS the control — a card wearing four
 * little icons at all times is the cluttered thing this replaced.
 *
 * **The cost of the double-tap, stated plainly:** once a detector has an
 * `onDoubleTap`, it can no longer report a single tap until the double-tap
 * window has passed, so [onTap] fires roughly 300 ms late. That is inherent to
 * the gesture, not a tunable — and it is why [onDoubleTap] is the one wired to
 * streaming: the fast path stays fast, and the delay lands on opening a panel,
 * where nobody can feel it.
 *
 * [expanded] is passed in rather than remembered here so the list behaves as an
 * accordion; see the note at its declaration in [EchoDashboard].
 */
@Composable
private fun HostCard(
    host: KnownHost,
    presence: Presence,
    relay: RelayStatus?,
    showTelemetry: Boolean,
    busy: Boolean,
    expandable: Boolean,
    expanded: Boolean,
    onTap: () -> Unit,
    onDoubleTap: () -> Unit,
    onLongPress: () -> Unit,
    onLaunch: (LaunchMode) -> Unit,
) {
    val accent = presence.accent()

    // The gesture detector is keyed on Unit so a recomposition never restarts it
    // mid-gesture — which means it captures the FIRST callbacks it is given, and
    // those close over state that moves (which card is expanded, this host's
    // current record). rememberUpdatedState is what keeps the frozen detector
    // calling today's lambdas.
    val currentTap by rememberUpdatedState(onTap)
    val currentDoubleTap by rememberUpdatedState(onDoubleTap)
    val currentLongPress by rememberUpdatedState(onLongPress)

    // combinedClickable brought its own ripple and its own semantics; a raw
    // pointerInput brings neither. Both are re-supplied here rather than
    // dropped: without the first the card stops acknowledging touches at all,
    // and without the second it becomes invisible to TalkBack.
    val interaction = remember { MutableInteractionSource() }
    Card(
        modifier = Modifier
            .fillMaxWidth()
            .indication(interaction, ripple())
            .pointerInput(Unit) {
                detectTapGestures(
                    // Feeds the ripple above. onPress is the only hook that
                    // knows where the finger landed and when it left, which is
                    // exactly what a bounded ripple needs.
                    onPress = { offset ->
                        val press = PressInteraction.Press(offset)
                        interaction.emit(press)
                        if (tryAwaitRelease()) interaction.emit(PressInteraction.Release(press))
                        else interaction.emit(PressInteraction.Cancel(press))
                    },
                    onTap = { currentTap() },
                    onDoubleTap = { currentDoubleTap() },
                    onLongPress = { currentLongPress() },
                )
            }
            .semantics(mergeDescendants = true) {
                onClick(label = if (expandable) "Launch modes" else "Connect") {
                    currentTap(); true
                }
                onLongClick(label = "Configure host") { currentLongPress(); true }
            },
        colors = CardDefaults.cardColors(containerColor = Carbon),
        // The border is the glow: an online host is outlined in its accent, a
        // cached one in the hairline edge. It reads at a glance across a room,
        // which a text label does not.
        border = BorderStroke(1.dp, if (presence == Presence.Cached) Edge else accent.copy(alpha = 0.55f)),
        shape = RoundedCornerShape(6.dp),
    ) {
        // animateContentSize sits on the OUTER column, which owns no padding of
        // its own, so the height it animates is exactly the panel's. It clips to
        // the animating bounds too, and that is what makes the row read as
        // sliding out from under the metrics rather than popping in beneath them.
        Column(
            Modifier
                .fillMaxWidth()
                .animateContentSize(animationSpec = tween(durationMillis = 180))
        ) {
            Column(Modifier.padding(14.dp), verticalArrangement = Arrangement.spacedBy(7.dp)) {
                Row(
                    Modifier.fillMaxWidth(),
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.SpaceBetween,
                ) {
                    Text(
                        host.displayName,
                        color = Text,
                        fontSize = 17.sp,
                        fontWeight = FontWeight.SemiBold,
                    )
                    Badge(presence.label(), accent)
                }

                Text(
                    "ENDPOINT: " + (host.lanAddress?.let { "$it:${host.port}" }
                        ?: host.wanEndpoint
                        ?: "not set"),
                    style = Telemetry,
                )

                Row(horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                    Tag(if (host.paired) "PAIRED" else "NOT PAIRED", if (host.paired) Matrix else Amber)
                    Tag(if (host.hasRelay) "RELAY" else "NO RELAY", if (host.hasRelay) Ion else TextDim)
                }

                // What tapping will do, when it is not "open the launch row". A
                // card that quietly does something other than what it says is
                // worse than a card that says so.
                val hint = when {
                    busy -> "session starting…"
                    !host.paired -> "TAP TO PAIR — Nova shows a PIN on the PC"
                    !host.streamable -> "HOLD TO SET AN ADDRESS OR A RELAY — no route to try"
                    // Reachable here and nowhere else. Worth saying plainly rather
                    // than letting the user discover it on the train.
                    !host.hasRelay -> "LAN ONLY — add a relay to reach this host from elsewhere"
                    // Why the badge says OFFLINE, in the relay's own words. Without
                    // this the card states a conclusion and withholds the evidence,
                    // which is the difference between "it is broken" and "the relay
                    // is up and this host is not announcing to it".
                    presence == Presence.Cached && relay != null && relay.detail.isNotBlank() ->
                        relay.detail.uppercase()
                    expandable && !expanded -> "TAP FOR MODES — DOUBLE-TAP STREAMS — HOLD CONFIGURES"
                    expandable -> "DOUBLE-TAP ANYWHERE ON THE CARD TO STREAM NOW"
                    else -> null
                }
                hint?.let { Text(it, style = Telemetry.copy(color = if (host.streamable) TextDim else Amber)) }

                if (showTelemetry) {
                    HorizontalDivider(color = Edge)
                    Text("NOVA FP  ${host.fingerprint.ifBlank { "—" }}", style = Telemetry)
                    Text("RELAY FP ${host.relayPin ?: "—"}", style = Telemetry)
                    host.relayUrl?.let { Text("RELAY    $it", style = Telemetry) }
                }
            }

            if (expanded) LaunchRow(onLaunch)
        }
    }
}

/**
 * The four launch modes, revealed under a card's metrics.
 *
 * Ground is [Void], not [Carbon] — the panel drops to the pitch black the rest
 * of the app sits on, so on an OLED screen the buttons float in what looks like
 * a hole cut out of the card. That is the whole visual trick and it costs
 * nothing: unlit pixels are the cheapest thing this screen can draw.
 */
@Composable
private fun LaunchRow(onLaunch: (LaunchMode) -> Unit) {
    Column(Modifier.fillMaxWidth()) {
        HorizontalDivider(color = Edge)
        Row(
            Modifier
                .fillMaxWidth()
                .background(Void)
                .padding(vertical = 10.dp, horizontal = 4.dp),
            horizontalArrangement = Arrangement.SpaceEvenly,
            verticalAlignment = Alignment.CenterVertically,
        ) {
            LaunchMode.entries.forEach { mode ->
                LaunchAction(mode.label) { onLaunch(mode) }
            }
        }
    }
}


/**
 * One text-only launch button.
 *
 * Deliberately not a [TextButton]: Material's 48dp minimum target plus its own
 * horizontal content padding makes four of these overflow a phone-width card,
 * and its container defaults fight the black ground the panel exists to show.
 * This is a Box with a ripple, which is all a text button really is.
 *
 * The labels rest in [Ion] against [Void] — neon on black, lit rather than
 * printed. Press therefore cannot be signalled by going cyan, since they
 * already are, so it goes the other way: the label flares up to near-white for
 * 90 ms, the way a filament does when the current rises. The [Ion] ripple
 * underneath is the other half; on pitch black a ripple alone is easy to miss
 * at the arm's length these are actually pressed from.
 */
@Composable
private fun LaunchAction(label: String, onClick: () -> Unit) {
    val interaction = remember { MutableInteractionSource() }
    val pressed by interaction.collectIsPressedAsState()
    val color by animateColorAsState(
        targetValue = if (pressed) Text else Ion,
        animationSpec = tween(durationMillis = 90),
        label = "launchActionColor",
    )
    Box(
        Modifier
            .clip(RoundedCornerShape(4.dp))
            .clickable(
                interactionSource = interaction,
                indication = ripple(color = Ion),
                onClick = onClick,
            )
            .padding(horizontal = 8.dp, vertical = 8.dp),
        contentAlignment = Alignment.Center,
    ) {
        Text(label, style = TelemetryStrong.copy(color = color, fontSize = 11.sp, letterSpacing = 1.sp))
    }
}

@Composable
fun Badge(text: String, accent: Color) {
    Box(
        Modifier
            .background(accent.copy(alpha = 0.10f), RoundedCornerShape(3.dp))
            .border(1.dp, accent.copy(alpha = 0.55f), RoundedCornerShape(3.dp))
            .padding(horizontal = 7.dp, vertical = 3.dp)
    ) {
        Text(text, style = TelemetryStrong.copy(color = accent, fontSize = 10.sp))
    }
}

@Composable
private fun Tag(text: String, accent: Color) {
    Text(text, style = Telemetry.copy(color = accent, fontSize = 10.sp, fontWeight = FontWeight.Bold))
}
