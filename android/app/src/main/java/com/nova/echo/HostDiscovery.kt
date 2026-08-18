package com.nova.echo

import android.content.Context
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.os.Build
import android.os.ext.SdkExtensions
import android.util.Log
import androidx.annotation.ChecksSdkIntAtLeast
import androidx.annotation.RequiresExtension
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import java.net.Inet4Address
import java.net.URI
import java.util.concurrent.Executor
import java.util.ArrayDeque

/**
 * A Nova host found on the LAN, assembled from its `_echo._tcp` record.
 *
 * ## [fingerprint] is a hint, never a credential
 *
 * mDNS is unauthenticated — anything on this network can advertise
 * `_echo._tcp` and claim any fingerprint. So this value may be used to
 * *recognise* a host the user has already paired with (does it match the one we
 * stored?) and never to *establish* trust. The fingerprint Echo persists comes
 * from the PIN handshake, where the host proves possession of the certificate's
 * private key. See `echo-client/src/pairing.rs` and Nova's
 * `nova-server/src/echo/discovery.rs`, which states the same rule from the
 * other side.
 *
 * Concretely: the setup screen fills the address and the relay fields from a
 * discovered host, and deliberately does **not** fill Nova's fingerprint. That
 * field is written only by a completed pairing.
 */
data class DiscoveredHost(
    /** Human-readable machine name from the TXT `name` key. */
    val name: String,
    /** IPv4 literal. */
    val address: String,
    /** Echo's control port, from the SRV record. */
    val port: Int,
    /** TXT `fp` — for recognition only. See the class doc. */
    val fingerprint: String,
    /** TXT `relay`, absent when the host has no WAN signalling configured. */
    val relayUrl: String?,
    /** TXT `relaypin`, present exactly when [relayUrl] is. */
    val relayPin: String?,
) {
    /** True when this host advertises a usable relay. */
    val hasRelay: Boolean get() = !relayUrl.isNullOrBlank() && !relayPin.isNullOrBlank()
}

/**
 * Browses the LAN for Nova hosts advertising `_echo._tcp`.
 *
 * ## Why `NsdManager` rather than an mDNS implementation in Rust
 *
 * The rest of Echo's networking lives in Rust, and symmetry with the host —
 * which advertises through the `mdns-sd` crate — would be the tidier-looking
 * choice. It is the wrong one here.
 *
 * Receiving multicast on Android is not simply a matter of binding a socket:
 * Wi-Fi hardware filters multicast when the interface is dozing, and an app
 * that wants those packets has to hold a `WifiManager.MulticastLock` across the
 * whole browse. A Rust listener would therefore need a Kotlin dependency
 * anyway, and would still be responsible for Doze and network-change handling
 * that the platform already solves. `NsdManager` delegates to the system's own
 * mDNS daemon, so none of that is this app's problem.
 *
 * The trade is worth making because discovery is not on the latency path: it
 * runs once, before a session, driven by a human looking at a list. The parts
 * where Rust symmetry genuinely matters — the pairing handshake and the session
 * itself — are untouched.
 *
 * ## The API 26–33 resolver bug, and why there is a queue
 *
 * Before API 34 the platform resolver handles **one** request at a time. A
 * second `resolveService` while one is outstanding fails with
 * `FAILURE_ALREADY_ACTIVE`, and on several releases it does worse than fail —
 * it wedges the resolver so that later requests never call back at all. With
 * two Nova machines on a network, or one machine seen on both Wi-Fi and
 * Ethernet, that is not a rare case; it is the normal one.
 *
 * So below API 34 every resolve goes through [pending], strictly one in flight.
 * From API 34 `registerServiceInfoCallback` supersedes `resolveService`
 * entirely: it is concurrent, and it keeps delivering updates when a host's
 * address or TXT record changes rather than answering once and going quiet.
 *
 * ## Threading
 *
 * Platform callbacks arrive on binder threads. All mutable state is guarded by
 * [lock], and results are published through a [StateFlow] that Compose can
 * collect directly.
 */
class HostDiscovery(context: Context) {

    private val appContext = context.applicationContext
    private val nsd = appContext.getSystemService(Context.NSD_SERVICE) as NsdManager

    private val _hosts = MutableStateFlow<List<DiscoveredHost>>(emptyList())

    /** Hosts currently visible, newest resolution last. */
    val hosts: StateFlow<List<DiscoveredHost>> = _hosts.asStateFlow()

    private val _browsing = MutableStateFlow(false)

    /** True while a browse is active — the UI's "searching…" state. */
    val browsing: StateFlow<Boolean> = _browsing.asStateFlow()

    private val lock = Any()

