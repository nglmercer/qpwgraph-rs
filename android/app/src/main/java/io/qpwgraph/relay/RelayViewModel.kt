package io.qpwgraph.relay

import android.Manifest
import android.app.Activity
import android.app.Application
import android.content.Intent
import android.content.pm.PackageManager
import android.util.Log
import androidx.core.content.ContextCompat
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.Job
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.collect
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull
import org.json.JSONArray
import org.json.JSONObject

/**
 * Single state holder for both user-selectable audio directions.
 *
 * The phone-to-PC client and PC-to-phone host each own a native handle, an audio
 * foreground service, and a 100 ms polling job that drains native events.
 * Discovery owns a third handle and polls on a slower cadence because the
 * peer snapshot replaces the whole list every tick.
 */
class RelayViewModel(application: Application) : AndroidViewModel(application) {
    // Keep this name aligned with backup_rules.xml/data_extraction_rules.xml:
    // sharedpref/relay.xml is intentionally excluded from backup and transfer.
    private val preferences = application.getSharedPreferences(
        TrustedCredentialStore.PREFERENCES_NAME,
        0,
    )
    private val settings = RelaySettingsRepository(preferences)
    private val trustedStore = TrustedPeerRepository(TrustedCredentialStore(preferences))
    private val service = RelayServiceController(application)
    private val deviceId = settings.deviceId
    private val initialSettings = settings.loadSettings()
    private val mutableState = MutableStateFlow(
        RelayUiState(
            direction = initialSettings.direction,
            settings = initialSettings,
            host = settings.loadHostSettings(),
        ),
    )
    /** Pending MediaProjection consent for device-playback capture. */
    @Volatile private var pendingMediaProjectionResultCode: Int = Activity.RESULT_CANCELED
    @Volatile private var pendingMediaProjectionData: Intent? = null
    val state: StateFlow<RelayUiState> = mutableState.asStateFlow()
    // The native handles and their teardown rules live in the controllers.
    // This class owns the UI state, the operation mutex that serializes the
    // two roles against each other, and nothing else about them.
    private val client = ClientController(viewModelScope, service)
    private val host = HostController(viewModelScope, service)
    private var usbPolling: Job? = null
    private var serviceEvents: Job? = null
    private val operationMutex = Mutex()
    private val directionRequestLock = Any()
    private var directionSwitchJob: Job? = null
    private var pendingDirection: AudioDirection? = null
    @Volatile private var directionWaiter: CompletableDeferred<DirectionResolution>? = null
    @Volatile private var directionWaitSessionId: Long? = null
    @Volatile private var directionWaitGeneration: Long = -1L
    private var usbWasPresent = false
    private var lastTrustedAutoAttemptAt = 0L
    private val trustedCandidateBackoff = TrustedCandidateBackoff()
    private val discovery = DiscoveryController(
        application = application,
        scope = viewModelScope,
        trusted = trustedStore,
    ) { snapshot -> onDiscoverySnapshot(snapshot) }

    init {
        settings.purgeLegacyPins()
        setState { it.copy(trustedPeers = trustedStore.summaries()) }
        serviceEvents = viewModelScope.launch(Dispatchers.IO) {
            RelayServiceBridge.events.collect { event ->
                handleServiceEvent(event)
            }
        }
        startUsbPolling()
    }

    private fun setState(transform: (RelayUiState) -> RelayUiState) {
        mutableState.value = transform(mutableState.value)
    }

    private fun text(id: Int, vararg args: Any): String =
        getApplication<Application>().getString(id, *args)

    private fun hasMicrophonePermission(): Boolean = ContextCompat.checkSelfPermission(
        getApplication(),
        Manifest.permission.RECORD_AUDIO,
    ) == PackageManager.PERMISSION_GRANTED

    /** Reading failures surface once, then degrade to an empty record. */
    private fun trustedPeers(): List<TrustedRelayPeer> =
        trustedStore.peers()
            .onFailure { error ->
                setState {
                    it.copy(message = error.message ?: text(R.string.relay_error_host_failed))
                }
            }
            .getOrDefault(emptyList())

    /**
     * Apply a store outcome to the UI: refresh the summaries on success,
     * surface the reason on failure, stay quiet when there was nothing to
     * store. Returns whether the credential is now persisted.
     */
    private fun applySaved(saved: TrustedPeerRepository.Saved): Boolean {
        when (saved) {
            TrustedPeerRepository.Saved.Stored ->
                setState { it.copy(trustedPeers = trustedStore.summaries()) }
            is TrustedPeerRepository.Saved.Failed ->
                setState { it.copy(message = saved.message) }
            TrustedPeerRepository.Saved.Skipped -> Unit
        }
        return saved.stored
    }

    private fun saveTrustedPeer(
        peerId: String,
        secret: String,
        name: String = "",
        address: String = "",
    ): Boolean = applySaved(trustedStore.save(peerId, secret, name, address))

    private fun rememberTrustedPeerFromJson(event: JSONObject) {
        applySaved(trustedStore.saveFrom(event))
    }

    private fun rememberTrustedPeerFromConnected(response: JSONObject) {
        response.optJSONObject("trusted_peer")?.let { rememberTrustedPeerFromJson(it) }
    }

    private fun rememberTrustedPeerFromNative() {
        client.trustedCredential()?.let { credential ->
            saveTrustedPeer(
                peerId = credential.optString("peer_id"),
                secret = credential.optString("secret"),
            )
        }
    }

    private fun removeStoredTrustedPeer(peerId: String): Boolean =
        trustedStore.remove(peerId)
            .onFailure { error ->
                setState {
                    it.copy(
                        message = error.message
                            ?: text(R.string.relay_error_trusted_revocation),
                    )
                }
            }
            .isSuccess

