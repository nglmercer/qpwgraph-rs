//! Raw `pw_filter` hosting for the built-in realtime effects.
//!
//! `pipewire-rs` 0.8 intentionally does not wrap `pw_filter`, so this module
//! keeps the small amount of FFI needed by the backend in one place.  A filter
//! owns its callback state and is always created and destroyed while the
//! driver's `ThreadLoop` lock is held.  That is the lifetime boundary PipeWire
//! requires before a callback data pointer may be released.
//!
//! The current effect SDK has one builtin processor and no native WASM host.
//! Built-in effects retain their established stereo FL/FR ports. Hush is
//! layout-aware and may instead expose one MONO pair; the callback still uses
//! a fixed two-pointer storage envelope so changing the selected layout never
//! reallocates on the realtime thread.

use super::filter_runtime::FilterRuntime;
use super::*;
use pw_graph_effects::{
    apply_parameters, AudioSpec, EffectHost, EffectInstanceConfig, EffectProcessor,
};
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};
use std::sync::{Mutex, TryLockError};

/// PipeWire's standard quantum is normally far smaller than this.  Keeping a
/// finite ceiling lets the callback build an exactly-sized Rust slice without
/// trusting an arbitrarily large duration supplied by an external graph.
const MAX_DSP_FRAMES: u32 = 16_384;
const PREPARED_SAMPLE_RATE: u32 = 48_000;
const DSP_CHANNELS: usize = 2;
const UNRESOLVED_ID: u64 = u64::MAX;
const DANGLING_FL: u32 = 1 << 0;
const DANGLING_FR: u32 = 1 << 1;
const DANGLING_BOTH: u32 = DANGLING_FL | DANGLING_FR;

/// Processing state that is never reallocated from the realtime callback.
struct ProcessorState {
    processor: Box<dyn EffectProcessor>,
    interleaved: Vec<f32>,
}

/// The callback owns a planar FL/FR pair and an interleaved processor buffer.
/// The port pointers are published before `pw_filter_connect`, then never
/// changed, so loading them atomically is sufficient even when PipeWire calls
/// `process` on its separate realtime data thread.
struct CallbackState {
    input_ports: [AtomicPtr<c_void>; DSP_CHANNELS],
    output_ports: [AtomicPtr<c_void>; DSP_CHANNELS],
    channels: usize,
    enabled: AtomicBool,
    processor_failed: AtomicBool,
    hush_diagnostics: Option<std::sync::Arc<pw_graph_effects::HushDiagnostics>>,
    /// Bit 0/1 reports a missing FL/FR input while the matching output buffer
    /// is active. This is deliberately metadata only: it never participates in
    /// sample generation.
    dangling_inputs: AtomicU32,
    /// A control update may run while PipeWire is processing.  The realtime
    /// callback uses `try_lock` and transparently bypasses a single quantum if
    /// the UI owns this mutex, rather than ever blocking the audio thread.
    ///
    /// This is intentionally a conservative bridge for the builtin Rust
    /// processors.  A future plugin ABI should replace it with a lock-free,
    /// preallocated control queue before hosting third-party processors.
    processor: Mutex<ProcessorState>,
}

impl CallbackState {
    #[cfg(test)]
    fn new(processor: Box<dyn EffectProcessor>, enabled: bool) -> Self {
        Self::new_with_channels(processor, enabled, DSP_CHANNELS)
    }

    fn new_with_channels(
        processor: Box<dyn EffectProcessor>,
        enabled: bool,
        channels: usize,
    ) -> Self {
        let channels = channels.clamp(1, DSP_CHANNELS);
        Self {
            input_ports: std::array::from_fn(|_| AtomicPtr::new(ptr::null_mut())),
            output_ports: std::array::from_fn(|_| AtomicPtr::new(ptr::null_mut())),
            channels,
            enabled: AtomicBool::new(enabled),
            processor_failed: AtomicBool::new(false),
            hush_diagnostics: processor.hush_diagnostics(),
            dangling_inputs: AtomicU32::new(0),
            processor: Mutex::new(ProcessorState {
                processor,
                interleaved: vec![0.0; MAX_DSP_FRAMES as usize * channels],
            }),
        }
    }

