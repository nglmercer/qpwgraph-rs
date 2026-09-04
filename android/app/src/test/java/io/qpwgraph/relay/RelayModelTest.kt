package io.qpwgraph.relay

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class RelayModelTest {
    @Test
    fun direction_maps_to_one_way_android_roles() {
        assertEquals(EffectiveAudioRole.Emitter, AudioDirection.MobileToDesktop.androidRole())
        assertEquals(EffectiveAudioRole.Host, AudioDirection.DesktopToMobile.androidRole())
        assertEquals("emit", AudioDirection.MobileToDesktop.androidClientRole())
        assertEquals("receive", AudioDirection.DesktopToMobile.androidClientRole())
        assertTrue(clientNeedsMicrophone(AudioDirection.MobileToDesktop))
        assertFalse(clientNeedsMicrophone(AudioDirection.DesktopToMobile))
        assertTrue(isOneWayAudioRole("emit"))
        assertTrue(isOneWayAudioRole("receive"))
        assertFalse(isOneWayAudioRole("both"))
        assertFalse(isOneWayAudioRole("unknown"))
        assertFalse(clientRoleEmits("both"))
        assertFalse(clientRoleReceives("both"))
    }

    @Test
    fun direction_settings_accept_canonical_and_legacy_values() {
        assertEquals(AudioDirection.MobileToDesktop, audioDirectionFromString("mobile_to_desktop"))
        assertEquals(AudioDirection.DesktopToMobile, audioDirectionFromString("desktop_to_mobile"))
        assertEquals(AudioDirection.DesktopToMobile, audioDirectionFromString("pc_to_mobile"))
        assertEquals(AudioDirection.MobileToDesktop, audioDirectionFromString("emit"))
        assertEquals(AudioDirection.DesktopToMobile, audioDirectionFromString("receive"))
        assertEquals(AudioDirection.MobileToDesktop, audioDirectionFromString("both"))
    }

    @Test
    fun pcm_buffer_size_uses_frames_channels_and_bytes_per_sample() {
        assertEquals(480 * 1 * 2, pcm16BufferBytes(480, 1))
        assertEquals(480 * 2 * 2, pcm16BufferBytes(480, 2))
        assertEquals(480, audioFrameCount(48_000, 10))
    }

    @Test
    fun android_host_default_is_the_usb_discovery_port_and_stereo() {
        assertEquals(48_123, DEFAULT_HOST_PORT)
        assertEquals(ANDROID_AUDIO_CHANNELS, HostSettings().channels)
        assertEquals(ANDROID_AUDIO_CHANNELS, RelaySettings().channels)
        assertEquals(2, ANDROID_AUDIO_CHANNELS)
    }

    @Test
    fun trusted_auto_connect_policy_requires_explicit_wifi_opt_in() {
        val usb = DiscoveredPeer("host", "Host", "192.168.42.1:48123", "usb")
        val wifi = DiscoveredPeer("host", "Host", "192.168.1.20:48123", "wifi")
        val lan = DiscoveredPeer("host", "Host", "10.0.0.20:48123", "lan")
        assertTrue(trustedAutoConnectAllowed(RelaySettings(), usb))
        assertFalse(trustedAutoConnectAllowed(RelaySettings(), wifi))
        assertTrue(
            trustedAutoConnectAllowed(
                RelaySettings(autoConnectTrustedWifi = true),
                wifi,
            ),
        )
        assertFalse(
            trustedAutoConnectAllowed(
                RelaySettings(autoConnectTrustedWifi = true),
                lan,
            ),
        )
        assertFalse(
            trustedAutoConnectAllowed(
                RelaySettings(autoConnectTrusted = false, autoConnectTrustedWifi = true),
                usb,
            ),
        )
    }

    @Test
    fun trusted_candidate_backoff_is_scoped_to_peer_and_address() {
        val backoff = TrustedCandidateBackoff(maxEntries = 2)
        backoff.noteFailure("peer-a", "192.168.42.1:48123", 0)

        assertFalse(backoff.allowed("peer-a", "192.168.42.1:48123", 1))
        assertTrue(
            backoff.allowed("peer-b", "192.168.42.1:48123", 1),
        )
        assertTrue(backoff.allowed("peer-a", "192.168.42.2:48123", 1))

        backoff.clear("peer-a", "192.168.42.1:48123")
        assertTrue(backoff.allowed("peer-a", "192.168.42.1:48123", 1))
    }

    @Test
    fun trusted_candidate_backoff_expires_and_stays_bounded() {
        val backoff = TrustedCandidateBackoff(maxEntries = 2)
        backoff.noteFailure("peer-a", "a:1", 0)
        assertFalse(backoff.allowed("peer-a", "a:1", 499))
        assertTrue(backoff.allowed("peer-a", "a:1", 500))

        backoff.noteFailure("peer-a", "a:1", 1_000)
        backoff.noteFailure("peer-b", "b:1", 1_000)
        backoff.noteFailure("peer-c", "c:1", 1_000)
        assertEquals(2, backoff.size())
    }

    @Test
    fun credential_bearing_models_do_not_reveal_secrets_in_to_string() {
        val secret = "ab".repeat(32)
        assertFalse(
            TrustedRelayPeer("host", secret).toString().contains(secret),
        )
        assertFalse(HostSettings(pin = "123456").toString().contains("123456"))
        assertFalse(RelaySettings(pin = "123456").toString().contains("123456"))
    }

    @Test
    fun service_geometry_uses_host_settings_for_host_and_client_settings_for_client() {
        val client = RelaySettings(sampleRate = 16_000, channels = 1, frameMs = 60)
        val host = HostSettings(sampleRate = 48_000, channels = 1, frameMs = 5)
        assertEquals(AudioGeometry(48_000, 1, 5), audioGeometryForHostMode(true, client, host))
        assertEquals(AudioGeometry(16_000, 1, 60), audioGeometryForHostMode(false, client, host))
    }
}