    /** Resolved hosts keyed by mDNS service name, so a re-announcement updates
     *  rather than duplicates. */
    private val found = LinkedHashMap<String, DiscoveredHost>()

    /** Services seen but not yet resolved. Pre-API-34 only. */
    private val pending = ArrayDeque<NsdServiceInfo>()

    /** Whether a `resolveService` is outstanding. Pre-API-34 only. */
    private var resolving = false

    /** Live per-service callbacks so they can be unregistered. API 34+ only. */
    private val serviceCallbacks = HashMap<String, NsdManager.ServiceInfoCallback>()

    private var discoveryListener: NsdManager.DiscoveryListener? = null

    /**
     * Runs each `ServiceInfoCallback` on whichever thread the framework
     * delivers it on.
     *
     * The obvious alternative, `Context.mainExecutor`, would put these on the
     * main thread — which is both an API-28 call this class does not otherwise
     * need, and the one arrangement that lets publishing a [StateFlow] value
     * resume a Compose collector inline. A direct executor keeps this path on a
     * binder thread, exactly like the pre-API-34 `ResolveListener`, so both
     * resolution paths have the same threading and everything downstream is
     * already written to be thread-safe.
     */
    private val callbackExecutor = Executor { it.run() }

    /**
     * Begin browsing. Idempotent — calling it while already browsing does
     * nothing, which lets a Compose effect call it without tracking state.
     */
    fun start() {
        synchronized(lock) {
            if (discoveryListener != null) return
            val listener = newDiscoveryListener()
            discoveryListener = listener
            try {
                nsd.discoverServices(SERVICE_TYPE, NsdManager.PROTOCOL_DNS_SD, listener)
            } catch (e: IllegalArgumentException) {
                // Thrown when a listener is somehow still registered. Recoverable
                // by dropping ours; the next start() gets a fresh one.
                Log.w(TAG, "discoverServices refused: ${e.message}")
                discoveryListener = null
                return
            }
        }
        _browsing.value = true
    }

    /**
     * Throw away what was found and browse again from scratch.
     *
     * For the "Search again" control. Distinct from [stop] + [start] because it
     * clears [hosts]: a manual rescan is the user saying the list is wrong, and
     * keeping stale entries through it would answer the wrong question — they
     * would have no way to tell a host that is still there from one that was
     * found ten minutes ago and has since gone.
     */
    fun restart() {
        stop()
        synchronized(lock) { found.clear() }
        _hosts.value = emptyList()
        start()
    }

    /**
     * Stop browsing and release every platform callback.
     *
     * Results are kept: a user who backgrounds the app and returns should not
     * watch the list rebuild itself. They are replaced wholesale by the next
     * browse.
     */
    fun stop() {
        val listener: NsdManager.DiscoveryListener?
        val callbacks: List<NsdManager.ServiceInfoCallback>
        synchronized(lock) {
            listener = discoveryListener
            discoveryListener = null
            callbacks = serviceCallbacks.values.toList()
            serviceCallbacks.clear()
            pending.clear()
            resolving = false
        }
        listener?.let {
            try {
                nsd.stopServiceDiscovery(it)
            } catch (e: IllegalArgumentException) {
                // Already stopped by the framework (commonly after a network
                // change). Nothing to undo.
                Log.w(TAG, "stopServiceDiscovery: ${e.message}")
            }
        }
        if (canUseServiceInfoCallback()) callbacks.forEach { unregister(it) }
        _browsing.value = false
    }

    // ── Discovery ───────────────────────────────────────────────────────────

    private fun newDiscoveryListener() = object : NsdManager.DiscoveryListener {
        override fun onDiscoveryStarted(serviceType: String) {
            Log.i(TAG, "browsing $serviceType")
        }

        override fun onServiceFound(service: NsdServiceInfo) {
            if (service.serviceType?.contains("_echo") != true) return
            if (canUseServiceInfoCallback()) registerCallback(service) else enqueue(service)
        }

        override fun onServiceLost(service: NsdServiceInfo) {
            val name = service.serviceName ?: return
            if (canUseServiceInfoCallback()) {
                synchronized(lock) { serviceCallbacks.remove(name) }?.let { unregister(it) }
            }
            forget(name)
        }

        override fun onDiscoveryStopped(serviceType: String) {
            _browsing.value = false
        }

        override fun onStartDiscoveryFailed(serviceType: String, errorCode: Int) {
            Log.w(TAG, "discovery failed to start: $errorCode")
            synchronized(lock) { discoveryListener = null }
            _browsing.value = false
        }

        override fun onStopDiscoveryFailed(serviceType: String, errorCode: Int) {
            Log.w(TAG, "discovery failed to stop: $errorCode")
            _browsing.value = false
        }
    }