    /// # Safety
    ///
    /// PipeWire invokes this with the callback data supplied to
    /// `pw_filter_new_simple`.  Both port pointers were returned by
    /// `pw_filter_add_port`, and the filter owns their lifetime. The DSP API
    /// guarantees F32 sample storage for each planar FL/FR channel.
    unsafe fn process(&self, position: *mut pw::spa::sys::spa_io_position) {
        if position.is_null() {
            return;
        }
        let frames = (*position).clock.duration;
        if frames == 0 || frames > u64::from(MAX_DSP_FRAMES) {
            self.processor_failed.store(true, Ordering::Relaxed);
            self.dangling_inputs.store(0, Ordering::Release);
            return;
        }
        let frames = frames as u32;
        let input_ports = self
            .input_ports
            .each_ref()
            .map(|port| port.load(Ordering::Acquire));
        let output_ports = self
            .output_ports
            .each_ref()
            .map(|port| port.load(Ordering::Acquire));
        if output_ports.iter().all(|port| port.is_null()) {
            self.dangling_inputs.store(0, Ordering::Release);
            return;
        }

        // PipeWire may not provide a DSP buffer for an unconnected port. Keep
        // each channel independent: a connected FL input must still reach its
        // output when FR is not patched, and an output-only effect must emit
        // digital silence.
        let inputs: [*mut c_void; DSP_CHANNELS] = std::array::from_fn(|channel| {
            let port = input_ports[channel];
            if port.is_null() {
                ptr::null_mut()
            } else {
                pw::sys::pw_filter_get_dsp_buffer(port, frames)
            }
        });
        let outputs: [*mut c_void; DSP_CHANNELS] = std::array::from_fn(|channel| {
            let port = output_ports[channel];
            if port.is_null() {
                ptr::null_mut()
            } else {
                pw::sys::pw_filter_get_dsp_buffer(port, frames)
            }
        });
        if outputs.iter().all(|buffer| buffer.is_null()) {
            self.dangling_inputs.store(0, Ordering::Release);
            return;
        }

        self.process_buffers(inputs, outputs, frames);
    }

    /// Process one already-resolved set of planar buffers. Keeping this
    /// separate from the PipeWire buffer lookup makes the safety policy easy
    /// to exercise without a running daemon and keeps every callback branch
    /// on the same fallback path.
    ///
    /// # Safety
    ///
    /// Each non-null input/output pointer refers to at least `frames` valid
    /// F32 samples, as returned by PipeWire or by a caller obeying the same
    /// contract. The pointers are not changed while this function runs.
    unsafe fn process_buffers(
        &self,
        inputs: [*mut c_void; DSP_CHANNELS],
        outputs: [*mut c_void; DSP_CHANNELS],
        frames: u32,
    ) {
        let frame_count = frames as usize;
        let mut dangling_inputs = 0_u32;
        for channel in 0..self.channels {
            if !outputs[channel].is_null() && inputs[channel].is_null() {
                dangling_inputs |= 1 << channel;
            }
        }
        self.dangling_inputs
            .store(dangling_inputs, Ordering::Release);

        // A stateful effect must learn about route changes before it queues
        // this block. The default implementation is a no-op; Hush uses the
        // atomic/generation-only hook to reset disconnected denoiser state.
        // A channel is useful to a stateful processor only when both sides of
        // the effect have a live DSP buffer. In particular, an input-only
        // buffer must not make Hush spend inference time on audio that has no
        // output path; an output-only buffer remains a diagnosed dangling
        // channel and is forced to silence below.
        let channel_mask = (0..self.channels).fold(0_u16, |mask, channel| {
            if !inputs[channel].is_null() && !outputs[channel].is_null() {
                mask | (1_u16 << channel)
            } else {
                mask
            }
        });
        if let Ok(mut state) = self.processor.try_lock() {
            state.processor.set_channel_mask(channel_mask);
        }

        // Publish a deterministic fallback before invoking user-extensible
        // DSP. If the processor returns an error or panics after mutating its
        // scratch buffer, these samples remain untouched and are still safe.
        copy_inputs_to_outputs(inputs, outputs, frame_count);

        let enabled = self.enabled.load(Ordering::Acquire);
        if !enabled && self.hush_diagnostics.is_none() {
            // Disabled effects remain transparent for connected channels and
            // produce exact silence for dangling or non-finite inputs.
            return;
        }

        let mut state = match self.processor.try_lock() {
            Ok(state) => state,
            Err(TryLockError::WouldBlock) => {
                // Parameter edits are intentionally allowed to cost one
                // bypassed quantum. Waiting for a non-realtime UI thread here
                // would risk an xrun for the entire PipeWire graph.
                // `copy_inputs_to_outputs` above is the safe transparent
                // fallback.
                return;
            }
            Err(TryLockError::Poisoned(_)) => {
                // A control-thread panic poisoned the processor state. The
                // audio fallback is already published; retain an error bit so
                // the control snapshot can report the failure separately.
                self.processor_failed.store(true, Ordering::Relaxed);
                return;
            }
        };
        let ProcessorState {
            processor,
            interleaved,
        } = &mut *state;
        processor.set_host_bypass(!enabled);
        let samples = &mut interleaved[..frame_count * self.channels];
        for frame in 0..frame_count {
            for channel in 0..self.channels {
                samples[frame * self.channels + channel] = input_sample(inputs[channel], frame);
            }
        }
        // No Rust panic may cross the C callback boundary. Builtin processors
        // are specified not to panic, but a defensive catch keeps a malformed
        // future implementation from invoking undefined behaviour here.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            processor.process(samples, frames)
        }));
        if matches!(result, Ok(Ok(()))) && samples.iter().all(|sample| sample.is_finite()) {
            copy_processed_to_outputs(
                samples,
                outputs,
                dangling_inputs,
                frame_count,
                self.channels,
            );
            // A Hush worker can fail while still returning its aligned dry
            // fallback. Preserve that audio and publish the diagnostic
            // separately instead of replacing it with an instantaneous pass-
            // through block.
            self.processor_failed
                .store(processor.has_failed(), Ordering::Relaxed);
        } else {
            // The fallback was published before processing, so an error,
            // panic, or non-finite result cannot expose partial or undefined
            // processor output. Keep the diagnostic state separate from audio.
            self.processor_failed.store(true, Ordering::Relaxed);
        }
    }
}

