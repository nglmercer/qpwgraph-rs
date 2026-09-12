package io.qpwgraph.relay

import android.Manifest
import android.annotation.SuppressLint
import android.app.Activity
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Intent
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioPlaybackCaptureConfiguration
import android.media.AudioRecord
import android.media.AudioTrack
import android.media.MediaRecorder
import android.media.audiofx.AcousticEchoCanceler
import android.media.audiofx.AudioEffect
import android.media.audiofx.AutomaticGainControl
import android.media.audiofx.NoiseSuppressor
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.IBinder
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.core.content.ContextCompat
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicInteger
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.asSharedFlow
import kotlin.math.roundToInt

/** Result delivered after all requested platform-audio workers are ready. */
internal data class RelayServiceStartResult(
    val started: Boolean,
    val message: String = "",
)

/** Startup failed without owning the already-running service instance. */
internal class RelayServiceStartException(
    message: String,
    val serviceWasAlreadyActive: Boolean = false,
) : IllegalStateException(message)

internal sealed interface RelayServiceEvent {
    data class AudioFailure(
        val mode: String,
        val handle: Long,
        val message: String,
    ) : RelayServiceEvent

    /** The service was destroyed outside the ViewModel's normal stop path. */
    data class ServiceStopped(
        val mode: String,
        val handle: Long,
    ) : RelayServiceEvent
}

/** Small in-process coordination bridge between the ViewModel and Service. */
internal object RelayServiceBridge {
    private val starts = ConcurrentHashMap<String, CompletableDeferred<RelayServiceStartResult>>()
    private val stopWaiters = CopyOnWriteArrayList<CompletableDeferred<Unit>>()
    private val mutableEvents = MutableSharedFlow<RelayServiceEvent>(extraBufferCapacity = 16)
    val events = mutableEvents.asSharedFlow()

    fun registerStart(token: String): CompletableDeferred<RelayServiceStartResult> {
        val deferred = CompletableDeferred<RelayServiceStartResult>()
        check(starts.putIfAbsent(token, deferred) == null) { "duplicate relay service start token" }
        return deferred
    }

    fun completeStart(token: String, result: RelayServiceStartResult) {
        starts.remove(token)?.complete(result)
    }

    fun cancelStart(token: String) {
        starts.remove(token)?.cancel()
    }

    fun registerStopWaiter(): CompletableDeferred<Unit> {
        return CompletableDeferred<Unit>().also(stopWaiters::add)
    }

    fun unregisterStopWaiter(waiter: CompletableDeferred<Unit>) {
        stopWaiters.remove(waiter)
    }

    fun serviceDestroyed() {
        stopWaiters.forEach { it.complete(Unit) }
        stopWaiters.clear()
    }

    fun reportFatal(mode: String, handle: Long, message: String) {
        mutableEvents.tryEmit(RelayServiceEvent.AudioFailure(mode, handle, message))
    }

    fun reportStopped(mode: String, handle: Long) {
        mutableEvents.tryEmit(RelayServiceEvent.ServiceStopped(mode, handle))
    }
}

/**
 * Foreground audio pump shared by both relay roles.
 *
 * There is intentionally one active immutable [AudioRequest]. A second mode
 * is rejected while it is running; live workers never observe their handle or
 * mode replaced underneath them. The ViewModel stops the current mode and
 * waits for [onDestroy] before starting the other one.
 */
class RelayService : Service() {
    companion object {
        const val EXTRA_MODE = "mode"
        const val EXTRA_HANDLE = "handle"
        const val EXTRA_ROLE = "role"
        const val EXTRA_SAMPLE_RATE = "sample_rate"
        const val EXTRA_CHANNELS = "channels"
        const val EXTRA_FRAME_MS = "frame_ms"
        const val EXTRA_START_TOKEN = "start_token"
        const val EXTRA_CAPTURE_SOURCE = "capture_source"
        const val EXTRA_MEDIA_PROJECTION_RESULT_CODE = "media_projection_result_code"
        const val EXTRA_MEDIA_PROJECTION_DATA = "media_projection_data"
        const val MODE_CLIENT = "client"
        const val MODE_HOST = "host"
        private const val CHANNEL = "relay-audio"
        private const val NOTIFICATION_ID = 48123
        private const val TAG = "RelayService"
        private const val WORKER_JOIN_TIMEOUT_MS = 5_000L
    }

