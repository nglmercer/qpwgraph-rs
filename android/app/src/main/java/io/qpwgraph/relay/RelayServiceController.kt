package io.qpwgraph.relay

import android.app.Application
import android.content.Intent
import java.util.UUID
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull

/**
 * Starting and stopping the foreground audio service, and waiting for it.
 *
 * The service pumps audio through the same native handle the ViewModel owns,
 * so the ordering rules here are safety-critical: nothing may invalidate a
 * handle while a service worker can still be inside a JNI call.
 */
class RelayServiceController(private val application: Application) {

    /**
     * Start the service and wait until it reports every requested worker
     * initialised. The caller supplies an immutable settings snapshot as
     * [geometry] so a concurrent UI edit cannot make the service geometry
     * disagree with the native handle it is pumping.
     *
     * @throws RelayServiceStartException when the service refused to start
     * @throws IllegalStateException when it never reported readiness
     */
    suspend fun start(
        mode: String,
        handle: Long,
        role: String,
        geometry: AudioGeometry,
        captureSource: CaptureSource = CaptureSource.MICROPHONE,
        mediaProjectionResultCode: Int = android.app.Activity.RESULT_CANCELED,
        mediaProjectionData: android.content.Intent? = null,
    ) {
        NativeRuntime.requireAvailable()
        require(handle != 0L) { "relay audio service requires a live native handle" }
        require(isOneWayAudioRole(role)) {
            "relay audio service accepts one-way Emitter or Receiver roles only"
        }
        validateAndroidAudioGeometry(geometry)
        val token = UUID.randomUUID().toString()
        val ready = RelayServiceBridge.registerStart(token)
        val intent = Intent(application, RelayService::class.java)
            .putExtra(RelayService.EXTRA_MODE, mode)
            .putExtra(RelayService.EXTRA_HANDLE, handle)
            .putExtra(RelayService.EXTRA_ROLE, role)
            .putExtra(RelayService.EXTRA_SAMPLE_RATE, geometry.sampleRate)
            .putExtra(RelayService.EXTRA_CHANNELS, geometry.channels)
            .putExtra(RelayService.EXTRA_FRAME_MS, geometry.frameMs)
            .putExtra(RelayService.EXTRA_START_TOKEN, token)
            .putExtra(RelayService.EXTRA_CAPTURE_SOURCE, captureSource.name.lowercase())
            .putExtra(RelayService.EXTRA_MEDIA_PROJECTION_RESULT_CODE, mediaProjectionResultCode)
            .putExtra(RelayService.EXTRA_MEDIA_PROJECTION_DATA, mediaProjectionData)
        try {
            application.startForegroundService(intent)
            val result = withTimeoutOrNull(SERVICE_START_TIMEOUT_MS) { ready.await() }
                ?: throw IllegalStateException("relay audio service did not become ready")
            if (!result.started) {
                val alreadyActive = result.message.contains("already active")
                throw RelayServiceStartException(result.message, alreadyActive)
            }
        } catch (error: Exception) {
            withContext(NonCancellable) {
                RelayServiceBridge.cancelStart(token)
                if (!error.serviceWasAlreadyActive) stopAndWait()
            }
            throw error
        }
    }

    /**
     * Quiesce the platform workers before touching the native handle they use.
     *
     * Every caller is on `Dispatchers.IO` (or the cleanup thread in
     * `onCleared`), so waiting here cannot freeze the Activity. This wait is
     * deliberately not bounded: invalidating a handle while a worker is still
     * in JNI is a much worse failure than keeping background cleanup pending
     * until `Service.onDestroy` has finished its bounded worker joins.
     */
    suspend fun stopAndWait() {
        val waiter = RelayServiceBridge.registerStopWaiter()
        val stopped = application.stopService(Intent(application, RelayService::class.java))
        if (!stopped) waiter.complete(Unit)
        try {
            waiter.await()
        } finally {
            // A cancelled lifecycle cleanup must not leave a completed or
            // forever-pending waiter retained by the service bridge.
            RelayServiceBridge.unregisterStopWaiter(waiter)
        }
    }

    private companion object {
        const val SERVICE_START_TIMEOUT_MS = 10_000L
    }
}

/**
 * A start failure that already left the service running is not something the
 * caller may tear down: some other operation owns it.
 */
val Throwable.serviceWasAlreadyActive: Boolean
    get() = (this as? RelayServiceStartException)?.serviceWasAlreadyActive == true
