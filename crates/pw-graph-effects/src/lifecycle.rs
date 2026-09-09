//! Asynchronous effect preparation.
//!
//! [`EffectProcessor::process`](crate::EffectProcessor::process) deliberately
//! remains synchronous and realtime-safe.  This module owns the lifecycle
//! around it: creating a processor, preparing its buffers/runtime, and
//! applying persisted parameters all happen on a small bounded loader pool.
//! The prepared processor is then transferred to the control/backend thread
//! for activation.

use crate::{
    AudioSpec, EffectDescriptor, EffectError, EffectHost, EffectProcessor, EffectProvider,
};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

/// Stable identifier for one asynchronous preparation request.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct EffectTicket(pub u64);

/// Cooperative cancellation token exposed to providers during preparation.
/// A provider may be inside one unavoidable blocking library call, but it can
/// observe this token between dependency/initialization steps and avoid doing
/// more work before the manager drops the result.
#[derive(Clone, Debug, Default)]
pub struct EffectCancellation {
    flag: Arc<AtomicBool>,
}

impl EffectCancellation {
    fn new() -> Self {
        Self::default()
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }

    fn cancel(&self) {
        self.flag.store(true, Ordering::Release);
    }
}

/// The coarse lifecycle stages exposed while an effect is being prepared.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EffectLoadStage {
    Queued,
    LoadingDependencies,
    Preparing,
}

/// Control-plane lifecycle for an effect instance.  Realtime processing is
/// only valid after `Active`; a failed preparation never becomes visible as a
/// graph node.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EffectLifecycle {
    Discovered,
    Queued,
    LoadingDependencies,
    Preparing,
    Ready,
    Activating,
    Active,
    Degraded,
    Failed,
    Stopping,
    Stopped,
}

impl From<EffectLoadStage> for EffectLifecycle {
    fn from(stage: EffectLoadStage) -> Self {
        match stage {
            EffectLoadStage::Queued => Self::Queued,
            EffectLoadStage::LoadingDependencies => Self::LoadingDependencies,
            EffectLoadStage::Preparing => Self::Preparing,
        }
    }
}

/// Everything needed to prepare one processor instance.  The request is
/// intentionally independent of a graph mutation: a backend can prepare it
/// before publishing or rewiring any audio node.
#[derive(Clone, Debug)]
pub struct EffectPrepareRequest {
    pub effect_id: String,
    pub module_path: Option<String>,
    pub spec: AudioSpec,
    pub parameters: BTreeMap<String, f32>,
    pub cancellation: EffectCancellation,
}

/// A processor that has completed all non-realtime preparation and is safe to
/// hand to a backend activation step.
pub struct PreparedEffect {
    pub descriptor: EffectDescriptor,
    pub spec: AudioSpec,
    pub processor: Box<dyn EffectProcessor>,
    pub preparation_duration_ms: u64,
}

impl fmt::Debug for PreparedEffect {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedEffect")
            .field("descriptor", &self.descriptor)
            .field("spec", &self.spec)
            .field("preparation_duration_ms", &self.preparation_duration_ms)
            .finish_non_exhaustive()
    }
}

impl PreparedEffect {
    pub fn into_processor(self) -> Box<dyn EffectProcessor> {
        self.processor
    }
}

/// Events consumed by a control/UI thread.  No event is emitted from the
/// realtime callback.
#[derive(Debug)]
pub enum EffectPreparationEvent {
    Loading {
        ticket: EffectTicket,
        stage: EffectLoadStage,
    },
    Ready {
        ticket: EffectTicket,
        prepared: PreparedEffect,
    },
    Failed {
        ticket: EffectTicket,
        error: EffectError,
    },
    Cancelled {
        ticket: EffectTicket,
    },
}

struct LoadJob {
    ticket: EffectTicket,
    provider: Arc<dyn EffectProvider>,
    request: EffectPrepareRequest,
    cancelled: EffectCancellation,
    events: mpsc::Sender<EffectPreparationEvent>,
}