    // ── Resolution, API 34+ ─────────────────────────────────────────────────

    /**
     * Whether `registerServiceInfoCallback` may be called.
     *
     * Two conditions rather than one, and the second is not pedantry. The method
     * arrived in API 34, but it is *also* delivered through the Tiramisu
     * extension SDK, and the platform annotates it as needing T-extension 7 —
     * so an API-level test alone is not the guard the platform documents, and a
     * device could satisfy one condition without the other. Checking both is
     * what makes the call safe and what static analysis can verify; anything
     * that fails either test takes the queued `resolveService` path, which works
     * everywhere.
     */
    @ChecksSdkIntAtLeast(api = 7, extension = Build.VERSION_CODES.TIRAMISU)
    private fun canUseServiceInfoCallback(): Boolean =
        // `getExtensionVersion` itself is API 30, so it needs its own floor
        // before it can be asked anything.
        Build.VERSION.SDK_INT >= Build.VERSION_CODES.R &&
            SdkExtensions.getExtensionVersion(Build.VERSION_CODES.TIRAMISU) >= 7

    /** Unregister one callback, tolerating a framework that already dropped it. */
    @RequiresExtension(extension = Build.VERSION_CODES.TIRAMISU, version = 7)
    private fun unregister(callback: NsdManager.ServiceInfoCallback) {
        try {
            nsd.unregisterServiceInfoCallback(callback)
        } catch (e: IllegalArgumentException) {
            Log.w(TAG, "unregisterServiceInfoCallback: ${e.message}")
        }
    }

    @RequiresExtension(extension = Build.VERSION_CODES.TIRAMISU, version = 7)
    private fun registerCallback(service: NsdServiceInfo) {
        val name = service.serviceName ?: return
        val callback = object : NsdManager.ServiceInfoCallback {
            override fun onServiceUpdated(info: NsdServiceInfo) = publish(name, info)

            override fun onServiceLost() = forget(name)

            override fun onServiceInfoCallbackRegistrationFailed(errorCode: Int) {
                Log.w(TAG, "callback registration failed for $name: $errorCode")
                synchronized(lock) { serviceCallbacks.remove(name) }
            }

            override fun onServiceInfoCallbackUnregistered() {
                synchronized(lock) { serviceCallbacks.remove(name) }
            }
        }
        synchronized(lock) {
            // Already watching this one. Re-registering the same service is an
            // error, not a refresh.
            if (serviceCallbacks.containsKey(name)) return
            serviceCallbacks[name] = callback
        }
        try {
            nsd.registerServiceInfoCallback(service, callbackExecutor, callback)
        } catch (e: IllegalArgumentException) {
            Log.w(TAG, "registerServiceInfoCallback: ${e.message}")
            synchronized(lock) { serviceCallbacks.remove(name) }
        }
    }

    // ── Resolution, API 26–33: strictly one at a time ───────────────────────

    private fun enqueue(service: NsdServiceInfo) {
        synchronized(lock) {
            val name = service.serviceName ?: return
            // Skip one already queued or already resolved — a browse re-announces
            // the same service repeatedly, and each announcement would otherwise
            // add another resolve to a queue that drains one at a time.
            if (pending.any { it.serviceName == name }) return
            if (found.containsKey(name)) return
            pending.addLast(service)
        }
        drain()
    }

    private fun drain() {
        val next: NsdServiceInfo = synchronized(lock) {
            if (resolving) return
            val head = pending.pollFirst() ?: return
            resolving = true
            head
        }
        @Suppress("DEPRECATION")
        nsd.resolveService(next, object : NsdManager.ResolveListener {
            override fun onServiceResolved(info: NsdServiceInfo) {
                publish(info.serviceName ?: next.serviceName ?: return, info)
                finish()
            }

            override fun onResolveFailed(info: NsdServiceInfo, errorCode: Int) {
                // Includes FAILURE_ALREADY_ACTIVE (3), which is what the queue
                // exists to avoid — log it as the signal it is rather than
                // retrying into the same wall.
                Log.w(TAG, "resolve failed for ${info.serviceName}: $errorCode")
                finish()
            }

            private fun finish() {
                synchronized(lock) { resolving = false }
                drain()
            }
        })
    }

    // ── Publishing ──────────────────────────────────────────────────────────

    // Both of these publish the new list OUTSIDE the lock. On API 34+ the
    // platform callbacks arrive on the main executor, which is also where
    // Compose collects, so assigning a StateFlow's value there can resume a
    // collector inline — running recomposition while this lock is held. It is
    // reentrant and would not deadlock today, but holding an internal lock
    // across arbitrary UI code is a trap to leave un-set.

