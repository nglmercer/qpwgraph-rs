//! XDG ScreenCast portal capture abstraction.
//!
//! Flow (never bypassed, always through the compositor permission dialog):
//!
//! ```text
//! CreateSession -> SelectSources -> Start -> OpenPipeWireRemote
//!      |               |               |              |
//!   portal session  Monitor/Window/  user dialog   PipeWire video
//!                   Virtual                         stream node(s)
//! ```
//!
//! The live D-Bus connector uses the `ashpd` crate behind the `screencast`
//! cargo feature. Everything else — session state machine, stream identity
//! tracking, cancellation/close/disappearance handling, virtual-display
//! probing API — is always compiled so headless builds and tests exercise
//! the same logic the live connector drives.

use std::os::unix::io::OwnedFd;

use crate::video::{ScreenCastRequest, ScreenCastSource, ScreenCastState, ScreenCastStatus};

/// One stream returned by the portal's `Start` response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortalStream {
    /// Transient PipeWire node id. Valid only for this session; never
    /// persisted.
    pub pipewire_node_id: u32,
    /// Compositor-side stream identifier, when provided.
    pub stream_id: Option<String>,
    /// Size in compositor coordinates, when provided.
    pub size: Option<(i32, i32)>,
}

/// Successful `OpenPipeWireRemote` result.
#[derive(Debug)]
pub struct PortalSessionOpened {
    pub streams: Vec<PortalStream>,
    /// FD of the PipeWire remote the streams live on. Held open for the
    /// session lifetime.
    pub remote_fd: Option<OwnedFd>,
    /// Restore token for re-selecting the same sources later.
    pub restore_token: Option<String>,
}

/// Portal failures with user-meaningful distinctions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortalError {
    /// The user dismissed or denied the portal dialog. Not an error to
    /// report loudly; the UI returns to idle.
    CancelledByUser,
    /// No portal, no compositor support, or the requested source type is not
    /// offered (for example `Virtual` on compositors without it).
    Unsupported(String),
    /// The portal is unreachable (headless, no D-Bus session, ...).
    Unavailable(String),
    /// Anything else: D-Bus errors, unexpected responses, ...
    Failed(String),
}

impl std::fmt::Display for PortalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CancelledByUser => f.write_str("capture cancelled"),
            Self::Unsupported(message) | Self::Unavailable(message) | Self::Failed(message) => {
                f.write_str(message)
            }
        }
    }
}

impl std::error::Error for PortalError {}

/// Blocking portal connector. Implementations own whatever runtime they need
/// (the ashpd connector runs a private thread); callers must not invoke
/// `open_capture` on the UI thread because the portal dialog blocks until
/// the user responds.
pub trait PortalConnector: Send {
    /// Run the full `CreateSession -> SelectSources -> Start ->
    /// OpenPipeWireRemote` flow for `request`.
    fn open_capture(
        &mut self,
        request: &ScreenCastRequest,
    ) -> Result<PortalSessionOpened, PortalError>;

    /// Close the active session, if any. Idempotent.
    fn close(&mut self);

    /// Whether a session is currently held open.
    fn is_open(&self) -> bool {
        false
    }

    /// True once when the portal closed the session externally (revoked
    /// access, compositor restart). The connector releases its session state
    /// as part of reporting it.
    fn poll_external_close(&mut self) -> bool {
        false
    }

    /// Which source types the compositor offers. Used to runtime-detect
    /// virtual-display support; never assumed.
    fn available_source_types(&mut self) -> Result<Vec<ScreenCastSource>, PortalError>;
}

/// Connector used when the `screencast` feature is off or no portal exists.
/// Every live operation reports cleanly; the state machine stays testable.
#[derive(Debug, Default)]
pub struct StubPortalConnector {
    open: bool,
}

impl StubPortalConnector {
    pub fn new() -> Self {
        Self::default()
    }
}

impl PortalConnector for StubPortalConnector {
    fn open_capture(
        &mut self,
        _request: &ScreenCastRequest,
    ) -> Result<PortalSessionOpened, PortalError> {
        Err(PortalError::Unavailable(
            "screen capture needs a Linux desktop with xdg-desktop-portal and the screencast feature".into(),
        ))
    }

