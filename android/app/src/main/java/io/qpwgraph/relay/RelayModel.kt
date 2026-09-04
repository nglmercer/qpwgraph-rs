package io.qpwgraph.relay

import java.util.LinkedHashMap

/** The only user-selectable relay state: which endpoint should carry audio. */
enum class AudioDirection {
    MobileToDesktop,
    DesktopToMobile,
}

/** Internal role derived from [AudioDirection]. It is never persisted. */
enum class EffectiveAudioRole {
    Emitter,
    Host,
}

fun AudioDirection.androidRole(): EffectiveAudioRole = when (this) {
    AudioDirection.MobileToDesktop -> EffectiveAudioRole.Emitter
    AudioDirection.DesktopToMobile -> EffectiveAudioRole.Host
}

/** Role understood by the Android client engine, when this direction uses it. */
fun AudioDirection.androidClientRole(): String = when (this) {
    AudioDirection.MobileToDesktop -> "emit"
    AudioDirection.DesktopToMobile -> "receive"
}

fun AudioDirection.serialized(): String = when (this) {
    AudioDirection.MobileToDesktop -> "mobile_to_desktop"
    AudioDirection.DesktopToMobile -> "desktop_to_mobile"
}

fun audioDirectionFromString(value: String?): AudioDirection = when (value?.trim()?.lowercase()) {
    "desktop_to_mobile", "pc_to_mobile", "receive" -> AudioDirection.DesktopToMobile
    // Legacy Android `emit` and `both` intentionally resolve to the safe,
    // deterministic Mobile → Desktop direction. Unknown values use the same
    // one-way default.
    else -> AudioDirection.MobileToDesktop
}

/** Settings for the phone-to-desktop client direction. */
data class RelaySettings(
    val target: String = "",
    // Pairing credentials are entered for the current session and are not
    // persisted or supplied as an insecure app-wide default.
    val pin: String = "",
    val direction: AudioDirection = AudioDirection.MobileToDesktop,
    /** Monotonic generation used by authenticated direction negotiation. */
    val directionGeneration: Long = 0L,
    val codec: String = "opus",
    val transport: String = "auto",
    /** Trusted auto-connect is explicit; USB is the only default candidate. */
    val autoConnectTrusted: Boolean = true,
    val autoConnectTrustedWifi: Boolean = false,
    val deviceName: String = "android-relay",
    val sampleRate: Int = 48_000,
    val channels: Int = ANDROID_AUDIO_CHANNELS,
    val frameMs: Int = 20,
    val captureSource: CaptureSource = CaptureSource.MICROPHONE,
) {
    override fun toString(): String =
        "RelaySettings(target=$target, direction=$direction, directionGeneration=$directionGeneration, " +
            "codec=$codec, transport=$transport, " +
            "autoConnectTrusted=$autoConnectTrusted, autoConnectTrustedWifi=$autoConnectTrustedWifi, " +
            "deviceName=$deviceName, sampleRate=$sampleRate, channels=$channels, frameMs=$frameMs, " +
            "captureSource=$captureSource)"
}

enum class CaptureSource {
    MICROPHONE,
    DEVICE_PLAYBACK,
}

enum class RelayHostAudioState {
    Stopped,
    Starting,
    Running,
    Error,
}

/** Settings for broadcasting this device's audio as a relay host. */
data class HostSettings(
    val deviceName: String = "android-relay",
    // The Android API requires an explicit caller-owned pairing PIN.
    val pin: String = "",
    // Fixed default port: desktop USB probing scans for hosts on 48123, so
    // an ephemeral port would make this host undiscoverable over USB.
    val port: Int = DEFAULT_HOST_PORT,
    val codec: String = "opus",
    val transport: String = "auto",
    val sampleRate: Int = 48_000,
    val channels: Int = ANDROID_AUDIO_CHANNELS,
    val frameMs: Int = 20,
    val captureSource: CaptureSource = CaptureSource.MICROPHONE,
) {
    override fun toString(): String =
        "HostSettings(deviceName=$deviceName, port=$port, codec=$codec, transport=$transport, " +
            "sampleRate=$sampleRate, channels=$channels, frameMs=$frameMs, captureSource=$captureSource)"
}

fun captureSourceFromString(value: String): CaptureSource = when (value.lowercase()) {
    "device_playback", "playback", "media" -> CaptureSource.DEVICE_PLAYBACK
    else -> CaptureSource.MICROPHONE
}

const val DEFAULT_HOST_PORT = 48123

/** Default channel count for the Android platform audio endpoints.
 * Stereo is the quality default — device-playback capture keeps left/right
 * separation and both AudioRecord/AudioTrack accept the stereo masks — while
 * the mono geometry stays valid for callers that ask for it. */
const val ANDROID_AUDIO_CHANNELS = 2
const val PCM16_BYTES_PER_SAMPLE = 2