/// Bounded preparation pool shared by built-in effects and future providers.
///
/// The manager is intended to live on a control thread.  Dropping it never
/// joins a loader: outstanding work is allowed to finish on detached worker
/// threads, so effect/UI destruction cannot synchronously wait for model or
/// module preparation.
pub struct EffectComponentManager {
    jobs: SyncSender<LoadJob>,
    stop: Arc<AtomicBool>,
    events: Receiver<EffectPreparationEvent>,
    event_sender: mpsc::Sender<EffectPreparationEvent>,
    cancellations: Mutex<BTreeMap<EffectTicket, EffectCancellation>>,
    next_ticket: u64,
    // Join handles are intentionally dropped with the manager.  A JoinHandle
    // detaches on drop; naming the field documents the ownership and keeps
    // the worker threads alive for as long as the manager is alive.
    _workers: Vec<JoinHandle<()>>,
}

impl EffectComponentManager {
    /// Create a manager with a bounded job queue and at most eight workers.
    pub fn new(worker_count: usize, queue_capacity: usize) -> Self {
        let worker_count = worker_count.clamp(1, 8);
        let queue_capacity = queue_capacity.max(1);
        let (jobs, job_receiver) = mpsc::sync_channel(queue_capacity);
        let job_receiver = Arc::new(Mutex::new(job_receiver));
        let stop = Arc::new(AtomicBool::new(false));
        let (event_sender, events) = mpsc::channel();
        let mut workers = Vec::with_capacity(worker_count);

        for index in 0..worker_count {
            let receiver = Arc::clone(&job_receiver);
            let worker_stop = Arc::clone(&stop);
            let worker = thread::Builder::new()
                .name(format!("qpwgraph-effect-loader-{index}"))
                .spawn(move || loop {
                    if worker_stop.load(Ordering::Acquire) {
                        break;
                    }
                    let received = receiver.lock().ok().map(|receiver| {
                        receiver.recv_timeout(std::time::Duration::from_millis(50))
                    });
                    match received {
                        Some(Ok(job)) => run_job(job),
                        Some(Err(RecvTimeoutError::Timeout)) => continue,
                        Some(Err(RecvTimeoutError::Disconnected)) | None => break,
                    }
                })
                .expect("effect loader worker should start");
            workers.push(worker);
        }

        Self {
            jobs,
            stop,
            events,
            event_sender,
            cancellations: Mutex::new(BTreeMap::new()),
            next_ticket: 1,
            _workers: workers,
        }
    }

    /// Queue preparation and return without waiting for dependencies or
    /// processor initialization.
    pub fn begin_prepare(
        &mut self,
        host: &EffectHost,
        mut request: EffectPrepareRequest,
    ) -> Result<EffectTicket, EffectError> {
        request.spec.validate()?;
        let provider =
            host.provider_for_request(&request.effect_id, request.module_path.as_deref())?;
        let ticket = EffectTicket(self.next_ticket);
        self.next_ticket = self.next_ticket.wrapping_add(1).max(1);
        let cancelled = EffectCancellation::new();
        request.cancellation = cancelled.clone();
        let job = LoadJob {
            ticket,
            provider,
            request,
            cancelled: cancelled.clone(),
            events: self.event_sender.clone(),
        };

        match self.jobs.try_send(job) {
            Ok(()) => {
                self.cancellations
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(ticket, cancelled);
                Ok(ticket)
            }
            Err(TrySendError::Full(_)) => Err(EffectError::PreparationQueueFull),
            Err(TrySendError::Disconnected(_)) => Err(EffectError::WorkerUnavailable(
                "effect loader pool is unavailable".into(),
            )),
        }
    }

    /// Request cancellation.  A worker that is already preparing finishes
    /// its current non-realtime operation but will suppress activation.
    pub fn cancel(&self, ticket: EffectTicket) -> bool {
        let cancellations = self
            .cancellations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(cancelled) = cancellations.get(&ticket) else {
            return false;
        };
        cancelled.cancel();
        true
    }

    /// Drain all preparation events currently available to the control
    /// thread.  Terminal events remove their cancellation token.
    pub fn poll_events(&mut self) -> Vec<EffectPreparationEvent> {
        let events: Vec<_> = self.events.try_iter().collect();
        if !events.is_empty() {
            let mut cancellations = self
                .cancellations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for event in &events {
                let ticket = match event {
                    EffectPreparationEvent::Ready { ticket, .. }
                    | EffectPreparationEvent::Failed { ticket, .. }
                    | EffectPreparationEvent::Cancelled { ticket } => Some(*ticket),
                    EffectPreparationEvent::Loading { .. } => None,
                };
                if let Some(ticket) = ticket {
                    cancellations.remove(&ticket);
                }
            }
        }
        events
    }

