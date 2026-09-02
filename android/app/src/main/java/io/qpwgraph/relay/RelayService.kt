package io.qpwgraph.relay

import android.Manifest
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
import android.media.projection.MediaProjection
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.IBinder
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.core.content.ContextCompat
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.CopyOnWriteArrayList
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
            Log.i(TAG, "HOST AUDIO START mode=${request.mode} handle=${request.handle} captureSource=${request.captureSource} sampleRate=${request.sampleRate} channels=${request.channels} frameMs=${request.frameMs}")
            startForegroundForRequest(request)
            startAudio(request)
        } catch (error: Throwable) {
            Log.e(TAG, "HOST AUDIO FAILURE during start: ${error.message}", error)
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
        val captureWanted = request.mode == MODE_HOST || clientRoleEmits(request.role)
        val playbackWanted = request.mode == MODE_HOST || clientRoleReceives(request.role)
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
        require(request.channels == 1 || request.channels == 2) {
            "Android relay audio supports mono or stereo, got ${request.channels} channels"
        }
        val frames = audioFrameCount(request.sampleRate, request.frameMs)
        val samples = frames * request.channels
        val captureWanted = request.mode == MODE_HOST || clientRoleEmits(request.role)
        val playbackWanted = request.mode == MODE_HOST || clientRoleReceives(request.role)

        if (captureWanted && running.get() && activeRequest === request) {
            captureThread = Thread(
                { runCapture(request, frames, samples) },
                "qpw-relay-capture-${request.mode}",
            ).also { it.start() }
        }
        if (playbackWanted && running.get() && activeRequest === request) {
            playbackThread = Thread(
                { runPlayback(request, frames, samples) },
                "qpw-relay-playback-${request.mode}",
            ).also { it.start() }
        }
    }

    private fun startForegroundForRequest(request: AudioRequest) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            var type = 0
            val captureSource = request.captureSource
            val isPlaybackCapture = captureSource == CaptureSource.DEVICE_PLAYBACK.name.lowercase() ||
                captureSource == "device_playback"
            if (request.mode == MODE_HOST || clientRoleEmits(request.role)) {
                type = if (isPlaybackCapture && Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                    type or ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION
                } else {
                    type or ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE
                }
            }
            if (request.mode == MODE_HOST || clientRoleReceives(request.role)) {
                type = type or ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK
            }
            // Ensure mediaPlayback is always present for modern Android
            if (type == 0) type = ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK
            Log.i(TAG, "HOST LISTENING? starting foreground type=$type mode=${request.mode}")
            startForeground(NOTIFICATION_ID, notification(), type)
        } else {
            startForeground(NOTIFICATION_ID, notification())
        }
    }

    private fun runCapture(request: AudioRequest, frames: Int, samples: Int) {
        var recorder: AudioRecord? = null
        var recording = false
        try {
            check(running.get() && activeRequest === request) { "relay audio service is stopping" }
            val captureSource = request.captureSource.lowercase()
            val isDevicePlayback = captureSource == "device_playback" || captureSource == "playback" || captureSource == "media"
            Log.i(TAG, "Capture starting source=$captureSource isDevicePlayback=$isDevicePlayback mode=${request.mode}")

            // RECORD_AUDIO is required for both MIC and playback capture
            if (ContextCompat.checkSelfPermission(this, Manifest.permission.RECORD_AUDIO) !=
                PackageManager.PERMISSION_GRANTED
            ) {
                throw SecurityException("the audio recording permission has not been granted")
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
            Log.i(TAG, "HOST AUDIO RUNNING capture source=$captureSource quantum=$quantum")
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
                Log.e(TAG, "HOST AUDIO FAILURE $prefix: ${error.message}", error)
                failAudio(request, "$prefix: ${error.message ?: error.javaClass.simpleName}")
            }
        } finally {
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

    private fun createMicrophoneRecord(request: AudioRequest, frames: Int): AudioRecord {
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
        val projection = projectionManager.getMediaProjection(request.mediaProjectionResultCode, data)
            ?: throw IllegalStateException("MediaProjection unavailable")
        mediaProjection = projection
        // Handle revocation
        projection.registerCallback(object : MediaProjection.Callback() {
            override fun onStop() {
                Log.w(TAG, "HOST AUDIO STOP Device playback capture stopped: projection revoked (bind=$projection)")
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

        val captureConfig = AudioPlaybackCaptureConfiguration.Builder(projection)
            .addMatchingUsage(AudioAttributes.USAGE_MEDIA)
            .addMatchingUsage(AudioAttributes.USAGE_GAME)
            .addMatchingUsage(AudioAttributes.USAGE_UNKNOWN)
            .build()

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
        Log.i(TAG, "HOST AUDIO STOP mode=${request?.mode} handle=${request?.handle} captureSource=${request?.captureSource}")
        runCatching { activeRecorder?.stop() }
        runCatching { activeTrack?.stop() }
        runCatching { mediaProjection?.stop() }
        mediaProjection = null
        captureThread?.let { thread ->
            if (thread !== Thread.currentThread()) runCatching { thread.join(500) }
        }
        playbackThread?.let { thread ->
            if (thread !== Thread.currentThread()) runCatching { thread.join(500) }
        }
        captureThread = null
        playbackThread = null
        activeRecorder = null
        activeTrack = null
        activeRequest = null
        if (request != null && request.handle != 0L) {
            when (request.mode) {
                MODE_HOST -> {
                    // Decouple: do NOT stop native host when audio service dies.
                    // Just notify ViewModel that audio stopped; host ownership stays with HostController.
                    Log.i(TAG, "HOST STOP audio service destroyed but keeping native host handle=${request.handle} listening")
                    // Do NOT call NativeBridge.hostStop here.
                }
                else -> {
                    runCatching { NativeBridge.disconnect(request.handle) }
                    runCatching { NativeBridge.release(request.handle) }
                }
            }
            RelayServiceBridge.reportStopped(request.mode, request.handle)
        }
        RelayServiceBridge.serviceDestroyed()
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null

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
