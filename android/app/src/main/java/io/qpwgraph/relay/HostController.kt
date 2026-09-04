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
 * The host-side native handle and the poll that drains its events.
 *
 * Like [ClientController] this owns the handle and the rules for retiring it,
 * and no UI state. The host's rules are stricter than the client's because a
 * listener can outlive a failed start: a handle is released only when native
 * confirms it is no longer active, and keeping a still-running handle is
 * safer than losing ownership of it -- the next serialized operation can
 * retry the stop.
 */
internal class HostController(
    private val scope: CoroutineScope,
    private val service: RelayServiceController,
) {
    @Volatile private var handle = 0L
    private var polling: Job? = null

    /**
     * The configuration the live handle was prepared with, so edits made
     * while the host is stopped can be compared before a restart.
     */
    var preparedSettings: HostSettings? = null
        private set
    private var preparedDirection: AudioDirection? = null
    private var preparedGeneration: Long = 0L

    val nativeHandle: Long get() = handle

    val isOpen: Boolean get() = handle != 0L

    fun owns(candidate: Long): Boolean = candidate != 0L && candidate == handle

    /** Whether a restart would have to rebuild the native host. */
    fun preparedFor(
        host: HostSettings,
        direction: AudioDirection,
        generation: Long,
    ): Boolean = handle != 0L && preparedSettings == host &&
        preparedDirection == direction && preparedGeneration == generation

    /**
     * Create the native host if there is not one already, remembering the
     * settings it was built from.
     *
     * @throws IllegalStateException when native refused
     */
    fun open(
        host: HostSettings,
        direction: AudioDirection,
        generation: Long,
        deviceId: String,
        trustedCredentialsJson: String,
        nullHandleMessage: () -> String,
    ) {
        if (handle != 0L) return
        handle = RelayJson.createdHandle(
            NativeBridge.hostCreate(
                host.deviceName,
                deviceId,
                trustedCredentialsJson,
                host.pin,
                host.port,
                host.codec,
                host.transport,
                direction.serialized(),
                generation,
                host.sampleRate,
                host.channels,
                host.frameMs,
            ),
            nullHandleMessage,
        )
        preparedSettings = host
        preparedDirection = direction
        preparedGeneration = generation
    }

    fun start(): JSONObject = JSONObject(NativeBridge.hostStart(handle))

    fun disconnectSession(sessionId: Long) {
        if (handle == 0L) return
        NativeBridge.hostDisconnectSession(handle, sessionId)
    }

    fun offerDirection(
        sessionId: Long,
        direction: AudioDirection,
        generation: Long,
    ): JSONObject = JSONObject(
        NativeBridge.hostOfferDirection(handle, sessionId, direction.serialized(), generation),
    )

    fun removeTrustedPeer(peerId: String): Boolean =
        runCatching { NativeBridge.hostRemoveTrustedPeer(handle, peerId) }.getOrDefault(false)

    /** The secret behind an enrollment transaction, or "" when native declined. */
    fun enrollmentSecret(transactionId: Long): String =
        runCatching { JSONObject(NativeBridge.hostTrustedEnrollmentSecret(handle, transactionId)) }
            .getOrNull()
            ?.takeIf { it.optString("type") == "trusted_enrollment_secret" }
            ?.optString("secret")
            .orEmpty()

    fun acceptEnrollment(transactionId: Long): Boolean =
        runCatching { NativeBridge.hostAcceptTrustedEnrollment(handle, transactionId) }
            .getOrDefault(false)

    fun rejectEnrollment(transactionId: Long, reason: String) {
        runCatching { NativeBridge.hostRejectTrustedEnrollment(handle, transactionId, reason) }
    }

    fun startPolling(
        onEvents: (String) -> Unit,
        onStatus: (JSONObject) -> Unit,
        onError: (Exception) -> Unit,
    ) {
        polling?.cancel()
        polling = scope.launch(Dispatchers.IO) {
            while (isActive) {
                if (handle != 0L) {
                    try {
                        onEvents(NativeBridge.hostPollEvents(handle))
                        onStatus(JSONObject(NativeBridge.hostStatus(handle)))
                    } catch (error: Exception) {
                        onError(error)
                    }
                }
                delay(POLL_INTERVAL_MS)
            }
        }
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
     * Native reported the host inactive while the UI still believed it was
     * running. Give up ownership so a later explicit start imports the
     * latest trusted credentials, or report that we could not.
     */
    fun releaseInactive(): Boolean {
        if (handle == 0L) return true
        val released = runCatching { NativeBridge.hostRelease(handle) }.getOrDefault(false)
        if (!released) return false
        clear()
        return true
    }

    /**
     * Stop a host and release its handle only when native confirms it is no
     * longer active. `hostRelease` returns false when a stale caller races a
     * still-running native host; that is a failed release, not merely "the
     * JNI call returned".
     */
    fun stopAndRelease(target: Long = handle): Boolean {
        val stopped = runCatching {
            JSONObject(NativeBridge.hostStop(target)).optString("type") == "host_stopped"
        }.getOrDefault(false)
        val inactive = runCatching {
            !JSONObject(NativeBridge.hostStatus(target)).optBoolean("host_active", true)
        }.getOrDefault(false)
        if (!stopped && !inactive) return false
        val released = runCatching { NativeBridge.hostRelease(target) }.getOrDefault(false)
        if (!released) return false
        if (handle == target) clear()
        return true
    }

    /**
     * The strict stop used by an explicit user action: any native refusal is
     * an error the caller must see, rather than something to retry later.
     *
     * @throws IllegalStateException when native refused the stop or the release
     */
    fun stopAndReleaseOrThrow() {
        if (handle == 0L) return
        val target = handle
        val response = JSONObject(NativeBridge.hostStop(target))
        if (response.optString("type") == "error") {
            throw IllegalStateException(response.optString("message"))
        }
        // Recreate the prepared host next time so credentials enrolled by a
        // client during this run are read from private preferences.
        if (!NativeBridge.hostRelease(target)) {
            throw IllegalStateException("native relay host handle is still active")
        }
        clear()
    }

    /** Stop polling, quiesce the audio service, then stop and release. */
    suspend fun quiesceAndStop() {
        stopPollingAndWait()
        service.stopAndWait()
        stopAndReleaseOrThrow()
    }

    private fun clear() {
        handle = 0L
        preparedSettings = null
        preparedDirection = null
        preparedGeneration = 0L
    }

    private companion object {
        const val POLL_INTERVAL_MS = 100L
    }
}