    pub fn pending_count(&self) -> usize {
        self.cancellations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }
}

impl Drop for EffectComponentManager {
    fn drop(&mut self) {
        // Loader workers are deliberately not joined here.  Set the stop bit
        // first so a worker that is between jobs exits after its bounded wait;
        // dropping the sender then also wakes the receiver with Disconnected.
        self.stop.store(true, Ordering::Release);
    }
}

fn run_job(job: LoadJob) {
    let _ = job.events.send(EffectPreparationEvent::Loading {
        ticket: job.ticket,
        stage: EffectLoadStage::Queued,
    });
    if is_cancelled(&job) {
        send_cancelled(job.ticket, &job.events);
        return;
    }
    let _ = job.events.send(EffectPreparationEvent::Loading {
        ticket: job.ticket,
        stage: EffectLoadStage::LoadingDependencies,
    });
    // The dependency stage is intentionally generic.  Providers can later
    // replace this adapter with model/WASM/resource work without changing
    // the manager's event contract.
    if is_cancelled(&job) {
        send_cancelled(job.ticket, &job.events);
        return;
    }
    let _ = job.events.send(EffectPreparationEvent::Loading {
        ticket: job.ticket,
        stage: EffectLoadStage::Preparing,
    });
    let ticket = job.ticket;
    let cancelled = job.cancelled.clone();
    let events = job.events.clone();
    let prepared = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        job.provider.prepare_instance(job.request)
    }))
    .map_err(|payload| {
        EffectError::WorkerUnavailable(format!(
            "effect preparation panicked: {}",
            panic_reason(&*payload)
        ))
    })
    .and_then(|result| result);

    if cancelled.is_cancelled() {
        send_cancelled(ticket, &events);
    } else {
        let event = match prepared {
            Ok(prepared) => EffectPreparationEvent::Ready { ticket, prepared },
            Err(error) => EffectPreparationEvent::Failed { ticket, error },
        };
        let _ = events.send(event);
    }
}

fn is_cancelled(job: &LoadJob) -> bool {
    job.cancelled.is_cancelled()
}

fn send_cancelled(ticket: EffectTicket, events: &mpsc::Sender<EffectPreparationEvent>) {
    let _ = events.send(EffectPreparationEvent::Cancelled { ticket });
}