    /**
     * Change the user-facing direction as one serialized lifecycle
     * transaction. A second tap while a switch is in flight is coalesced to
     * the newest requested direction, so a quick A → B → A gesture cannot
     * leave the app running the stale middle direction.
     */
    fun setDirection(next: AudioDirection) {
        synchronized(directionRequestLock) {
            val current = mutableState.value
            if (!current.switchingDirection && current.direction == next) return
            pendingDirection = next
            if (directionSwitchJob?.isActive == true) return
            directionSwitchJob = viewModelScope.launch(Dispatchers.IO) {
                setState { it.copy(switchingDirection = true) }
                try {
                    while (true) {
                        val requested = synchronized(directionRequestLock) {
                            pendingDirection.also { pendingDirection = null }
                        } ?: break
                        val plan = operationMutex.withLock {
                            val old = mutableState.value.direction
                            if (old == requested) {
                                null
                            } else {
                                val wasClientLive = client.isOpen ||
                                    mutableState.value.connection == RelayConnectionState.Connected ||
                                    mutableState.value.connection == RelayConnectionState.Connecting
                                val wasHostLive = host.isOpen ||
                                    mutableState.value.hostState == RelayHostState.Running ||
                                    mutableState.value.hostState == RelayHostState.Starting
                                val generation = nextDirectionGeneration(
                                    mutableState.value.settings.directionGeneration,
                                )
                                val updatedSettings = mutableState.value.settings.copy(
                                    direction = requested,
                                    directionGeneration = generation,
                                )
                                setState {
                                    it.copy(
                                        direction = requested,
                                        settings = updatedSettings,
                                        switchingDirection = true,
                                    )
                                }
                                settings.save(updatedSettings)

                                val activeSession = when (old) {
                                    AudioDirection.MobileToDesktop ->
                                        mutableState.value.sessionId?.takeIf { client.isOpen }
                                    AudioDirection.DesktopToMobile ->
                                        mutableState.value.sessions.firstOrNull()?.id
                                            ?.takeIf { host.isOpen }
                                }
                                if (activeSession == null) {
                                    when (old) {
                                        AudioDirection.MobileToDesktop -> stopClientLocked()
                                        AudioDirection.DesktopToMobile -> stopHostLocked()
                                    }
                                    DirectionResumePlan(
                                        startNewSide = wasClientLive || wasHostLive,
                                        previous = old,
                                        direction = requested,
                                        generation = generation,
                                    )
                                } else {
                                    // Keep the old endpoint alive until the
                                    // authenticated peer acknowledges the new
                                    // direction. The resolved event is the
                                    // commit point for the local teardown.
                                    val waiter = CompletableDeferred<DirectionResolution>()
                                    directionWaiter = waiter
                                    directionWaitSessionId = activeSession
                                    directionWaitGeneration = generation
                                    val response = when (old) {
                                        AudioDirection.MobileToDesktop ->
                                            client.offerDirection(activeSession, requested, generation)
                                        AudioDirection.DesktopToMobile ->
                                            host.offerDirection(activeSession, requested, generation)
                                    }
                                    if (response.optString("type") == "error") {
                                        directionWaiter = null
                                        directionWaitSessionId = null
                                        directionWaitGeneration = -1L
                                        setState {
                                            it.copy(message = response.optString("message"))
                                        }
                                        when (old) {
                                            AudioDirection.MobileToDesktop -> stopClientLocked()
                                            AudioDirection.DesktopToMobile -> stopHostLocked()
                                        }
                                        DirectionResumePlan(
                                            startNewSide = wasClientLive || wasHostLive,
                                            previous = old,
                                            direction = requested,
                                            generation = generation,
                                        )
                                    } else {
                                        DirectionResumePlan(
                                            startNewSide = wasClientLive || wasHostLive,
                                            previous = old,
                                            direction = requested,
                                            generation = generation,
                                            sessionId = activeSession,
                                            waiter = waiter,
                                            peer = mutableState.value.sessions.firstOrNull {
                                                it.id == activeSession
                                            },
                                        )
                                    }
                                }
                            }
                        }

                        if (plan != null) {
                            val resolution = plan.waiter?.let { waiter ->
                                awaitDirectionResolution(plan.sessionId!!, plan.generation, waiter)
                            }
                            val resolvedDirection = resolution?.direction ?: plan.direction
                            val resolvedGeneration = resolution?.generation ?: plan.generation
                            val resume = operationMutex.withLock {
                                if (plan.waiter != null) {
                                    when (plan.previous) {
                                        AudioDirection.MobileToDesktop -> stopClientLocked()
                                        AudioDirection.DesktopToMobile -> stopHostLocked()
                                    }
                                }
                                val currentSettings = mutableState.value.settings
                                val resolvedSettings = currentSettings.copy(
                                    direction = resolvedDirection,
                                    directionGeneration = maxOf(
                                        currentSettings.directionGeneration,
                                        resolvedGeneration,
                                    ),
                                )
                                setState {
                                    it.copy(
                                        direction = resolvedDirection,
                                        settings = resolvedSettings,
                                        switchingDirection = true,
                                    )
                                }
                                settings.save(resolvedSettings)
                                plan.copy(
                                    direction = resolvedDirection,
                                    generation = resolvedGeneration,
                                    peer = plan.peer,
                                )
                            }
                            if (resume.startNewSide) {
                                when (resume.direction) {
                                    AudioDirection.MobileToDesktop -> {
                                        val peer = resume.peer
                                        if (resume.previous == AudioDirection.DesktopToMobile && peer != null) {
                                            reconnectHostPeer(peer)
                                        } else {
                                            reconnectConfiguredOrTrusted()
                                        }
                                    }
                                    AudioDirection.DesktopToMobile -> startHost()
                                }
                            }
                        }
                    }
                } finally {
                    setState { it.copy(switchingDirection = false) }
                }
            }
        }
    }

    private data class DirectionResumePlan(
        val startNewSide: Boolean,
        val previous: AudioDirection,
        val direction: AudioDirection,
        val generation: Long,
        val sessionId: Long? = null,
        val waiter: CompletableDeferred<DirectionResolution>? = null,
        val peer: RelaySessionInfo? = null,
    )

    private fun nextDirectionGeneration(current: Long): Long =
        if (current == Long.MAX_VALUE) Long.MAX_VALUE else current + 1L

    private suspend fun awaitDirectionResolution(
        sessionId: Long,
        generation: Long,
        waiter: CompletableDeferred<DirectionResolution>,
    ): DirectionResolution? {
        val resolution = withTimeoutOrNull(DIRECTION_SWITCH_TIMEOUT_MS) {
            waiter.await()
        }?.takeIf { it.sessionId == sessionId && it.generation >= generation }
        synchronized(directionRequestLock) {
            if (directionWaiter === waiter) {
                directionWaiter = null
                directionWaitSessionId = null
                directionWaitGeneration = -1L
            }
        }
        if (resolution == null) {
            setState { it.copy(message = text(R.string.relay_direction_switch_timeout)) }
        }
        return resolution
    }