    private data class AudioRequest(
        val mode: String,
        val handle: Long,
        val role: String,
        val sampleRate: Int,
        val channels: Int,
        val frameMs: Int,
        val startToken: String,
        val captureSource: String,
        val mediaProjectionResultCode: Int,
        val mediaProjectionData: Intent?,
    )

    private data class WorkerLatches(
        val capture: CountDownLatch?,
        val playback: CountDownLatch?,
    )

    /** Best-effort microphone effects; an unsupported effect never blocks audio. */
    private class OptionalAudioEffects(private val effects: List<AudioEffect>) {
        fun release() {
            effects.asReversed().forEach { effect -> runCatching { effect.release() } }
        }
    }

    private val running = AtomicBoolean(false)
    private val startupRemaining = AtomicInteger(0)
    private val startupFinished = AtomicBoolean(false)
    private val fatalReported = AtomicBoolean(false)
    @Volatile private var activeRequest: AudioRequest? = null
    @Volatile private var captureThread: Thread? = null
    @Volatile private var playbackThread: Thread? = null
    @Volatile private var activeRecorder: AudioRecord? = null
    @Volatile private var activeTrack: AudioTrack? = null
    @Volatile private var mediaProjection: MediaProjection? = null
    @Volatile private var workerLatches: WorkerLatches? = null

    override fun onCreate() {
        super.onCreate()
        createNotificationChannel()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val request = intent?.let { audioRequest(it) }
        if (request == null || request.handle == 0L) {
            if (activeRequest == null && !running.get()) {
                stopSelfResult(startId)
            }
            return START_NOT_STICKY
        }

        if (!NativeRuntime.available) {
            RelayServiceBridge.completeStart(
                request.startToken,
                RelayServiceStartResult(false, NativeRuntime.diagnostic),
            )
            stopSelfResult(startId)
            return START_NOT_STICKY
        }

        if (!isOneWayAudioRole(request.role)) {
            RelayServiceBridge.completeStart(
                request.startToken,
                RelayServiceStartResult(
                    false,
                    "relay audio role '${request.role}' is not supported; both directions are disabled",
                ),
            )
            return START_NOT_STICKY
        }

        val previous = activeRequest
        if (previous != null || running.get()) {
            RelayServiceBridge.completeStart(
                request.startToken,
                RelayServiceStartResult(
                    false,
                    "relay audio service is already active in ${previous?.mode ?: "another mode"} mode",
                ),
            )
            return START_NOT_STICKY
        }

        activeRequest = request
        running.set(true)
        startupFinished.set(false)
        fatalReported.set(false)
        val workers = workerCount(request)
        startupRemaining.set(workers)
        try {
            if (workers == 0) {
                throw IllegalArgumentException("no audio direction was requested")
            }
            Log.i(TAG, "RELAY AUDIO START mode=${request.mode} handle=${request.handle} role=${request.role} source=${request.captureSource} sampleRate=${request.sampleRate} channels=${request.channels} frameMs=${request.frameMs}")
            startForegroundForRequest(request)
            startAudio(request)
        } catch (error: Throwable) {
            Log.e(TAG, "RELAY AUDIO FAILURE during start: ${error.message}", error)
            failAudio(request, "could not start relay audio: ${error.message ?: error.javaClass.simpleName}")
        }
        return START_NOT_STICKY
    }

