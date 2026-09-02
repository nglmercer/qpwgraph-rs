package io.qpwgraph.relay

import android.Manifest
import android.app.Activity
import android.content.Context
import android.content.pm.PackageManager
import android.media.projection.MediaProjectionManager
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.camera.core.CameraSelector
import androidx.camera.core.ImageAnalysis
import androidx.camera.core.Preview
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.camera.view.PreviewView
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Refresh
import androidx.compose.material.icons.outlined.Cable
import androidx.compose.material.icons.outlined.Mic
import androidx.compose.material.icons.outlined.QrCodeScanner
import androidx.compose.material.icons.outlined.Search
import androidx.compose.material.icons.outlined.Speaker
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.CenterAlignedTopAppBar
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ElevatedCard
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ExposedDropdownMenuBox
import androidx.compose.material3.ExposedDropdownMenuDefaults
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.Icon
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Tab
import androidx.compose.material3.TabRow
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.material3.Switch
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.compose.ui.window.Dialog
import androidx.compose.ui.window.DialogProperties
import androidx.core.content.ContextCompat
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel
import com.google.mlkit.vision.barcode.BarcodeScannerOptions
import com.google.mlkit.vision.barcode.BarcodeScanning
import com.google.mlkit.vision.barcode.common.Barcode
import com.google.mlkit.vision.common.InputImage
import io.qpwgraph.relay.ui.components.AlertSeverity
import io.qpwgraph.relay.ui.components.AppAlert
import io.qpwgraph.relay.ui.components.AppTooltip
import io.qpwgraph.relay.ui.components.InfoTooltip
import io.qpwgraph.relay.ui.components.SectionCard
import io.qpwgraph.relay.ui.theme.QpwRelayTheme
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean

private val LINK_OPTIONS = listOf("auto", "wifi", "bluetooth", "lan", "adb")

