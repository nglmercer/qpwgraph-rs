package io.qpwgraph.relay

import android.os.Build

private const val NATIVE_LIBRARY_NAME = "pw_graph_relay_android"

/** Snapshot of native availability that is safe to expose to the UI. */
data class NativeRuntimeStatus(
    val available: Boolean,
    val diagnostic: String,
    val supportedAbis: List<String>,
)

/**
 * Load the JNI library behind a controlled boundary. Keeping this function
 * injectable makes the missing-library path testable on the JVM without
 * loading Android's native library there.
 */
fun loadNativeRuntime(
    loadLibrary: () -> Unit,
    supportedAbis: List<String>,
): NativeRuntimeStatus {
    val abis = supportedAbis.ifEmpty { listOf("unknown") }
    return try {
        loadLibrary()
        NativeRuntimeStatus(
            available = true,
            diagnostic = "",
            supportedAbis = abis,
        )
    } catch (error: Throwable) {
        val expected = abis.joinToString(separator = ", ") {
            "lib/$it/lib$NATIVE_LIBRARY_NAME.so"
        }
        val reason = error.message?.takeIf { it.isNotBlank() }
            ?: error::class.java.simpleName
        NativeRuntimeStatus(
            available = false,
            diagnostic = "Native relay library unavailable.\n" +
                "Device ABI: ${abis.joinToString(", ")}\n" +
                "Expected: $expected\n" +
                "Reason: $reason",
            supportedAbis = abis,
        )
    }
}

/** Process-wide JNI availability boundary. No JNI call is valid when false. */
internal object NativeRuntime {
    val status: NativeRuntimeStatus = loadNativeRuntime(
        loadLibrary = { System.loadLibrary(NATIVE_LIBRARY_NAME) },
        supportedAbis = runCatching { Build.SUPPORTED_ABIS.toList() }.getOrDefault(emptyList()),
    )

    val available: Boolean get() = status.available
    val diagnostic: String get() = status.diagnostic

    fun requireAvailable() {
        check(available) { diagnostic }
    }
}