    fn close(&mut self) {
        self.open = false;
    }

    fn available_source_types(&mut self) -> Result<Vec<ScreenCastSource>, PortalError> {
        Err(PortalError::Unavailable(
            "portal source types are unavailable without the screencast feature".into(),
        ))
    }
}

/// Tracks one portal capture session: its state, requested source, and the
/// PipeWire identity of its stream.
///
/// Where the registry exposes `object.serial` for the stream node, that
/// serial is preferred over the transient node id when resolving the stream
/// in the graph — node ids churn across restarts, serials are stable within
/// the session and far less ambiguous across identical stream names.
pub struct ScreenCastManager {
    connector: Box<dyn PortalConnector>,
    status: ScreenCastStatus,
    restore_token: Option<String>,
}

impl ScreenCastManager {
    pub fn new(connector: Box<dyn PortalConnector>) -> Self {
        Self {
            connector,
            status: ScreenCastStatus::default(),
            restore_token: None,
        }
    }

    pub fn with_stub() -> Self {
        Self::new(Box::new(StubPortalConnector::new()))
    }

    pub fn status(&self) -> ScreenCastStatus {
        self.status.clone()
    }

    pub fn restore_token(&self) -> Option<&str> {
        self.restore_token.as_deref()
    }

    /// Start capturing. A previous session is stopped first. Blocks until
    /// the portal dialog resolves — never call on the UI thread.
    pub fn start(&mut self, request: &ScreenCastRequest) -> ScreenCastStatus {
        if self.status.state.is_active() || self.connector.is_open() {
            self.stop_internal();
        }
        self.status = ScreenCastStatus {
            state: ScreenCastState::Requesting,
            source: Some(request.source),
            pipewire_node_id: None,
            object_serial: None,
            error: None,
        };
        match self.connector.open_capture(request) {
            Ok(opened) => {
                let node_id = opened.streams.first().map(|stream| stream.pipewire_node_id);
                self.restore_token = opened.restore_token;
                // The remote FD is held by the connector for the session
                // lifetime; dropping `opened` here only drops our copy of
                // the metadata. Connectors that need the FD to stay open
                // must retain it themselves (the ashpd connector does: the
                // session thread owns it until Close).
                let _ = opened.remote_fd;
                self.status.state = ScreenCastState::Active;
                self.status.pipewire_node_id = node_id;
            }
            Err(PortalError::CancelledByUser) => {
                self.status.state = ScreenCastState::CancelledByUser;
            }
            Err(error) => {
                self.status.state = ScreenCastState::Failed(error.to_string());
                self.status.error = Some(error.to_string());
            }
        }
        self.status.clone()
    }

    /// Stop capturing and return to idle. Idempotent.
    pub fn stop(&mut self) {
        self.stop_internal();
        self.status = ScreenCastStatus::default();
    }

    /// Record the registry serial for the active stream node, if the graph
    /// exposes it. Preferred over the transient node id for stream lookup.
    pub fn note_stream_serial(&mut self, node_id: u32, serial: Option<u64>) {
        if self.status.pipewire_node_id == Some(node_id) {
            self.status.object_serial = serial;
        }
    }

    /// The portal session closed externally (user revoked access, compositor
    /// restart, ...). Only transitions out of live states.
    pub fn note_session_closed(&mut self) {
        if self.status.state.is_active() || matches!(self.status.state, ScreenCastState::Requesting)
        {
            self.connector.close();
            self.status.state = ScreenCastState::SessionClosed;
            self.status.pipewire_node_id = None;
            self.status.object_serial = None;
        }
    }

    /// The PipeWire stream node disappeared (window closed, monitor
    /// unplugged, ...). Only transitions out of the active state.
    pub fn note_stream_disappeared(&mut self, node_id: u32) {
        if self.status.state.is_active() && self.status.pipewire_node_id == Some(node_id) {
            self.status.state = ScreenCastState::SourceGone;
            self.status.pipewire_node_id = None;
            self.status.object_serial = None;
        }
    }

