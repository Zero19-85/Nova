package com.nova.echo

import android.content.Context
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.util.Log

/**
 * Watches which network the device is actually using, and tells the engine when
 * it changes.
 *
 * ## Why this is not just "listen for disconnects"
 *
 * The handover case that produces a black screen is not a disconnect. Walking
 * out of Wi-Fi range onto 5G, or a VPN coming up, hands the process a *different
 * default network* while the old one may still linger for seconds. Nothing
 * errors. The session's UDP socket keeps accepting `send` calls and its
 * datagrams go to an interface that no longer routes to the host. Every layer
 * reports health, and the picture freezes.
 *
 * So the signal that matters is [ConnectivityManager.NetworkCallback.onAvailable]
 * / [onLost] on the **default** network — "which network are we on" — rather
 * than "is there internet". `registerDefaultNetworkCallback` answers exactly
 * that question and is the one callback whose semantics match the failure.
 *
 * ## `bindProcessToNetwork` is the load-bearing line
 *
 * Raising the engine's epoch makes it rebind a socket. Rebinding a socket does
 * **not**, on its own, put it on the new network: a fresh UDP socket inherits
 * the process's network binding, and a process still bound to a departed network
 * binds its new socket to the departed network too. That reconnect then fails in
 * exactly the way the old one did, forever, and reads as "the host is
 * unreachable" rather than "we are dialling out of a dead interface".
 *
 * Binding the process to the new default before raising the epoch is what makes
 * the rebind land somewhere real. It is ordered that way on purpose.
 *
 * ## Deliberately not held to the session's lifetime
 *
 * Registered for as long as the app can hold a session, not for as long as a
 * session exists. A session that is *between* attempts is precisely the one that
 * most needs to hear about the next network change, and a monitor scoped to a
 * live session would be unregistered at exactly that moment.
 */
class NetworkMonitor private constructor(private val context: Context) {

    private val cm =
        context.getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager

    /**
     * The last default network we told the engine about.
     *
     * Guards against the callback storm Android produces around a transition:
     * `onAvailable`, several `onCapabilitiesChanged`, `onLinkPropertiesChanged`
     * and `onLost` can all fire for one walk between rooms. Raising the epoch on
     * each would abandon attempts that were about to succeed — the epoch is
     * cheap, but an attempt in flight when it moves is thrown away, so an epoch
     * raised every 200 ms is a session that can never finish connecting.
     */
    private var current: Network? = null

    private val callback = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) {
            onDefaultNetwork(network, "available")
        }

        /**
         * A transport change on the *same* [Network] handle — Wi-Fi dropping to
         * cellular under a bonded connection, or a VPN attaching. The handle is
         * unchanged, so [onAvailable] never fires, but the path underneath it is
         * a different one and the NAT mapping is gone all the same.
         */
        override fun onCapabilitiesChanged(network: Network, caps: NetworkCapabilities) {
            val transport = transportOf(caps)
            if (network != current || transport == lastTransport) return
            lastTransport = transport
            // Re-bound as well as re-raised, even though the handle is the same
            // one the process is already bound to. The binding is cheap and
            // idempotent, and doing it unconditionally means there is exactly
            // one code path that can leave the process bound to a network — see
            // the ordering note in [onDefaultNetwork].
            runCatching { cm.bindProcessToNetwork(network) }
            val epoch = runCatching { EchoNative.nativeNetworkChanged() }.getOrDefault(0L)
            Log.i(TAG, "default network transport → $transport, epoch $epoch")
        }

        /**
         * Losing the default is reported but does **not** raise the epoch.
         *
         * There is nothing to reconnect *to* yet. Raising here would spend an
         * attempt on an interfaceless device and then have to spend another one
         * when the replacement arrives moments later, which is one extra
         * reconnect during the exact window the user is watching a frozen
         * picture. The arrival of the replacement is the actionable event.
         */
        override fun onLost(network: Network) {
            if (network == current) {
                Log.i(TAG, "default network lost — waiting for its replacement")
                current = null
            }
        }
    }

    private var lastTransport: String = "?"
    private var registered = false

    fun start() {
        if (registered) return
        val request = NetworkRequest.Builder()
            .addCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
            .build()
        runCatching { cm.registerDefaultNetworkCallback(callback) }
            .onFailure {
                // A device that refuses the registration still streams; it just
                // falls back to the engine's stall watchdog, which is seconds
                // rather than milliseconds. Never fatal.
                Log.w(TAG, "could not register the default-network callback", it)
                return
            }
        registered = true
        // Whatever is already active counts as a change: the app may have been
        // started, or resumed, on a different network from the one the last
        // session used.
        cm.activeNetwork?.let { onDefaultNetwork(it, "startup") }
    }

    fun stop() {
        if (!registered) return
        runCatching { cm.unregisterNetworkCallback(callback) }
        registered = false
        current = null
    }

    private fun onDefaultNetwork(network: Network, why: String) {
        if (network == current) return
        current = network
        lastTransport = cm.getNetworkCapabilities(network)?.let(::transportOf) ?: "?"

        // ORDER MATTERS. Bind first, raise second — see the class comment. A
        // socket opened by an epoch raised before this line inherits the old
        // binding and dials out of an interface that is gone.
        val bound = runCatching { cm.bindProcessToNetwork(network) }.getOrDefault(false)
        if (!bound) {
            // Not fatal: without an explicit binding the kernel routes by its own
            // default, which is usually the same network. Worth logging because
            // when it is *not* the same network this is the reason a reconnect
            // loop never converges.
            Log.w(TAG, "bindProcessToNetwork refused — sockets will follow the system default")
        }

        val epoch = runCatching { EchoNative.nativeNetworkChanged() }.getOrDefault(0L)
        Log.i(TAG, "default network → $lastTransport ($why), epoch $epoch")
    }

    private fun transportOf(caps: NetworkCapabilities): String = when {
        caps.hasTransport(NetworkCapabilities.TRANSPORT_WIFI) -> "wifi"
        caps.hasTransport(NetworkCapabilities.TRANSPORT_CELLULAR) -> "cellular"
        caps.hasTransport(NetworkCapabilities.TRANSPORT_ETHERNET) -> "ethernet"
        caps.hasTransport(NetworkCapabilities.TRANSPORT_VPN) -> "vpn"
        else -> "other"
    }

    companion object {
        private const val TAG = "EchoNet"

        @Volatile
        private var instance: NetworkMonitor? = null

        /**
         * Process-scoped, like [EchoController], and for the same reason: an
         * Activity teardown the process survives must not stop a session from
         * hearing about the network.
         */
        fun of(context: Context): NetworkMonitor =
            instance ?: synchronized(this) {
                instance ?: NetworkMonitor(context.applicationContext).also { instance = it }
            }
    }
}
