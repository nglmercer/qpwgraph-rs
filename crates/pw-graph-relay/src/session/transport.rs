//! Sockets: the UDP and TCP audio slots, the binds that create them, and
//! the live migration that moves a running audio socket to a new interface.

use super::*;

/// One authenticated, full-duplex TCP stream used as the audio transport for
/// ADB. A slot rather than a stream is stored in the session so a resumed
/// control connection can install a replacement without destroying the
/// negotiated audio keys or queues.
pub(crate) struct TcpAudioSlot {
    pub(super) current: Mutex<Option<Arc<TcpAudioConnection>>>,
    pub(super) changed: Condvar,
    pub(super) connecting: AtomicBool,
}

/// A replaceable, interface-scoped UDP socket. The slot lets an authenticated
/// resume install a socket on the newly selected link while the audio workers
/// keep their queues, AEAD counters, and replay window. Replacing the Arc
/// retires the old socket as soon as in-flight receives finish.
pub(crate) struct UdpAudioSlot {
    pub(super) current: Mutex<Option<Arc<UdpSocket>>>,
}

impl UdpAudioSlot {
    pub(crate) fn new(socket: UdpSocket) -> std::io::Result<Arc<Self>> {
        socket.set_read_timeout(Some(Duration::from_millis(500)))?;
        Ok(Arc::new(Self {
            current: Mutex::new(Some(Arc::new(socket))),
        }))
    }

    pub(crate) fn install(&self, socket: UdpSocket) -> std::io::Result<()> {
        socket.set_read_timeout(Some(Duration::from_millis(500)))?;
        let mut current = self
            .current
            .lock()
            .map_err(|_| std::io::Error::other("UDP audio socket slot is poisoned"))?;
        *current = Some(Arc::new(socket));
        Ok(())
    }

    pub(super) fn current(&self) -> Option<Arc<UdpSocket>> {
        self.current.lock().ok().and_then(|current| current.clone())
    }

    pub(crate) fn local_addr(&self) -> Option<SocketAddr> {
        self.current().and_then(|socket| socket.local_addr().ok())
    }

    /// Unlink the current socket from the slot and wait until this thread
    /// holds the only reference to it, so that dropping the returned socket
    /// really closes the file descriptor.
    ///
    /// Taking the `Arc` out of the slot is not enough on its own: the RX and
    /// TX workers lease a clone for each iteration, so the underlying socket
    /// stays open while any lease is outstanding. A migration that needs the
    /// old address released — a wildcard socket standing in the way of a
    /// specific bind on the same port, or the reverse — must wait for those
    /// leases. They are short: a worker holds one only across a single
    /// `recv_from`/`send_to`, and the slot's read timeout bounds that at
    /// 500ms, so this wait is bounded rather than open-ended.
    ///
    /// Returns `None` when the slot was already empty, `Some(Ok(socket))`
    /// when the caller now owns the socket outright, and `Some(Err(socket))`
    /// when the leases did not drain in time — the caller must then
    /// [`restore`](Self::restore) it.
    pub(super) fn take_exclusive(
        &self,
        timeout: Duration,
    ) -> Option<Result<UdpSocket, Arc<UdpSocket>>> {
        let mut socket = self.current.lock().ok()?.take()?;
        let deadline = Instant::now() + timeout;
        loop {
            match Arc::try_unwrap(socket) {
                Ok(socket) => return Some(Ok(socket)),
                Err(still_leased) => {
                    if Instant::now() >= deadline {
                        return Some(Err(still_leased));
                    }
                    socket = still_leased;
                    std::thread::sleep(UDP_MIGRATION_DRAIN_POLL);
                }
            }
        }
    }

    pub(super) fn restore(&self, socket: Arc<UdpSocket>) {
        if let Ok(mut current) = self.current.lock() {
            *current = Some(socket);
        }
    }
}

pub(super) struct TcpAudioConnection {
    pub(super) reader: Mutex<TcpStream>,
    pub(super) writer: Mutex<TcpStream>,
}