    private fun completeDirectionResolution(event: JSONObject) {
        val sessionId = event.optLong("session")
        val generation = event.optLong("generation", -1L)
        val direction = audioDirectionFromString(event.optString("direction"))
        if (sessionId == directionWaitSessionId && generation >= directionWaitGeneration) {
            directionWaiter?.let { waiter ->
                if (!waiter.isCompleted) {
                    waiter.complete(DirectionResolution(sessionId, direction, generation))
                }
            }
            return
        }

        // The other peer may have initiated the switch. There is no local
        // waiter in that case, but an authenticated resolution is still the
        // commit point for adopting its direction. `setDirection` serializes
        // the teardown/restart and coalesces it with any local tab gesture.
        val current = mutableState.value
        if (generation < current.settings.directionGeneration) return
        if (direction != current.direction) {
            setDirection(direction)
        } else if (generation > current.settings.directionGeneration) {
            val updated = current.settings.copy(directionGeneration = generation)
            setState { it.copy(settings = updated) }
            settings.save(updated)
        }
    }

    private fun reconnectHostPeer(peer: RelaySessionInfo) {
        val discovered = mutableState.value.peers.firstOrNull {
            it.address == peer.address
        }
        if (discovered != null && trustedStore.peer(discovered.id) != null) {
            connectToTrustedPeer(discovered)
            return
        }
        val updated = mutableState.value.settings.copy(target = peer.address)
        update(updated)
        if (updated.pin.isNotBlank()) connectInternal(null)
        else setState { it.copy(message = text(R.string.relay_validation_missing_pin)) }
    }

    /** Called by the Activity when a required runtime permission was denied. */
    fun permissionDenied(host: Boolean) {
        if (host) {
            setState {
                it.copy(
                    hostState = RelayHostState.Error,
                    hostAudioState = RelayHostAudioState.Error,
                    hostAudioMessage = text(R.string.relay_error_microphone_permission),
                    hostPort = null,
                    hostActive = false,
                    hostAddress = null,
                    hostMessage = text(R.string.relay_error_microphone_permission),
                )
            }
        } else {
            setState {
                it.copy(
                    connection = RelayConnectionState.Error,
                    sessionId = null,
                    hostName = "",
                    transport = "",
                    link = "",
                    audioChannelState = "",
                    message = text(R.string.relay_error_microphone_permission),
                )
            }
        }
    }

    fun setHostCaptureSource(source: CaptureSource) {
        if (mutableState.value.hostState == RelayHostState.Starting ||
            mutableState.value.hostState == RelayHostState.Running
        ) return
        val updated = mutableState.value.host.copy(captureSource = source)
        setState { it.copy(host = updated) }
        settings.saveHost(updated)
    }

    fun onMediaProjectionResult(resultCode: Int, data: Intent?) {
        pendingMediaProjectionResultCode = resultCode
        pendingMediaProjectionData = data
        if (resultCode != Activity.RESULT_OK || data == null) {
            Log.w(TAG, "HOST AUDIO FAILURE MediaProjection permission denied")
            setState {
                it.copy(
                    hostAudioState = RelayHostAudioState.Error,
                    hostAudioMessage = text(R.string.relay_error_media_projection_denied),
                    hostMessage = text(R.string.relay_error_media_projection_denied),
                )
            }
        } else {
            Log.i(TAG, "MediaProjection consent granted")
        }
    }

    fun hasMediaProjectionConsent(): Boolean =
        pendingMediaProjectionResultCode == Activity.RESULT_OK && pendingMediaProjectionData != null

    // ------------------------------------------------------------------
    // Receiver (client)
    // ------------------------------------------------------------------

    fun update(updated: RelaySettings) {
        val previousDirection = mutableState.value.direction
        setState { it.copy(settings = updated) }
        settings.save(updated)
        if (updated.direction != previousDirection) {
            setDirection(updated.direction)
        }
    }

    fun forgetTrustedPeer(peerId: String) {
        viewModelScope.launch(Dispatchers.IO) {
            operationMutex.withLock {
                // A false native result means the handle is stale/unknown,
                // not that revocation succeeded. Keep the encrypted record
                // until every live owner has acknowledged removal.
                val clientRevoked = !client.isOpen || client.removeTrustedPeer(peerId)
                val hostRevoked = !host.isOpen || host.removeTrustedPeer(peerId)
                if (!clientRevoked || !hostRevoked || !removeStoredTrustedPeer(peerId)) {
                    setState {
                        it.copy(message = text(R.string.relay_error_trusted_revocation))
                    }
                    return@withLock
                }
                setState { it.copy(trustedPeers = trustedStore.summaries()) }
            }
        }
    }

    fun connect() {
        val settings = mutableState.value.settings
        if (settings.direction != AudioDirection.MobileToDesktop) {
            setState {
                it.copy(message = text(R.string.relay_direction_host_required))
            }
            return
        }
        if (clientNeedsMicrophone(settings.direction) && !hasMicrophonePermission()) {
            permissionDenied(host = false)
            return
        }
        if (settings.target.isBlank()) {
            setState {
                it.copy(
                    connection = RelayConnectionState.Error,
                    message = text(R.string.relay_validation_missing_target),
                )
            }
            return
        }
        if (settings.pin.isBlank()) {
            setState {
                it.copy(
                    connection = RelayConnectionState.Error,
                    message = text(R.string.relay_validation_missing_pin),
                )
            }
            return
        }
        connectInternal(null)
    }

    /** Connect to a discovered peer with its previously enrolled credential. */
    fun connectToTrustedPeer(peer: DiscoveredPeer) {
        if (mutableState.value.direction != AudioDirection.MobileToDesktop) {
            setState { it.copy(message = text(R.string.relay_direction_host_required)) }
            return
        }
        // This is an explicit user action, so it may retry this candidate
        // immediately even while automatic reconnect has it backed off.
        val trusted = trustedStore.peer(peer.id) ?: return
        update(mutableState.value.settings.copy(target = peer.address))
        connectInternal(trusted)
    }