/** Audio geometry copied into the foreground service start request. */
data class AudioGeometry(
    val sampleRate: Int,
    val channels: Int,
    val frameMs: Int,
)

/** Select the geometry belonging to the operation being started. */
fun audioGeometryForHostMode(
    hostMode: Boolean,
    client: RelaySettings,
    host: HostSettings,
): AudioGeometry = if (hostMode) {
    AudioGeometry(host.sampleRate, host.channels, host.frameMs)
} else {
    AudioGeometry(client.sampleRate, client.channels, client.frameMs)
}

/** Service worker policy. A two-way role is deliberately not accepted. */
fun clientRoleEmits(role: String): Boolean = role.equals("emit", ignoreCase = true)

fun clientRoleReceives(role: String): Boolean = role.equals("receive", ignoreCase = true)

fun isOneWayAudioRole(role: String): Boolean =
    clientRoleEmits(role) != clientRoleReceives(role)

fun clientNeedsMicrophone(role: String): Boolean = clientRoleEmits(role)

fun clientNeedsMicrophone(direction: AudioDirection): Boolean =
    direction == AudioDirection.MobileToDesktop

/** Number of interleaved PCM frames in one configured relay quantum. */
fun audioFrameCount(sampleRate: Int, frameMs: Int): Int {
    require(sampleRate > 0 && frameMs > 0) { "audio geometry must be positive" }
    val frames = sampleRate.toLong() * frameMs.toLong() / 1000L
    require(frames in 1..Int.MAX_VALUE) { "audio frame count is out of range" }
    return frames.toInt()
}

/** Byte count expected by AudioRecord/AudioTrack for PCM16 interleaved data. */
fun pcm16BufferBytes(frames: Int, channels: Int = ANDROID_AUDIO_CHANNELS): Int {
    require(frames > 0 && channels > 0) { "PCM geometry must be positive" }
    val bytes = frames.toLong() * channels.toLong() * PCM16_BYTES_PER_SAMPLE
    require(bytes <= Int.MAX_VALUE) { "PCM buffer is too large" }
    return bytes.toInt()
}

enum class RelayConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Error,
}

enum class RelayHostState {
    Idle,
    Starting,
    Running,
    Error,
}

/** An active USB tether link, auto-detected by the native layer. */
data class UsbLinkInfo(
    val name: String,
    val addr: String,
)

/** One usable local IPv4 link, ranked best-first by the native layer. */
data class LocalLinkInfo(
    val name: String,
    val addr: String,
    val kind: String,
)

/**
 * Parse a scanned QR payload into `(target, pin)`. Accepts the app's own
 * `qpw-relay://host:port?pin=123456` URI as well as a plain `host:port`
 * string, so any generic QR carrying the address still works.
 */
fun parseRelayQr(raw: String): Pair<String, String?>? {
    val text = raw.trim()
    if (text.isEmpty()) return null
    if (text.startsWith("qpw-relay://")) {
        val rest = text.removePrefix("qpw-relay://")
        val parts = rest.split('?', limit = 2)
        val target = parts[0].trimEnd('/')
        if (target.isEmpty()) return null
        val pin = parts.getOrNull(1)
            ?.split('&')
            ?.firstOrNull { it.startsWith("pin=") }
            ?.removePrefix("pin=")
            ?.takeIf { it.isNotBlank() }
        return target to pin
    }
    if (Regex("""^[\w.\-]+:\d+$""").matches(text)) return text to null
    return null
}

/** A relay host seen on the local network during discovery. */
data class DiscoveredPeer(
    val id: String,
    val name: String,
    val address: String,
    /** Discovery hint only; authentication still proves the peer identity. */
    val link: String = "",
)

/** Identity-scoped key for bounded trusted reconnect backoff. */
data class TrustedCandidateKey(
    val peerId: String,
    val address: String,
)

private data class TrustedCandidateFailure(val count: Int, val retryAt: Long)

/**
 * Bounded candidate-local retry state. Public discovery addresses are
 * untrusted, so a failure must not quarantine every peer that later receives
 * the same address (for example after USB address reuse).
 */
class TrustedCandidateBackoff(private val maxEntries: Int = 256) {
    init {
        require(maxEntries > 0) { "candidate backoff capacity must be positive" }
    }

    private val failures = LinkedHashMap<TrustedCandidateKey, TrustedCandidateFailure>()

    fun allowed(peerId: String, address: String, now: Long): Boolean {
        val key = TrustedCandidateKey(peerId, address)
        val failure = failures[key] ?: return true
        if (failure.retryAt <= now) {
            failures.remove(key)
            return true
        }
        return false
    }

