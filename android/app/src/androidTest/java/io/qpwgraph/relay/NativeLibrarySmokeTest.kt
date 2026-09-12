package io.qpwgraph.relay

import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class NativeLibrarySmokeTest {
    @Test
    fun native_library_loads_for_the_device_abi() {
        assertTrue(NativeRuntime.status.diagnostic, NativeRuntime.available)
    }
}