    private fun connectInternal(trusted: TrustedRelayPeer?) {
        if (mutableState.value.connection == RelayConnectionState.Connecting) return
        val settings = mutableState.value.settings
        if (settings.direction != AudioDirection.MobileToDesktop) return
        if (clientNeedsMicrophone(settings.direction) && !hasMicrophonePermission()) {
            permissionDenied(host = false)
            return
        }
        if (settings.target.isBlank()) {
            setState {
                it.copy(
                    connection = RelayConnectionState.Error,
                    message = text(R.string.relay_validation_missing_target),
                )
            }
            return
        }
        if (trusted == null && settings.pin.isBlank()) {
            setState {
                it.copy(
                    connection = RelayConnectionState.Error,
                    message = text(R.string.relay_validation_missing_pin),
                )
            }
            return
        }
        setState {
            it.copy(
                connection = RelayConnectionState.Connecting,
                message = if (trusted == null) {
                    text(R.string.relay_connecting)
                } else {
                    text(R.string.relay_trusted_connecting)
                },
            )
        }
        viewModelScope.launch(Dispatchers.IO) {
            operationMutex.withLock {
                if (mutableState.value.direction != AudioDirection.MobileToDesktop) {
                    return@withLock
                }
                var nativeConnected = false
                try {
                    // RelayService has one audio-pump instance. Stop the host
                    // before connecting a client so no live worker can observe
                    // a different mode or native handle.
                    if (mutableState.value.hostState == RelayHostState.Running ||
                        mutableState.value.hostState == RelayHostState.Starting
                    ) {
                        stopHostLocked()
                    }
                    client.open(settings, deviceId, trustedStore.credentialsJson()) {
                        text(R.string.relay_error_native_create)
                    }
                    val response = if (trusted == null) {
                        client.connect(settings.target, settings.pin)
                    } else {
                        client.connectTrusted(settings.target, trusted)
                    }
                    if (response.optString("type") == "error") {
                        val message = response.optString("message")
                        if (response.optString("code") == "unknown_client_handle") {
                            client.forgetHandle()
                        }
                        if (trusted != null) {
                            noteTrustedCandidateFailure(trusted.peerId, settings.target)
                        }
                        clientError(message)
                        return@withLock
                    }
                    require(response.optString("type") == "connected") {
                        "native connection returned an unexpected response"
                    }
                    val session = response.optLong("session")
                    require(session != 0L) { "native connection returned no session id" }
                    val host = response.optString("host").ifBlank { "Unknown host" }
                    // The credential accessor is deliberately separate from
                    // normal connection/status JSON. It returns a secret only
                    // to this persistence path after the host acknowledged it.
                    if (trusted == null) {
                        rememberTrustedPeerFromNative()
                    } else {
                        trustedCandidateBackoff.clear(trusted.peerId, settings.target)
                    }
                    nativeConnected = true

                    // Do not publish Connected until the foreground audio
                    // service has initialized every requested worker.
                    service.start(
                        RelayService.MODE_CLIENT,
                        client.nativeHandle,
                        settings.direction.androidClientRole(),
                        audioGeometryForHostMode(
                            hostMode = false,
                            client = settings,
                            host = mutableState.value.host,
                        ),
                        captureSource = settings.captureSource,
                        mediaProjectionResultCode = pendingMediaProjectionResultCode,
                        mediaProjectionData = pendingMediaProjectionData,
                    )
                    setState {
                        it.copy(
                            connection = RelayConnectionState.Connected,
                            hostName = host,
                            sessionId = session,
                            message = text(R.string.relay_connected),
                        )
                    }
                    startClientPolling()
                } catch (error: Exception) {
                    // Stop the platform workers before invalidating the native
                    // handle they are polling. The service owns the same
                    // handle and must be quiescent before disconnect/release.
                    withContext(NonCancellable) {
                        if (nativeConnected && !error.serviceWasAlreadyActive) {
                            service.stopAndWait()
                        }
                        if (nativeConnected) client.releaseNow()
                    }
                    if (trusted != null) {
                        noteTrustedCandidateFailure(trusted.peerId, settings.target)
                    }
                    clientError(error.message ?: text(R.string.relay_error_connect_failed))
                }
            }
        }
    }

    /** Resume the client side after a direction switch when it was live. */
    private fun reconnectConfiguredOrTrusted() {
        if (mutableState.value.direction != AudioDirection.MobileToDesktop) return
        val current = mutableState.value
        if (current.settings.target.isNotBlank() && current.settings.pin.isNotBlank()) {
            connectInternal(null)
            return
        }
        val trustedPeer = current.peers.firstOrNull { peer ->
            peer.id.isNotBlank() && trustedStore.peer(peer.id) != null
        }
        if (trustedPeer != null) connectToTrustedPeer(trustedPeer)
    }

    /** Discovery tap-to-connect: adopt the peer address, then connect. */
    fun connectToPeer(address: String) {
        if (mutableState.value.direction != AudioDirection.MobileToDesktop) {
            setState { it.copy(message = text(R.string.relay_direction_host_required)) }
            return
        }
        val discovered = mutableState.value.peers.firstOrNull { it.address == address }
        if (discovered != null && trustedStore.peer(discovered.id) != null) {
            connectToTrustedPeer(discovered)
            return
        }
        update(mutableState.value.settings.copy(target = address))
        connect()
    }

    fun disconnect() {
        viewModelScope.launch(Dispatchers.IO) {
            operationMutex.withLock {
                stopClientLocked()
                setState {
                    it.copy(
                        connection = RelayConnectionState.Disconnected,
                        sessionId = null,
                        hostName = "",
                        message = text(R.string.relay_disconnected),
                        rms = 0f,
                    )
                }
            }
        }
    }

    private fun startClientPolling() {
        client.startPolling(
            onEvents = ::consumeClientEvents,
            onStatus = ::applyClientStatus,
            onError = { error ->
                clientError(error.message ?: text(R.string.relay_error_connect_failed))
            },
        )
    }