/// Read one planar input sample without allowing a missing or non-finite
/// value to enter DSP.
#[inline]
unsafe fn input_sample(input: *mut c_void, frame: usize) -> f32 {
    if input.is_null() {
        return 0.0;
    }
    let sample = *input.cast::<f32>().add(frame);
    if sample.is_finite() {
        sample
    } else {
        0.0
    }
}

/// Fill active output ports with sanitized input, or exact silence when the
/// corresponding input port has no DSP buffer.
#[inline]
unsafe fn copy_inputs_to_outputs(
    inputs: [*mut c_void; DSP_CHANNELS],
    outputs: [*mut c_void; DSP_CHANNELS],
    frames: usize,
) {
    for frame in 0..frames {
        for channel in 0..DSP_CHANNELS {
            if !outputs[channel].is_null() {
                *outputs[channel].cast::<f32>().add(frame) = input_sample(inputs[channel], frame);
            }
        }
    }
}

/// Copy a successful, finite processor result to active output ports. The
/// caller has already written the transparent fallback, so this function does
/// not need an error path in the realtime callback.
#[inline]
unsafe fn copy_processed_to_outputs(
    samples: &[f32],
    outputs: [*mut c_void; DSP_CHANNELS],
    dangling_inputs: u32,
    frames: usize,
    channels: usize,
) {
    for frame in 0..frames {
        for channel in 0..channels {
            if !outputs[channel].is_null() {
                let sample = if dangling_inputs & (1 << channel) != 0 {
                    // A stateful processor may still have a queued tail after
                    // its input is disconnected. Do not let that tail turn a
                    // passive meter into an audio source.
                    0.0
                } else {
                    samples[frame * channels + channel]
                };
                *outputs[channel].cast::<f32>().add(frame) = sample;
            }
        }
    }
}

unsafe extern "C" fn filter_process(
    data: *mut c_void,
    position: *mut pw::spa::sys::spa_io_position,
) {
    // `data` is a Box<CallbackState> retained by FilterRuntime until after
    // `pw_filter_destroy` has detached all callbacks. Never unwind over C.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        let Some(state) = data.cast::<CallbackState>().as_ref() else {
            return;
        };
        state.process(position);
    }));
}

/// Backend-owned metadata paired with a live PipeWire filter.
pub(super) struct NativeEffect {
    pub(super) instance: EffectInstance,
    runtime: FilterRuntime<CallbackState>,
    node_name: String,
    position: [f32; 2],
}

