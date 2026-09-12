package io.qpwgraph.relay

import androidx.test.ext.junit.runners.AndroidJUnit4
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class NativeBridgeSmokeTest {
    @Test
    fun local_links_returns_valid_json() {
        val result = JSONObject(NativeBridge.localLinks())
        assertEquals("links", result.optString("type"))
        assertTrue(result.has("links"))
    }

    @Test
    fun client_handles_can_be_created_and_released_repeatedly() {
        repeat(100) {
            val created = JSONObject(
                NativeBridge.createMode(
                    "android-test",
                    "android-test-id",
                    "[]",
                    "emitter",
                    0L,
                    "pcm",
                    "auto",
                    48_000,
                    1,
                    20,
                ),
            )
            val handle = created.optLong("handle")
            assertEquals("created", created.optString("type"))
            assertTrue(handle > 0L)
            NativeBridge.release(handle)
        }
    }

    @Test
    fun invalid_client_handle_returns_json_error_instead_of_crashing() {
        val result = JSONObject(NativeBridge.clientStatus(0L))
        assertEquals("error", result.optString("type"))
    }
}