@Composable
private fun linkDisplayMap(): Map<String, String> = mapOf(
    "auto" to stringResource(R.string.link_auto),
    "wifi" to stringResource(R.string.link_wifi),
    "bluetooth" to stringResource(R.string.link_bluetooth_pan),
    "lan" to stringResource(R.string.link_lan),
    "adb" to stringResource(R.string.link_adb),
)

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContent {
            QpwRelayTheme { RelayApp() }
        }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun RelayApp(viewModel: RelayViewModel = viewModel()) {
    val context = LocalContext.current
    val state by viewModel.state.collectAsStateWithLifecycle()
    var showScanner by remember { mutableStateOf(false) }
    var pendingPermissionAction by remember { mutableStateOf<(() -> Unit)?>(null) }
    var pendingMicrophonePermission by remember { mutableStateOf(false) }
    var pendingHostAction by remember { mutableStateOf(false) }
    val snackbarHostState = remember { SnackbarHostState() }

    // Surface transient ViewModel messages in Snackbar as well as inline alerts.
    LaunchedEffect(state.message) {
        if (state.message.isNotBlank() && state.connection == RelayConnectionState.Error) {
            snackbarHostState.showSnackbar(state.message)
        }
    }
    LaunchedEffect(state.hostMessage) {
        if (state.hostMessage.isNotBlank() && state.hostState == RelayHostState.Error) {
            snackbarHostState.showSnackbar(state.hostMessage)
        }
    }

    val permissionLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestMultiplePermissions(),
    ) { permissions ->
        val microphoneGranted = permissions[Manifest.permission.RECORD_AUDIO] != false
        val action = pendingPermissionAction
        val needsMicrophone = pendingMicrophonePermission
        val hostAction = pendingHostAction
        pendingPermissionAction = null
        pendingMicrophonePermission = false
        pendingHostAction = false
        if (needsMicrophone && !microphoneGranted) {
            viewModel.permissionDenied(hostAction)
        } else {
            action?.invoke()
        }
    }
    val mediaProjectionLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.StartActivityForResult(),
    ) { result ->
        viewModel.onMediaProjectionResult(result.resultCode, result.data)
        val hasAudio = ContextCompat.checkSelfPermission(context, Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED
        if (result.resultCode == Activity.RESULT_OK && hasAudio) {
            viewModel.startHost()
        }
    }
    val cameraPermissionLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestPermission(),
    ) { granted -> if (granted) showScanner = true }

    fun runWithServicePermissions(
        requiresMicrophone: Boolean,
        host: Boolean,
        action: () -> Unit,
    ) {
        val permissions = buildList {
            if (requiresMicrophone) add(Manifest.permission.RECORD_AUDIO)
            if (Build.VERSION.SDK_INT >= 33) add(Manifest.permission.POST_NOTIFICATIONS)
        }
        val missing = permissions.filter {
            ContextCompat.checkSelfPermission(context, it) != PackageManager.PERMISSION_GRANTED
        }
        if (missing.isEmpty()) action()
        else {
            pendingPermissionAction = action
            pendingMicrophonePermission = requiresMicrophone
            pendingHostAction = host
            permissionLauncher.launch(missing.toTypedArray())
        }
    }

    fun openScanner() {
        val granted = ContextCompat.checkSelfPermission(context, Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED
        if (granted) showScanner = true
        else cameraPermissionLauncher.launch(Manifest.permission.CAMERA)
    }

    Scaffold(
        topBar = {
            CenterAlignedTopAppBar(
                title = {
                    Column(horizontalAlignment = Alignment.CenterHorizontally) {
                        Text(stringResource(R.string.relay_app_title), style = MaterialTheme.typography.titleMedium)
                        Text(
                            stringResource(R.string.relay_app_subtitle),
                            style = MaterialTheme.typography.bodySmall,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    }
                },
                colors = TopAppBarDefaults.centerAlignedTopAppBarColors(
                    containerColor = MaterialTheme.colorScheme.surface,
                ),
            )
        },
        snackbarHost = { SnackbarHost(snackbarHostState) },
        containerColor = MaterialTheme.colorScheme.surface,
    ) { innerPadding ->
        Column(
            modifier = Modifier
                .fillMaxSize()
                .verticalScroll(rememberScrollState())
                .padding(innerPadding)
                .padding(horizontal = 16.dp, vertical = 12.dp),
            verticalArrangement = Arrangement.spacedBy(16.dp),
        ) {
            RelayTabs(mode = state.mode, onSelected = viewModel::setMode)
            UsbStatusBanner(link = state.usbLink)

            // Global inline alert for current operation error
            val globalError = when (state.mode) {
                RelayMode.Receiver -> if (state.connection == RelayConnectionState.Error) state.message else ""
                RelayMode.Emitter -> if (state.hostState == RelayHostState.Error) state.hostMessage else ""
                RelayMode.Discover -> if (state.discoveryMessage.isNotBlank() && state.peers.isEmpty()) state.discoveryMessage else ""
            }
            val globalSeverity = when (state.mode) {
                RelayMode.Receiver -> if (state.connection == RelayConnectionState.Error) AlertSeverity.Error else AlertSeverity.Info
                RelayMode.Emitter -> if (state.hostState == RelayHostState.Error) AlertSeverity.Error else AlertSeverity.Info
                else -> AlertSeverity.Info
            }
            if (globalError.isNotBlank() && state.mode != RelayMode.Discover) {
                AppAlert(message = globalError, severity = globalSeverity)
            }

            when (state.mode) {
                RelayMode.Receiver -> ReceiverTab(
                    state, viewModel,
                    connectWithPermission = {
                        runWithServicePermissions(clientNeedsMicrophone(state.settings.role), host = false, action = viewModel::connect)
                    },
                    openScanner = ::openScanner,
                )
                RelayMode.Emitter -> EmitterTab(
                    state, viewModel,
                    startHost = {
                        val source = state.host.captureSource
                        if (source == CaptureSource.DEVICE_PLAYBACK) {
                            runWithServicePermissions(requiresMicrophone = true, host = true, action = {
                                if (viewModel.hasMediaProjectionConsent()) viewModel.startHost()
                                else {
                                    val mgr = context.getSystemService(Context.MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
                                    mediaProjectionLauncher.launch(mgr.createScreenCaptureIntent())
                                }
                            })
                        } else {
                            runWithServicePermissions(requiresMicrophone = true, host = true, action = viewModel::startHost)
                        }
                    },
                    requestPlaybackConsent = {
                        val mgr = context.getSystemService(Context.MEDIA_PROJECTION_SERVICE) as MediaProjectionManager
                        mediaProjectionLauncher.launch(mgr.createScreenCaptureIntent())
                    },
                )
                RelayMode.Discover -> DiscoverTab(
                    state, viewModel,
                    connectToPeer = { address ->
                        runWithServicePermissions(clientNeedsMicrophone(state.settings.role), host = false, action = { viewModel.connectToPeer(address) })
                    },
                )
            }
            TrustedDevicesCard(state, viewModel)
            Spacer(Modifier.height(8.dp))
        }
    }
    if (showScanner) {
        QrScannerDialog(
            onDetected = { value -> showScanner = false; viewModel.applyScannedQr(value) },
            onDismiss = { showScanner = false },
        )
    }
}

@Composable
private fun RelayTabs(mode: RelayMode, onSelected: (RelayMode) -> Unit) {
    val tabs = listOf(
        Triple(stringResource(R.string.nav_receiver), RelayMode.Receiver, Icons.Outlined.Mic),
        Triple(stringResource(R.string.nav_emitter), RelayMode.Emitter, Icons.Outlined.Speaker),
        Triple(stringResource(R.string.nav_discover), RelayMode.Discover, Icons.Outlined.Search),
    )
    TabRow(selectedTabIndex = tabs.indexOfFirst { it.second == mode }.coerceAtLeast(0)) {
        tabs.forEach { (label, tabMode, icon) ->
            Tab(
                selected = mode == tabMode,
                onClick = { onSelected(tabMode) },
                text = { Text(label, style = MaterialTheme.typography.labelLarge) },
                icon = { Icon(icon, contentDescription = null, modifier = Modifier.size(18.dp)) },
            )
        }
    }
}

@Composable
private fun UsbStatusBanner(link: UsbLinkInfo?) {
    if (link != null) {
        AppAlert(
            message = stringResource(R.string.relay_usb_detected, link.name, link.addr),
            severity = AlertSeverity.Success,
            title = stringResource(R.string.link_auto),
        )
    } else {
        AppAlert(
            message = stringResource(R.string.relay_usb_none),
            severity = AlertSeverity.Info,
        )
    }
}

// ------------------------------------------------------------------
// Scanner
// ------------------------------------------------------------------

@androidx.annotation.OptIn(androidx.camera.core.ExperimentalGetImage::class)
@Composable
private fun QrScannerDialog(onDetected: (String) -> Unit, onDismiss: () -> Unit) {
    val context = LocalContext.current
    val lifecycleOwner = LocalLifecycleOwner.current
    val executor = remember { Executors.newSingleThreadExecutor() }
    val scanner = remember {
        BarcodeScanning.getClient(BarcodeScannerOptions.Builder().setBarcodeFormats(Barcode.FORMAT_QR_CODE).build())
    }
    val detected = remember { AtomicBoolean(false) }
    var cameraProvider by remember { mutableStateOf<ProcessCameraProvider?>(null) }
    DisposableEffect(Unit) {
        onDispose { cameraProvider?.unbindAll(); executor.shutdown(); scanner.close() }
    }
    Dialog(onDismissRequest = onDismiss, properties = DialogProperties(usePlatformDefaultWidth = false)) {
        Box(modifier = Modifier.size(320.dp).clip(RoundedCornerShape(16.dp))) {
            AndroidView(
                factory = { ctx ->
                    PreviewView(ctx).also { previewView ->
                        val future = ProcessCameraProvider.getInstance(ctx)
                        future.addListener({
                            val provider = future.get()
                            cameraProvider = provider
                            val preview = Preview.Builder().build().also { it.setSurfaceProvider(previewView.surfaceProvider) }
                            val analysis = ImageAnalysis.Builder().setBackpressureStrategy(ImageAnalysis.STRATEGY_KEEP_ONLY_LATEST).build()
                            analysis.setAnalyzer(executor) { proxy ->
                                val image = proxy.image
                                if (image == null || detected.get()) { proxy.close(); return@setAnalyzer }
                                val input = InputImage.fromMediaImage(image, proxy.imageInfo.rotationDegrees)
                                scanner.process(input)
                                    .addOnSuccessListener { codes ->
                                        val value = codes.firstNotNullOfOrNull { it.rawValue }
                                        if (value != null && detected.compareAndSet(false, true)) onDetected(value)
                                    }
                                    .addOnCompleteListener { proxy.close() }
                            }
                            provider.unbindAll()
                            provider.bindToLifecycle(lifecycleOwner, CameraSelector.DEFAULT_BACK_CAMERA, preview, analysis)
                        }, ContextCompat.getMainExecutor(ctx))
                    }
                }, modifier = Modifier.fillMaxSize(),
            )
            TextButton(onClick = onDismiss, modifier = Modifier.align(Alignment.BottomCenter)) {
                Text(stringResource(R.string.action_cancel))
            }
        }
    }
}

// ------------------------------------------------------------------
// Receiver
// ------------------------------------------------------------------

@Composable
private fun ReceiverTab(
    state: RelayUiState,
    viewModel: RelayViewModel,
    connectWithPermission: () -> Unit,
    openScanner: () -> Unit,
) {
    val linkDisplay = linkDisplayMap()
    SectionCard(title = stringResource(R.string.receiver_host_address_label), tooltip = stringResource(R.string.receiver_host_address_tooltip)) {
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp), verticalAlignment = Alignment.Top) {
            OutlinedTextField(
                value = state.settings.target,
                onValueChange = { viewModel.update(state.settings.copy(target = it)) },
                label = { Text(stringResource(R.string.receiver_host_address_label)) },
                placeholder = { Text(stringResource(R.string.receiver_host_address_hint)) },
                modifier = Modifier.weight(1f),
                singleLine = true,
            )
            AppTooltip(text = stringResource(R.string.receiver_scan_qr_tooltip)) {
                OutlinedButton(onClick = openScanner, modifier = Modifier.padding(top = 8.dp)) {
                    Icon(Icons.Outlined.QrCodeScanner, contentDescription = null, modifier = Modifier.size(18.dp))
                    Spacer(Modifier.width(6.dp))
                    Text(stringResource(R.string.receiver_scan_qr))
                }
            }
        }
        Spacer(Modifier.height(8.dp))
        OutlinedTextField(
            value = state.settings.pin,
            onValueChange = { viewModel.update(state.settings.copy(pin = it)) },
            label = { Text(stringResource(R.string.receiver_pin_label)) },
            modifier = Modifier.fillMaxWidth(),
            singleLine = true,
        )
        Spacer(Modifier.height(8.dp))
        DropdownField(
            label = stringResource(R.string.receiver_role_label),
            value = state.settings.role,
            options = listOf("emit", "receive", "both"),
            display = mapOf(
                "emit" to stringResource(R.string.receiver_role_emit),
                "receive" to stringResource(R.string.receiver_role_receive),
                "both" to stringResource(R.string.receiver_role_both),
            ),
            tooltip = stringResource(R.string.receiver_role_tooltip),
            onSelected = { viewModel.update(state.settings.copy(role = it)) },
        )
        Spacer(Modifier.height(8.dp))
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            DropdownField(
                label = stringResource(R.string.receiver_codec_label),
                value = state.settings.codec,
                options = listOf("opus", "pcm"),
                tooltip = stringResource(R.string.receiver_codec_tooltip),
                onSelected = { viewModel.update(state.settings.copy(codec = it)) },
                modifier = Modifier.weight(1f),
            )
            DropdownField(
                label = stringResource(R.string.receiver_link_label),
                value = state.settings.transport,
                options = LINK_OPTIONS,
                display = linkDisplay,
                tooltip = stringResource(R.string.receiver_link_tooltip),
                onSelected = { viewModel.update(state.settings.copy(transport = it)) },
                modifier = Modifier.weight(1f),
            )
        }
    }

    SectionCard(title = stringResource(R.string.receiver_trusted_auto_title), tooltip = stringResource(R.string.receiver_trusted_auto_tooltip)) {
        Row(modifier = Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween, verticalAlignment = Alignment.CenterVertically) {
            Text(stringResource(R.string.receiver_trusted_auto_title), style = MaterialTheme.typography.bodyMedium, modifier = Modifier.weight(1f).padding(end = 12.dp))
            Switch(checked = state.settings.autoConnectTrusted, onCheckedChange = { viewModel.update(state.settings.copy(autoConnectTrusted = it)) })
        }
        if (state.settings.autoConnectTrusted) {
            Spacer(Modifier.height(8.dp))
            Row(modifier = Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween, verticalAlignment = Alignment.CenterVertically) {
                Text(stringResource(R.string.receiver_trusted_wifi_title), style = MaterialTheme.typography.bodyMedium, modifier = Modifier.weight(1f).padding(end = 12.dp))
                InfoTooltip(tooltip = stringResource(R.string.receiver_trusted_wifi_tooltip))
                Switch(checked = state.settings.autoConnectTrustedWifi, onCheckedChange = { viewModel.update(state.settings.copy(autoConnectTrustedWifi = it)) })
            }
            Spacer(Modifier.height(6.dp))
            Text(stringResource(R.string.receiver_trusted_hint), style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
        }
    }

    Row(horizontalArrangement = Arrangement.spacedBy(8.dp), modifier = Modifier.fillMaxWidth()) {
        val isConnected = state.connection == RelayConnectionState.Connected || state.connection == RelayConnectionState.Connecting
        if (isConnected) {
            Button(onClick = viewModel::disconnect, modifier = Modifier.weight(1f)) { Text(stringResource(R.string.action_disconnect)) }
        } else {
            Button(onClick = connectWithPermission, modifier = Modifier.weight(1f)) { Text(stringResource(R.string.action_connect)) }
        }
    }

    SectionCard(title = stringResource(R.string.label_status)) {
        Text(
            state.connection.name.lowercase().replace('_', ' '),
            style = MaterialTheme.typography.titleSmall,
            color = when (state.connection) {
                RelayConnectionState.Connected -> MaterialTheme.colorScheme.primary
                RelayConnectionState.Error -> MaterialTheme.colorScheme.error
                else -> MaterialTheme.colorScheme.onSurface
            },
        )
        if (state.hostName.isNotBlank()) Text(stringResource(R.string.label_host, state.hostName), style = MaterialTheme.typography.bodyMedium)
        if (state.sessionId != null) Text(stringResource(R.string.label_session, state.sessionId!!), style = MaterialTheme.typography.bodySmall)
        if (state.transport.isNotBlank()) {
            Text(stringResource(R.string.label_connected_via, state.link.ifBlank { stringResource(R.string.label_unknown_link) }, state.transport), style = MaterialTheme.typography.bodySmall)
        }
        if (state.audioChannelState == "reconnecting") {
            Spacer(Modifier.height(6.dp))
            AppAlert(message = stringResource(R.string.label_reconnecting_audio), severity = AlertSeverity.Warning)
        }
        if (state.message.isNotBlank() && state.connection != RelayConnectionState.Error) {
            Spacer(Modifier.height(6.dp))
            Text(state.message, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
        }
        Spacer(Modifier.height(10.dp))
        LevelIndicator(level = state.rms)
    }
}

@Composable
private fun LevelIndicator(level: Float, modifier: Modifier = Modifier) {
    Column(modifier = modifier.fillMaxWidth(), verticalArrangement = Arrangement.spacedBy(6.dp)) {
        Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.SpaceBetween, modifier = Modifier.fillMaxWidth()) {
            Text(stringResource(R.string.cd_level_meter), style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
            Text(stringResource(R.string.label_level, (level * 100).toInt()), style = MaterialTheme.typography.bodySmall)
        }
        LinearProgressIndicator(
            progress = { level.coerceIn(0f, 1f) },
            modifier = Modifier.fillMaxWidth().height(8.dp).clip(RoundedCornerShape(4.dp)),
        )
    }
}

@Composable
private fun TrustedDevicesCard(state: RelayUiState, viewModel: RelayViewModel) {
    if (state.trustedPeers.isEmpty()) return
    SectionCard(title = stringResource(R.string.trusted_devices_title), tooltip = stringResource(R.string.trusted_devices_tooltip)) {
        state.trustedPeers.forEach { peer ->
            Row(modifier = Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween, verticalAlignment = Alignment.CenterVertically) {
                Column(modifier = Modifier.weight(1f)) {
                    Text(peer.name.ifBlank { peer.peerId }, style = MaterialTheme.typography.bodyMedium)
                    if (peer.address.isNotBlank()) Text(peer.address, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                }
                TextButton(onClick = { viewModel.forgetTrustedPeer(peer.peerId) }) { Text(stringResource(R.string.action_forget)) }
            }
        }
    }
}

// ------------------------------------------------------------------
// Emitter
// ------------------------------------------------------------------

@Composable
private fun EmitterTab(
    state: RelayUiState,
    viewModel: RelayViewModel,
    startHost: () -> Unit,
    requestPlaybackConsent: () -> Unit,
) {
    val hostEditable = state.hostState != RelayHostState.Starting && state.hostState != RelayHostState.Running
    val linkDisplay = linkDisplayMap()

    SectionCard(title = stringResource(R.string.emitter_device_name_label), tooltip = stringResource(R.string.emitter_device_name_tooltip)) {
        OutlinedTextField(
            value = state.host.deviceName,
            onValueChange = { viewModel.updateHost(state.host.copy(deviceName = it)) },
            enabled = hostEditable,
            label = { Text(stringResource(R.string.emitter_device_name_label)) },
            modifier = Modifier.fillMaxWidth(), singleLine = true,
        )
        Spacer(Modifier.height(8.dp))
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp), verticalAlignment = Alignment.CenterVertically, modifier = Modifier.fillMaxWidth()) {
            OutlinedTextField(
                value = state.host.pin,
                onValueChange = { viewModel.updateHost(state.host.copy(pin = it)) },
                enabled = hostEditable,
                label = { Text(stringResource(R.string.emitter_pin_label)) },
                modifier = Modifier.weight(1f), singleLine = true,
            )
            AppTooltip(text = stringResource(R.string.emitter_regenerate_pin_desc)) {
                FilledTonalButton(onClick = { viewModel.regenerateHostPin() }, enabled = hostEditable, modifier = Modifier.padding(top = 4.dp)) {
                    Icon(Icons.Filled.Refresh, contentDescription = stringResource(R.string.emitter_regenerate_pin_desc), modifier = Modifier.size(18.dp))
                }
            }
        }
        Spacer(Modifier.height(8.dp))
        OutlinedTextField(
            value = state.host.port.toString(),
            onValueChange = { value -> value.toIntOrNull()?.let { viewModel.updateHost(state.host.copy(port = it)) } },
            enabled = hostEditable,
            label = { Text(stringResource(R.string.emitter_port_label)) },
            modifier = Modifier.fillMaxWidth(), singleLine = true,
        )
        Spacer(Modifier.height(8.dp))
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            DropdownField(
                label = stringResource(R.string.emitter_codec_label),
                value = state.host.codec,
                options = listOf("opus", "pcm"),
                onSelected = { viewModel.updateHost(state.host.copy(codec = it)) },
                enabled = hostEditable, modifier = Modifier.weight(1f),
            )
            DropdownField(
                label = stringResource(R.string.emitter_link_label),
                value = state.host.transport,
                options = LINK_OPTIONS,
                display = linkDisplay,
                onSelected = { viewModel.updateHost(state.host.copy(transport = it)) },
                enabled = hostEditable, modifier = Modifier.weight(1f),
            )
        }
        Spacer(Modifier.height(8.dp))
        DropdownField(
            label = stringResource(R.string.emitter_capture_source_label),
            value = state.host.captureSource.name.lowercase(),
            options = listOf("microphone", "device_playback"),
            display = mapOf("microphone" to stringResource(R.string.emitter_capture_microphone), "device_playback" to stringResource(R.string.emitter_capture_playback)),
            tooltip = stringResource(R.string.emitter_capture_tooltip),
            onSelected = { viewModel.setHostCaptureSource(captureSourceFromString(it)) },
            enabled = hostEditable,
        )
        if (state.host.captureSource == CaptureSource.DEVICE_PLAYBACK) {
            Spacer(Modifier.height(8.dp))
            AppAlert(message = stringResource(R.string.emitter_playback_hint), severity = AlertSeverity.Info)
            Spacer(Modifier.height(8.dp))
            AppAlert(
                message = stringResource(R.string.emitter_playback_call_audio_hint),
                severity = AlertSeverity.Warning,
            )
            if (!viewModel.hasMediaProjectionConsent() && hostEditable) {
                Spacer(Modifier.height(8.dp))
                OutlinedButton(onClick = requestPlaybackConsent) { Text(stringResource(R.string.emitter_grant_consent)) }
            }
        }
    }

    Row(horizontalArrangement = Arrangement.spacedBy(8.dp), modifier = Modifier.fillMaxWidth()) {
        if (state.hostState == RelayHostState.Running) {
            Button(onClick = viewModel::stopHost, modifier = Modifier.weight(1f), colors = ButtonDefaults.buttonColors(containerColor = MaterialTheme.colorScheme.error)) {
                Text(stringResource(R.string.emitter_stop_host))
            }
        } else {
            Button(onClick = startHost, modifier = Modifier.weight(1f)) { Text(stringResource(R.string.emitter_start_host)) }
        }
    }

    SectionCard(title = stringResource(R.string.label_status)) {
        Text(stringResource(R.string.emitter_host_status, state.hostState.name.lowercase(), state.hostAudioState.name.lowercase()), style = MaterialTheme.typography.bodyMedium)
        if (state.hostPort != null) Text(stringResource(R.string.emitter_listening_port, state.hostPort!!), style = MaterialTheme.typography.bodySmall)
        if (state.hostMessage.isNotBlank() && state.hostState != RelayHostState.Error) {
            Spacer(Modifier.height(6.dp)); Text(state.hostMessage, style = MaterialTheme.typography.bodySmall)
        }
        if (state.hostAudioMessage.isNotBlank() && state.hostAudioMessage != state.hostMessage) {
            Spacer(Modifier.height(4.dp))
            AppAlert(message = state.hostAudioMessage, severity = if (state.hostAudioState == RelayHostAudioState.Error) AlertSeverity.Error else AlertSeverity.Info)
        }
        Spacer(Modifier.height(10.dp))
        LevelIndicator(level = state.hostRms)
        Spacer(Modifier.height(4.dp))
        Text(stringResource(R.string.emitter_capture_label, state.host.captureSource.name.lowercase()), style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
    }

    val hostPort = state.hostPort
    val hostAddress = state.hostAddress
    if (state.hostState == RelayHostState.Running && hostPort != null && hostAddress != null) {
        SectionCard(title = stringResource(R.string.emitter_reachable_title), tooltip = stringResource(R.string.emitter_reachable_tooltip)) {
            Text("$hostAddress:$hostPort", style = MaterialTheme.typography.titleSmall)
        }
    }
    if (state.sessions.isNotEmpty()) {
        SectionCard(title = stringResource(R.string.emitter_active_sessions)) {
            state.sessions.forEach { session ->
                Row(modifier = Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.SpaceBetween, verticalAlignment = Alignment.CenterVertically) {
                    Column(modifier = Modifier.weight(1f)) {
                        Text("${session.name} — ${session.address}", style = MaterialTheme.typography.bodyMedium)
                        if (session.transport.isNotBlank()) {
                            Text(
                                "${session.link.ifBlank { stringResource(R.string.label_unknown_link) }} / ${session.transport}" + if (session.audioChannelState == "reconnecting") " — ${stringResource(R.string.label_reconnecting_audio)}" else "",
                                style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant,
                            )
                        }
                    }
                    TextButton(onClick = { viewModel.disconnectSession(session.id) }) { Text(stringResource(R.string.action_disconnect)) }
                }
            }
        }
    }
}