impl NativeEffect {
    pub(super) fn create(
        host: &EffectHost,
        thread_loop: &pw::thread_loop::ThreadLoop,
        request: EffectNodeRequest,
    ) -> BackendResult<Self> {
        validate_request(&request)?;
        if request.module_path.is_some() {
            return Err(BackendError::unsupported(
                "WASM/native effect modules are not yet hosted by the PipeWire filter runtime",
            ));
        }

        // All setup and parameter validation happens before the raw filter is
        // published to PipeWire. Legacy effects always retain their planar
        // FL/FR pair. Hush can use one MONO pair when the caller selected a
        // mono route, which avoids constructing/running a second denoiser.
        let is_hush = request.effect_id == pw_graph_effects::HUSH_NOISE_SUPPRESSOR_ID;
        let channels = if is_hush {
            super::resolve_hush_channels(request.channels, None)? as usize
        } else {
            DSP_CHANNELS
        };
        if is_hush {
            eprintln!(
                "INFO hush: channel mode={} resolved_channels={} standalone=true",
                if request.channels.is_some() {
                    "Explicit"
                } else {
                    "Auto"
                },
                channels
            );
        }
        let mut processor = host
            .create(&request.effect_id)
            .map_err(BackendError::effect_create_failed)?;
        processor
            .prepare(AudioSpec {
                sample_rate: PREPARED_SAMPLE_RATE,
                channels: channels as u16,
                max_frames: MAX_DSP_FRAMES,
            })
            .map_err(BackendError::native)?;
        apply_parameters(&mut *processor, &request.parameters).map_err(BackendError::native)?;

        let effect_name = processor.descriptor().name.clone();
        validate_pipewire_text("effect name", &effect_name)?;
        // This value is intentionally both friendly and unique. PortKey uses
        // its node name, so two Noise Gate nodes must not collapse into the
        // same persistence/undo endpoint after a registry refresh.
        let node_name = format!("{effect_name} ({})", request.instance_id);

        let callback = Box::new(CallbackState::new_with_channels(
            processor,
            request.enabled,
            channels,
        ));
        let filter_properties = pw::properties::properties! {
            NODE_NAME => node_name.as_str(),
            NODE_DESCRIPTION => node_name.as_str(),
            MEDIA_TYPE => MEDIA_TYPE_AUDIO,
            PROP_MEDIA_CATEGORY => MEDIA_CATEGORY_FILTER,
            PROP_MEDIA_ROLE => MEDIA_ROLE_DSP,
            MEDIA_CLASS => MEDIA_CLASS_AUDIO_FILTER,
            PROP_NODE_VIRTUAL => "true",
            // Filters are deliberately patchable graph nodes. Never let a
            // session manager route a newly-created effect to a default device.
            PROP_NODE_AUTOCONNECT => "false",
            PROP_NODE_GROUP => "qpwgraph-rs",
            "qpwgraph-rs.effect.instance" => request.instance_id.as_str(),
            "qpwgraph-rs.effect.id" => request.effect_id.as_str(),
        };
        let runtime = FilterRuntime::create(
            thread_loop,
            &node_name,
            filter_properties,
            Some(filter_process),
            callback,
        )?;

        let mut input_ports = [ptr::null_mut(); DSP_CHANNELS];
        let mut output_ports = [ptr::null_mut(); DSP_CHANNELS];
        let channel_names: [&str; DSP_CHANNELS] = if channels == 1 {
            ["MONO", ""]
        } else {
            ["FL", "FR"]
        };
        for (index, channel) in channel_names.iter().take(channels).enumerate() {
            input_ports[index] = runtime.add_port(
                pw::spa::sys::SPA_DIRECTION_INPUT,
                &format!("input_{channel}"),
                channel,
            )?;
            output_ports[index] = runtime.add_port(
                pw::spa::sys::SPA_DIRECTION_OUTPUT,
                &format!("output_{channel}"),
                channel,
            )?;
        }
        for channel in 0..DSP_CHANNELS {
            runtime.callback().input_ports[channel].store(input_ports[channel], Ordering::Release);
            runtime.callback().output_ports[channel]
                .store(output_ports[channel], Ordering::Release);
        }
        runtime.connect()?;

        let instance = EffectInstance {
            config: EffectInstanceConfig {
                instance_id: request.instance_id,
                effect_id: request.effect_id,
                module_path: request.module_path,
                enabled: request.enabled,
                parameters: request.parameters,
                // Persist the effective Hush layout.  Writing `None` here
                // would make a restored standalone mono node silently become
                // stereo again.  Non-Hush effects retain their legacy config.
                channels: if is_hush {
                    Some(channels as u16)
                } else {
                    request.channels
                },
            },
            node_id: NodeId(UNRESOLVED_ID),
            input_port: PortId(UNRESOLVED_ID),
            output_port: PortId(UNRESOLVED_ID),
            source: None,
            destination: None,
            error: None,
            diagnostics: None,
        };
        Ok(Self {
            instance,
            runtime,
            node_name,
            position: request.position,
        })
    }

