package io.qpwgraph.relay

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class AndroidReliabilityPolicyTest {
    @Test
    fun emitter_microphone_requires_record_audio() {
        assertTrue(captureRequiresRecordAudio(RelayMode.Emitter, CaptureSource.MICROPHONE))
    }

    @Test
    fun emitter_device_playback_requires_record_audio() {
        assertTrue(captureRequiresRecordAudio(RelayMode.Emitter, CaptureSource.DEVICE_PLAYBACK))
    }

    @Test
    fun receiver_does_not_require_record_audio() {
        assertFalse(captureRequiresRecordAudio(RelayMode.Receiver, CaptureSource.MICROPHONE))
        assertFalse(captureRequiresRecordAudio(RelayMode.Receiver, CaptureSource.DEVICE_PLAYBACK))
    }

    @Test
    fun projection_grant_is_single_use() {
        val grants = PendingProjectionGrantStore<String>()
        grants.set(PendingProjectionGrant(1, "projection-token"))

        assertEquals("projection-token", grants.consume()?.data)
        assertNull(grants.consume())
    }

    @Test
    fun replacing_or_clearing_projection_grant_does_not_leave_old_data() {
        val grants = PendingProjectionGrantStore<String>()
        grants.set(PendingProjectionGrant(1, "old"))
        grants.set(PendingProjectionGrant(2, "new"))
        assertEquals(PendingProjectionGrant(2, "new"), grants.consume())

        grants.set(PendingProjectionGrant(3, "discarded"))
        grants.clear()
        assertNull(grants.consume())
    }

    @Test
    fun background_emitter_reconnect_is_deferred_for_both_capture_sources() {
        assertEquals(
            AutoReconnectAction.DeferUntilForeground,
            autoReconnectAction(RelayMode.Emitter, CaptureSource.MICROPHONE, foreground = false),
        )
        assertEquals(
            AutoReconnectAction.DeferUntilForeground,
            autoReconnectAction(RelayMode.Emitter, CaptureSource.DEVICE_PLAYBACK, foreground = false),
        )
        assertEquals(
            AutoReconnectAction.Allow,
            autoReconnectAction(RelayMode.Emitter, CaptureSource.MICROPHONE, foreground = true),
        )
    }

    @Test
    fun receiver_reconnect_policy_does_not_require_capture_permission() {
        assertEquals(
            AutoReconnectAction.Allow,
            autoReconnectAction(RelayMode.Receiver, CaptureSource.MICROPHONE, foreground = false),
        )
    }

    @Test
    fun playback_capture_always_excludes_the_application_uid() {
        val policy = playbackCapturePolicy(42)

        assertTrue(policy.excludedUids.contains(42))
        assertTrue(policy.includedUsages.isNotEmpty())
    }

    @Test
    fun native_load_failure_is_controlled_and_mentions_the_target_abi() {
        val status = loadNativeRuntime(
            loadLibrary = { throw UnsatisfiedLinkError("missing test library") },
            supportedAbis = listOf("x86_64"),
        )

        assertFalse(status.available)
        assertTrue(status.diagnostic.contains("x86_64"))
        assertTrue(status.diagnostic.contains("lib/x86_64/libpw_graph_relay_android.so"))
        assertTrue(status.diagnostic.contains("missing test library"))
    }

    @Test
    fun invalid_android_audio_geometry_is_rejected_before_service_start() {
        for (channels in listOf(0, 3, 8)) {
            val error = runCatching {
                validateAndroidAudioGeometry(AudioGeometry(48_000, channels, 20))
            }.exceptionOrNull()
            assertTrue("channels=$channels", error is IllegalArgumentException)
        }
        for (geometry in listOf(
            AudioGeometry(0, 1, 20),
            AudioGeometry(-48_000, 1, 20),
            AudioGeometry(48_000, 1, 0),
            AudioGeometry(48_000, 1, -20),
        )) {
            assertTrue(
                "geometry=$geometry",
                runCatching { validateAndroidAudioGeometry(geometry) }
                    .exceptionOrNull() is IllegalArgumentException,
            )
        }
    }
}
