package io.qpwgraph.relay

import android.content.SharedPreferences
import java.util.UUID

/**
 * Everything the relay keeps in `SharedPreferences`: the installation
 * identity, the client settings and the host settings.
 *
 * Credentials never pass through here. PINs are deliberately not persisted —
 * a client enters one per pairing session (or scans the host's QR code), and
 * an Android host's PIN lives only in the in-memory UI state. Trusted
 * credentials belong to [TrustedCredentialStore], which encrypts them.
 *
 * Keep [TrustedCredentialStore.PREFERENCES_NAME] aligned with
 * `backup_rules.xml` / `data_extraction_rules.xml`: `sharedpref/relay.xml` is
 * intentionally excluded from backup and transfer.
 */
class RelaySettingsRepository(private val preferences: SharedPreferences) {

    /**
     * Stable identity used by discovery and trusted handshakes.
     *
     * Identity is installation state, so a generated one is committed before
     * it is returned: a process death must not be able to leave the engine
     * advertising an identity that was never written.
     */
    val deviceId: String by lazy {
        preferences.getString("device_id", null)
            ?: UUID.randomUUID().toString().also { generated ->
                check(preferences.edit().putString("device_id", generated).commit()) {
                    "could not persist relay installation identity"
                }
            }
    }

    /**
     * Remove PINs written by older builds. They are credentials, not app
     * preferences, so upgrading must not leave a usable one behind.
     */
    fun purgeLegacyPins() {
        preferences.edit().remove("pin").remove("host_pin").apply()
    }

    fun loadSettings(): RelaySettings = RelaySettings(
        target = preferences.getString("target", "") ?: "",
        pin = "",
        direction = audioDirectionFromString(
            if (preferences.contains("audio_direction")) {
                preferences.getString("audio_direction", null)
            } else {
                // One-release migration: old Android builds stored the
                // client role. `both` is intentionally resolved by
                // audioDirectionFromString to Mobile → Desktop.
                preferences.getString("role", null)
            },
        ),
        directionGeneration = preferences.getLong("audio_direction_generation", 0L),
        codec = preferences.getString("codec", "opus") ?: "opus",
        transport = migrateTransport(preferences.getString("transport", "auto") ?: "auto"),
        deviceName = preferences.getString("device_name", "android-relay") ?: "android-relay",
        autoConnectTrusted = preferences.getBoolean("auto_connect_trusted", true),
        autoConnectTrustedWifi = preferences.getBoolean("auto_connect_trusted_wifi", false),
        captureSource = captureSourceFromString(
            preferences.getString("capture_source", "microphone") ?: "microphone",
        ),
    )

    fun save(settings: RelaySettings) {
        preferences.edit()
            .putString("target", settings.target)
            .putString("audio_direction", settings.direction.serialized())
            .putLong("audio_direction_generation", settings.directionGeneration)
            .putString("codec", settings.codec)
            .putString("transport", settings.transport)
            .putString("device_name", settings.deviceName)
            .putBoolean("auto_connect_trusted", settings.autoConnectTrusted)
            .putBoolean("auto_connect_trusted_wifi", settings.autoConnectTrustedWifi)
            .putString("capture_source", settings.captureSource.name.lowercase())
            .apply()
    }

    /** Persist a direction transition without rewriting unrelated UI fields. */
    fun saveDirection(direction: AudioDirection) {
        preferences.edit()
            .putString("audio_direction", direction.serialized())
            .apply()
    }

    /** Persist a direction offer and its monotonic negotiation generation. */
    fun saveDirection(direction: AudioDirection, generation: Long) {
        preferences.edit()
            .putString("audio_direction", direction.serialized())
            .putLong("audio_direction_generation", generation)
            .apply()
    }

    fun loadHostSettings(): HostSettings = HostSettings(
        deviceName = preferences.getString("host_device_name", "android-relay")
            ?: "android-relay",
        pin = "",
        port = preferences.getInt("host_port", DEFAULT_HOST_PORT),
        codec = preferences.getString("host_codec", "opus") ?: "opus",
        transport = migrateTransport(preferences.getString("host_transport", "auto") ?: "auto"),
        captureSource = captureSourceFromString(preferences.getString("host_capture_source", "microphone") ?: "microphone"),
    )

    fun saveHost(host: HostSettings) {
        preferences.edit()
            .putString("host_device_name", host.deviceName)
            .putInt("host_port", host.port)
            .putString("host_codec", host.codec)
            .putString("host_transport", host.transport)
            .putString("host_capture_source", host.captureSource.name.lowercase())
            .apply()
    }

    private companion object {
        /** USB is auto-detected now; legacy explicit selections fall back. */
        fun migrateTransport(value: String): String = if (value == "usb") "auto" else value
    }
}