    pub(super) fn node_name(&self) -> &str {
        &self.node_name
    }

    pub(super) fn runtime_node_id(&self) -> Option<NodeId> {
        self.runtime.node_id()
    }

    pub(super) fn position(&self) -> [f32; 2] {
        self.position
    }

    pub(super) fn set_position(&mut self, position: [f32; 2]) {
        self.position = position;
    }

    pub(super) fn set_identity(
        &mut self,
        node_id: NodeId,
        input_port: PortId,
        output_port: PortId,
    ) {
        self.instance.node_id = node_id;
        self.instance.input_port = input_port;
        self.instance.output_port = output_port;
    }

    pub(super) fn resolved(&self) -> bool {
        self.instance.node_id.0 != UNRESOLVED_ID
            && self.instance.input_port.0 != UNRESOLVED_ID
            && self.instance.output_port.0 != UNRESOLVED_ID
    }

    pub(super) fn snapshot(&self) -> EffectInstance {
        let mut instance = self.instance.clone();
        let callback = self.runtime.callback();
        if let Some(diagnostics) = &callback.hush_diagnostics {
            instance.diagnostics = Some(diagnostics.status_text());
            instance.error = diagnostics.failure_reason();
        }
        if callback.processor_failed.load(Ordering::Relaxed) && instance.error.is_none() {
            instance.error = Some("effect processor reported a realtime or worker failure".into());
        } else if let Some(message) =
            dangling_input_message(callback.dangling_inputs.load(Ordering::Acquire))
        {
            // This is rendered from a control-thread snapshot. The realtime
            // callback only publishes the bit mask and never formats text.
            instance.error = Some(message.into());
        }
        instance
    }

    pub(super) fn set_enabled(&mut self, enabled: bool) {
        let callback = self.runtime.callback();
        let previous = callback.enabled.swap(enabled, Ordering::AcqRel);
        if previous != enabled && callback.hush_diagnostics.is_none() {
            // An enabled/disabled transition is a bypass transition too. A
            // stateful processor must discard queued wet audio and recurrent
            // state before it is allowed back into the graph. This lock is
            // taken only by the control path; the realtime callback uses
            // `try_lock` and never waits for it.
            if let Ok(mut state) = callback.processor.lock() {
                if !state.processor.set_host_bypass(!enabled) {
                    state.processor.reset();
                }
            }
        }
        self.instance.config.enabled = enabled;
    }

    pub(super) fn set_parameter(&mut self, parameter: &str, value: f32) -> BackendResult<()> {
        if let Some(diagnostics) = &self.runtime.callback().hush_diagnostics {
            diagnostics
                .set_control_parameter(parameter, value)
                .map_err(BackendError::native)?;
        } else {
            let mut state = self
                .runtime
                .callback()
                .processor
                .lock()
                .map_err(|_| BackendError::native("effect processor lock was poisoned"))?;
            state
                .processor
                .set_parameter(parameter, value)
                .map_err(BackendError::native)?;
        }
        self.instance
            .config
            .parameters
            .insert(parameter.to_owned(), value);
        Ok(())
    }
}

fn dangling_input_message(mask: u32) -> Option<&'static str> {
    match mask & (DANGLING_FL | DANGLING_FR) {
        DANGLING_FL => Some("effect output is active while the FL input is disconnected"),
        DANGLING_FR => Some("effect output is active while the FR input is disconnected"),
        DANGLING_BOTH => Some("effect output is active while both inputs are disconnected"),
        _ => None,
    }
}

fn validate_request(request: &EffectNodeRequest) -> BackendResult<()> {
    if request.instance_id.trim().is_empty() {
        return Err(BackendError::native("effect instance id cannot be empty"));
    }
    validate_pipewire_text("effect instance id", &request.instance_id)?;
    validate_pipewire_text("effect id", &request.effect_id)?;
    Ok(())
}