    private fun audioRequest(intent: Intent): AudioRequest {
        val projectionData: Intent? = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            intent.getParcelableExtra(EXTRA_MEDIA_PROJECTION_DATA, Intent::class.java)
        } else {
            @Suppress("DEPRECATION")
            intent.getParcelableExtra(EXTRA_MEDIA_PROJECTION_DATA)
        }
        return AudioRequest(
            mode = intent.getStringExtra(EXTRA_MODE) ?: MODE_CLIENT,
            handle = intent.getLongExtra(EXTRA_HANDLE, 0L),
            role = intent.getStringExtra(EXTRA_ROLE) ?: "emit",
            sampleRate = intent.getIntExtra(EXTRA_SAMPLE_RATE, 48_000),
            channels = intent.getIntExtra(EXTRA_CHANNELS, 1),
            frameMs = intent.getIntExtra(EXTRA_FRAME_MS, 20),
            startToken = intent.getStringExtra(EXTRA_START_TOKEN).orEmpty(),
            captureSource = intent.getStringExtra(EXTRA_CAPTURE_SOURCE) ?: CaptureSource.MICROPHONE.name.lowercase(),
            mediaProjectionResultCode = intent.getIntExtra(EXTRA_MEDIA_PROJECTION_RESULT_CODE, Activity.RESULT_CANCELED),
            mediaProjectionData = projectionData,
        )
    }

    private fun workerCount(request: AudioRequest): Int {
        val captureWanted = clientRoleEmits(request.role)
        val playbackWanted = clientRoleReceives(request.role)
        return (if (captureWanted) 1 else 0) + (if (playbackWanted) 1 else 0)
    }

    private fun pushCapture(request: AudioRequest, samples: FloatArray, length: Int): Int =
        when (request.mode) {
            MODE_HOST -> NativeBridge.hostPushCapture(request.handle, samples, length)
            else -> NativeBridge.pushCapture(request.handle, samples, length)
        }

    private fun pullPlayback(request: AudioRequest, output: FloatArray): Int =
        when (request.mode) {
            MODE_HOST -> NativeBridge.hostPullPlayback(request.handle, output)
            else -> NativeBridge.pullPlayback(request.handle, output)
        }

    private fun startAudio(request: AudioRequest) {
        validateAndroidAudioGeometry(AudioGeometry(request.sampleRate, request.channels, request.frameMs))
        val frames = audioFrameCount(request.sampleRate, request.frameMs)
        val samples = frames * request.channels
        val captureWanted = clientRoleEmits(request.role)
        val playbackWanted = clientRoleReceives(request.role)
        val latches = WorkerLatches(
            capture = if (captureWanted) CountDownLatch(1) else null,
            playback = if (playbackWanted) CountDownLatch(1) else null,
        )
        workerLatches = latches

        if (captureWanted && running.get() && activeRequest === request) {
            captureThread = Thread(
                {
                    try {
                        runCapture(request, frames, samples)
                    } finally {
                        latches.capture?.countDown()
                    }
                },
                "qpw-relay-capture-${request.mode}",
            ).also { it.start() }
        }
        if (playbackWanted && running.get() && activeRequest === request) {
            playbackThread = Thread(
                {
                    try {
                        runPlayback(request, frames, samples)
                    } finally {
                        latches.playback?.countDown()
                    }
                },
                "qpw-relay-playback-${request.mode}",
            ).also { it.start() }
        }
    }

    @SuppressLint("InlinedApi")
    private fun startForegroundForRequest(request: AudioRequest) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            var type = 0
            val captureSource = request.captureSource
            val isPlaybackCapture = captureSource == CaptureSource.DEVICE_PLAYBACK.name.lowercase() ||
                captureSource == "device_playback"
            if (clientRoleEmits(request.role)) {
                type = if (isPlaybackCapture) {
                    type or ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION
                } else if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                    type or ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE
                } else {
                    type
                }
            }
            if (clientRoleReceives(request.role)) {
                type = type or ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK
            }
            Log.i(TAG, "RELAY FOREGROUND type=$type mode=${request.mode} role=${request.role}")
            if (type == 0) {
                // API 29 has no microphone foreground-service type. The
                // two-argument overload is valid there and avoids claiming an
                // unrelated mediaPlayback operation.
                startForeground(NOTIFICATION_ID, notification())
            } else {
                startForeground(NOTIFICATION_ID, notification(), type)
            }
        } else {
            startForeground(NOTIFICATION_ID, notification())
        }
    }

    private fun runCapture(request: AudioRequest, frames: Int, samples: Int) {
        var recorder: AudioRecord? = null
        var optionalEffects: OptionalAudioEffects? = null
        var recording = false
        try {
            check(running.get() && activeRequest === request) { "relay audio service is stopping" }
            val captureSource = request.captureSource.lowercase()
            val isDevicePlayback = captureSource == "device_playback" || captureSource == "playback" || captureSource == "media"
            Log.i(TAG, "Emitter capture starting source=$captureSource isDevicePlayback=$isDevicePlayback mode=${request.mode}")

            // Both microphone and device-playback capture require
            // RECORD_AUDIO. Playback capture additionally needs a
            // user-approved MediaProjection grant.
            if (ContextCompat.checkSelfPermission(this, Manifest.permission.RECORD_AUDIO) !=
                PackageManager.PERMISSION_GRANTED
            ) {
                throw SecurityException("the audio-recording permission has not been granted")
            }

            recorder = if (isDevicePlayback) {
                createPlaybackCaptureRecord(request, frames)
            } else {
                createMicrophoneRecord(request, frames)
            }
            activeRecorder = recorder
            check(recorder.state == AudioRecord.STATE_INITIALIZED) {
                "AudioRecord is not initialized (state=${recorder.state})"
            }
            try {
                recorder.startRecording()
                recording = true
            } catch (error: Throwable) {
                throw IllegalStateException("AudioRecord.startRecording failed", error)
            }
            if (!isDevicePlayback) {
                optionalEffects = attachOptionalMicrophoneEffects(recorder.audioSessionId)
            }
            val pcm = ShortArray(samples)
            // Accumulate exact quanta to avoid inconsistent packet sizes.
            // samples = frames * channels is the negotiated quantum.
            val quantum = samples
            val pending = FloatArray(quantum)
            var pendingPos = 0
            var captureSamplesRead = 0L
            var captureSamplesSubmitted = 0L
            var captureSamplesAccepted = 0L
            var captureSamplesDropped = 0L
            Log.i(TAG, "Emitter audio running source=$captureSource quantum=$quantum")
            workerReady(request)
            while (running.get() && activeRequest === request) {
                val count = recorder.read(pcm, 0, pcm.size)
                when {
                    count < 0 -> throw IllegalStateException("AudioRecord.read failed with code $count")
                    count == 0 -> Thread.sleep(2)
                    else -> {
                        captureSamplesRead += count.toLong()
                        var srcOffset = 0
                        while (srcOffset < count) {
                            val needed = quantum - pendingPos
                            val available = count - srcOffset
                            val toCopy = minOf(needed, available)
                            for (index in 0 until toCopy) {
                                pending[pendingPos + index] = pcm[srcOffset + index] / 32768f
                            }
                            pendingPos += toCopy
                            srcOffset += toCopy
                            if (pendingPos == quantum) {
                                captureSamplesSubmitted += quantum.toLong()
                                val accepted = pushCapture(request, pending, quantum)
                                if (accepted != quantum) {
                                    captureSamplesDropped += (quantum - accepted).toLong()
                                    Log.w(
                                        TAG,
                                        "Relay capture drop: requested=$quantum accepted=$accepted dropped=${quantum - accepted} read=$captureSamplesRead submitted=$captureSamplesSubmitted accepted=$captureSamplesAccepted",
                                    )
                                } else {
                                    captureSamplesAccepted += accepted.toLong()
                                }
                                // Preserve leftover samples: accumulator reset but loop continues
                                pendingPos = 0
                            }
                        }
                    }
                }
            }
        } catch (error: Throwable) {
            if (running.get()) {
                val prefix = if (request.captureSource.lowercase().contains("playback")) "device playback audio failed" else "microphone audio failed"
                Log.e(TAG, "Emitter audio failure $prefix: ${error.message}", error)
                failAudio(request, "$prefix: ${error.message ?: error.javaClass.simpleName}")
            }
        } finally {
            optionalEffects?.release()
            if (recording) runCatching { recorder?.stop() }
            recorder?.release()
            if (activeRecorder === recorder) activeRecorder = null
            // Clean up MediaProjection if we created one
            if (request.captureSource.lowercase().contains("playback")) {
                runCatching { mediaProjection?.stop() }
                mediaProjection = null
            }
        }
    }

    /**
     * Android audio effects are device/OEM dependent. Auto-enable only an
     * effect the platform reports as available, and treat every creation or
     * enable failure as a diagnostic rather than a capture failure.
     */
    private fun attachOptionalMicrophoneEffects(audioSessionId: Int): OptionalAudioEffects {
        val effects = listOfNotNull(
            createOptionalEffect(
                name = "AEC",
                available = { AcousticEchoCanceler.isAvailable() },
                create = { AcousticEchoCanceler.create(audioSessionId) },
            ),
            createOptionalEffect(
                name = "noise suppression",
                available = { NoiseSuppressor.isAvailable() },
                create = { NoiseSuppressor.create(audioSessionId) },
            ),
            createOptionalEffect(
                name = "automatic gain control",
                available = { AutomaticGainControl.isAvailable() },
                create = { AutomaticGainControl.create(audioSessionId) },
            ),
        )
        if (effects.isNotEmpty()) {
            Log.i(TAG, "Microphone effects enabled: ${effects.size}")
        }
        return OptionalAudioEffects(effects)
    }

    private fun createOptionalEffect(
        name: String,
        available: () -> Boolean,
        create: () -> AudioEffect?,
    ): AudioEffect? {
        if (!runCatching { available() }.getOrDefault(false)) return null
        val effect = runCatching { create() }
            .onFailure { error ->
                Log.w(TAG, "Optional microphone effect unavailable: $name (${error.message})")
            }
            .getOrNull() ?: return null
        return runCatching {
            effect.enabled = true
            if (!effect.enabled) {
                throw IllegalStateException("effect did not enable")
            }
            effect
        }.onFailure { error ->
            Log.w(TAG, "Optional microphone effect could not be enabled: $name (${error.message})")
            runCatching { effect.release() }
        }.getOrNull()
    }

    private fun createMicrophoneRecord(request: AudioRequest, frames: Int): AudioRecord {
        check(ContextCompat.checkSelfPermission(this, Manifest.permission.RECORD_AUDIO) ==
            PackageManager.PERMISSION_GRANTED
        ) { "the audio-recording permission has not been granted" }
        val inMask = if (request.channels == 2) {
            AudioFormat.CHANNEL_IN_STEREO
        } else {
            AudioFormat.CHANNEL_IN_MONO
        }
        val minimum = AudioRecord.getMinBufferSize(
            request.sampleRate,
            inMask,
            AudioFormat.ENCODING_PCM_16BIT,
        )
        require(minimum > 0) { "AudioRecord returned invalid minimum buffer size $minimum" }
        return AudioRecord(
            MediaRecorder.AudioSource.MIC,
            request.sampleRate,
            inMask,
            AudioFormat.ENCODING_PCM_16BIT,
            maxOf(minimum, pcm16BufferBytes(frames, request.channels)),
        )
    }

    private fun createPlaybackCaptureRecord(request: AudioRequest, frames: Int): AudioRecord {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) {
            throw IllegalStateException("device playback capture requires Android 10 or newer")
        }
        val data = request.mediaProjectionData
            ?: throw SecurityException("MediaProjection permission denied: no capture consent")
        if (request.mediaProjectionResultCode != Activity.RESULT_OK) {
            throw SecurityException("MediaProjection permission denied")
        }
        val projectionManager = getSystemService(MediaProjectionManager::class.java)
            ?: throw IllegalStateException("MediaProjectionManager unavailable")
        check(ContextCompat.checkSelfPermission(this, Manifest.permission.RECORD_AUDIO) ==
            PackageManager.PERMISSION_GRANTED
        ) { "the audio-recording permission has not been granted" }
        val projection = projectionManager.getMediaProjection(request.mediaProjectionResultCode, data)
            ?: throw IllegalStateException("MediaProjection unavailable")
        mediaProjection = projection
        // Handle revocation
        projection.registerCallback(object : MediaProjection.Callback() {
            override fun onStop() {
                Log.w(TAG, "Emitter capture stopped: projection revoked")
                if (activeRequest === request && running.get()) {
                    failAudio(request, "Device playback capture stopped: projection revoked")
                }
            }
        }, null)

        val inMask = if (request.channels == 2) {
            AudioFormat.CHANNEL_IN_STEREO
        } else {
            AudioFormat.CHANNEL_IN_MONO
        }
        val minimum = AudioRecord.getMinBufferSize(
            request.sampleRate,
            inMask,
            AudioFormat.ENCODING_PCM_16BIT,
        )
        require(minimum > 0) { "AudioRecord returned invalid minimum buffer size $minimum" }

        val policy = playbackCapturePolicy(applicationInfo.uid)
        val captureConfigBuilder = AudioPlaybackCaptureConfiguration.Builder(projection)
        policy.includedUsages.forEach { usage -> captureConfigBuilder.addMatchingUsage(usage) }
        policy.excludedUids.forEach { uid -> captureConfigBuilder.excludeUid(uid) }
        val captureConfig = captureConfigBuilder.build()

        val format = AudioFormat.Builder()
            .setSampleRate(request.sampleRate)
            .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
            .setChannelMask(inMask)
            .build()

        // Do NOT combine setAudioSource with setAudioPlaybackCaptureConfig
        return AudioRecord.Builder()
            .setAudioFormat(format)
            .setBufferSizeInBytes(maxOf(minimum, pcm16BufferBytes(frames, request.channels)))
            .setAudioPlaybackCaptureConfig(captureConfig)
            .build()
    }

    private fun runPlayback(request: AudioRequest, frames: Int, samples: Int) {
        var track: AudioTrack? = null
        var playing = false
        try {
            check(running.get() && activeRequest === request) { "relay audio service is stopping" }
            val outMask = if (request.channels == 2) {
                AudioFormat.CHANNEL_OUT_STEREO
            } else {
                AudioFormat.CHANNEL_OUT_MONO
            }
            val minimum = AudioTrack.getMinBufferSize(
                request.sampleRate,
                outMask,
                AudioFormat.ENCODING_PCM_16BIT,
            )
            require(minimum > 0) { "AudioTrack returned invalid minimum buffer size $minimum" }
            val created = AudioTrack.Builder()
                .setAudioAttributes(
                    AudioAttributes.Builder()
                        .setUsage(AudioAttributes.USAGE_MEDIA)
                        .setContentType(AudioAttributes.CONTENT_TYPE_SPEECH)
                        .build(),
                )
                .setAudioFormat(
                    AudioFormat.Builder()
                        .setSampleRate(request.sampleRate)
                        .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                        .setChannelMask(outMask)
                        .build(),
                )
                .setBufferSizeInBytes(maxOf(minimum, pcm16BufferBytes(frames, request.channels)))
                .build()
            track = created
            activeTrack = created
            check(created.state == AudioTrack.STATE_INITIALIZED) {
                "AudioTrack is not initialized (state=${created.state})"
            }
            try {
                created.play()
                playing = true
            } catch (error: Throwable) {
                throw IllegalStateException("AudioTrack.play failed", error)
            }
            val floats = FloatArray(samples)
            val pcm = ShortArray(samples)
            workerReady(request)
            while (running.get() && activeRequest === request) {
                val count = pullPlayback(request, floats).coerceIn(0, samples)
                if (count == 0) {
                    Thread.sleep(2)
                    continue
                }
                for (index in 0 until count) {
                    pcm[index] = (floats[index].coerceIn(-1f, 1f) * Short.MAX_VALUE)
                        .roundToInt().toShort()
                }
                var offset = 0
                while (offset < count && running.get() && activeRequest === request) {
                    val written = created.write(
                        pcm,
                        offset,
                        count - offset,
                        AudioTrack.WRITE_BLOCKING,
                    )
                    when {
                        written < 0 -> throw IllegalStateException("AudioTrack.write failed with code $written")
                        written == 0 -> Thread.sleep(2)
                        else -> offset += written
                    }
                }
            }
        } catch (error: Throwable) {
            if (running.get()) {
                Log.e(TAG, "playback audio failed: ${error.message}", error)
                failAudio(request, "playback audio failed: ${error.message ?: error.javaClass.simpleName}")
            }
        } finally {
            if (playing) runCatching { track?.stop() }
            track?.release()
            if (activeTrack === track) activeTrack = null
        }
    }

    private fun workerReady(request: AudioRequest) {
        if (request.startToken.isBlank()) return
        if (startupRemaining.decrementAndGet() == 0 && startupFinished.compareAndSet(false, true)) {
            RelayServiceBridge.completeStart(request.startToken, RelayServiceStartResult(true))
        }
    }

    private fun failAudio(request: AudioRequest, message: String) {
        if (activeRequest !== request) return
        // For HOST, decouple audio failure from network host lifetime.
        if (request.mode == MODE_HOST) {
            Log.w(TAG, "HOST AUDIO FAILURE (keeping host listening): $message handle=${request.handle} captureSource=${request.captureSource}")
            // Surface via both native and bridge paths but do NOT kill the TCP listener.
            runCatching { NativeBridge.hostReportError(request.handle, message) }
            if (request.startToken.isNotBlank() && startupFinished.compareAndSet(false, true)) {
                RelayServiceBridge.completeStart(request.startToken, RelayServiceStartResult(false, message))
            } else if (fatalReported.compareAndSet(false, true)) {
                RelayServiceBridge.reportFatal(request.mode, request.handle, message)
            }
            // Keep service alive (running=true) so playback can continue; failing thread will exit.
            return
        }
        // Client mode retains original fatal behavior
        running.set(false)
        runCatching { NativeBridge.reportError(request.handle, message) }
        if (request.startToken.isNotBlank() && startupFinished.compareAndSet(false, true)) {
            RelayServiceBridge.completeStart(request.startToken, RelayServiceStartResult(false, message))
        } else if (fatalReported.compareAndSet(false, true)) {
            RelayServiceBridge.reportFatal(request.mode, request.handle, message)
        }
        Log.w(TAG, "CLIENT AUDIO FAILURE stopping service: $message")
        stopSelf()
    }

    override fun onDestroy() {
        val request = activeRequest
        running.set(false)
        Log.i(TAG, "RELAY AUDIO STOP mode=${request?.mode} handle=${request?.handle} captureSource=${request?.captureSource}")
        runCatching { activeRecorder?.stop() }
        runCatching { activeTrack?.stop() }
        runCatching { mediaProjection?.stop() }
        mediaProjection = null
        val latches = workerLatches
        awaitWorkerExit("capture", captureThread, latches?.capture)
        awaitWorkerExit("playback", playbackThread, latches?.playback)
        captureThread = null
        playbackThread = null
        workerLatches = null
        activeRecorder = null
        activeTrack = null
        activeRequest = null
        if (request != null && request.handle != 0L) {
            when (request.mode) {
                MODE_HOST -> Log.i(
                    TAG,
                    "HOST AUDIO STOP service destroyed; native host remains owned by controller handle=${request.handle}",
                )
                else -> Unit
            }
            RelayServiceBridge.reportStopped(request.mode, request.handle)
        }
        RelayServiceBridge.serviceDestroyed()
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null

    /**
     * A timeout is a diagnostic threshold, not permission to release a native
     * handle. If a platform audio implementation ignores stop(), wait until
     * the worker's finally block confirms that it has exited.
     */
    private fun awaitWorkerExit(name: String, thread: Thread?, latch: CountDownLatch?) {
        if (thread == null || latch == null || thread === Thread.currentThread()) return
        var interrupted = false
        try {
            if (!latch.await(WORKER_JOIN_TIMEOUT_MS, TimeUnit.MILLISECONDS)) {
                Log.e(
                    TAG,
                    "RELAY AUDIO worker did not exit within ${WORKER_JOIN_TIMEOUT_MS}ms; " +
                        "waiting before handle teardown: $name",
                )
            }
        } catch (_: InterruptedException) {
            interrupted = true
            Log.e(TAG, "RELAY AUDIO worker wait interrupted; continuing until safe teardown: $name")
        }
        while (latch.count > 0L) {
            try {
                latch.await()
            } catch (_: InterruptedException) {
                interrupted = true
            }
        }
        if (interrupted) Thread.currentThread().interrupt()
    }

    private fun createNotificationChannel() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val channel = NotificationChannel(
                CHANNEL,
                getString(R.string.relay_notification_channel),
                NotificationManager.IMPORTANCE_LOW,
            )
            getSystemService(NotificationManager::class.java).createNotificationChannel(channel)
        }
    }

    private fun contentIntent(): android.app.PendingIntent = android.app.PendingIntent.getActivity(
        this,
        0,
        Intent(this, MainActivity::class.java)
            .setAction(Intent.ACTION_MAIN)
            .addCategory(Intent.CATEGORY_LAUNCHER)
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP),
        android.app.PendingIntent.FLAG_IMMUTABLE or android.app.PendingIntent.FLAG_UPDATE_CURRENT,
    )

    private fun notification(): Notification = NotificationCompat.Builder(this, CHANNEL)
        .setContentTitle(getString(R.string.relay_app_title))
        .setContentText(getString(R.string.relay_notification_active))
        .setSmallIcon(R.drawable.ic_relay_notification)
        .setCategory(NotificationCompat.CATEGORY_SERVICE)
        .setForegroundServiceBehavior(androidx.core.app.NotificationCompat.FOREGROUND_SERVICE_IMMEDIATE)
        .setContentIntent(contentIntent())
        .setOngoing(true)
        .setShowWhen(false)
        .build()
}