    private suspend fun consumeClientEvents(raw: String): Boolean {
        // Native returns a JSON object for an invalidated handle, while a
        // healthy poll returns an array. Handle that shape explicitly so a
        // service that died before its ServiceStopped event is observed still
        // clears the stale ViewModel handle.
        RelayJson.pollError(raw)?.let { failure ->
            if (failure.unknownHandle) {
                operationMutex.withLock { client.forgetHandle() }
            }
            clientError(failure.message)
            return !failure.unknownHandle
        }
        val events = JSONArray(raw)
        var sessionLost = false
        for (index in 0 until events.length()) {
            val event = events.getJSONObject(index)
            when (event.optString("type")) {
                "connected" -> {
                    rememberTrustedPeerFromConnected(event)
                    setState {
                        it.copy(
                            connection = RelayConnectionState.Connected,
                            hostName = event.optString("host"),
                            sessionId = event.optLong("session"),
                            message = text(R.string.relay_connected),
                        )
                    }
                }

                "disconnected" -> {
                    sessionLost = true
                    setState {
                        it.copy(
                            connection = RelayConnectionState.Disconnected,
                        sessionId = null,
                        hostName = "",
                        transport = "",
                        link = "",
                        audioChannelState = "",
                        message = event.optString("message"),
                    )
                    }
                }

                "level" -> setState {
                    it.copy(rms = event.optDouble("rms").toFloat().coerceIn(0f, 1f))
                }

                "trusted_peer" -> rememberTrustedPeerFromJson(event)
                "trusted_peer_available" -> rememberTrustedPeerFromNative()
                "direction_resolved" -> completeDirectionResolution(event)
                "error" -> clientError(event.optString("message"))
            }
        }
        if (!sessionLost || mutableState.value.connection != RelayConnectionState.Disconnected) {
            return true
        }

        // A native SessionLost leaves the SDK client object in its registry
        // until the embedding releases it. Releasing only the UI state would
        // make a later trusted auto-connect reuse a Connected native handle
        // and receive "client is already connected" forever. Quiesce the
        // foreground service first, then retire the native handle. This runs
        // on the polling coroutine, so it must not cancel-and-join itself.
        operationMutex.withLock {
            if (client.isOpen &&
                mutableState.value.connection == RelayConnectionState.Disconnected
            ) {
                service.stopAndWait()
                client.releaseNow()
            }
        }
        return false
    }

    private fun clientError(message: String) {
        val mapped = when {
            message.contains("No route to host", ignoreCase = true)
                || message.contains("os error 113")
                || message.contains("os error 101") ->
                text(R.string.relay_error_no_route) + if (message.isNotBlank()) " — $message" else ""
            message.contains("Connection refused", ignoreCase = true) || message.contains("os error 111") ->
                text(R.string.relay_error_tcp_refused) + if (message.isNotBlank()) " — $message" else ""
            message.contains("trusted", ignoreCase = true) && message.contains("fail", ignoreCase = true) ->
                text(R.string.relay_error_trusted_auth_failed) + " — $message"
            message.contains("Relay session lost", ignoreCase = true) && message.contains("os error 111") ->
                text(R.string.relay_error_tcp_refused) + " — $message"
            else -> message
        }
        setState {
            it.copy(
                connection = RelayConnectionState.Error,
                sessionId = null,
                hostName = "",
                rms = 0f,
                transport = "",
                link = "",
                audioChannelState = "",
                message = mapped,
            )
        }
    }

    private fun applyClientStatus(status: JSONObject) {
        val session = status.optJSONArray("sessions")?.let { sessions ->
            if (sessions.length() > 0) sessions.optJSONObject(0) else null
        }
        setState {
            it.copy(
                transport = session?.optString("transport").orEmpty(),
                link = session?.optString("link").orEmpty(),
                audioChannelState = session?.optString("audio_channel_state").orEmpty(),
            )
        }
    }

    // ------------------------------------------------------------------
    // Emitter (host)
    // ------------------------------------------------------------------

    fun updateHost(host: HostSettings) {
        // The native host and its QR/PIN describe one immutable hosting
        // session. Do not let text-field edits make the UI advertise a
        // different PIN, port, or geometry while that session is live.
        if (mutableState.value.hostState == RelayHostState.Starting ||
            mutableState.value.hostState == RelayHostState.Running
        ) {
            return
        }
        setState { it.copy(host = host) }
        settings.saveHost(host)
    }

    fun startHost() {
        if (mutableState.value.direction != AudioDirection.DesktopToMobile) {
            setState { it.copy(message = text(R.string.relay_direction_client_required)) }
            return
        }
        if (mutableState.value.hostState == RelayHostState.Starting ||
            mutableState.value.hostState == RelayHostState.Running
        ) {
            return
        }
        var wanted = mutableState.value.host
        if (wanted.pin.isBlank()) {
            val newPin = (100000..999999).random().toString()
            wanted = wanted.copy(pin = newPin)
            setState { it.copy(host = wanted) }
            settings.saveHost(wanted)
        }
        setState {
            it.copy(
                hostState = RelayHostState.Starting,
                hostAudioState = RelayHostAudioState.Starting,
                hostAudioMessage = "",
                hostMessage = text(R.string.relay_host_starting),
            )
        }
        Log.i(TAG, "HOST START pin present, transport=${wanted.transport} captureSource=${wanted.captureSource} port=${wanted.port}")
        viewModelScope.launch(Dispatchers.IO) {
            operationMutex.withLock {
                if (mutableState.value.direction != AudioDirection.DesktopToMobile) {
                    return@withLock
                }
                var nativeStarted = false
                var nativePort: Int? = null
                var nativeAddress: String? = null
                try {
                    if (mutableState.value.connection == RelayConnectionState.Connected ||
                        mutableState.value.connection == RelayConnectionState.Connecting
                    ) {
                        stopClientLocked()
                    }
                    if (host.isOpen &&
                        (!host.preparedFor(
                            wanted,
                            mutableState.value.direction,
                            mutableState.value.settings.directionGeneration,
                        ) ||
                            mutableState.value.hostState == RelayHostState.Error)
                    ) {
                        service.stopAndWait()
                        if (!host.stopAndRelease()) {
                            throw IllegalStateException("previous relay host is still running")
                        }
                    }
                    host.open(
                        wanted,
                        mutableState.value.direction,
                        mutableState.value.settings.directionGeneration,
                        deviceId,
                        trustedStore.credentialsJson(),
                    ) { text(R.string.relay_error_native_create) }
                    val response = host.start()
                    if (response.optString("type") != "host_started") {
                        hostError(response.optString("message"))
                        return@withLock
                    }
                    val port = response.optInt("port")
                    val address = response.optString("address")
                        .takeIf { it.isNotBlank() }
                    nativeStarted = true
                    nativePort = port
                    nativeAddress = address
                    Log.i(TAG, "HOST LISTENING port=$port address=$address transport=${wanted.transport} captureSource=${wanted.captureSource}")

                    try {
                        service.start(
                            RelayService.MODE_HOST,
                            host.nativeHandle,
                            AudioDirection.DesktopToMobile.androidClientRole(),
                            audioGeometryForHostMode(
                                hostMode = true,
                                client = mutableState.value.settings,
                                host = wanted,
                            ),
                            captureSource = wanted.captureSource,
                            mediaProjectionResultCode = pendingMediaProjectionResultCode,
                            mediaProjectionData = pendingMediaProjectionData,
                        )
                        Log.i(TAG, "HOST AUDIO START success captureSource=${wanted.captureSource}")
                        setState {
                            it.copy(
                                hostState = RelayHostState.Running,
                                hostAudioState = RelayHostAudioState.Running,
                                hostAudioMessage = "",
                                hostPort = port,
                                hostActive = true,
                                hostAddress = address,
                                hostMessage = text(R.string.relay_listening, port),
                            )
                        }
                    } catch (audioError: Exception) {
                        // Decouple: keep TCP host listening even if audio failed.
                        Log.w(TAG, "HOST AUDIO FAILURE keeping host listening: ${audioError.message}")
                        if (!audioError.serviceWasAlreadyActive) {
                            // Do not stop native host; audio service may have already stopped itself.
                            // Ensure any partially started service is quiesced but keep host.
                            runCatching { withContext(NonCancellable) { service.stopAndWait() } }
                        }
                        setState {
                            it.copy(
                                hostState = RelayHostState.Running,
                                hostAudioState = RelayHostAudioState.Error,
                                hostAudioMessage = audioError.message ?: text(R.string.relay_error_host_audio_failed),
                                hostPort = port,
                                hostActive = true,
                                hostAddress = address,
                                hostMessage = text(R.string.relay_listening, port) + " — " + text(R.string.relay_error_host_audio_failed),
                            )
                        }
                    }
                    startHostPolling()
                } catch (error: Exception) {
                    withContext(NonCancellable) {
                        if (nativeStarted && !error.serviceWasAlreadyActive) {
                            service.stopAndWait()
                        }
                        if (nativeStarted) host.stopAndRelease()
                    }
                    // If we already published Running with audio error, don't overwrite with Error.
                    if (nativePort != null && mutableState.value.hostState == RelayHostState.Running) {
                        Log.w(TAG, "host start audio path already handled, ignoring error ${error.message}")
                    } else {
                        hostError(error.message ?: text(R.string.relay_error_host_failed))
                    }
                }
            }
        }
    }