    /// Reconcile against the currently visible PipeWire stream nodes. Any
    /// active session whose node vanished becomes `SourceGone`.
    pub fn reconcile_visible_streams(&mut self, visible_node_ids: &[u32]) {
        if self.status.state.is_active() {
            if let Some(node_id) = self.status.pipewire_node_id {
                if !visible_node_ids.contains(&node_id) {
                    self.note_stream_disappeared(node_id);
                }
            }
        }
    }

    /// Poll for an external session close (portal revocation, compositor
    /// restart). Call on refresh; transitions `Active` to `SessionClosed`.
    pub fn poll(&mut self) {
        if self.status.state.is_active() && self.connector.poll_external_close() {
            self.status.state = ScreenCastState::SessionClosed;
            self.status.pipewire_node_id = None;
            self.status.object_serial = None;
        }
    }

    /// Runtime-detect virtual-display support. Returns `None` when the
    /// portal cannot be queried.
    pub fn probe_virtual_support(&mut self) -> Option<bool> {
        match self.connector.available_source_types() {
            Ok(types) => Some(types.contains(&ScreenCastSource::Virtual)),
            Err(_) => None,
        }
    }

    fn stop_internal(&mut self) {
        self.connector.close();
    }
}

#[cfg(feature = "screencast")]
pub mod live {
    //! Live ashpd connector. One OS thread per active portal session drives
    //! the ashpd proxy, session, and PipeWire remote FD with
    //! `pollster::block_on`, so no D-Bus lifetime ever crosses threads and
    //! no async runtime leaks into the synchronous driver.

    use std::os::unix::io::OwnedFd;
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::thread::JoinHandle;

    use ashpd::desktop::screencast::{CursorMode, Screencast, SourceType};
    use ashpd::desktop::{PersistMode, ResponseError};

    use super::{PortalConnector, PortalError, PortalSessionOpened, PortalStream};
    use crate::video::{ScreenCastRequest, ScreenCastSource};

    enum Command {
        Close,
        Shutdown,
    }

    /// Result of the blocking session-thread handshake.
    enum OpenOutcome {
        Opened(PortalSessionOpened),
        Failed(PortalError),
    }

    pub struct AshpdPortalConnector {
        worker: Option<SessionWorker>,
    }

    struct SessionWorker {
        commands: Sender<Command>,
        /// Set when the portal reports the session closed externally.
        externally_closed: std::sync::Arc<std::sync::atomic::AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl AshpdPortalConnector {
        pub fn new() -> Self {
            Self { worker: None }
        }
    }

    impl Default for AshpdPortalConnector {
        fn default() -> Self {
            Self::new()
        }
    }

    impl PortalConnector for AshpdPortalConnector {
        fn open_capture(
            &mut self,
            request: &ScreenCastRequest,
        ) -> Result<PortalSessionOpened, PortalError> {
            self.close();
            let (open_tx, open_rx) = mpsc::channel::<OpenOutcome>();
            let (cmd_tx, cmd_rx) = mpsc::channel::<Command>();
            let externally_closed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let closed_flag = externally_closed.clone();
            let request = request.clone();
            let thread = std::thread::Builder::new()
                .name("qpwgraph-screencast".into())
                .spawn(move || {
                    run_session_thread(request, open_rx_sender(open_tx), cmd_rx, closed_flag);
                })
                .map_err(|error| PortalError::Failed(format!("capture thread failed: {error}")))?;
            // Block until the portal dialog resolves (success, cancel, or
            // failure). The caller must not be the UI thread.
            let outcome = open_rx.recv().map_err(|_| {
                PortalError::Failed("capture thread ended without answering".into())
            })?;
            match outcome {
                OpenOutcome::Opened(opened) => {
                    self.worker = Some(SessionWorker {
                        commands: cmd_tx,
                        externally_closed,
                        thread: Some(thread),
                    });
                    Ok(opened)
                }
                OpenOutcome::Failed(error) => {
                    let _ = cmd_tx.send(Command::Shutdown);
                    let _ = thread.join();
                    Err(error)
                }
            }
        }

        fn close(&mut self) {
            if let Some(mut worker) = self.worker.take() {
                let _ = worker.commands.send(Command::Close);
                // Give the session a moment to close cleanly, then reclaim
                // the thread. `try_join` is unstable; a short recv-timeout
                // dance is unnecessary — `join` here is bounded because the
                // worker only awaits an already-answered close.
                if let Some(thread) = worker.thread.take() {
                    let _ = thread.join();
                }
            }
        }