impl TcpAudioSlot {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            current: Mutex::new(None),
            changed: Condvar::new(),
            connecting: AtomicBool::new(false),
        })
    }

    pub(super) fn begin_connect(&self) -> bool {
        self.connecting
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(super) fn end_connect(&self) {
        self.connecting.store(false, Ordering::Release);
    }

    pub(super) fn install(&self, stream: TcpStream) -> std::io::Result<()> {
        let reader = stream.try_clone()?;
        let connection = Arc::new(TcpAudioConnection {
            reader: Mutex::new(reader),
            writer: Mutex::new(stream),
        });
        let mut current = self
            .current
            .lock()
            .map_err(|_| std::io::Error::other("TCP audio slot is poisoned"))?;
        if let Some(previous) = current.replace(connection) {
            previous.shutdown();
        }
        self.changed.notify_all();
        Ok(())
    }

    pub(super) fn current(&self) -> Option<Arc<TcpAudioConnection>> {
        self.current.lock().ok().and_then(|current| current.clone())
    }

    pub(crate) fn is_active(&self) -> bool {
        self.current().is_some()
    }

    pub(super) fn wait(&self, stop: &AtomicBool) -> Option<Arc<TcpAudioConnection>> {
        let mut current = self.current.lock().ok()?;
        while current.is_none() && !stop.load(Ordering::Relaxed) {
            current = self
                .changed
                .wait_timeout(current, Duration::from_millis(250))
                .ok()?
                .0;
        }
        current.clone()
    }

    pub(super) fn clear(&self, connection: &Arc<TcpAudioConnection>) {
        if let Ok(mut current) = self.current.lock() {
            if current
                .as_ref()
                .is_some_and(|installed| Arc::ptr_eq(installed, connection))
            {
                *current = None;
                connection.shutdown();
            }
        }
    }
}

impl TcpAudioConnection {
    /// Wake both worker directions when a resumed connection replaces this
    /// one. A reader may be blocked in `read_exact` while the slot already
    /// points at the replacement; shutting down the underlying stream is what
    /// makes that worker observe the replacement instead of waiting forever
    /// on the stale socket.
    pub(super) fn shutdown(&self) {
        if let Ok(reader) = self.reader.lock() {
            let _ = reader.shutdown(Shutdown::Both);
        }
        if let Ok(writer) = self.writer.lock() {
            let _ = writer.shutdown(Shutdown::Both);
        }
    }
}

/// Bind normal UDP audio to the interface selected for the control path. A
/// wildcard is used only when Auto has no classified link information at all;
/// this is the documented container/no-link fallback, not the migration
/// strategy used during normal operation.
pub(super) fn bind_udp_audio_socket(
    inner: &Arc<EngineInner>,
    target: SocketAddr,
    host_side: bool,
) -> std::io::Result<UdpSocket> {
    bind_udp_audio_socket_on(inner, target, host_side, None)
}

/// The local address the audio socket for `target` belongs on, or an error
/// when the selected link is gone. `None` is the documented wildcard
/// fallback for an Auto host with no classified link at all.
pub(super) fn audio_bind_addr(
    inner: &Arc<EngineInner>,
    target: SocketAddr,
    host_side: bool,
) -> std::io::Result<Option<Ipv4Addr>> {
    let config = inner.config();
    let links = netlink::local_links();
    let bind = if host_side {
        host_bind_addr(&config)
    } else {
        netlink::outbound_bind_addr(&links, target, config.transport)
    };
    if bind.is_none() && (config.transport != crate::TransportPreference::Auto || !links.is_empty())
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AddrNotAvailable,
            "the selected relay interface is not available",
        ));
    }
    Ok(bind)
}

pub(super) fn bind_udp_audio_socket_on(
    inner: &Arc<EngineInner>,
    target: SocketAddr,
    host_side: bool,
    port: Option<u16>,
) -> std::io::Result<UdpSocket> {
    let bind = audio_bind_addr(inner, target, host_side)?;
    UdpSocket::bind((bind.unwrap_or(Ipv4Addr::UNSPECIFIED), port.unwrap_or(0)))
}

pub(super) fn connect_control_tcp(
    target: SocketAddr,
    bind: Option<Ipv4Addr>,
    transport: crate::TransportPreference,
) -> std::io::Result<TcpStream> {
    if transport == crate::TransportPreference::Adb && !target.ip().is_loopback() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "ADB transport requires a localhost forwarding target",
        ));
    }
    // A loopback target already forces the kernel onto the loopback adapter.
    // Avoid an explicit source bind here: Windows can reject a nonblocking
    // socket bound to 127.0.0.1 with WSAEINVAL when many short-lived loopback
    // listeners are active (the common parallel-test and ADB-forward case).
    // Physical-link targets still use the policy-selected source address.
    let bind = if target.ip().is_loopback() {
        None
    } else {
        bind
    };
    netlink::connect_tcp(target, bind, CONNECT_TIMEOUT).map_err(|error| {
        if transport == crate::TransportPreference::Adb && target.ip().is_loopback() {
            std::io::Error::new(
                error.kind(),
                format!(
                    "ADB forwarding is not reachable on {target}. Create the adb reverse/forward rule and retry."
                ),
            )
        } else {
            error
        }
    })
}

