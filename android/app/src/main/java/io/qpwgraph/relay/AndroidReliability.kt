package io.qpwgraph.relay

import android.media.AudioAttributes

/** A MediaProjection grant is valid for one capture session only. */
data class PendingProjectionGrant<T>(
    val resultCode: Int,
    val data: T,
)

enum class AppVisibility {
    Foreground,
    Background,
}

/** Thread-safe, in-memory storage for a single consumable projection grant. */
class PendingProjectionGrantStore<T> {
    private var pending: PendingProjectionGrant<T>? = null

    @Synchronized
    fun set(grant: PendingProjectionGrant<T>) {
        pending = grant
    }

    @Synchronized
    fun peek(): PendingProjectionGrant<T>? = pending

    @Synchronized
    fun consume(): PendingProjectionGrant<T>? = pending.also { pending = null }

    @Synchronized
    fun clear() {
        pending = null
    }
}

/** Policy decision for a trusted reconnect discovered while the app is hidden. */
enum class AutoReconnectAction {
    Allow,
    DeferUntilForeground,
}

/**
 * Capture is a user-controlled foreground operation on Android. Both the
 * microphone and playback-capture paths require RECORD_AUDIO, so either one
 * must wait for the Activity when an automatic reconnect happens in the
 * background. Receiver mode only plays audio and does not capture.
 */
fun autoReconnectAction(
    mode: RelayMode,
    source: CaptureSource,
    foreground: Boolean,
): AutoReconnectAction = when {
    mode == RelayMode.Emitter && !foreground && when (source) {
        CaptureSource.MICROPHONE, CaptureSource.DEVICE_PLAYBACK -> true
    } -> AutoReconnectAction.DeferUntilForeground
    else -> AutoReconnectAction.Allow
}

/** Playback capture must never feed this app's own AudioTrack back to itself. */
data class PlaybackCapturePolicy(
    val includedUsages: Set<Int>,
    val excludedUids: Set<Int>,
)

fun playbackCapturePolicy(applicationUid: Int): PlaybackCapturePolicy {
    require(applicationUid >= 0) { "application UID must not be negative" }
    return PlaybackCapturePolicy(
        includedUsages = setOf(
            AudioAttributes.USAGE_MEDIA,
            AudioAttributes.USAGE_GAME,
            AudioAttributes.USAGE_UNKNOWN,
        ),
        excludedUids = setOf(applicationUid),
    )
}