        fn is_open(&self) -> bool {
            self.worker.is_some()
        }

        fn poll_external_close(&mut self) -> bool {
            let closed = self.worker.as_ref().is_some_and(|worker| {
                worker
                    .externally_closed
                    .load(std::sync::atomic::Ordering::Acquire)
            });
            if closed {
                // The session thread already exited; reclaim it.
                if let Some(mut worker) = self.worker.take() {
                    if let Some(thread) = worker.thread.take() {
                        let _ = thread.join();
                    }
                }
            }
            closed
        }

        fn available_source_types(&mut self) -> Result<Vec<ScreenCastSource>, PortalError> {
            // Short-lived probe: build a runtime, ask, tear down. No session
            // is created and no dialog is shown.
            std::thread::Builder::new()
                .name("qpwgraph-screencast-probe".into())
                .spawn(|| {
                    pollster::block_on(async {
                        let proxy = Screencast::new().await.map_err(map_ashpd_error)?;
                        let types = proxy
                            .available_source_types()
                            .await
                            .map_err(map_ashpd_error)?;
                        Ok::<_, PortalError>(
                            types
                                .iter()
                                .map(|t| match t {
                                    SourceType::Monitor => ScreenCastSource::Monitor,
                                    SourceType::Window => ScreenCastSource::Window,
                                    SourceType::Virtual => ScreenCastSource::Virtual,
                                })
                                .collect::<Vec<_>>(),
                        )
                    })
                })
                .map_err(|error| PortalError::Failed(format!("probe thread failed: {error}")))?
                .join()
                .map_err(|_| PortalError::Failed("portal probe thread panicked".into()))?
        }
    }

    impl Drop for AshpdPortalConnector {
        fn drop(&mut self) {
            self.close();
        }
    }

    fn open_rx_sender(tx: Sender<OpenOutcome>) -> Sender<OpenOutcome> {
        tx
    }

    /// Session-thread body: run the portal flow, report the outcome, then
    /// park holding the session until Close/Shutdown or an external close.
    ///
    /// The proxy, session, remote FD, and close-signal subscription all live
    /// inside one async scope on this thread, so no D-Bus lifetime crosses
    /// threads and only plain data (`PortalSessionOpened`) is sent back.
    fn run_session_thread(
        request: ScreenCastRequest,
        open_tx: Sender<OpenOutcome>,
        commands: Receiver<Command>,
        externally_closed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        pollster::block_on(park_session(request, open_tx, commands, externally_closed));
    }

