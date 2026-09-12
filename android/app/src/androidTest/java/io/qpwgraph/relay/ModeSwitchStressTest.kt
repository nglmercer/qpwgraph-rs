package io.qpwgraph.relay

import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class ModeSwitchStressTest {
    @Test
    fun rapid_mode_requests_never_create_a_bidirectional_audio_role() {
        var finalMode = RelayMode.Emitter
        repeat(100) { index ->
            finalMode = if (index % 2 == 0) RelayMode.Receiver else RelayMode.Emitter
            val role = finalMode.androidClientRole()
            assertTrue(isOneWayAudioRole(role))
            assertFalse(clientRoleEmits(role) && clientRoleReceives(role))
        }
        assertEquals(RelayMode.Emitter, finalMode)
    }
}
