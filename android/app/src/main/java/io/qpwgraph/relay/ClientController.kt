package io.qpwgraph.relay

import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import org.json.JSONObject

/**
 * The client-side native handle and the poll that drains its events.
 *
 * This owns the handle and every rule about when it may be invalidated. It
 * owns no UI state: the ViewModel is the only thing that mutates the state
 * flow, and it serializes calls into here with its operation mutex.
 *
 * The one ordering rule that matters: the foreground service pumps audio
 * through this same handle, so it must be stopped and awaited before the
 * handle is disconnected or released. [quiesceAndRelease] is the only correct
 * teardown; [releaseNow] exists for the paths that have already stopped the
 * service themselves.
 */
internal class ClientController(
    private val scope: CoroutineScope,
    private val service: RelayServiceController,
) {
    @Volatile private var handle = 0L
    private var polling: Job? = null

    /** 0 when no native client is open. */
    val nativeHandle: Long get() = handle

    val isOpen: Boolean get() = handle != 0L

    /** Whether an event refers to the handle this controller currently owns. */
    fun owns(candidate: Long): Boolean = candidate != 0L && candidate == handle

    /**
     * Create the native client if there is not one already.
     *
     * @throws IllegalStateException when native refused
     */
    fun open(
        settings: RelaySettings,
        deviceId: String,
        trustedCredentialsJson: String,
        nullHandleMessage: () -> String,
    ) {
        if (handle != 0L) return
        handle = RelayJson.createdHandle(
            NativeBridge.create(
                settings.deviceName,
                deviceId,
                trustedCredentialsJson,
                settings.direction.serialized(),
                settings.directionGeneration,
                settings.codec,
                settings.transport,
                settings.sampleRate,
                settings.channels,
                settings.frameMs,
            ),
            nullHandleMessage,
        )
    }

    fun connect(target: String, pin: String): JSONObject =
        JSONObject(NativeBridge.connect(handle, target, pin))

    fun openMode(
        settings: RelaySettings,
        deviceId: String,
        trustedCredentialsJson: String,
        nullHandleMessage: () -> String,
    ) {
        if (handle != 0L) return
        handle = RelayJson.createdHandle(
            NativeBridge.createMode(
                settings.deviceName,
                deviceId,
                trustedCredentialsJson,
                settings.mode.serialized(),
                settings.modeGeneration,
                settings.codec,
                settings.transport,
                settings.sampleRate,
                settings.channels,
                settings.frameMs,
            ),
            nullHandleMessage,
        )
    }

    fun connectTrusted(target: String, peer: TrustedRelayPeer): JSONObject =
        JSONObject(NativeBridge.connectTrusted(handle, target, peer.peerId, peer.secret))

    fun offerDirection(
        sessionId: Long,
        direction: AudioDirection,
        generation: Long,
    ): JSONObject = JSONObject(
        NativeBridge.offerDirection(handle, sessionId, direction.serialized(), generation),
    )

    fun offerMode(sessionId: Long, mode: RelayMode, generation: Long): JSONObject =
        JSONObject(NativeBridge.offerMode(handle, sessionId, mode.serialized(), generation))

    /**
     * Native told us the handle is unknown. Drop it without calling back into
     * native, which would only fail the same way.
     */
    fun forgetHandle() {
        handle = 0L
    }

    /**
     * The credential the host acknowledged, if any. Deliberately separate
     * from normal connection/status JSON: it returns a secret only to the
     * persistence path, and only after the host acknowledged it.
     */
    fun trustedCredential(): JSONObject? =
        runCatching { JSONObject(NativeBridge.clientTrustedPeer(handle)) }
            .getOrNull()
            ?.takeIf { it.optString("type") == "trusted_peer" }

    fun removeTrustedPeer(peerId: String): Boolean =
        runCatching { NativeBridge.removeTrustedPeer(handle, peerId) }.getOrDefault(false)

    /**
     * Hand the connected client a discovery snapshot so its resume worker can
     * authenticate the same host at a new address instead of retrying the
     * original IP forever.
     */
    fun updatePeers(peersJson: String) {
        if (handle == 0L) return
        NativeBridge.updateClientPeers(handle, peersJson)
    }

    /**
     * Drain native events on a fixed cadence.
     *
     * [onEvents] returns false to end the loop -- the handle is gone and the
     * caller has already retired it. Status is only reported when native
     * answered with one.
     */
    fun startPolling(
        onEvents: suspend (String) -> Boolean,
        onStatus: (JSONObject) -> Unit,
        onError: (Exception) -> Unit,
    ) {
        polling?.cancel()
        polling = scope.launch(Dispatchers.IO) {
            while (isActive) {
                if (handle != 0L) {
                    try {
                        if (!onEvents(NativeBridge.pollEvents(handle))) break
                        status()?.let(onStatus)
                    } catch (error: Exception) {
                        onError(error)
                    }
                }
                delay(POLL_INTERVAL_MS)
            }
        }
    }

    private fun status(): JSONObject? {
        val open = handle
        if (open == 0L) return null
        return runCatching { JSONObject(NativeBridge.clientStatus(open)) }
            .getOrNull()
            ?.takeIf { it.optString("type") != "error" }
    }

    fun cancelPolling() {
        polling?.cancel()
        polling = null
    }

    suspend fun stopPollingAndWait() {
        polling?.cancelAndJoin()
        polling = null
    }

    /**
     * Disconnect and release without stopping the service first. Only for
     * callers that have already quiesced the platform workers.
     */
    fun releaseNow() {
        val open = handle
        handle = 0L
        if (open == 0L) return
        runCatching { NativeBridge.disconnect(open) }
        runCatching { NativeBridge.release(open) }
    }

    /** Stop polling, quiesce the audio service, then retire the handle. */
    suspend fun quiesceAndRelease() {
        stopPollingAndWait()
        service.stopAndWait()
        releaseNow()
    }

    private companion object {
        const val POLL_INTERVAL_MS = 100L
    }
}