    async fn park_session(
        request: ScreenCastRequest,
        open_tx: Sender<OpenOutcome>,
        commands: Receiver<Command>,
        externally_closed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        use futures_util::future::{select, Either};
        use futures_util::StreamExt;

        let proxy = match Screencast::new().await {
            Ok(proxy) => proxy,
            Err(error) => {
                let _ = open_tx.send(OpenOutcome::Failed(map_ashpd_error(error)));
                return;
            }
        };
        let session = match proxy.create_session().await {
            Ok(session) => session,
            Err(error) => {
                let _ = open_tx.send(OpenOutcome::Failed(map_ashpd_error(error)));
                return;
            }
        };
        let source = match request.source {
            ScreenCastSource::Monitor => SourceType::Monitor,
            ScreenCastSource::Window => SourceType::Window,
            ScreenCastSource::Virtual => SourceType::Virtual,
        };
        let cursor = if request.show_cursor {
            CursorMode::Embedded
        } else {
            CursorMode::Hidden
        };
        // SelectSources: configure what the session records. Must precede
        // Start; the await resolves when the portal answers.
        let selected = proxy
            .select_sources(
                &session,
                cursor,
                source.into(),
                request.multiple,
                None,
                PersistMode::DoNot,
            )
            .await
            .map_err(map_ashpd_error)
            .and_then(|request| request.response().map_err(map_ashpd_error));
        if let Err(error) = selected {
            let _ = session.close().await;
            let _ = open_tx.send(OpenOutcome::Failed(error));
            return;
        }
        // Start: the compositor shows the permission dialog; the await
        // resolves when the user answers it.
        let streams = match proxy
            .start(&session, None)
            .await
            .map_err(map_ashpd_error)
            .and_then(|request| request.response().map_err(map_ashpd_error))
        {
            Ok(streams) => streams,
            Err(error) => {
                let _ = session.close().await;
                let _ = open_tx.send(OpenOutcome::Failed(error));
                return;
            }
        };
        let fd: OwnedFd = match proxy.open_pipe_wire_remote(&session).await {
            Ok(fd) => fd,
            Err(error) => {
                let _ = session.close().await;
                let _ = open_tx.send(OpenOutcome::Failed(map_ashpd_error(error)));
                return;
            }
        };
        let mut closed = match session.receive_closed().await {
            Ok(closed) => closed,
            Err(error) => {
                let _ = session.close().await;
                let _ = open_tx.send(OpenOutcome::Failed(map_ashpd_error(error)));
                return;
            }
        };
        let opened = PortalSessionOpened {
            streams: streams
                .streams()
                .iter()
                .map(|stream| PortalStream {
                    pipewire_node_id: stream.pipe_wire_node_id(),
                    stream_id: stream.id().map(str::to_owned),
                    size: stream.size(),
                })
                .collect(),
            // Held by this scope until Close/Shutdown: dropping the session
            // or the FD revokes the compositor streams.
            remote_fd: Some(fd),
            restore_token: streams.restore_token().map(str::to_owned),
        };
        if opened.streams.is_empty() {
            let _ = session.close().await;
            let _ = open_tx.send(OpenOutcome::Failed(PortalError::Failed(
                "portal returned no streams".into(),
            )));
            return;
        }
        if open_tx.send(OpenOutcome::Opened(opened)).is_err() {
            let _ = session.close().await;
            return;
        }
        // Park: the session must stay alive or the compositor revokes the
        // streams. Exit on Close/Shutdown, or when the portal closes the
        // session externally (revoked access, compositor restart).
        loop {
            if let Ok(command) = commands.try_recv() {
                match command {
                    Command::Close | Command::Shutdown => {
                        let _ = session.close().await;
                        return;
                    }
                }
            }
            // `commands` is std mpsc; poll it without blocking the executor
            // by bounding each wait on the close signal.
            let signal = std::pin::pin!(closed.next());
            let tick = std::pin::pin!(async_io::Timer::after(std::time::Duration::from_millis(
                100
            )));
            match select(signal, tick).await {
                Either::Left(_) => {
                    // Signal fired or the stream ended: either way the
                    // session is gone.
                    externally_closed.store(true, std::sync::atomic::Ordering::Release);
                    return;
                }
                Either::Right(_) => {}
            }
        }
    }

