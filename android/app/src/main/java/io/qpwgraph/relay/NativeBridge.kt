package io.qpwgraph.relay

/**
 * JNI surface of `crates/pw-graph-relay-android`.
 *
 * Keep this class in the `io.qpwgraph.relay` package with exactly this name:
 * the native symbol names (`Java_io_qpwgraph_relay_NativeBridge_*`) are
 * mangled from it. All interchange is JSON strings; every call returns
 * `{"type":"error","message":...}` on failure. Android PINs are
 * caller-owned values for the requested host/client lifetime; native code
 * neither persists nor silently regenerates them.
 */
internal object NativeBridge {
    init {
        System.loadLibrary("pw_graph_relay_android")
    }

    // Receiver (client) --------------------------------------------------
    external fun create(
        deviceName: String,
        deviceId: String,
        trustedPeersJson: String,
        direction: String,
        generation: Long,
        codec: String,
        transport: String,
        sampleRate: Int,
        channels: Int,
        frameMs: Int,
    ): String

    external fun connect(handle: Long, target: String, pin: String): String
    external fun connectTrusted(
        handle: Long,
        target: String,
        peerId: String,
        secret: String,
    ): String
    /** Credential-only result; normal connected/status JSON never carries it. */
    external fun clientTrustedPeer(handle: Long): String
    external fun removeTrustedPeer(handle: Long, peerId: String): Boolean
    external fun disconnect(handle: Long): Boolean
    external fun pollEvents(handle: Long): String
    external fun clientStatus(handle: Long): String
    /** Feed the separate discovery handle's snapshot to client resume logic. */
    external fun updateClientPeers(handle: Long, peersJson: String): Boolean
    /** Surface a fatal platform-audio failure through the relay event queue. */
    external fun reportError(handle: Long, message: String): Boolean
    external fun offerDirection(
        handle: Long,
        sessionId: Long,
        direction: String,
        generation: Long,
    ): String

    /**
     * Offer `length` samples from the prefix of `samples` to the realtime
     * engine. `length` must be non-negative, no greater than the array size,
     * and no greater than the native realtime quantum limit. Returns `length`
     * only when the complete quantum was accepted; returns zero when the
     * engine is busy, unavailable, or the quantum is oversized.
     *
     * The native layer reuses a buffer per calling thread, but JNI still has
     * to copy the Java array into native memory. Callers must therefore reuse
     * the Java array too when this is used in an audio loop.
     */
    external fun pushCapture(handle: Long, samples: FloatArray, length: Int): Int

    /**
     * Fill the prefix of `output` with available playback samples. Returns
     * zero for no data, a busy engine, or an unavailable handle; otherwise it
     * may return a partial quantum. The native maximum is applied to the
     * output length, and entries after the returned count are untouched.
     */
    external fun pullPlayback(handle: Long, output: FloatArray): Int
    external fun release(handle: Long)

    // Emitter (host) -----------------------------------------------------
    external fun hostCreate(
        deviceName: String,
        deviceId: String,
        trustedPeersJson: String,
        pin: String,
        port: Int,
        codec: String,
        transport: String,
        direction: String,
        generation: Long,
        sampleRate: Int,
        channels: Int,
        frameMs: Int,
    ): String

    external fun hostStart(handle: Long): String
    external fun hostStop(handle: Long): String
    external fun hostPollEvents(handle: Long): String
    external fun hostStatus(handle: Long): String
    external fun hostTrustedEnrollmentSecret(handle: Long, transactionId: Long): String
    external fun hostAcceptTrustedEnrollment(handle: Long, transactionId: Long): Boolean
    external fun hostRejectTrustedEnrollment(handle: Long, transactionId: Long, reason: String): Boolean
    external fun hostRemoveTrustedPeer(handle: Long, peerId: String): Boolean
    external fun hostDisconnectSession(handle: Long, sessionId: Long): String
    /** Surface a fatal platform-audio failure through the host event queue. */
    external fun hostReportError(handle: Long, message: String): Boolean
    external fun hostOfferDirection(
        handle: Long,
        sessionId: Long,
        direction: String,
        generation: Long,
    ): String

    /** Same contract as [pushCapture], using the running host engine. */
    external fun hostPushCapture(handle: Long, samples: FloatArray, length: Int): Int

    /** Same contract as [pullPlayback], using the running host engine. */
    external fun hostPullPlayback(handle: Long, output: FloatArray): Int
    /** Returns false when the native host is still running or transitioning. */
    external fun hostRelease(handle: Long): Boolean

    // Discovery ----------------------------------------------------------
    external fun discoveryCreate(deviceName: String): String
    external fun discoveryStart(handle: Long): String
    external fun discoveryStop(handle: Long): String
    /** Remove only direct USB-probe peers when the tether link disappears. */
    external fun discoveryUsbLinkLost(handle: Long): Boolean
    external fun discoveryPeers(handle: Long): String
    external fun discoveryPollEvents(handle: Long): String
    external fun discoveryRelease(handle: Long)

    // Link detection -----------------------------------------------------
    /** JSON snapshot of the active USB tether link, or `{"type":"none"}`. */
    external fun usbLink(): String

    /** JSON snapshot of all usable local links, ranked best-first. */
    external fun localLinks(): String
}
