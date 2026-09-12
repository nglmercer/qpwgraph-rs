package io.qpwgraph.relay

import android.app.Application
import android.net.wifi.WifiManager
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import org.json.JSONArray
import org.json.JSONObject

/**
 * Peer discovery: its own native engine, the multicast lock mDNS needs, and
 * the poll that replaces the whole peer snapshot each tick.
 *
 * The controller decides nothing about connecting. It hands each snapshot to
 * [onSnapshot] and the ViewModel — the only owner of the UI state and of the
 * client handle — decides what to do with it.
 */
internal class DiscoveryController(
    private val application: Application,
    private val scope: CoroutineScope,
    private val trusted: TrustedPeerRepository,
    private val onSnapshot: (Snapshot) -> Unit,
) {
    private val mutex = Mutex()
    private var handle = 0L
    private var polling: Job? = null
    private var multicastLock: WifiManager.MulticastLock? = null

    /** What a start attempt did, for the caller to turn into UI state. */
    sealed interface Started {
        /** Running. mDNS works only when [multicastAvailable]. */
        data class Ok(val multicastAvailable: Boolean) : Started

        data class Failed(val message: String?) : Started

        /** Already running; nothing changed. */
        data object AlreadyRunning : Started
    }

    /** One poll's worth of discovery output. */
    sealed interface Snapshot {
        /**
         * A completed poll. [message] is set when native also reported an
         * error event this tick — the peer list is still valid and must still
         * be published. A *blank* message means native gave no text and the
         * caller should substitute its own localised wording.
         */
        data class Peers(
            val peers: List<DiscoveredPeer>,
            val message: String?,
        ) : Snapshot

        /** The poll itself failed; the previous peer list still stands. */
        data class Failed(val message: String) : Snapshot
    }

    suspend fun start(deviceName: String, alreadyActive: () -> Boolean): Started =
        mutex.withLock {
            if (alreadyActive()) return@withLock Started.AlreadyRunning
            try {
                NativeRuntime.requireAvailable()
                val multicastAvailable = acquireMulticastLock()
                if (handle == 0L) {
                    handle = RelayJson.createdHandle(
                        NativeBridge.discoveryCreate(deviceName),
                    ) { "native discovery returned the null handle" }
                }
                val response = JSONObject(NativeBridge.discoveryStart(handle))
                if (response.optString("type") != "discovery_started") {
                    releaseMulticastLock()
                    return@withLock Started.Failed(response.optString("message"))
                }
                startPolling()
                Started.Ok(multicastAvailable)
            } catch (error: Exception) {
                releaseMulticastLock()
                Started.Failed(error.message)
            }
        }

    suspend fun stop() = mutex.withLock {
        stopPollingAndWait()
        if (handle != 0L) {
            NativeBridge.discoveryStop(handle)
        }
        releaseMulticastLock()
    }

    /**
     * Poll once outside the timer, for a caller that already knows the link
     * topology changed and should not wait a full tick to reflect it.
     */
    fun refreshNow() {
        if (handle != 0L) poll()
    }

    /** A USB link went away; drop the peers it carried. */
    fun usbLinkLost() {
        if (handle == 0L) return
        runCatching { NativeBridge.discoveryUsbLinkLost(handle) }
    }

    /** Release the native engine and the multicast lock, for `onCleared`. */
    suspend fun release() = mutex.withLock {
        stopPollingAndWait()
        val discovery = handle
        handle = 0L
        if (discovery != 0L) {
            runCatching { NativeBridge.discoveryRelease(discovery) }
        }
        releaseMulticastLock()
    }

    private fun startPolling() {
        polling?.cancel()
        polling = scope.launch(Dispatchers.IO) {
            while (isActive) {
                if (handle != 0L) poll()
                delay(DISCOVERY_POLL_INTERVAL_MS)
            }
        }
    }

    private suspend fun stopPollingAndWait() {
        polling?.cancelAndJoin()
        polling = null
    }

    /**
     * Replace the whole peer list from the native snapshot, then add the
     * durable last-known address of every trusted peer that did not advertise
     * it. That address is only a dial candidate: the trusted proof still has
     * to authenticate the stable identity.
     */
    private fun poll() {
        try {
            val eventMessage = consumeEvents(NativeBridge.discoveryPollEvents(handle))
            val advertised = RelayJson.discoveredPeers(NativeBridge.discoveryPeers(handle))
            val peers = advertised.toMutableList()
            for (stored in trusted.peersOrEmpty()) {
                if (stored.address.isNotBlank() &&
                    peers.none { it.id == stored.peerId && it.address == stored.address }
                ) {
                    peers += DiscoveredPeer(
                        id = stored.peerId,
                        name = stored.name,
                        address = stored.address,
                    )
                }
            }
            onSnapshot(Snapshot.Peers(peers, eventMessage))
        } catch (error: Exception) {
            onSnapshot(Snapshot.Failed(error.message ?: UNSPECIFIED_ERROR))
        }
    }

    /**
     * The last error native reported this tick, if any — the same one the
     * user would have been left looking at when each event overwrote the
     * message in turn.
     */
    private fun consumeEvents(raw: String): String? = try {
        val events = JSONArray(raw)
        (0 until events.length())
            .map { events.getJSONObject(it) }
            .lastOrNull { it.optString("type") == "error" }
            ?.optString("message")
            ?.ifBlank { UNSPECIFIED_ERROR }
    } catch (error: Exception) {
        error.message ?: UNSPECIFIED_ERROR
    }

    /**
     * Multicast is needed only by mDNS; direct USB probes remain usable if
     * Android refuses the lock or the device is on a non-Wi-Fi transport.
     */
    private fun acquireMulticastLock(): Boolean {
        if (multicastLock?.isHeld == true) return true
        val wifi = application.getSystemService(WifiManager::class.java) ?: return false
        var lock: WifiManager.MulticastLock? = null
        try {
            lock = wifi.createMulticastLock("qpwgraph-relay-discovery")
            lock.setReferenceCounted(false)
            lock.acquire()
            multicastLock = lock
            return true
        } catch (_: RuntimeException) {
            if (lock?.isHeld == true) runCatching { lock.release() }
            multicastLock = null
            return false
        }
    }

    private fun releaseMulticastLock() {
        multicastLock?.let { lock ->
            if (lock.isHeld) runCatching { lock.release() }
        }
        multicastLock = null
    }

    private companion object {
        const val DISCOVERY_POLL_INTERVAL_MS = 250L

        /** Native failed but said nothing; the caller supplies the wording. */
        const val UNSPECIFIED_ERROR = ""
    }
}
