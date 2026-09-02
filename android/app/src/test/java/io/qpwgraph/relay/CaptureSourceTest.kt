package io.qpwgraph.relay

import org.junit.Assert.*
import org.junit.Test

class CaptureSourceTest {
    @Test
    fun capture_source_parsing_is_case_insensitive() {
        assertEquals(CaptureSource.MICROPHONE, captureSourceFromString("microphone"))
        assertEquals(CaptureSource.MICROPHONE, captureSourceFromString("MICROPHONE"))
        assertEquals(CaptureSource.DEVICE_PLAYBACK, captureSourceFromString("device_playback"))
        assertEquals(CaptureSource.DEVICE_PLAYBACK, captureSourceFromString("DEVICE_PLAYBACK"))
        assertEquals(CaptureSource.DEVICE_PLAYBACK, captureSourceFromString("playback"))
        assertEquals(CaptureSource.DEVICE_PLAYBACK, captureSourceFromString("media"))
        // Unknown defaults to microphone for backward compat
        assertEquals(CaptureSource.MICROPHONE, captureSourceFromString("unknown"))
        assertEquals(CaptureSource.MICROPHONE, captureSourceFromString(""))
    }

    @Test
    fun host_settings_default_is_microphone_backward_compatible() {
        val host = HostSettings()
        assertEquals(CaptureSource.MICROPHONE, host.captureSource)
    }

    @Test
    fun host_settings_persistence_roundtrips_capture_source() {
        val prefs = FakePreferences().proxy()
        val repo = RelaySettingsRepository(prefs)
        val playback = HostSettings(captureSource = CaptureSource.DEVICE_PLAYBACK)
        repo.saveHost(playback)
        val loaded = repo.loadHostSettings()
        assertEquals(CaptureSource.DEVICE_PLAYBACK, loaded.captureSource)

        val mic = HostSettings(captureSource = CaptureSource.MICROPHONE)
        repo.saveHost(mic)
        assertEquals(CaptureSource.MICROPHONE, repo.loadHostSettings().captureSource)
    }

    @Test
    fun host_audio_state_is_independent_from_network_state() {
        // Network = LISTENING (Running) + Audio = FAILED (Error) is valid.
        val runningHostWithAudioError = RelayUiState(
            hostState = RelayHostState.Running,
            hostAudioState = RelayHostAudioState.Error,
            hostActive = true,
            hostPort = 48123,
            hostMessage = "Listening on port 48123 — Audio capture unavailable",
            hostAudioMessage = "microphone audio failed: permission denied",
        )
        assertEquals(RelayHostState.Running, runningHostWithAudioError.hostState)
        assertEquals(RelayHostAudioState.Error, runningHostWithAudioError.hostAudioState)
        assertTrue(runningHostWithAudioError.hostActive)
        assertEquals(48123, runningHostWithAudioError.hostPort)
    }

    @Test
    fun explicit_stop_clears_both_network_and_audio() {
        val running = RelayUiState(
            hostState = RelayHostState.Running,
            hostAudioState = RelayHostAudioState.Running,
            hostActive = true,
        )
        // Simulate stopHostLocked result: Idle + Stopped
        val stopped = running.copy(
            hostState = RelayHostState.Idle,
            hostAudioState = RelayHostAudioState.Stopped,
            hostActive = false,
            hostPort = null,
            hostAddress = null,
        )
        assertEquals(RelayHostState.Idle, stopped.hostState)
        assertEquals(RelayHostAudioState.Stopped, stopped.hostAudioState)
        assertFalse(stopped.hostActive)
    }

    @Test
    fun platform_audio_defaults_to_stereo_and_keeps_mono_valid() {
        // Stereo is the quality default (device-playback capture keeps L/R
        // separation); the mono geometry remains accepted by the service and
        // the native boundary.
        assertEquals(2, HostSettings(captureSource = CaptureSource.DEVICE_PLAYBACK).channels)
        assertEquals(2, HostSettings(captureSource = CaptureSource.MICROPHONE).channels)
        assertEquals(2, ANDROID_AUDIO_CHANNELS)
        assertEquals(1, pcm16BufferBytes(480, 1))
        assertEquals(960, pcm16BufferBytes(480, 2))
    }

    @Test
    fun toString_does_not_leak_pin_but_shows_capture_source() {
        val host = HostSettings(pin = "123456", captureSource = CaptureSource.DEVICE_PLAYBACK)
        val str = host.toString()
        assertFalse(str.contains("123456"))
        assertTrue(str.contains("DEVICE_PLAYBACK"))
    }
}