    private fun publish(name: String, info: NsdServiceInfo) {
        val host = info.toDiscoveredHost() ?: return
        val snapshot = synchronized(lock) {
            found[name] = host
            found.values.toList()
        }
        _hosts.value = snapshot
    }

    private fun forget(name: String) {
        val snapshot = synchronized(lock) {
            if (found.remove(name) == null) return
            found.values.toList()
        }
        _hosts.value = snapshot
    }

    private companion object {
        const val TAG = "EchoDiscovery"

        /** Must match `nova-server/src/echo/discovery.rs::SERVICE_TYPE`.
         *  `NsdManager` wants it without the trailing `.local.` the Rust side
         *  includes. */
        const val SERVICE_TYPE = "_echo._tcp"
    }
}

/**
 * Convert a resolved service to a [DiscoveredHost], or null when it is missing
 * something a client cannot proceed without.
 *
 * A record with no address or no fingerprint is dropped rather than shown with
 * blanks: offering a host that cannot be paired with is worse than showing
 * nothing, because the failure arrives later and further from its cause.
 */
private fun NsdServiceInfo.toDiscoveredHost(): DiscoveredHost? {
    val address = ipv4Literal() ?: return null
    val txt = attributes ?: emptyMap()

    fun str(key: String): String? =
        txt[key]?.let { String(it, Charsets.UTF_8) }?.trim()?.takeIf { it.isNotEmpty() }

    val fingerprint = str("fp") ?: return null
    val relayUrl = str("relay")
    val relayPin = str("relaypin")

    return DiscoveredHost(
        name = str("name") ?: serviceName ?: "Nova",
        address = address,
        port = port,
        fingerprint = fingerprint,
        // Both or neither. The host only ever advertises the pair, but a
        // spoofed or truncated record could carry one, and a relay URL without
        // its pin is not usable — the pin is what authenticates the relay.
        relayUrl = if (relayUrl != null && relayPin != null) relayUrl.reachableFrom(address) else null,
        relayPin = if (relayUrl != null && relayPin != null) relayPin else null,
    )
}

/**
 * Point a loopback relay URL at the host that advertised it.
 *
 * `[echo.signaling] url` is written from the PC's point of view, where
 * `https://127.0.0.1:8443/...` is correct — the relay usually runs on that same
 * machine. Read on a phone, `127.0.0.1` is the *phone*, so the connection went
 * to its own loopback and came back `Connection refused (os error 111)` (live
 * 2026-08-18).
 *
 * Nova rewrites this before advertising now, so on a current host this is a
 * no-op. It stays because the app has to work against hosts it did not ship
 * with: an older build, or one whose config was hand-edited after install. A
 * loopback address is never meaningful to a recipient on another machine, so
 * there is no reading under which passing it through unchanged is right.
 *
 * Safe with respect to TLS: the relay is authenticated by certificate
 * fingerprint (`relaypin`), never by hostname, so moving the authority cannot
 * turn a verified connection into an unverified one.
 */
private val LOOPBACK_V4 = Regex("""^127\.\d{1,3}\.\d{1,3}\.\d{1,3}$""")

private fun String.reachableFrom(address: String): String {
    val uri = runCatching { URI(this) }.getOrNull() ?: return this
    val host = uri.host ?: return this
    // Literal comparison, never `InetAddress.getByName`: this runs on a binder
    // callback thread, and resolving an arbitrary hostname there would put a
    // DNS round trip — and its timeout — inside service resolution.
    val loopback = host.equals("localhost", ignoreCase = true) ||
        host == "::1" || host == "[::1]" ||
        // 127.0.0.0/8 in its entirety, not just 127.0.0.1.
        LOOPBACK_V4.matches(host)
    if (!loopback) return this

    // Rebuild rather than string-replace: the port and path carry the
    // signalling endpoint, and losing either reads from the far end as though
    // the rewrite had not happened at all.
    return runCatching {
        URI(uri.scheme, uri.userInfo, address, uri.port, uri.path, uri.query, uri.fragment).toString()
    }.getOrDefault(this)
}

/**
 * The service's IPv4 address as a literal.
 *
 * IPv4 only, deliberately: Nova binds its listeners to `0.0.0.0` and advertises
 * an IPv4 address, so a link-local IPv6 answer — which Android will happily
 * supply alongside it — would resolve to a host that is not listening there.
 */
@Suppress("DEPRECATION")
private fun NsdServiceInfo.ipv4Literal(): String? =
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
        // `hostAddresses` is plain API 34 — no extension requirement, unlike
        // the ServiceInfoCallback family — so the API-level test is the whole
        // guard here.
        hostAddresses.filterIsInstance<Inet4Address>().firstOrNull()?.hostAddress
    } else {
        (host as? Inet4Address)?.hostAddress
    }
