package com.nova.echo

import android.annotation.SuppressLint
import android.content.Context
import android.util.Log
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import org.json.JSONArray
import org.json.JSONObject
import java.util.UUID

/**
 * A host Echo knows about, whether or not it can be seen right now.
 *
 * The distinction from [DiscoveredHost] is the whole point of this file.
 * [DiscoveredHost] is *evidence from the network* — it exists only while mDNS
 * keeps answering, and it evaporates the instant the phone leaves the Wi-Fi.
 * A [KnownHost] is *what the user has decided about a machine*: its alias, the
 * relay that reaches it from anywhere, the certificate it proved possession of.
 * None of that stops being true because the phone moved to 5G.
 *
 * Before this existed the dashboard rendered the mDNS list directly, so
 * switching networks — or Nova simply restarting — made the card vanish, and
 * with it the only route to a host that was still perfectly reachable over the
 * relay. The card the user needs when the LAN is gone is exactly the card the
 * LAN was drawing.
 */
data class KnownHost(
    /**
     * Stable primary key.
     *
     * The certificate fingerprint once one is known, because that is the only
     * identity a host actually proves. A manually-added host that has never
     * been paired gets a generated `manual:<uuid>` and is re-keyed to the
     * fingerprint the moment pairing supplies one.
     */
    val id: String,
    /** SHA-256 of the host cert, or blank before pairing. */
    val fingerprint: String,
    /** The name the host advertises (COMPUTERNAME). */
    val name: String,
    /** The user's own name for it, which always wins when set. */
    val alias: String?,
    /** Last address this host answered on, over mDNS or by hand. */
    val lanAddress: String?,
    /** Echo's control port. 48011 unless the host says otherwise. */
    val port: Int,
    /** Relay signalling URL — how this host is reached from outside the LAN. */
    val relayUrl: String?,
    /** The relay's certificate fingerprint. Without it the URL is unusable. */
    val relayPin: String?,
    /**
     * A WAN address or domain the user entered by hand.
     *
     * Recorded for the LAN-direct selector being built host-side; today it is
     * displayed and persisted but not yet dialled, because `session::open_path`
     * is still unconditionally relay-mediated.
     */
    val wanEndpoint: String?,
    /** Whether a PIN handshake has completed with this machine. */
    val paired: Boolean,
    /** Wall clock of the last mDNS sighting, for the "last seen" line. */
    val lastSeenMs: Long,
) {
    /** What to draw. */
    val displayName: String get() = alias?.takeIf { it.isNotBlank() } ?: name

    /**
     * Whether the relay pair is usable. Both or neither: a relay URL without
     * its pin cannot be dialled, since the pin is what authenticates it.
     */
    val hasRelay: Boolean get() = !relayUrl.isNullOrBlank() && !relayPin.isNullOrBlank()

    /**
     * Whether a stream can be started.
     *
     * Needs this machine's identity, and at least one route to it. A relay is
     * no longer required: the transport cascade tries the LAN first, and stage
     * 1 needs no relay, no STUN and no internet — so a host paired on the local
     * network streams with nothing else configured. Without a relay it is
     * simply unreachable from anywhere else, which the badge already says.
     */
    val streamable: Boolean
        get() = paired && fingerprint.isNotBlank() && (hasRelay || !lanAddress.isNullOrBlank())

    fun toJson(): JSONObject = JSONObject()
        .put("id", id)
        .put("fingerprint", fingerprint)
        .put("name", name)
        .put("alias", alias ?: JSONObject.NULL)
        .put("lan", lanAddress ?: JSONObject.NULL)
        .put("port", port)
        .put("relay", relayUrl ?: JSONObject.NULL)
        .put("relaypin", relayPin ?: JSONObject.NULL)
        .put("wan", wanEndpoint ?: JSONObject.NULL)
        .put("paired", paired)
        .put("seen", lastSeenMs)

    companion object {
        const val DEFAULT_PORT = 48011

        fun fromJson(o: JSONObject): KnownHost? {
            val id = o.optString("id").takeIf { it.isNotBlank() } ?: return null
            return KnownHost(
                id = id,
                fingerprint = o.optString("fingerprint"),
                name = o.optString("name").ifBlank { "Nova" },
                alias = o.optStringOrNull("alias"),
                lanAddress = o.optStringOrNull("lan"),
                port = o.optInt("port", DEFAULT_PORT),
                relayUrl = o.optStringOrNull("relay"),
                relayPin = o.optStringOrNull("relaypin"),
                wanEndpoint = o.optStringOrNull("wan"),
                paired = o.optBoolean("paired", false),
                lastSeenMs = o.optLong("seen", 0L),
            )
        }
    }
}

