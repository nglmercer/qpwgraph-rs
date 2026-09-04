//! Spawning the per-session threads and reporting whether they came up.

use super::*;

pub(super) type Worker = Box<dyn FnOnce() + Send + 'static>;
pub(super) type WorkerSpawner = fn(String, Worker) -> std::io::Result<std::thread::JoinHandle<()>>;

pub(super) fn spawn_named(
    name: String,
    worker: Worker,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new().name(name).spawn(worker)
}

pub(super) fn spawn_worker_with_report(
    spawn: &mut impl FnMut(String, Worker) -> std::io::Result<std::thread::JoinHandle<()>>,
    stop: &AtomicBool,
    id: SessionId,
    direction: &str,
    worker: Worker,
) -> Result<(), String> {
    if let Err(error) = spawn(format!("relay-{direction}-{id}"), worker) {
        stop.store(true, Ordering::Relaxed);
        return Err(format!(
            "could not start {direction} worker for {id}: {error}"
        ));
    }
    Ok(())
}

pub(super) fn wait_for_worker_startup(
    ready: Receiver<Result<(), String>>,
    stop: &AtomicBool,
    id: SessionId,
    direction: &str,
) -> Result<(), String> {
    match ready.recv_timeout(HANDSHAKE_TIMEOUT) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(reason)) => {
            stop.store(true, Ordering::Relaxed);
            Err(format!(
                "{direction} worker for {id} failed during startup: {reason}"
            ))
        }
        Err(error) => {
            stop.store(true, Ordering::Relaxed);
            Err(format!(
                "{direction} worker for {id} did not become ready: {error}"
            ))
        }
    }
}

pub(super) fn report_worker_startup(
    ready: Option<SyncSender<Result<(), String>>>,
    result: Result<(), String>,
) {
    if let Some(ready) = ready {
        let _ = ready.send(result);
    }
}

/// Start every worker required by a session before advertising it as
/// established. If a later worker fails, the caller removes the session and
/// the already-started worker observes that removal and exits.
pub(super) fn spawn_session_workers(
    inner: &Arc<EngineInner>,
    record: &Arc<SessionRecord>,
    socket: &Option<Arc<UdpAudioSlot>>,
    host_side: bool,
) -> Result<(), String> {
    spawn_session_workers_with(inner, record, socket, host_side, spawn_named)
}

pub(super) fn spawn_session_workers_with(
    inner: &Arc<EngineInner>,
    record: &Arc<SessionRecord>,
    socket: &Option<Arc<UdpAudioSlot>>,
    host_side: bool,
    mut spawn: impl FnMut(String, Worker) -> std::io::Result<std::thread::JoinHandle<()>>,
) -> Result<(), String> {
    // Keep both transport workers alive for the lifetime of an authenticated
    // session. Flow switches update `active_roles` atomically; starting and
    // stopping threads from the control watcher would race teardown and can
    // briefly leave two paths active. The workers themselves gate their audio
    // work on that state, so a switch is a one-path transition without a new
    // socket or a new cipher timeline.
    {
        let inner = Arc::clone(inner);
        let record = Arc::clone(record);
        let socket = socket.clone();
        let stop = Arc::clone(&record.stop);
        let id = record.id;
        let (ready_tx, ready_rx) = mpsc::sync_channel(0);
        let tcp_audio = record.tcp_audio.clone();
        spawn_worker_with_report(
            &mut spawn,
            &stop,
            id,
            "RX",
            Box::new(move || {
                if let Some(tcp_audio) = tcp_audio {
                    run_tcp_rx(inner, record, tcp_audio, Some(ready_tx));
                } else {
                    let Some(socket) = socket else {
                        report_worker_startup(
                            Some(ready_tx),
                            Err("UDP audio socket is unavailable".into()),
                        );
                        return;
                    };
                    run_rx(inner, record, socket, host_side, Some(ready_tx));
                }
            }),
        )?;
        wait_for_worker_startup(ready_rx, &stop, id, "RX")?;
    }
    {
        let inner = Arc::clone(inner);
        let record = Arc::clone(record);
        let socket = socket.clone();
        let stop = Arc::clone(&record.stop);
        let id = record.id;
        let (ready_tx, ready_rx) = mpsc::sync_channel(0);
        let tcp_audio = record.tcp_audio.clone();
        spawn_worker_with_report(
            &mut spawn,
            &stop,
            id,
            "TX",
            Box::new(move || {
                if let Some(tcp_audio) = tcp_audio {
                    run_tcp_tx(inner, record, tcp_audio, Some(ready_tx));
                } else {
                    let Some(socket) = socket else {
                        report_worker_startup(
                            Some(ready_tx),
                            Err("UDP audio socket is unavailable".into()),
                        );
                        return;
                    };
                    run_tx(inner, record, socket, Some(ready_tx));
                }
            }),
        )?;
        wait_for_worker_startup(ready_rx, &stop, id, "TX")?;
    }
    // ADB's secondary stream is client-initiated. It gets one supervisor for
    // the lifetime of the session, independent from the control watcher and
    // from the two audio directions. The slot's connect gate prevents a
    // control resume and the supervisor from creating duplicate races.
    if !host_side {
        if let Some(tcp_audio) = record.tcp_audio.clone() {
            let inner = Arc::clone(inner);
            let record = Arc::clone(record);
            let stop = Arc::clone(&record.stop);
            let id = record.id;
            let target = record.peer.addr;
            spawn_worker_with_report(
                &mut spawn,
                &stop,
                id,
                "ADB-audio-supervisor",
                Box::new(move || run_tcp_audio_supervisor(inner, record, tcp_audio, target)),
            )?;
        }
    }
    Ok(())
}
