package com.nova.echo

import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.net.InetSocketAddress
import java.net.Socket
import java.net.URI
import org.json.JSONObject

/**
 * The result of one reachability probe.
 *
 * [ms] is a TCP connect time, which is deliberately not called a "ping": it
 * includes the SYN/SYN-ACK round trip and nothing else, so it measures the path
 * rather than the service. That is the honest thing to show, because it is
 * exactly the question the user is asking when a card says OFFLINE — is the
 * machine there at all, or is it the app.
 */
data class ProbeResult(
    val reachable: Boolean,
    val millis: Long,
    /** What to show when it failed. Empty on success. */
    val detail: String = "",
) {
    companion object {
        val Unknown = ProbeResult(false, -1, "not probed")
    }
}

/**
 * What the relay knows about a host.
 *
 * [registered] is the load-bearing field: it means the relay is currently
 * holding candidates the host announced, so the host is alive and reachable
 * through it. Distinct from "the relay answered", which is a much weaker claim
 * and the one a plain TCP probe makes.
 */
data class RelayStatus(
    val registered: Boolean,
    /** Why not, when [registered] is false. Empty on success. */
    val detail: String = "",
    /** Where the relay says the host can be punched. Diagnostics only. */
    val candidates: List<String> = emptyList(),
)

/**
 * TCP reachability, used for the host card badge and the diagnostics sheet.
 *
 * A TCP connect rather than ICMP because Android gives an unprivileged app no
 * raw sockets, so the usual `ping` is not available at all; and rather than a
 * real Echo handshake because this must be safe to run against every cached
 * host whenever the dashboard opens. Opening and immediately closing a socket
 * costs the host nothing and cannot disturb a live session.
 */
object Probe {

    /** How long to wait before calling a host unreachable. */
    const val TIMEOUT_MS = 1_500

    suspend fun tcp(host: String, port: Int, timeoutMs: Int = TIMEOUT_MS): ProbeResult =
        withContext(Dispatchers.IO) {
            val started = System.nanoTime()
            try {
                Socket().use { socket ->
                    socket.connect(InetSocketAddress(host, port), timeoutMs)
                    ProbeResult(true, (System.nanoTime() - started) / 1_000_000)
                }
            } catch (e: Exception) {
                // Every failure mode is interesting to the user and none of them
                // should throw: an unresolvable name, a refused connection and a
                // silent timeout all mean "cannot reach it from here", and the
                // message is the only thing that separates them.
                ProbeResult(false, (System.nanoTime() - started) / 1_000_000, e.message ?: e.javaClass.simpleName)
            }
        }

    /** Probe a relay from its signalling URL. Null when the URL is unusable. */
    suspend fun relay(url: String?, timeoutMs: Int = TIMEOUT_MS): ProbeResult? {
        val (host, port) = relayEndpoint(url) ?: return null
        return tcp(host, port, timeoutMs)
    }

    /**
     * Ask the relay whether it can reach [host] right now.
     *
     * This is the question a card's badge should be answering, and a TCP probe
     * cannot: reaching the relay proves the relay is up, and a relay running
     * beside a switched-off Nova answers that probe perfectly while the host is
     * unreachable. `lookup` returns the candidates the host is announcing, and
     * Nova re-announces on a keepalive, so a stale registration ages out rather
     * than lingering as a false positive.
     *
     * Never throws: the native side reports an unreachable relay as
     * `registered = false` with a reason, because that is the ordinary
     * condition being detected rather than a fault.
     */
    suspend fun hostRegistered(host: KnownHost, filesDir: String): RelayStatus =
        withContext(Dispatchers.IO) {
            if (!host.hasRelay || host.fingerprint.isBlank()) {
                return@withContext RelayStatus(false, "no relay configured for this host")
            }
            val config = JSONObject()
                .put("identity_dir", filesDir)
                .put("relay_url", host.relayUrl)
                .put("relay_pin", host.relayPin)
                .put("host_fingerprint", host.fingerprint)
            runCatching {
                val answer = JSONObject(EchoNative.nativeRelayLookup(config.toString()))
                RelayStatus(
                    registered = answer.optBoolean("registered", false),
                    detail = answer.optString("detail"),
                    candidates = answer.optJSONArray("candidates")?.let { array ->
                        (0 until array.length()).map { array.optString(it) }
                    } ?: emptyList(),
                )
            }.getOrElse { RelayStatus(false, it.message ?: "relay lookup failed") }
        }

    /**
     * Split a relay URL into host and port.
     *
     * `URI` rather than string surgery so a path and a non-default port survive,
     * and the scheme decides the port only when the URL omits one — the relay
     * is commonly on 8443, which no default would guess.
     */
    fun relayEndpoint(url: String?): Pair<String, Int>? {
        if (url.isNullOrBlank()) return null
        val uri = runCatching { URI(url) }.getOrNull() ?: return null
        val host = uri.host ?: return null
        val port = if (uri.port > 0) uri.port else if (uri.scheme == "http") 80 else 443
        return host to port
    }
}