fn validate_pipewire_text(label: &str, value: &str) -> BackendResult<()> {
    if value.contains('\0') {
        return Err(BackendError::native(format!(
            "{label} contains a NUL byte and cannot be passed to PipeWire"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pw_graph_effects::{AudioSpec, EffectDescriptor, EffectError, EffectProcessor};
    use std::ffi::c_void;
    use std::ptr;
    use std::sync::atomic::{AtomicU16, Ordering};
    use std::sync::Arc;

    #[derive(Clone, Copy)]
    enum ProcessBehavior {
        Gain,
        EmitsConstant,
        ErrorAfterMutation,
        PanicAfterMutation,
        NonFinite,
    }

    struct TestProcessor {
        descriptor: EffectDescriptor,
        factor: f32,
        behavior: ProcessBehavior,
        observed_channel_mask: Option<Arc<AtomicU16>>,
    }

    impl TestProcessor {
        fn new(behavior: ProcessBehavior) -> Self {
            Self::with_observed_channel_mask(behavior, None)
        }

        fn with_observed_channel_mask(
            behavior: ProcessBehavior,
            observed_channel_mask: Option<Arc<AtomicU16>>,
        ) -> Self {
            Self {
                descriptor: EffectDescriptor {
                    id: "test.pipewire-effect".into(),
                    name: "PipeWire callback test effect".into(),
                    vendor: "qpwgraph-rs".into(),
                    version: "1".into(),
                    parameters: Vec::new(),
                },
                factor: 2.0,
                behavior,
                observed_channel_mask,
            }
        }
    }

    impl EffectProcessor for TestProcessor {
        fn descriptor(&self) -> &EffectDescriptor {
            &self.descriptor
        }

        fn prepare(&mut self, spec: AudioSpec) -> Result<(), EffectError> {
            spec.validate()
        }

        fn process(&mut self, buffer: &mut [f32], _frames: u32) -> Result<(), EffectError> {
            for sample in buffer.iter_mut() {
                *sample *= self.factor;
            }
            match self.behavior {
                ProcessBehavior::Gain => Ok(()),
                ProcessBehavior::EmitsConstant => {
                    buffer.fill(0.25);
                    Ok(())
                }
                ProcessBehavior::ErrorAfterMutation => Err(EffectError::NotPrepared),
                ProcessBehavior::PanicAfterMutation => panic!("test processor panic"),
                ProcessBehavior::NonFinite => {
                    buffer[0] = f32::NAN;
                    Ok(())
                }
            }
        }

        fn set_parameter(&mut self, id: &str, value: f32) -> Result<(), EffectError> {
            if id != "factor" {
                return Err(EffectError::UnsupportedParameter(id.into()));
            }
            self.factor = value;
            Ok(())
        }

        fn reset(&mut self) {}

        fn set_channel_mask(&mut self, mask: u16) {
            if let Some(observed_channel_mask) = &self.observed_channel_mask {
                observed_channel_mask.store(mask, Ordering::Release);
            }
        }
    }

    fn run_buffers(
        state: &CallbackState,
        inputs: [Option<&mut [f32]>; DSP_CHANNELS],
        outputs: [Option<&mut [f32]>; DSP_CHANNELS],
        frames: usize,
    ) {
        let input_ptrs = inputs.map(|input| {
            input.map_or(ptr::null_mut(), |buffer| {
                buffer.as_mut_ptr().cast::<c_void>()
            })
        });
        let output_ptrs = outputs.map(|output| {
            output.map_or(ptr::null_mut(), |buffer| {
                buffer.as_mut_ptr().cast::<c_void>()
            })
        });
        unsafe { state.process_buffers(input_ptrs, output_ptrs, frames as u32) };
    }

    #[test]
    fn dangling_enabled_effect_publishes_exact_silence() {
        let state = CallbackState::new(
            Box::new(TestProcessor::new(ProcessBehavior::EmitsConstant)),
            true,
        );
        let mut left = [9.0; 32];
        let mut right = [9.0; 32];

        run_buffers(
            &state,
            [None, None],
            [Some(&mut left), Some(&mut right)],
            32,
        );

        assert_eq!(left, [0.0; 32]);
        assert_eq!(right, [0.0; 32]);
        assert_eq!(
            state.dangling_inputs.load(Ordering::Acquire),
            DANGLING_FL | DANGLING_FR
        );
    }

    #[test]
    fn a_meter_only_output_cannot_make_a_dangling_effect_generate_audio() {
        // A live output buffer models the hidden capture stream that activates
        // an otherwise unconnected effect output in PipeWire.
        let state = CallbackState::new(
            Box::new(TestProcessor::new(ProcessBehavior::EmitsConstant)),
            true,
        );
        let mut observed = [1.0; 64];

        run_buffers(&state, [None, None], [Some(&mut observed), None], 64);

        assert_eq!(observed, [0.0; 64]);
    }

    #[test]
    fn disconnected_channels_cannot_leak_a_processor_tail() {
        let state = CallbackState::new(
            Box::new(TestProcessor::new(ProcessBehavior::EmitsConstant)),
            true,
        );
        let mut connected_input = [0.5; 16];
        let mut left_output = [9.0; 16];
        let mut right_output = [9.0; 16];

        // Establish a non-zero processor result first, then disconnect both
        // inputs while keeping the output buffers active as a meter would.
        run_buffers(
            &state,
            [Some(&mut connected_input), None],
            [Some(&mut left_output), Some(&mut right_output)],
            16,
        );
        assert_eq!(left_output, [0.25; 16]);
        assert_eq!(right_output, [0.0; 16]);

        let mut left_output = [9.0; 16];
        let mut right_output = [9.0; 16];
        run_buffers(
            &state,
            [None, None],
            [Some(&mut left_output), Some(&mut right_output)],
            16,
        );
        assert_eq!(left_output, [0.0; 16]);
        assert_eq!(right_output, [0.0; 16]);
    }

    #[test]
    fn disabled_dangling_effect_is_silent() {
        let state = CallbackState::new(Box::new(TestProcessor::new(ProcessBehavior::Gain)), false);
        let mut left = [7.0; 16];
        let mut right = [7.0; 16];

        run_buffers(
            &state,
            [None, None],
            [Some(&mut left), Some(&mut right)],
            16,
        );

        assert_eq!(left, [0.0; 16]);
        assert_eq!(right, [0.0; 16]);
    }

    #[test]
    fn partial_stereo_inputs_are_processed_independently() {
        let state = CallbackState::new(Box::new(TestProcessor::new(ProcessBehavior::Gain)), true);
        let mut left_input = [0.25, -0.5, 0.75, -1.0];
        let mut left_output = [9.0; 4];
        let mut right_output = [9.0; 4];

        run_buffers(
            &state,
            [Some(&mut left_input), None],
            [Some(&mut left_output), Some(&mut right_output)],
            4,
        );

        assert_eq!(left_output, [0.5, -1.0, 1.5, -2.0]);
        assert_eq!(right_output, [0.0; 4]);
        assert_eq!(state.dangling_inputs.load(Ordering::Acquire), DANGLING_FR);

        let mut right_input = [0.125, -0.25, 0.375, -0.5];
        let mut left_output = [9.0; 4];
        let mut right_output = [9.0; 4];
        run_buffers(
            &state,
            [None, Some(&mut right_input)],
            [Some(&mut left_output), Some(&mut right_output)],
            4,
        );
        assert_eq!(left_output, [0.0; 4]);
        assert_eq!(right_output, [0.25, -0.5, 0.75, -1.0]);
        assert_eq!(state.dangling_inputs.load(Ordering::Acquire), DANGLING_FL);
    }

    #[test]
    fn mono_callback_uses_one_interleaved_channel_and_one_output_port() {
        let state = CallbackState::new_with_channels(
            Box::new(TestProcessor::new(ProcessBehavior::Gain)),
            true,
            1,
        );
        let mut input = [0.25, -0.5, 0.75, -1.0];
        let mut output = [9.0; 4];
        run_buffers(
            &state,
            [Some(&mut input), None],
            [Some(&mut output), None],
            4,
        );
        assert_eq!(output, [0.5, -1.0, 1.5, -2.0]);
        assert_eq!(state.channels, 1);
        assert_eq!(state.dangling_inputs.load(Ordering::Acquire), 0);
    }

    #[test]
    fn input_only_channel_is_not_reported_as_active() {
        let observed_mask = Arc::new(AtomicU16::new(u16::MAX));
        let state = CallbackState::new_with_channels(
            Box::new(TestProcessor::with_observed_channel_mask(
                ProcessBehavior::Gain,
                Some(observed_mask.clone()),
            )),
            true,
            2,
        );
        let mut left_input = [0.25; 4];
        let mut right_input = [0.75; 4];
        let mut left_output = [9.0; 4];

        run_buffers(
            &state,
            [Some(&mut left_input), Some(&mut right_input)],
            [Some(&mut left_output), None],
            4,
        );

        assert_eq!(observed_mask.load(Ordering::Acquire), 1);
        assert_eq!(left_output, [0.5; 4]);
    }

    #[test]
    fn lock_contention_is_transparent_on_connected_channels_and_silent_on_missing_channels() {
        let state = CallbackState::new(Box::new(TestProcessor::new(ProcessBehavior::Gain)), true);
        let _processor_guard = state.processor.lock().expect("test processor lock");
        let mut input = [0.25, -0.5, 0.75, -1.0];
        let mut left = [9.0; 4];
        let mut right = [9.0; 4];

        run_buffers(
            &state,
            [Some(&mut input), None],
            [Some(&mut left), Some(&mut right)],
            4,
        );

        assert_eq!(left, input);
        assert_eq!(right, [0.0; 4]);
    }

    #[test]
    fn processor_error_restores_the_sanitized_pass_through_fallback() {
        let state = CallbackState::new(
            Box::new(TestProcessor::new(ProcessBehavior::ErrorAfterMutation)),
            true,
        );
        let mut input = [0.25, -0.5, 0.75, -1.0];
        let mut left = [9.0; 4];
        let mut right = [9.0; 4];

        run_buffers(
            &state,
            [Some(&mut input), None],
            [Some(&mut left), Some(&mut right)],
            4,
        );

        assert_eq!(left, input);
        assert_eq!(right, [0.0; 4]);
        assert!(state.processor_failed.load(Ordering::Acquire));
    }

    #[test]
    fn processor_panic_is_caught_and_restores_the_pass_through_fallback() {
        let state = CallbackState::new(
            Box::new(TestProcessor::new(ProcessBehavior::PanicAfterMutation)),
            true,
        );
        let mut input = [0.25, -0.5, 0.75, -1.0];
        let mut left = [9.0; 4];
        let mut right = [9.0; 4];

        run_buffers(
            &state,
            [Some(&mut input), None],
            [Some(&mut left), Some(&mut right)],
            4,
        );

        assert_eq!(left, input);
        assert_eq!(right, [0.0; 4]);
        assert!(state.processor_failed.load(Ordering::Acquire));
    }

    #[test]
    fn non_finite_processor_output_is_rejected_without_replacing_the_fallback() {
        let state = CallbackState::new(
            Box::new(TestProcessor::new(ProcessBehavior::NonFinite)),
            true,
        );
        let mut input = [0.25, -0.5, 0.75, -1.0];
        let mut left = [9.0; 4];

        run_buffers(&state, [Some(&mut input), None], [Some(&mut left), None], 4);

        assert_eq!(left, input);
        assert!(state.processor_failed.load(Ordering::Acquire));
    }

    #[test]
    fn parameter_changes_cannot_inject_audio_into_missing_channels() {
        let state = CallbackState::new(Box::new(TestProcessor::new(ProcessBehavior::Gain)), true);
        for value in [0.0, 0.5, 1.0, 4.0, -2.0] {
            state
                .processor
                .lock()
                .expect("test processor lock")
                .processor
                .set_parameter("factor", value)
                .expect("test parameter");
            let mut left = [3.0; 24];
            let mut right = [3.0; 24];
            run_buffers(
                &state,
                [None, None],
                [Some(&mut left), Some(&mut right)],
                24,
            );
            assert_eq!(left, [0.0; 24]);
            assert_eq!(right, [0.0; 24]);
        }
    }

    #[test]
    fn repeated_dangling_blocks_remain_silent() {
        let state = CallbackState::new(
            Box::new(TestProcessor::new(ProcessBehavior::EmitsConstant)),
            true,
        );
        for _ in 0..64 {
            let mut left = [5.0; 128];
            let mut right = [5.0; 128];
            run_buffers(
                &state,
                [None, None],
                [Some(&mut left), Some(&mut right)],
                128,
            );
            assert_eq!(left, [0.0; 128]);
            assert_eq!(right, [0.0; 128]);
        }
    }

    #[test]
    fn missing_and_non_finite_inputs_are_read_as_zero() {
        let state = CallbackState::new(Box::new(TestProcessor::new(ProcessBehavior::Gain)), true);
        let mut input = [f32::NAN, f32::INFINITY, -f32::INFINITY, 0.25];
        let mut output = [9.0; 4];

        run_buffers(
            &state,
            [Some(&mut input), None],
            [Some(&mut output), None],
            4,
        );

        assert_eq!(output, [0.0, 0.0, 0.0, 0.5]);
    }

    #[test]
    fn dangling_input_diagnostics_are_metadata_only() {
        assert_eq!(dangling_input_message(0), None);
        assert_eq!(
            dangling_input_message(DANGLING_FL),
            Some("effect output is active while the FL input is disconnected")
        );
        assert_eq!(
            dangling_input_message(DANGLING_FR),
            Some("effect output is active while the FR input is disconnected")
        );
        assert_eq!(
            dangling_input_message(DANGLING_FL | DANGLING_FR),
            Some("effect output is active while both inputs are disconnected")
        );
    }
}