    fun regenerateHostPin() {
        val newPin = (100000..999999).random().toString()
        val updated = mutableState.value.host.copy(pin = newPin)
        setState { it.copy(host = updated) }
        settings.saveHost(updated)
    }

    fun stopHost() {
        viewModelScope.launch(Dispatchers.IO) {
            operationMutex.withLock {
                try {
                    stopHostLocked()
                    setState {
                        it.copy(
                            hostState = RelayHostState.Idle,
                            hostPort = null,
                            hostActive = false,
                            hostAddress = null,
                            hostMessage = text(R.string.relay_host_stopped),
                            hostRms = 0f,
                            sessions = emptyList(),
                            // Keep last PIN for reuse – user can refresh via button.
                            host = it.host,
                        )
                    }
                } catch (error: Exception) {
                    hostError(error.message ?: text(R.string.relay_error_host_failed))
                }
            }
        }
    }

    fun disconnectSession(sessionId: Long) {
        viewModelScope.launch(Dispatchers.IO) {
            host.disconnectSession(sessionId)
        }
    }

    private fun startHostPolling() {
        host.startPolling(
            onEvents = ::consumeHostEvents,
            onStatus = ::applyHostStatus,
            onError = { error ->
                hostError(error.message ?: text(R.string.relay_error_host_failed))
            },
        )
    }

    private fun consumeHostEvents(raw: String) {
        val events = JSONArray(raw)
        for (index in 0 until events.length()) {
            val event = events.getJSONObject(index)
            when (event.optString("type")) {
                "connected" -> setState {
                    it.copy(
                        hostMessage = text(R.string.relay_session_connected, event.optString("host")),
                    )
                }

                "trusted_peer" -> rememberTrustedPeerFromJson(event)

                "trusted_enrollment_requested" -> {
                    val transactionId = event.optLong("transaction_id", -1L)
                    val peerId = event.optString("peer_id").ifBlank { event.optString("id") }
                    val previous = trustedStore.peer(peerId)
                    val secret = host.enrollmentSecret(transactionId)
                    val persisted = secret.isNotBlank() && saveTrustedPeer(
                        peerId = peerId,
                        secret = secret,
                        name = event.optString("name"),
                        address = event.optString("address"),
                    )
                    if (persisted) {
                        if (!host.acceptEnrollment(transactionId)) {
                            // The host did not commit/ack the transaction.
                            // Restore the previous encrypted record so a
                            // failed rotation cannot strand either side.
                            if (previous != null) {
                                saveTrustedPeer(
                                    previous.peerId,
                                    previous.secret,
                                    previous.name,
                                    previous.address,
                                )
                            } else {
                                removeStoredTrustedPeer(peerId)
                            }
                            setState {
                                it.copy(message = text(R.string.relay_error_trusted_enrollment))
                            }
                        }
                    } else {
                        host.rejectEnrollment(
                            transactionId,
                            "trusted credential could not be durably persisted",
                        )
                    }
                }

                "disconnected" -> setState {
                    it.copy(hostMessage = event.optString("message"))
                }

                "direction_resolved" -> completeDirectionResolution(event)

                "level" -> setState {
                    it.copy(hostRms = event.optDouble("rms").toFloat().coerceIn(0f, 1f))
                }

                "error" -> hostError(event.optString("message"))
            }
        }
    }

    private fun applyHostStatus(status: JSONObject) {
        if (status.optString("type") != "status") return
        val active = status.optBoolean("host_active")
        if (!active && mutableState.value.hostState == RelayHostState.Running) {
            // The service or native host may have stopped independently
            // (for example after the process lost its foreground service).
            // Reflect that transition and release the prepared handle so a
            // later explicit start imports the latest trusted credentials.
            if (!host.releaseInactive()) {
                hostError("native relay host became inactive but its handle is still owned")
                return
            }
            setState {
                it.copy(
                    hostState = RelayHostState.Idle,
                    hostAudioState = RelayHostAudioState.Stopped,
                    hostAudioMessage = "",
                    hostPort = null,
                    hostActive = false,
                    hostAddress = null,
                    sessions = emptyList(),
                    host = it.host,
                    hostMessage = text(R.string.relay_host_stopped),
                )
            }
            return
        }
        setState {
            it.copy(
                sessions = RelayJson.sessions(status),
                hostActive = active,
                hostPort = status.optInt("port").takeIf { port -> port > 0 },
                hostAddress = status.optString("address").takeIf { address -> address.isNotBlank() },
            )
        }
    }

