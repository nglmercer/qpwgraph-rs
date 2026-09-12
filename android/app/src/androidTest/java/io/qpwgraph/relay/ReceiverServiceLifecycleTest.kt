package io.qpwgraph.relay

import android.app.Application
import androidx.test.core.app.ActivityScenario
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import kotlinx.coroutines.runBlocking
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class ReceiverServiceLifecycleTest {
    @Test
    fun receiver_audio_service_starts_and_stops_without_microphone_permission() {
        val application = ApplicationProvider.getApplicationContext<Application>()
        val controller = RelayServiceController(application)
        // Keep the application in the foreground while exercising the
        // foreground service. This is a service lifecycle test, not a test
        // of Android's background-start restriction.
        val activity = ActivityScenario.launch(MainActivity::class.java)

        try {
            repeat(10) {
                val created = JSONObject(
                    NativeBridge.hostCreateMode(
                        "android-test-host",
                        "android-test-host-id",
                        "[]",
                        "123456",
                        48_123,
                        "pcm",
                        "auto",
                        "receiver",
                        0L,
                        48_000,
                        1,
                        20,
                    ),
                )
                val handle = created.optLong("handle")
                assertEquals("created", created.optString("type"))
                assertTrue(handle > 0L)
                var hostStarted = false
                try {
                    val started = JSONObject(NativeBridge.hostStart(handle))
                    assertEquals("host_started", started.optString("type"))
                    hostStarted = true
                    runBlocking {
                        controller.start(
                            mode = RelayService.MODE_HOST,
                            handle = handle,
                            role = "receive",
                            geometry = AudioGeometry(48_000, 1, 20),
                        )
                        controller.stopAndWait()
                    }
                } finally {
                    if (hostStarted) NativeBridge.hostStop(handle)
                    NativeBridge.hostRelease(handle)
                }
            }
        } finally {
            activity.close()
        }
    }
}