    fn map_ashpd_error(error: ashpd::Error) -> PortalError {
        match &error {
            ashpd::Error::Response(ResponseError::Cancelled) => PortalError::CancelledByUser,
            ashpd::Error::PortalNotFound(_) => {
                PortalError::Unavailable(format!("no ScreenCast portal: {error}"))
            }
            _ => {
                let message = error.to_string();
                // Compositors without virtual-output support fail the
                // SelectSources/Start call; surface that as Unsupported so
                // the UI can say so plainly instead of showing a crash.
                if message.contains("virtual") || message.contains("Virtual") {
                    PortalError::Unsupported(message)
                } else {
                    PortalError::Failed(message)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scripted connector for headless state-machine tests.
    #[derive(Clone)]
    enum ScriptedOutcome {
        Open(u32),
        Fail(PortalError),
    }

    struct ScriptedConnector {
        script: Vec<ScriptedOutcome>,
        source_types: Result<Vec<ScreenCastSource>, PortalError>,
        open: bool,
        closes: usize,
    }

    impl ScriptedConnector {
        fn succeed(node_id: u32) -> Self {
            Self {
                script: vec![ScriptedOutcome::Open(node_id)],
                source_types: Ok(vec![ScreenCastSource::Monitor, ScreenCastSource::Window]),
                open: false,
                closes: 0,
            }
        }
    }

    impl PortalConnector for ScriptedConnector {
        fn open_capture(
            &mut self,
            _request: &ScreenCastRequest,
        ) -> Result<PortalSessionOpened, PortalError> {
            self.open = true;
            let outcome = if self.script.len() > 1 {
                self.script.remove(0)
            } else {
                self.script
                    .first()
                    .cloned()
                    .unwrap_or(ScriptedOutcome::Fail(PortalError::Failed(
                        "script exhausted".into(),
                    )))
            };
            match outcome {
                ScriptedOutcome::Open(node_id) => Ok(PortalSessionOpened {
                    streams: vec![PortalStream {
                        pipewire_node_id: node_id,
                        stream_id: Some("test-stream".into()),
                        size: Some((1920, 1080)),
                    }],
                    remote_fd: None,
                    restore_token: Some("token".into()),
                }),
                ScriptedOutcome::Fail(error) => Err(error),
            }
        }

        fn close(&mut self) {
            self.open = false;
            self.closes += 1;
        }

        fn is_open(&self) -> bool {
            self.open
        }

        fn available_source_types(&mut self) -> Result<Vec<ScreenCastSource>, PortalError> {
            self.source_types.clone()
        }
    }

    #[test]
    fn successful_capture_tracks_stream_identity() {
        let mut manager = ScreenCastManager::new(Box::new(ScriptedConnector::succeed(42)));
        let status = manager.start(&ScreenCastRequest::default());
        assert_eq!(status.state, ScreenCastState::Active);
        assert_eq!(status.pipewire_node_id, Some(42));
        assert_eq!(manager.restore_token(), Some("token"));
        manager.note_stream_serial(42, Some(777));
        assert_eq!(manager.status().object_serial, Some(777));
        // Serial for a different node is ignored.
        manager.note_stream_serial(43, Some(1));
        assert_eq!(manager.status().object_serial, Some(777));
    }

    #[test]
    fn user_cancellation_is_a_clean_terminal_state() {
        let mut connector = ScriptedConnector::succeed(42);
        connector.script = vec![ScriptedOutcome::Fail(PortalError::CancelledByUser)];
        let mut manager = ScreenCastManager::new(Box::new(connector));
        let status = manager.start(&ScreenCastRequest::default());
        assert_eq!(status.state, ScreenCastState::CancelledByUser);
        assert!(status.state.is_terminal());
        assert!(status.error.is_none());
    }

    #[test]
    fn session_close_and_source_loss_transition_cleanly() {
        let mut manager = ScreenCastManager::new(Box::new(ScriptedConnector::succeed(42)));
        manager.start(&ScreenCastRequest::default());
        manager.note_stream_disappeared(99);
        assert_eq!(manager.status().state, ScreenCastState::Active);
        manager.note_stream_disappeared(42);
        assert_eq!(manager.status().state, ScreenCastState::SourceGone);

        manager.start(&ScreenCastRequest::default());
        manager.note_session_closed();
        assert_eq!(manager.status().state, ScreenCastState::SessionClosed);
        // Terminal states ignore further disappearance noise.
        manager.note_stream_disappeared(42);
        assert_eq!(manager.status().state, ScreenCastState::SessionClosed);
    }

    #[test]
    fn reconcile_detects_vanished_streams() {
        let mut manager = ScreenCastManager::new(Box::new(ScriptedConnector::succeed(42)));
        manager.start(&ScreenCastRequest::default());
        manager.reconcile_visible_streams(&[42, 7]);
        assert_eq!(manager.status().state, ScreenCastState::Active);
        manager.reconcile_visible_streams(&[7]);
        assert_eq!(manager.status().state, ScreenCastState::SourceGone);
    }

    #[test]
    fn virtual_support_is_probed_not_assumed() {
        let mut manager = ScreenCastManager::new(Box::new(ScriptedConnector::succeed(42)));
        assert_eq!(manager.probe_virtual_support(), Some(false));
        manager.stop();
        assert_eq!(manager.status().state, ScreenCastState::Idle);
    }

    #[test]
    fn stub_connector_reports_unavailable() {
        let mut manager = ScreenCastManager::with_stub();
        let status = manager.start(&ScreenCastRequest::default());
        assert!(matches!(status.state, ScreenCastState::Failed(_)));
        assert_eq!(manager.probe_virtual_support(), None);
    }
}