private fun JSONObject.optStringOrNull(key: String): String? =
    if (isNull(key)) null else optString(key).takeIf { it.isNotBlank() }

/**
 * The persisted dashboard.
 *
 * SharedPreferences rather than DataStore, deliberately. The whole store is a
 * handful of records read once at startup and rewritten on a user action; that
 * is precisely the workload SharedPreferences was built for, and DataStore
 * would add two dependencies and a coroutine scope to this app to solve a
 * contention problem it does not have. `apply()` writes on a background thread,
 * so no disk IO lands on the frame this runs in.
 *
 * The list is published as a [StateFlow] so Compose collects it the same way it
 * collects [HostDiscovery]. All mutation goes through [mutate], which is what
 * keeps "write the file" and "tell the UI" from ever drifting apart.
 */
class HostStore private constructor(context: Context) {

    private val prefs = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)

    private val _hosts = MutableStateFlow(load())

    /** Every host this device remembers, most recently seen first. */
    val hosts: StateFlow<List<KnownHost>> = _hosts.asStateFlow()

    init {
        // Runs once, ever. The flag is written before the work rather than
        // after, so a crash mid-migration cannot resurrect a host on every
        // subsequent launch — and, more importantly, a host the user
        // deliberately forgets is not re-adopted the next time the app opens.
        if (!prefs.getBoolean(KEY_MIGRATED, false)) {
            prefs.edit().putBoolean(KEY_MIGRATED, true).apply()
            adoptLegacySetup(context)
        }
    }

    /**
     * Carry the old single-host setup screen into the store.
     *
     * Before the dashboard existed, Echo streamed to exactly one machine and
     * kept its details in five loose SharedPreferences strings. The pairing
     * those describe is real — the private key and the host trust live in the
     * Rust identity directory and are untouched by this rewrite — so dropping
     * them would have forced a walk to the PC for a new PIN purely because the
     * UI changed. That is the kind of upgrade that makes people not upgrade.
     *
     * `paired = true` here is not the app trusting an advertisement: the
     * fingerprint being adopted was written by a completed PIN handshake, on
     * this device, by the previous version of this app.
     */
    private fun adoptLegacySetup(context: Context) {
        val legacy = context.getSharedPreferences(LEGACY_PREFS, Context.MODE_PRIVATE)
        val fingerprint = legacy.getString(LEGACY_HOST_FP, "").orEmpty().trim()
        if (fingerprint.isBlank()) return

        val address = legacy.getString(LEGACY_HOST, "").orEmpty().trim().takeIf { it.isNotBlank() }
        val relayUrl = legacy.getString(LEGACY_RELAY_URL, "").orEmpty().trim().takeIf { it.isNotBlank() }
        val relayPin = legacy.getString(LEGACY_RELAY_PIN, "").orEmpty().trim().takeIf { it.isNotBlank() }
        Log.i(TAG, "adopting the pre-dashboard host setup for ${fingerprint.take(12)}…")
        mutate { list ->
            // A record for this identity can already exist — mDNS will have
            // created one, labelled with the fingerprint it advertises but
            // marked unpaired, because an advertisement is never evidence of
            // pairing. The legacy prefs ARE that evidence, so this promotes the
            // existing card rather than adding a second one beside it.
            val existing = list.indexOfFirst { it.fingerprint.equals(fingerprint, true) }
            if (existing >= 0) {
                val host = list[existing]
                list[existing] = host.copy(
                    paired = true,
                    lanAddress = host.lanAddress ?: address,
                    relayUrl = host.relayUrl ?: relayUrl,
                    relayPin = host.relayPin ?: relayPin,
                )
                return@mutate
            }
            list.add(
                KnownHost(
                    id = fingerprint,
                    fingerprint = fingerprint,
                    // A placeholder until mDNS supplies the real machine name,
                    // at which point `observed` overwrites it. The old screen
                    // never recorded one — it only ever had an address.
                    name = address ?: "Nova",
                    alias = null,
                    lanAddress = address,
                    port = KnownHost.DEFAULT_PORT,
                    relayUrl = relayUrl,
                    relayPin = relayPin,
                    wanEndpoint = null,
                    paired = true,
                    lastSeenMs = System.currentTimeMillis(),
                )
            )
        }
    }

    private fun load(): List<KnownHost> = runCatching {
        val array = JSONArray(prefs.getString(KEY, "[]") ?: "[]")
        (0 until array.length()).mapNotNull { KnownHost.fromJson(array.getJSONObject(it)) }
    }.getOrElse {
        // A store that cannot be parsed was written by a version that no longer
        // exists. Losing it costs a re-pair; refusing to start costs the app.
        Log.w(TAG, "host store unreadable, starting empty: ${it.message}")
        emptyList()
    }

    private fun mutate(block: (MutableList<KnownHost>) -> Unit) {
        val next = _hosts.value.toMutableList().apply(block)
            .sortedByDescending { it.lastSeenMs }
        prefs.edit()
            .putString(KEY, JSONArray().apply { next.forEach { put(it.toJson()) } }.toString())
            .apply()
        _hosts.value = next
    }

    /**
     * Fold a live mDNS sighting into the store.
     *
     * Everything the user decided is preserved and everything the network
     * observed is refreshed. The one subtlety is matching: a host is the same
     * host if the fingerprints agree, and *also* if an un-fingerprinted manual
     * entry names the address this record arrived from — otherwise adding a
     * host by hand and then discovering it produces two cards for one machine.
     */
    fun observed(found: DiscoveredHost) {
        val now = System.currentTimeMillis()
        val existing = _hosts.value.firstOrNull {
            (it.fingerprint.isNotBlank() && it.fingerprint.equals(found.fingerprint, true)) ||
                (it.fingerprint.isBlank() && it.lanAddress == found.address)
        }
        if (existing != null) {
            val next = existing.copy(
                // Adopt the advertised fingerprint only as a LABEL for an entry
                // that has none. It is never promoted to `paired` here: mDNS is
                // unauthenticated, so this value may be used to recognise a
                // host and never to establish trust in one. Pairing writes that.
                fingerprint = existing.fingerprint.ifBlank { found.fingerprint },
                name = found.name,
                lanAddress = found.address,
                port = found.port,
                // Relay details are refreshed from the advertisement because the
                // host is the authority on its own relay, and a moved or
                // re-pinned relay would otherwise leave this app dialling an
                // endpoint that stopped answering.
                relayUrl = if (found.hasRelay) found.relayUrl else existing.relayUrl,
                relayPin = if (found.hasRelay) found.relayPin else existing.relayPin,
                lastSeenMs = now,
            )
            // A re-announcement that says nothing new is not worth a disk write
            // or a recomposition. mDNS re-announces on its own schedule and the
            // API-34 callback keeps delivering updates for the life of the
            // browse, so without this the store rewrites itself all afternoon to
            // change one timestamp nobody is reading to the second.
            val unchanged = next.copy(lastSeenMs = existing.lastSeenMs) == existing
            if (unchanged && now - existing.lastSeenMs < SEEN_REFRESH_MS) return
            mutate { list ->
                val index = list.indexOfFirst { it.id == existing.id }
                if (index >= 0) list[index] = next
            }
        } else {
            mutate { list ->
                list.add(
                    KnownHost(
                        id = found.fingerprint.ifBlank { "manual:${UUID.randomUUID()}" },
                        fingerprint = found.fingerprint,
                        name = found.name,
                        alias = null,
                        lanAddress = found.address,
                        port = found.port,
                        relayUrl = found.relayUrl,
                        relayPin = found.relayPin,
                        wanEndpoint = null,
                        paired = false,
                        lastSeenMs = now,
                    )
                )
            }
        }
    }

    /** Add a host the user typed in. Returns its id. */
    fun addManual(name: String, address: String, port: Int): String {
        val id = "manual:${UUID.randomUUID()}"
        mutate { list ->
            list.add(
                KnownHost(
                    id = id,
                    fingerprint = "",
                    name = name.ifBlank { address },
                    alias = null,
                    lanAddress = address,
                    port = port,
                    relayUrl = null,
                    relayPin = null,
                    wanEndpoint = null,
                    paired = false,
                    // Stamped now so it sorts to the top; the card still reads
                    // OFFLINE until mDNS or a probe says otherwise.
                    lastSeenMs = System.currentTimeMillis(),
                )
            )
        }
        return id
    }

    /** Apply an edit to one host. Nothing happens if it has been forgotten. */
    fun update(id: String, edit: (KnownHost) -> KnownHost) = mutate { list ->
        val index = list.indexOfFirst { it.id == id }
        if (index >= 0) list[index] = edit(list[index])
    }

    /**
     * Record a completed PIN handshake.
     *
     * This is the ONLY path that sets [KnownHost.paired] or writes a
     * fingerprint the app will later trust, and it takes that fingerprint from
     * the handshake — never from an advertisement. The entry is re-keyed to the
     * fingerprint at the same time, so a manual entry and its paired identity
     * converge on one record instead of leaving a stale twin behind.
     */
    fun paired(id: String, fingerprint: String) = mutate { list ->
        if (fingerprint.isBlank()) return@mutate
        val index = list.indexOfFirst { it.id == id }
        if (index < 0) return@mutate
        var host = list[index].copy(id = fingerprint, fingerprint = fingerprint, paired = true)
        // Any other record already claiming this identity is the same machine
        // seen twice — discovered before it was added by hand, or the reverse.
        // Keeping both leaves the user tapping the one that cannot stream, so
        // the survivor absorbs anything the duplicate knew and it is dropped.
        val duplicate = list.indexOfFirst { it.id != id && it.fingerprint.equals(fingerprint, true) }
        if (duplicate >= 0) {
            val other = list[duplicate]
            host = host.copy(
                alias = host.alias ?: other.alias,
                relayUrl = host.relayUrl ?: other.relayUrl,
                relayPin = host.relayPin ?: other.relayPin,
                wanEndpoint = host.wanEndpoint ?: other.wanEndpoint,
            )
        }
        list[index] = host
        if (duplicate >= 0) list.removeAt(duplicate)
    }

    /** Forget a host entirely — card, alias, cached relay, and identity. */
    fun forget(id: String) = mutate { list -> list.removeAll { it.id == id } }

    companion object {
        private const val TAG = "EchoHostStore"
        private const val PREFS = "echo_hosts"
        private const val KEY = "hosts"
        private const val KEY_MIGRATED = "legacy_adopted"

        /** How stale a sighting must be before it is worth re-recording. */
        private const val SEEN_REFRESH_MS = 60_000L

        // The pre-dashboard setup screen. Read once, never written.
        private const val LEGACY_PREFS = "echo"
        private const val LEGACY_HOST = "host"
        private const val LEGACY_HOST_FP = "host_fp"
        private const val LEGACY_RELAY_URL = "relay_url"
        private const val LEGACY_RELAY_PIN = "relay_pin"

        // Application context only — see EchoController.of for the same note.
        // There is nothing here to outlive the process.
        @SuppressLint("StaticFieldLeak")
        private var instance: HostStore? = null

        /**
         * The one store for this process.
         *
         * Process-scoped rather than remembered in a composable so the
         * dashboard, the settings sheet and any future background reconnect all
         * read the same list. A `remember`ed store would reload from disk on
         * every Activity recreation and drop any edit made in between.
         */
        @Synchronized
        fun of(context: Context): HostStore =
            instance ?: HostStore(context.applicationContext).also { instance = it }
    }
}