    fun noteFailure(peerId: String, address: String, now: Long) {
        val key = TrustedCandidateKey(peerId, address)
        val previous = failures[key]
        if (previous == null && failures.size >= maxEntries) {
            // Expired entries are deterministic garbage. If none expired,
            // evict the oldest insertion so attacker-controlled discovery
            // cannot grow this table without bound.
            val expired = failures.entries.firstOrNull { it.value.retryAt <= now }?.key
            failures.remove(expired ?: failures.entries.first().key)
        }
        val count = (previous?.count ?: 0).plus(1).coerceAtMost(7)
        val delay = minOf(30_000L, 500L shl (count - 1))
        failures[key] = TrustedCandidateFailure(count, now + delay)
    }

    fun clear(peerId: String, address: String) {
        failures.remove(TrustedCandidateKey(peerId, address))
    }

    internal fun size(): Int = failures.size
}

/**
 * Policy gate for background reconnect. The caller must additionally prove
 * that [peer.id] has a stored credential; a public discovery ID alone is not
 * sufficient. USB tethering is the conservative default, while Wi-Fi and
 * other IP links require an explicit opt-in.
 */
fun trustedAutoConnectAllowed(settings: RelaySettings, peer: DiscoveredPeer): Boolean =
    settings.autoConnectTrusted && when {
        peer.link.equals("usb", ignoreCase = true) || isLikelyUsbAddress(peer.address) -> true
        !settings.autoConnectTrustedWifi -> false
        // A durable last-known address may have no discovery link hint. The
        // explicit Wi-Fi opt-in permits that fallback, while a classified
        // Bluetooth/LAN candidate remains disallowed.
        peer.link.isBlank() || peer.link.equals("wifi", ignoreCase = true) -> true
        else -> false
    }

/** Durable metadata is preferred over public discovery ordering. */
fun trustedCandidateRank(peer: DiscoveredPeer, stored: TrustedRelayPeer?): Int {
    if (stored?.address == peer.address) return 0
    return when {
        peer.link.equals("usb", ignoreCase = true) || isLikelyUsbAddress(peer.address) -> 1
        peer.link.equals("wifi", ignoreCase = true) -> 2
        peer.link.equals("bluetooth", ignoreCase = true) -> 3
        peer.link.equals("lan", ignoreCase = true) -> 4
        else -> 5
    }
}

fun isLikelyUsbAddress(address: String): Boolean {
    val host = address.substringBeforeLast(':').removePrefix("[")
    return host.startsWith("192.168.42.") || host.startsWith("10.42.")
}

/** Durable credential created after a successful explicit PIN pairing. */
data class TrustedRelayPeer(
    val peerId: String,
    val secret: String,
    val name: String = "",
    val address: String = "",
) {
    override fun toString(): String =
        "TrustedRelayPeer(peerId=$peerId, name=$name, address=$address)"
}

/** Metadata shown in management UI; it intentionally contains no secret. */
data class TrustedRelayPeerSummary(
    val peerId: String,
    val name: String,
    val address: String,
)

/** One live session on the local host. */
data class RelaySessionInfo(
    val id: Long,
    val name: String,
    val address: String,
    val sending: Boolean,
    val receiving: Boolean,
    val transport: String = "",
    val link: String = "",
    val controlState: String = "",
    val audioChannelState: String = "",
    val trusted: Boolean = false,
)

/** Authenticated winner reported by a live relay session. */
data class DirectionResolution(
    val sessionId: Long,
    val direction: AudioDirection,
    val generation: Long,
)

data class RelayUiState(
    /** Direction is the authority for which local engine may run. */
    val direction: AudioDirection = AudioDirection.MobileToDesktop,
    val switchingDirection: Boolean = false,
    // Phone → PC client section.
    val settings: RelaySettings = RelaySettings(),
    val connection: RelayConnectionState = RelayConnectionState.Disconnected,
    val hostName: String = "",
    val sessionId: Long? = null,
    val message: String = "",
    val rms: Float = 0f,
    // PC → Phone host section.
    val host: HostSettings = HostSettings(),
    val hostState: RelayHostState = RelayHostState.Idle,
    val hostAudioState: RelayHostAudioState = RelayHostAudioState.Stopped,
    val hostAudioMessage: String = "",
    val hostActive: Boolean = false,
    val hostPort: Int? = null,
    val hostAddress: String? = null,
    val hostMessage: String = "",
    val hostRms: Float = 0f,
    val transport: String = "",
    val link: String = "",
    val audioChannelState: String = "",
    val sessions: List<RelaySessionInfo> = emptyList(),
    // Discovery section (shared by both modes).
    val discoveryActive: Boolean = false,
    val peers: List<DiscoveredPeer> = emptyList(),
    val discoveryMessage: String = "",
    // Auto-detected USB tether link, when one is up.
    val usbLink: UsbLinkInfo? = null,
    // All usable local links, best-first; shown with the host port.
    val localLinks: List<LocalLinkInfo> = emptyList(),
    val trustedPeers: List<TrustedRelayPeerSummary> = emptyList(),
)