/// How long a migration waits for outstanding UDP socket leases to drain
/// before giving up and restoring the socket it was trying to replace. The
/// audio workers' 500ms read timeout bounds a single lease, so this leaves
/// several timeouts of headroom while still guaranteeing the migration
/// cannot block a control thread indefinitely.
pub(super) const UDP_MIGRATION_DRAIN_TIMEOUT: Duration = Duration::from_millis(2_000);
/// Poll interval used while waiting for those leases to drain.
pub(super) const UDP_MIGRATION_DRAIN_POLL: Duration = Duration::from_millis(5);

/// Binds one audio socket at an already resolved local address.
///
/// Injected as a function pointer so the socket-lifecycle and resume paths
/// can be driven deterministically in tests, the same way `WorkerSpawner`
/// is threaded through session startup.
pub(super) type AudioBinder<'a> = &'a dyn Fn(SocketAddr) -> std::io::Result<UdpSocket>;

pub(super) fn bind_audio_socket_at(addr: SocketAddr) -> std::io::Result<UdpSocket> {
    let socket = UdpSocket::bind(addr)?;
    tune_audio_socket(&socket);
    Ok(socket)
}

/// Why an audio-socket migration failed, and — crucially — whether the
/// session it belongs to can carry on afterwards.
#[derive(Debug)]
pub(super) enum AudioMigrationError {
    /// The socket the session was already using is still installed and
    /// usable. The move did not happen, but the audio path is unharmed and a
    /// later authenticated resume can retry it.
    Recoverable(std::io::Error),
    /// The negotiated audio endpoint is gone and could not be restored.
    ///
    /// The peer is still sending to the port it negotiated and nothing is
    /// bound there any more, so the session's audio is black-holed. A caller
    /// that acknowledged this resume would report a healthy session that can
    /// never carry audio again; it must tear the session down instead and let
    /// the peer negotiate a fresh one.
    Fatal(std::io::Error),
}

impl AudioMigrationError {
    pub(super) fn is_fatal(&self) -> bool {
        matches!(self, Self::Fatal(_))
    }
}

impl std::fmt::Display for AudioMigrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Recoverable(error) => write!(f, "{error}"),
            Self::Fatal(error) => write!(f, "{error}"),
        }
    }
}

/// Whether the socket at `old` must be closed before `desired:port` can be
/// bound.
///
/// A wildcard socket owns its port on every local address, so it conflicts
/// with any specific bind on that port, and a specific bind likewise blocks
/// a later wildcard on the same port. Two *different* specific addresses on
/// the same port do not conflict, and a migration that does not preserve the
/// port cannot conflict at all.
pub(super) fn udp_binds_collide(old: SocketAddr, desired: Ipv4Addr, port: u16) -> bool {
    port != 0 && old.port() == port && (old.ip().is_unspecified() || desired.is_unspecified())
}

