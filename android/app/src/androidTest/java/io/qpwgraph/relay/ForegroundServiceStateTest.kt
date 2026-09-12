package io.qpwgraph.relay

import android.content.ComponentName
import android.content.Context
import android.content.pm.PackageManager
import android.content.pm.ServiceInfo
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class ForegroundServiceStateTest {
    @Test
    fun relay_service_declares_only_the_audio_service_capabilities_it_can_use() {
        val context = ApplicationProvider.getApplicationContext<Context>()
        val component = ComponentName(context, RelayService::class.java)
        @Suppress("DEPRECATION")
        val info = context.packageManager.getServiceInfo(component, PackageManager.GET_META_DATA)

        val types = info.foregroundServiceType
        assertTrue(types and ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE != 0)
        assertTrue(types and ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK != 0)
        assertTrue(types and ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION != 0)
        assertFalse(info.exported)
    }
}