fn panic_reason(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or("unknown panic")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EffectDescriptor, EffectParameter, EffectProvider};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    struct SlowFactory {
        descriptor: EffectDescriptor,
        delay: Duration,
        created: Arc<AtomicUsize>,
    }

    impl EffectProvider for SlowFactory {
        fn descriptor(&self) -> &EffectDescriptor {
            &self.descriptor
        }

        fn create(&self) -> Box<dyn EffectProcessor> {
            self.created.fetch_add(1, Ordering::Relaxed);
            Box::new(SlowProcessor {
                descriptor: self.descriptor.clone(),
                delay: self.delay,
            })
        }
    }

    struct SlowProcessor {
        descriptor: EffectDescriptor,
        delay: Duration,
    }

    impl EffectProcessor for SlowProcessor {
        fn descriptor(&self) -> &EffectDescriptor {
            &self.descriptor
        }

        fn prepare(&mut self, spec: AudioSpec) -> Result<(), EffectError> {
            thread::sleep(self.delay);
            spec.validate()
        }

        fn process(&mut self, buffer: &mut [f32], _frames: u32) -> Result<(), EffectError> {
            buffer.fill(0.0);
            Ok(())
        }

        fn set_parameter(&mut self, _id: &str, _value: f32) -> Result<(), EffectError> {
            Ok(())
        }

        fn reset(&mut self) {}
    }

    fn request(id: &str) -> EffectPrepareRequest {
        EffectPrepareRequest {
            effect_id: id.into(),
            module_path: None,
            spec: AudioSpec {
                sample_rate: 48_000,
                channels: 1,
                max_frames: 128,
            },
            parameters: BTreeMap::new(),
            cancellation: EffectCancellation::default(),
        }
    }

    fn host(delay: Duration, created: Arc<AtomicUsize>) -> EffectHost {
        let mut host = EffectHost::default();
        host.register(Box::new(SlowFactory {
            descriptor: EffectDescriptor {
                id: "test.slow".into(),
                name: "Slow test effect".into(),
                vendor: "tests".into(),
                version: "1".into(),
                parameters: vec![EffectParameter {
                    id: "test".into(),
                    name: "Test".into(),
                    minimum: 0.0,
                    maximum: 1.0,
                    default: 0.0,
                    unit: "boolean".into(),
                }],
            },
            delay,
            created,
        }));
        host
    }

    #[test]
    fn begin_prepare_returns_before_slow_processor_initialization_finishes() {
        let created = Arc::new(AtomicUsize::new(0));
        let host = host(Duration::from_millis(100), Arc::clone(&created));
        let mut manager = EffectComponentManager::new(1, 1);
        let started = Instant::now();
        let ticket = manager.begin_prepare(&host, request("test.slow")).unwrap();
        assert!(started.elapsed() < Duration::from_millis(50));

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut ready = false;
        while Instant::now() < deadline {
            for event in manager.poll_events() {
                if let EffectPreparationEvent::Ready {
                    ticket: event_ticket,
                    prepared,
                } = event
                {
                    assert_eq!(event_ticket, ticket);
                    assert!(prepared.preparation_duration_ms >= 90);
                    ready = true;
                }
            }
            if ready {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(ready, "slow preparation did not complete");
        assert_eq!(created.load(Ordering::Relaxed), 1);
        assert_eq!(manager.pending_count(), 0);
    }

    #[test]
    fn cancellation_prevents_a_prepared_effect_from_becoming_ready() {
        let created = Arc::new(AtomicUsize::new(0));
        let host = host(Duration::from_millis(80), created);
        let mut manager = EffectComponentManager::new(1, 1);
        let ticket = manager.begin_prepare(&host, request("test.slow")).unwrap();
        assert!(manager.cancel(ticket));

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut cancelled = false;
        while Instant::now() < deadline {
            for event in manager.poll_events() {
                match event {
                    EffectPreparationEvent::Ready { .. } => {
                        panic!("cancelled preparation became ready")
                    }
                    EffectPreparationEvent::Cancelled {
                        ticket: event_ticket,
                    } => {
                        assert_eq!(event_ticket, ticket);
                        cancelled = true;
                    }
                    _ => {}
                }
            }
            if cancelled {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(cancelled, "cancellation event was not delivered");
        assert_eq!(manager.pending_count(), 0);
    }

    #[test]
    fn hush_preparation_is_loader_owned_and_ready_only_after_worker_init() {
        let host = EffectHost::new();
        let mut manager = EffectComponentManager::new(1, 1);
        let started = Instant::now();
        let ticket = manager
            .begin_prepare(
                &host,
                EffectPrepareRequest {
                    effect_id: crate::HUSH_NOISE_SUPPRESSOR_ID.into(),
                    module_path: None,
                    spec: AudioSpec {
                        sample_rate: 48_000,
                        channels: 1,
                        max_frames: 512,
                    },
                    parameters: BTreeMap::new(),
                    cancellation: EffectCancellation::default(),
                },
            )
            .expect("Hush should queue on the loader");
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "begin_prepare waited for Hush initialization"
        );

        let deadline = Instant::now() + Duration::from_secs(30);
        let mut ready = false;
        while Instant::now() < deadline {
            for event in manager.poll_events() {
                match event {
                    EffectPreparationEvent::Ready {
                        ticket: event_ticket,
                        prepared,
                    } => {
                        assert_eq!(event_ticket, ticket);
                        assert_eq!(prepared.spec.channels, 1);
                        let diagnostics = prepared
                            .processor
                            .hush_diagnostics()
                            .expect("Hush should expose diagnostics");
                        assert!(diagnostics.model_init_completed.load(Ordering::Acquire));
                        assert!(diagnostics.model_initialized.load(Ordering::Acquire));
                        ready = true;
                    }
                    EffectPreparationEvent::Failed { error, .. } => {
                        panic!("Hush preparation failed: {error}")
                    }
                    _ => {}
                }
            }
            if ready {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(ready, "Hush preparation did not complete");
        assert_eq!(manager.pending_count(), 0);
    }
}