/// Move `slot` onto `desired`, keeping `preserve_port` when one is given.
///
/// This is the socket-lifecycle half of a migration, split out from
/// interface discovery so it can be driven deterministically in tests:
/// `bind` receives an already resolved local address.
///
/// When the old and new addresses collide on the port being preserved, the
/// old socket is removed from the slot, drained of outstanding worker leases,
/// and closed *before* the new bind is attempted.
///
/// A preserved port is a *negotiated* port: the peer is already sending to
/// it and there is no port renegotiation during a resume. So if the new bind
/// fails, the only acceptable recovery is a wildcard socket back on that same
/// port. Falling back to an ephemeral port would leave the slot holding an
/// address the peer will never send to, which is worse than failing loudly —
/// hence [`AudioMigrationError::Fatal`] rather than a silent port change.
pub(super) fn migrate_udp_slot(
    slot: &UdpAudioSlot,
    desired: Ipv4Addr,
    preserve_port: Option<u16>,
    bind: &dyn Fn(SocketAddr) -> std::io::Result<UdpSocket>,
) -> Result<(), AudioMigrationError> {
    let port = preserve_port.unwrap_or(0);
    let wanted = SocketAddr::from((desired, port));
    let Some(old_addr) = slot.local_addr() else {
        // Nothing is installed, so there is no address to collide with and
        // nothing to roll back to: just fill the slot.
        let socket = bind(wanted).map_err(AudioMigrationError::Recoverable)?;
        return slot
            .install(socket)
            .map_err(AudioMigrationError::Recoverable);
    };
    if !udp_binds_collide(old_addr, desired, port) {
        // The old socket can stay open across the swap; installing the new
        // one retires it as soon as the last in-flight lease finishes. If the
        // bind fails the old socket is untouched and still serving.
        let socket = bind(wanted).map_err(AudioMigrationError::Recoverable)?;
        return slot
            .install(socket)
            .map_err(AudioMigrationError::Recoverable);
    }

    let Some(taken) = slot.take_exclusive(UDP_MIGRATION_DRAIN_TIMEOUT) else {
        return Err(AudioMigrationError::Recoverable(std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            "UDP audio socket is not ready",
        )));
    };
    let old = match taken {
        Ok(socket) => socket,
        Err(still_leased) => {
            // Put the still-live socket back rather than leaving the session
            // without an audio endpoint; the caller reports the failure.
            slot.restore(still_leased);
            return Err(AudioMigrationError::Recoverable(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                format!(
                    "UDP audio socket on {old_addr} is still in use; migration to {wanted} timed out"
                ),
            )));
        }
    };
    // Closing here is the whole point: `old_addr` and `wanted` share a port,
    // so the kernel refuses the new bind while the old socket lives.
    drop(old);

    let error = match bind(wanted) {
        Ok(next) => return slot.install(next).map_err(AudioMigrationError::Fatal),
        Err(error) => error,
    };
    // The old socket is gone for good. The negotiated port is the only
    // address the peer will send to, so restore a wildcard socket on exactly
    // that port — never on an ephemeral one.
    match bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, old_addr.port()))) {
        Ok(fallback) => match slot.install(fallback) {
            Ok(()) => Err(AudioMigrationError::Recoverable(std::io::Error::new(
                error.kind(),
                format!(
                    "could not move UDP audio to {wanted}: {error}; kept a wildcard socket on the negotiated port {}",
                    old_addr.port()
                ),
            ))),
            Err(install_error) => Err(AudioMigrationError::Fatal(install_error)),
        },
        Err(fallback_error) => Err(AudioMigrationError::Fatal(std::io::Error::new(
            error.kind(),
            format!(
                "could not move UDP audio to {wanted}: {error}; the negotiated port {} could not be reopened either: {fallback_error}",
                old_addr.port()
            ),
        ))),
    }
}

/// Move one authenticated session's UDP socket to the link selected for its
/// newly authenticated control path. The host preserves its UDP port so the
/// client can continue using the negotiated destination; the client may use a
/// fresh local port and announces it with the existing AEAD key.
pub(super) fn migrate_udp_audio_socket(
    inner: &Arc<EngineInner>,
    record: &Arc<SessionRecord>,
    target: SocketAddr,
    host_side: bool,
) -> Result<(), AudioMigrationError> {
    migrate_udp_audio_socket_with(inner, record, target, host_side, &bind_audio_socket_at)
}

pub(super) fn migrate_udp_audio_socket_with(
    inner: &Arc<EngineInner>,
    record: &Arc<SessionRecord>,
    target: SocketAddr,
    host_side: bool,
    bind: AudioBinder,
) -> Result<(), AudioMigrationError> {
    let Some(slot) = record.udp_audio.as_ref() else {
        return Ok(());
    };
    let old_addr = slot.local_addr();
    // A resume normally re-authenticates over the very link the socket is
    // already on. Rebinding then means asking the kernel for an address the
    // live socket still holds, which fails with EADDRINUSE and reports a
    // migration error for a session that never needed to move. Only rebind
    // when the selected interface actually changed.
    let desired =
        audio_bind_addr(inner, target, host_side).map_err(AudioMigrationError::Recoverable)?;
    let unchanged = match (old_addr.map(|addr| addr.ip()), desired) {
        (Some(IpAddr::V4(current)), Some(next)) => current == next,
        (Some(current), None) => current.is_unspecified(),
        _ => false,
    };
    if unchanged {
        return Ok(());
    }
    let preserve_port = if host_side {
        old_addr.map(|addr| addr.port())
    } else {
        None
    };
    migrate_udp_slot(
        slot,
        desired.unwrap_or(Ipv4Addr::UNSPECIFIED),
        preserve_port,
        bind,
    )
}