// ------------------------------------------------------------------
// Discover
// ------------------------------------------------------------------

@Composable
private fun DiscoverTab(
    state: RelayUiState,
    viewModel: RelayViewModel,
    connectToPeer: (String) -> Unit,
) {
    AppTooltip(text = stringResource(R.string.discover_start_tooltip)) {
        Button(
            onClick = { if (state.discoveryActive) viewModel.stopDiscovery() else viewModel.startDiscovery() },
            modifier = Modifier.fillMaxWidth(),
        ) {
            Icon(if (state.discoveryActive) Icons.Filled.Refresh else Icons.Outlined.Search, contentDescription = null, modifier = Modifier.size(18.dp))
            Spacer(Modifier.width(8.dp))
            Text(if (state.discoveryActive) stringResource(R.string.discover_stop) else stringResource(R.string.discover_start))
        }
    }
    if (state.discoveryMessage.isNotBlank()) {
        AppAlert(message = state.discoveryMessage, severity = AlertSeverity.Info)
    }
    if (state.peers.isEmpty()) {
        Text(stringResource(R.string.discover_empty), style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
    }
    state.peers.forEach { peer ->
        ElevatedCard(modifier = Modifier.fillMaxWidth(), elevation = CardDefaults.cardElevation(defaultElevation = 1.dp)) {
            Row(modifier = Modifier.fillMaxWidth().padding(12.dp), horizontalArrangement = Arrangement.SpaceBetween, verticalAlignment = Alignment.CenterVertically) {
                Column(modifier = Modifier.weight(1f)) {
                    Text(peer.name, style = MaterialTheme.typography.bodyMedium)
                    Text(peer.address, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                    if (peer.link.isNotBlank()) Text(peer.link, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                }
                Spacer(Modifier.width(8.dp))
                AppTooltip(text = stringResource(R.string.discover_connect_tooltip)) {
                    Button(onClick = { connectToPeer(peer.address) }) { Text(stringResource(R.string.discover_connect)) }
                }
            }
        }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun DropdownField(
    label: String,
    value: String,
    options: List<String>,
    onSelected: (String) -> Unit,
    enabled: Boolean = true,
    modifier: Modifier = Modifier,
    display: Map<String, String> = emptyMap(),
    tooltip: String? = null,
) {
    var expanded by remember { mutableStateOf(false) }
    Row(modifier = modifier, verticalAlignment = Alignment.CenterVertically) {
        ExposedDropdownMenuBox(
            expanded = expanded,
            onExpandedChange = { if (enabled) expanded = !expanded },
            modifier = Modifier.weight(1f),
        ) {
            OutlinedTextField(
                value = display[value] ?: value,
                onValueChange = {},
                readOnly = true,
                enabled = enabled,
                label = { Text(label) },
                trailingIcon = { ExposedDropdownMenuDefaults.TrailingIcon(expanded) },
                modifier = Modifier.menuAnchor().fillMaxWidth(),
            )
            ExposedDropdownMenu(expanded = expanded && enabled, onDismissRequest = { expanded = false }) {
                options.forEach { option ->
                    DropdownMenuItem(
                        text = { Text(display[option] ?: option) },
                        onClick = { onSelected(option); expanded = false },
                    )
                }
            }
        }
        if (tooltip != null) {
            Spacer(Modifier.width(4.dp))
            InfoTooltip(tooltip = tooltip)
        }
    }
}