    private fun hostError(message: String) {
        val mapped = when {
            message.contains("No route to host", ignoreCase = true)
                || message.contains("os error 113")
                || message.contains("os error 101") ->
                text(R.string.relay_error_no_route)
            message.contains("Connection refused", ignoreCase = true) || message.contains("os error 111") ->
                text(R.string.relay_error_tcp_refused)
            message.contains("trusted", ignoreCase = true) && message.contains("fail", ignoreCase = true) ->
                text(R.string.relay_error_trusted_auth_failed)
            else -> message
        }
        Log.e(TAG, "HOST STOP/error: $mapped (raw=$message)")
        setState {
            it.copy(
                hostState = RelayHostState.Error,
                hostAudioState = RelayHostAudioState.Error,
                hostAudioMessage = mapped,
                hostPort = null,
                hostActive = false,
                hostAddress = null,
                sessions = emptyList(),
                host = it.host,
                hostMessage = mapped,
            )
        }
    }

    private fun hostAudioError(message: String) {
        Log.w(TAG, "HOST AUDIO FAILURE (host stays listening): $message")
        setState {
            it.copy(
                hostAudioState = RelayHostAudioState.Error,
                hostAudioMessage = message,
                hostMessage = if (it.hostState == RelayHostState.Running && it.hostPort != null) {
                    text(R.string.relay_listening, it.hostPort!!) + " — " + message
                } else message,
            )
        }
    }

    /** Stop client-side native/audio state while the operation mutex is held. */
    private suspend fun stopClientLocked() {
        client.quiesceAndRelease()
        setState {
            it.copy(
                connection = RelayConnectionState.Disconnected,
                sessionId = null,
                hostName = "",
                rms = 0f,
                transport = "",
                link = "",
                audioChannelState = "",
            )
        }
    }

    /** Stop host-side native/audio state while the operation mutex is held. */
    private suspend fun stopHostLocked() {
        Log.i(TAG, "HOST STOP explicit user request")
        host.quiesceAndStop()
        setState {
            it.copy(
                hostState = RelayHostState.Idle,
                hostAudioState = RelayHostAudioState.Stopped,
                hostAudioMessage = "",
                hostPort = null,
                hostActive = false,
                hostAddress = null,
                sessions = emptyList(),
                host = it.host,
                hostMessage = text(R.string.relay_host_stopped),
            )
        }
    }

    private suspend fun handleServiceEvent(event: RelayServiceEvent) {
        operationMutex.withLock {
            when (event) {
                is RelayServiceEvent.ServiceStopped -> if (
                    event.mode == RelayService.MODE_CLIENT && client.owns(event.handle)
                ) {
                    client.cancelPolling()
                    client.releaseNow()
                    setState {
                        it.copy(
                            connection = RelayConnectionState.Error,
                            sessionId = null,
                            hostName = "",
                            rms = 0f,
                            transport = "",
                            link = "",
                            audioChannelState = "",
                            message = text(R.string.relay_error_audio_service_stopped),
                        )
                    }
                } else if (
                    event.mode == RelayService.MODE_HOST &&
                    host.owns(event.handle) &&
                    mutableState.value.hostState != RelayHostState.Idle
                ) {
                    // Audio service died but host listener stays alive (decoupled).
                    Log.w(TAG, "HOST AUDIO STOP service destroyed handle=${event.handle} keeping host listening")
                    // Do not stop native host; just mark audio as stopped.
                    host.cancelPolling()
                    // Restart polling so host status continues? Keep polling alive for sessions.
                    // We keep host handle; just update audio state.
                    setState {
                        it.copy(
                            hostAudioState = RelayHostAudioState.Stopped,
                            hostAudioMessage = text(R.string.relay_error_audio_service_stopped),
                            hostMessage = if (it.hostPort != null) {
                                text(R.string.relay_listening, it.hostPort!!) + " — " + text(R.string.relay_error_audio_service_stopped)
                            } else text(R.string.relay_error_audio_service_stopped),
                        )
                    }
                    // Resume host polling if it was cancelled; startHostPolling handles idempotent start.
                    if (host.isOpen) startHostPolling()
                }
                is RelayServiceEvent.AudioFailure -> if (event.mode == RelayService.MODE_HOST) {
                    if (host.owns(event.handle)) {
                        // Decouple: do NOT stop native host on audio failure.
                        Log.w(TAG, "HOST AUDIO FAILURE event: ${event.message} keeping host listening")
                        // MediaProjection is session-scoped: a revoked consent must not be
                        // silently reused on the next capture attempt. Clear stale credentials
                        // so the next HOST AUDIO START requires fresh user consent.
                        if (event.message.contains("projection revoked", ignoreCase = true) ||
                            event.message.contains("MediaProjection", ignoreCase = true)
                        ) {
                            pendingMediaProjectionResultCode = Activity.RESULT_CANCELED
                            pendingMediaProjectionData = null
                            Log.w(TAG, "Cleared stale MediaProjection consent after revocation")
                        }
                        hostAudioError(event.message)
                        // Keep polling alive; do not stop service/host.
                    }
                } else if (client.owns(event.handle)) {
                    client.quiesceAndRelease()
                    clientError(event.message)
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Discovery
    // ------------------------------------------------------------------

    fun startDiscovery() {
        if (mutableState.value.discoveryActive) return
        viewModelScope.launch(Dispatchers.IO) {
            when (
                val started = discovery.start(mutableState.value.settings.deviceName) {
                    mutableState.value.discoveryActive
                }
            ) {
                is DiscoveryController.Started.Ok -> setState {
                    it.copy(
                        discoveryActive = true,
                        peers = emptyList(),
                        discoveryMessage = if (started.multicastAvailable) {
                            text(R.string.relay_discovery_started)
                        } else {
                            text(R.string.relay_discovery_multicast_unavailable)
                        },
                    )
                }

                is DiscoveryController.Started.Failed -> setState {
                    it.copy(
                        discoveryMessage = started.message
                            ?: text(R.string.relay_error_discovery_failed),
                    )
                }

                DiscoveryController.Started.AlreadyRunning -> Unit
            }
        }
    }

    fun stopDiscovery() {
        viewModelScope.launch(Dispatchers.IO) {
            discovery.stop()
            setState {
                it.copy(
                    discoveryActive = false,
                    peers = emptyList(),
                    discoveryMessage = text(R.string.relay_discovery_stopped),
                )
            }
        }
    }

    /**
     * Publish a discovery snapshot, keep a connected client supplied with it,
     * and decide whether it justifies an automatic trusted reconnect.
     *
     * Discovery has its own native engine on Android, so the connected client
     * has to be handed the snapshot explicitly: its resume worker can then
     * authenticate the same host at a new USB/Wi-Fi address instead of
     * retrying the original IP forever.
     */
    private fun onDiscoverySnapshot(snapshot: DiscoveryController.Snapshot) {
        if (snapshot is DiscoveryController.Snapshot.Failed) {
            // The poll failed outright, so there is no new peer list. Report
            // it and leave the last known peers standing.
            setState { it.copy(discoveryMessage = discoveryText(snapshot.message)) }
            return
        }
        val completed = snapshot as DiscoveryController.Snapshot.Peers
        // An error *event* does not invalidate the snapshot: report it and
        // still publish the peers this tick found.
        completed.message?.let { message ->
            setState { it.copy(discoveryMessage = discoveryText(message)) }
        }
        val peers = completed.peers
        client.updatePeers(RelayJson.discoveredPeersJson(peers))
        setState { it.copy(peers = peers) }
        autoConnectTrustedCandidate(peers)
    }

    private fun discoveryText(message: String): String =
        message.ifBlank { text(R.string.relay_error_discovery_failed) }

    /**
     * Pick the best trusted peer worth dialling automatically, if the UI is in
     * a state where connecting is appropriate and the candidate is not backed
     * off after a recent failure.
     */
    private fun autoConnectTrustedCandidate(peers: List<DiscoveredPeer>) {
        val current = mutableState.value
        val trustedRecords = trustedPeers()
        val now = android.os.SystemClock.elapsedRealtime()
        val candidate = peers
            .filter { peer ->
                peer.id.isNotBlank() && trustedRecords.any { it.peerId == peer.id } &&
                    trustedAutoConnectAllowed(current.settings, peer) &&
                    trustedCandidateAllowed(peer.id, peer.address, now)
            }
            .minWithOrNull(
                compareBy<DiscoveredPeer> { peer ->
                    trustedCandidateRank(
                        peer,
                        trustedRecords.firstOrNull { stored -> stored.peerId == peer.id },
                    )
                }.thenBy { it.address },
            )
            ?: return
        val microphoneReady = !clientNeedsMicrophone(current.settings.direction) ||
            hasMicrophonePermission()
        if (
            microphoneReady &&
            (current.connection == RelayConnectionState.Disconnected ||
                current.connection == RelayConnectionState.Error) &&
            current.hostState != RelayHostState.Starting &&
            current.hostState != RelayHostState.Running &&
            now - lastTrustedAutoAttemptAt >= TRUSTED_AUTO_RETRY_INTERVAL_MS
        ) {
            lastTrustedAutoAttemptAt = now
            connectToTrustedPeer(candidate)
        }
    }

    private fun trustedCandidateAllowed(peerId: String, address: String, now: Long): Boolean =
        trustedCandidateBackoff.allowed(peerId, address, now)

    private fun noteTrustedCandidateFailure(peerId: String, address: String) {
        trustedCandidateBackoff.noteFailure(
            peerId,
            address,
            android.os.SystemClock.elapsedRealtime(),
        )
    }

    // ------------------------------------------------------------------
    // Local link detection (USB tether + host addresses)
    // ------------------------------------------------------------------

    /**
     * Poll the native layer for local links. The one-second fallback is a
     * bounded link watcher for Android devices where no public RNDIS/NCM
     * callback is available. A newly visible USB link starts discovery so a
     * tethered host appears without requiring the user to revisit the tab.
     */
    private fun startUsbPolling() {
        usbPolling?.cancel()
        usbPolling = viewModelScope.launch(Dispatchers.IO) {
            while (isActive) {
                refreshLinks()
                delay(USB_LINK_POLL_INTERVAL_MS)
            }
        }
    }

    private fun refreshLinks() {
        val links = RelayJson.localLinks(NativeBridge.localLinks()) ?: return
        val usb = links.firstOrNull { it.kind == "usb" }?.let { UsbLinkInfo(it.name, it.addr) }
        val current = mutableState.value
        val usbAppeared = usb != null && !usbWasPresent
        val usbDisappeared = usb == null && usbWasPresent
        usbWasPresent = usb != null
        if (links != current.localLinks || usb != current.usbLink) {
            setState { it.copy(localLinks = links, usbLink = usb) }
        }
        if (usbDisappeared) {
            discovery.usbLinkLost()
            if (mutableState.value.discoveryActive) discovery.refreshNow()
        }
        if (usbAppeared && !mutableState.value.discoveryActive) startDiscovery()
    }

    /** Fill target (and PIN, when the QR carries one) from a scanned code. */
    fun applyScannedQr(raw: String) {
        val parsed = parseRelayQr(raw)
        if (parsed == null) {
            setState { it.copy(message = text(R.string.relay_qr_invalid)) }
            return
        }
        val (target, pin) = parsed
        val settings = mutableState.value.settings
        update(settings.copy(target = target, pin = pin ?: settings.pin))
        setState {
            it.copy(message = text(R.string.relay_qr_applied, target))
        }
    }

    // ------------------------------------------------------------------
    // Shared plumbing
    // ------------------------------------------------------------------

    override fun onCleared() {
        client.cancelPolling()
        host.cancelPolling()
        serviceEvents?.cancel()
        serviceEvents = null
        usbPolling?.cancel()
        usbPolling = null
        // Retire the in-memory host credential with the ViewModel as well as
        // on an explicit stop. It is never serialized to preferences.
        setState { it.copy(host = it.host.copy(pin = "")) }
        // Service workers use these same native handles. Serialize cleanup
        // with in-flight connect/host/discovery operations, then stop and
        // await the service before invalidating them. This runs away from the
        // lifecycle/main thread; service destruction is idempotent with the
        // cleanup below, which also covers a service that never started.
        Thread({
            runCatching {
                runBlocking {
                    operationMutex.withLock {
                        client.stopPollingAndWait()
                        host.stopPollingAndWait()
                        service.stopAndWait()
                        client.releaseNow()
                        if (host.isOpen) host.stopAndRelease()
                    }
                    discovery.release()
                }
            }
        }, "qpw-relay-viewmodel-cleanup").start()
        super.onCleared()
    }

    private companion object {
        const val TAG = "RelayViewModel"
        const val POLL_INTERVAL_MS = 100L
        const val USB_LINK_POLL_INTERVAL_MS = 1_000L
        const val TRUSTED_AUTO_RETRY_INTERVAL_MS = 5_000L
        const val DIRECTION_SWITCH_TIMEOUT_MS = 5_000L
    }
}
