//! Routing semantics, asserted rather than listened to.
//!
//! These are the Phase 3 exit criterion: the graph rules §9 of the Windows
//! parity roadmap requires -- connect, fan-out, mixing, transactional
//! replacement, honest failure -- proved against in-memory endpoints, so they
//! are settled before any driver exists to blame.

use pw_graph_effects::{AudioSpec, EffectDescriptor, EffectError, EffectProcessor};

use super::diagnostics::RouteFault;
use super::endpoints::{BufferSource, CaptureSink, Captured, LostSource};
use super::engine::{
    AudioSink, BranchSpec, DestinationSpec, ProcessorId, RouteId, RouteSpec, RouterConfig,
    RouterCore, RouterError, SinkId, SourceId,
};
use super::format::AudioFormat;

const MONO: AudioFormat = AudioFormat::new(48_000, 1);
const STEREO: AudioFormat = AudioFormat::new(48_000, 2);
const MONO_HALF_RATE: AudioFormat = AudioFormat::new(24_000, 1);

/// Four-frame blocks keep the expected buffers short enough to write out in
/// full, which is what makes a failure readable.
fn core() -> RouterCore {
    RouterCore::new(RouterConfig {
        block_frames: 4,
        clock_rate: 48_000,
    })
}

fn add_source(core: &mut RouterCore, id: u64, format: AudioFormat, samples: Vec<f32>) {
    core.add_source(
        SourceId(id),
        Box::new(BufferSource::looping(format, samples)),
    )
    .expect("the source id is fresh");
}

fn add_sink(core: &mut RouterCore, id: u64, format: AudioFormat) -> Captured {
    let (sink, captured) = CaptureSink::new(format);
    core.add_sink(SinkId(id), Box::new(sink))
        .expect("the sink id is fresh");
    captured
}

fn recorded(captured: &Captured) -> Vec<f32> {
    captured
        .lock()
        .expect("the capture mutex is not poisoned")
        .clone()
}

/// A gain stage, so effect behaviour can be asserted by arithmetic rather
/// than by ear.
struct Doubler {
    descriptor: EffectDescriptor,
    factor: f32,
    prepared: bool,
    fail: bool,
    panic: bool,
    non_finite: bool,
}

impl Doubler {
    fn new() -> Self {
        Self {
            descriptor: EffectDescriptor {
                id: "test.doubler".into(),
                name: "Doubler".into(),
                vendor: "qpwgraph-rs".into(),
                version: "1".into(),
                parameters: Vec::new(),
            },
            factor: 2.0,
            prepared: false,
            fail: false,
            panic: false,
            non_finite: false,
        }
    }

    fn failing() -> Self {
        Self {
            fail: true,
            ..Self::new()
        }
    }

    fn panicking() -> Self {
        Self {
            panic: true,
            ..Self::new()
        }
    }

    fn non_finite() -> Self {
        Self {
            non_finite: true,
            ..Self::new()
        }
    }
}

impl EffectProcessor for Doubler {
    fn descriptor(&self) -> &EffectDescriptor {
        &self.descriptor
    }

    fn prepare(&mut self, spec: AudioSpec) -> Result<(), EffectError> {
        spec.validate()?;
        self.prepared = true;
        Ok(())
    }

    fn process(&mut self, buffer: &mut [f32], _frames: u32) -> Result<(), EffectError> {
        assert!(
            self.prepared,
            "the router must prepare an effect before use"
        );
        for sample in buffer.iter_mut() {
            *sample *= self.factor;
        }
        if self.fail {
            return Err(EffectError::NotPrepared);
        }
        if self.panic {
            panic!("test processor panic");
        }
        if self.non_finite {
            buffer[0] = f32::NAN;
        }
        Ok(())
    }

    fn set_parameter(&mut self, id: &str, value: f32) -> Result<(), EffectError> {
        if id != "factor" {
            return Err(EffectError::UnsupportedParameter(id.into()));
        }
        self.factor = value;
        Ok(())
    }

    fn reset(&mut self) {}
}

fn spec(format: AudioFormat, frames: u32) -> AudioSpec {
    AudioSpec {
        sample_rate: format.sample_rate,
        channels: format.channels,
        max_frames: frames,
    }
}

/// One source into one destination, through one effect.
fn through(id: RouteId, source: SourceId, effect: ProcessorId, sink: SinkId) -> RouteSpec {
    RouteSpec {
        id,
        source,
        gain: 1.0,
        branches: vec![BranchSpec {
            processors: vec![effect],
            gain: 1.0,
            destinations: vec![DestinationSpec::new(sink)],
        }],
    }
}

/// One source into several destinations on a single unprocessed branch.
fn fan_out(id: RouteId, source: SourceId, destinations: Vec<DestinationSpec>) -> RouteSpec {
    RouteSpec {
        id,
        source,
        gain: 1.0,
        branches: vec![BranchSpec::to(destinations)],
    }
}

// ---------------------------------------------------------------- topology

#[test]
fn a_direct_route_carries_its_source_to_its_destination_unchanged() {
    let mut core = core();
    add_source(&mut core, 1, MONO, vec![0.1, 0.2, 0.3, 0.4]);
    let captured = add_sink(&mut core, 1, MONO);
    core.set_routes(&[RouteSpec::direct(RouteId(1), SourceId(1), SinkId(1))])
        .expect("a source and a sink that both exist");

    core.process();

    // Same rate, same channels: the route must be sample-exact, not merely
    // close. A conversion that quietly ran here would show up as rounding.
    assert_eq!(recorded(&captured), vec![0.1, 0.2, 0.3, 0.4]);
}

#[test]
fn one_source_can_fan_out_to_several_destinations() {
    let mut core = core();
    add_source(&mut core, 1, MONO, vec![0.5, 0.5, 0.5, 0.5]);
    let left = add_sink(&mut core, 1, MONO);
    let right = add_sink(&mut core, 2, MONO);
    core.set_routes(&[fan_out(
        RouteId(1),
        SourceId(1),
        vec![
            DestinationSpec::new(SinkId(1)),
            DestinationSpec::new(SinkId(2)),
        ],
    )])
    .expect("a route may hold several destinations");

    core.process();

    assert_eq!(recorded(&left), vec![0.5; 4]);
    assert_eq!(recorded(&right), vec![0.5; 4]);
}

#[test]
fn several_sources_into_one_destination_are_summed_not_raced() {
    let mut core = core();
    add_source(&mut core, 1, MONO, vec![0.25, 0.25, 0.25, 0.25]);
    add_source(&mut core, 2, MONO, vec![0.5, 0.5, 0.5, 0.5]);
    let captured = add_sink(&mut core, 1, MONO);
    core.set_routes(&[
        RouteSpec::direct(RouteId(1), SourceId(1), SinkId(1)),
        RouteSpec::direct(RouteId(2), SourceId(2), SinkId(1)),
    ])
    .expect("two routes may share a destination");

    core.process();

    // Whichever route ran second must not have overwritten the first.
    assert_eq!(recorded(&captured), vec![0.75; 4]);
}

#[test]
fn a_destination_is_written_once_per_block_however_many_routes_feed_it() {
    let mut core = core();
    add_source(&mut core, 1, MONO, vec![0.25; 4]);
    add_source(&mut core, 2, MONO, vec![0.25; 4]);
    let captured = add_sink(&mut core, 1, MONO);
    core.set_routes(&[
        RouteSpec::direct(RouteId(1), SourceId(1), SinkId(1)),
        RouteSpec::direct(RouteId(2), SourceId(2), SinkId(1)),
    ])
    .expect("two routes may share a destination");

    core.process();

    // Four frames, not eight: a second write per block would double the
    // sink's consumption rate and drift away from its clock.
    assert_eq!(recorded(&captured).len(), 4);
}

// -------------------------------------------------------------- conversion

#[test]
fn a_mono_source_reaches_a_stereo_destination_on_both_channels() {
    let mut core = core();
    add_source(&mut core, 1, MONO, vec![0.5, -0.5, 0.5, -0.5]);
    let captured = add_sink(&mut core, 1, STEREO);
    core.set_routes(&[RouteSpec::direct(RouteId(1), SourceId(1), SinkId(1))])
        .expect("channel counts are converted, not rejected");

    core.process();

    assert_eq!(
        recorded(&captured),
        vec![0.5, 0.5, -0.5, -0.5, 0.5, 0.5, -0.5, -0.5]
    );
}

#[test]
fn a_route_between_different_rates_delivers_proportionally_more_frames() {
    let mut core = core();
    add_source(&mut core, 1, MONO_HALF_RATE, vec![0.1, 0.2, 0.3, 0.4]);
    let captured = add_sink(&mut core, 1, MONO);
    core.set_routes(&[RouteSpec::direct(RouteId(1), SourceId(1), SinkId(1))])
        .expect("sample rates are converted, not rejected");

    // Sixteen source frames at 24 kHz are thirty-two at 48 kHz, give or take
    // the frames still inside the resampler.
    for _ in 0..4 {
        core.process();
    }

    let delivered = recorded(&captured).len();
    assert!(
        (30..=32).contains(&delivered),
        "expected about 32 frames at the doubled rate, got {delivered}"
    );
}

#[test]
fn the_conversion_ratio_is_reported_in_the_route_diagnostics() {
    let mut core = core();
    add_source(&mut core, 1, MONO_HALF_RATE, vec![0.1; 4]);
    add_sink(&mut core, 1, MONO);
    core.set_routes(&[RouteSpec::direct(RouteId(1), SourceId(1), SinkId(1))])
        .expect("a valid route");

    core.process();

    let metrics = core.metrics(RouteId(1)).expect("the route exists");
    // Half an input frame per output frame: the 24 kHz source feeds a 48 kHz
    // destination.
    assert_eq!(metrics.resampler_ratio, 0.5);
}

// -------------------------------------------------------------------- gain

#[test]
fn route_gain_is_applied_before_the_destinations() {
    let mut core = core();
    add_source(&mut core, 1, MONO, vec![0.25; 4]);
    let captured = add_sink(&mut core, 1, MONO);
    core.set_routes(&[RouteSpec {
        gain: 2.0,
        ..RouteSpec::direct(RouteId(1), SourceId(1), SinkId(1))
    }])
    .expect("a valid route");

    core.process();

    assert_eq!(recorded(&captured), vec![0.5; 4]);
}

#[test]
fn software_gain_goes_above_unity_where_a_windows_endpoint_cannot() {
    let mut core = core();
    add_source(&mut core, 1, MONO, vec![0.4; 4]);
    let captured = add_sink(&mut core, 1, MONO);
    core.set_routes(&[RouteSpec {
        gain: 1.5,
        ..RouteSpec::direct(RouteId(1), SourceId(1), SinkId(1))
    }])
    .expect("a valid route");

    core.process();

    // This is the boost parity §13 asks for: the endpoint's own volume still
    // tops out at unity and is reported honestly, and the extra gain lives in
    // the route qpwgraph owns.
    for sample in recorded(&captured) {
        assert!((sample - 0.6).abs() < 1e-6, "expected 0.6, got {sample}");
    }
}

#[test]
fn each_destination_carries_its_own_gain() {
    let mut core = core();
    add_source(&mut core, 1, MONO, vec![0.5; 4]);
    let loud = add_sink(&mut core, 1, MONO);
    let quiet = add_sink(&mut core, 2, MONO);
    core.set_routes(&[fan_out(
        RouteId(1),
        SourceId(1),
        vec![
            DestinationSpec {
                sink: SinkId(1),
                gain: 1.0,
            },
            DestinationSpec {
                sink: SinkId(2),
                gain: 0.5,
            },
        ],
    )])
    .expect("a valid route");

    core.process();

    assert_eq!(recorded(&loud), vec![0.5; 4]);
    assert_eq!(recorded(&quiet), vec![0.25; 4]);
}

// ----------------------------------------------------------------- effects

#[test]
fn an_effect_in_a_route_processes_the_audio_that_passes_through_it() {
    let mut core = core();
    add_source(&mut core, 1, MONO, vec![0.25; 4]);
    let captured = add_sink(&mut core, 1, MONO);
    core.add_processor(ProcessorId(1), Box::new(Doubler::new()), spec(MONO, 4))
        .expect("a fresh effect id");
    core.set_routes(&[through(RouteId(1), SourceId(1), ProcessorId(1), SinkId(1))])
        .expect("a valid route");

    core.process();

    assert_eq!(recorded(&captured), vec![0.5; 4]);
}

#[test]
fn an_effect_on_one_branch_does_not_reach_a_sibling_fan_out() {
    let mut core = core();
    add_source(&mut core, 1, MONO, vec![0.25; 4]);
    let processed = add_sink(&mut core, 1, MONO);
    let untouched = add_sink(&mut core, 2, MONO);
    core.add_processor(ProcessorId(1), Box::new(Doubler::new()), spec(MONO, 4))
        .expect("a fresh effect id");
    core.set_routes(&[RouteSpec {
        id: RouteId(1),
        source: SourceId(1),
        gain: 1.0,
        branches: vec![
            BranchSpec {
                processors: vec![ProcessorId(1)],
                gain: 1.0,
                destinations: vec![DestinationSpec::new(SinkId(1))],
            },
            BranchSpec::to(vec![DestinationSpec::new(SinkId(2))]),
        ],
    }])
    .expect("one source may feed a processed and an unprocessed path");

    core.process();

    // This is the whole reason branches exist. Inserting an effect into one
    // link must not process the other link out of the same source.
    assert_eq!(recorded(&processed), vec![0.5; 4]);
    assert_eq!(recorded(&untouched), vec![0.25; 4]);
}

#[test]
fn each_branch_of_a_route_meters_its_own_output() {
    let mut core = core();
    add_source(&mut core, 1, MONO, vec![0.25; 4]);
    add_sink(&mut core, 1, MONO);
    add_sink(&mut core, 2, MONO);
    core.add_processor(ProcessorId(1), Box::new(Doubler::new()), spec(MONO, 4))
        .expect("a fresh effect id");
    core.set_routes(&[RouteSpec {
        id: RouteId(1),
        source: SourceId(1),
        gain: 1.0,
        branches: vec![
            BranchSpec {
                processors: vec![ProcessorId(1)],
                gain: 1.0,
                destinations: vec![DestinationSpec::new(SinkId(1))],
            },
            BranchSpec::to(vec![DestinationSpec::new(SinkId(2))]),
        ],
    }])
    .expect("a valid route");

    core.process();

    // One level per path, because the two paths genuinely carry different
    // audio. A single route-level meter would have to lie about one of them.
    // The route-level reading is the first branch's, and reading it is what
    // clears that branch's held peak -- so it is read once, here.
    assert_eq!(core.meter(RouteId(1)).expect("the route exists").peak, 0.5);
    assert_eq!(
        core.branch_meter(RouteId(1), 1).expect("branch 1").peak,
        0.25
    );
    assert!(core.branch_meter(RouteId(1), 2).is_none());
}

#[test]
fn a_branch_carries_its_own_gain_after_its_effects() {
    let mut core = core();
    add_source(&mut core, 1, MONO, vec![0.25; 4]);
    let captured = add_sink(&mut core, 1, MONO);
    core.add_processor(ProcessorId(1), Box::new(Doubler::new()), spec(MONO, 4))
        .expect("a fresh effect id");
    core.set_routes(&[RouteSpec {
        id: RouteId(1),
        source: SourceId(1),
        gain: 2.0,
        branches: vec![BranchSpec {
            processors: vec![ProcessorId(1)],
            gain: 0.5,
            destinations: vec![DestinationSpec::new(SinkId(1))],
        }],
    }])
    .expect("a valid route");

    core.process();

    // 0.25 into route gain 2.0, doubled by the effect, then halved by the
    // branch: the order matters, and this pins it down.
    assert_eq!(recorded(&captured), vec![0.5; 4]);
}

#[test]
fn an_effect_may_not_be_used_by_two_branches_at_once() {
    let mut core = core();
    add_source(&mut core, 1, MONO, vec![0.25; 4]);
    add_sink(&mut core, 1, MONO);
    add_sink(&mut core, 2, MONO);
    core.add_processor(ProcessorId(1), Box::new(Doubler::new()), spec(MONO, 4))
        .expect("a fresh effect id");

    let error = core
        .set_routes(&[RouteSpec {
            id: RouteId(1),
            source: SourceId(1),
            gain: 1.0,
            branches: vec![
                BranchSpec {
                    processors: vec![ProcessorId(1)],
                    gain: 1.0,
                    destinations: vec![DestinationSpec::new(SinkId(1))],
                },
                BranchSpec {
                    processors: vec![ProcessorId(1)],
                    gain: 1.0,
                    destinations: vec![DestinationSpec::new(SinkId(2))],
                },
            ],
        }])
        // An effect is a stateful object, not a formula. Running one instance
        // over two different streams would interleave their state.
        .expect_err("one effect instance cannot process two streams");

    assert_eq!(error, RouterError::ProcessorInUse(ProcessorId(1)));
}

#[test]
fn a_bypassed_effect_passes_audio_through_untouched() {
    let mut core = core();
    add_source(&mut core, 1, MONO, vec![0.25; 4]);
    let captured = add_sink(&mut core, 1, MONO);
    core.add_processor(ProcessorId(1), Box::new(Doubler::new()), spec(MONO, 4))
        .expect("a fresh effect id");
    core.set_routes(&[through(RouteId(1), SourceId(1), ProcessorId(1), SinkId(1))])
        .expect("a valid route");

    core.set_processor_bypassed(ProcessorId(1), true)
        .expect("the effect exists");
    core.process();

    assert_eq!(recorded(&captured), vec![0.25; 4]);
}

#[test]
fn bypass_keeps_the_effect_and_its_configuration_rather_than_destroying_them() {
    let mut core = core();
    add_source(&mut core, 1, MONO, vec![0.25; 4]);
    let captured = add_sink(&mut core, 1, MONO);
    core.add_processor(ProcessorId(1), Box::new(Doubler::new()), spec(MONO, 4))
        .expect("a fresh effect id");
    core.set_routes(&[through(RouteId(1), SourceId(1), ProcessorId(1), SinkId(1))])
        .expect("a valid route");

    core.set_processor_parameter(ProcessorId(1), "factor", 4.0)
        .expect("the effect takes the parameter");
    core.set_processor_bypassed(ProcessorId(1), true)
        .expect("the effect exists");
    core.set_processor_bypassed(ProcessorId(1), false)
        .expect("the effect exists");
    core.process();

    // Coming back from bypass restores the configured factor, not the
    // default one: §12.1 requires bypass not to destroy configuration.
    assert_eq!(recorded(&captured), vec![1.0; 4]);
    assert_eq!(core.is_processor_bypassed(ProcessorId(1)), Some(false));
}

#[test]
fn an_effect_keeps_its_configuration_across_a_route_table_replacement() {
    let mut core = core();
    add_source(&mut core, 1, MONO, vec![0.25; 4]);
    let first = add_sink(&mut core, 1, MONO);
    let second = add_sink(&mut core, 2, MONO);
    core.add_processor(ProcessorId(1), Box::new(Doubler::new()), spec(MONO, 4))
        .expect("a fresh effect id");
    core.set_processor_parameter(ProcessorId(1), "factor", 3.0)
        .expect("the effect takes the parameter");
    core.set_routes(&[through(RouteId(1), SourceId(1), ProcessorId(1), SinkId(1))])
        .expect("a valid route");
    core.process();

    // Reroute the same effect onto a different destination.
    core.set_routes(&[through(RouteId(1), SourceId(1), ProcessorId(1), SinkId(2))])
        .expect("a valid route");
    core.process();

    assert_eq!(recorded(&first), vec![0.75; 4]);
    // The instance identity survived the move, so the parameter did too.
    assert_eq!(recorded(&second), vec![0.75; 4]);
}

#[test]
fn a_failing_effect_is_bypassed_for_the_block_and_reported() {
    let mut core = core();
    add_source(&mut core, 1, MONO, vec![0.25; 4]);
    let captured = add_sink(&mut core, 1, MONO);
    core.add_processor(ProcessorId(1), Box::new(Doubler::failing()), spec(MONO, 4))
        .expect("a fresh effect id");
    core.set_routes(&[through(RouteId(1), SourceId(1), ProcessorId(1), SinkId(1))])
        .expect("a valid route");

    core.process();

    // Audio keeps flowing -- a bad parameter must not become silence the
    // user cannot explain -- but the failure is on the record.
    assert_eq!(recorded(&captured), vec![0.25; 4]);
    let metrics = core.metrics(RouteId(1)).expect("the route exists");
    assert_eq!(metrics.fault, RouteFault::ProcessorFailed);
    assert!(metrics.fault_message().is_some());
}

#[test]
fn a_panicking_effect_is_bypassed_without_publishing_partial_audio() {
    let mut core = core();
    add_source(&mut core, 1, MONO, vec![0.25; 4]);
    let captured = add_sink(&mut core, 1, MONO);
    core.add_processor(
        ProcessorId(1),
        Box::new(Doubler::panicking()),
        spec(MONO, 4),
    )
    .expect("a fresh effect id");
    core.set_routes(&[through(RouteId(1), SourceId(1), ProcessorId(1), SinkId(1))])
        .expect("a valid route");

    core.process();

    assert_eq!(recorded(&captured), vec![0.25; 4]);
    assert_eq!(
        core.metrics(RouteId(1)).expect("the route exists").fault,
        RouteFault::ProcessorFailed
    );
}

#[test]
fn a_non_finite_effect_result_is_bypassed_without_publishing_invalid_audio() {
    let mut core = core();
    add_source(&mut core, 1, MONO, vec![0.25; 4]);
    let captured = add_sink(&mut core, 1, MONO);
    core.add_processor(
        ProcessorId(1),
        Box::new(Doubler::non_finite()),
        spec(MONO, 4),
    )
    .expect("a fresh effect id");
    core.set_routes(&[through(RouteId(1), SourceId(1), ProcessorId(1), SinkId(1))])
        .expect("a valid route");

    core.process();

    assert_eq!(recorded(&captured), vec![0.25; 4]);
    assert_eq!(
        core.metrics(RouteId(1)).expect("the route exists").fault,
        RouteFault::ProcessorFailed
    );
}

#[test]
fn an_effect_prepared_for_another_geometry_is_refused_rather_than_handed_the_wrong_buffer() {
    let mut core = core();
    add_source(&mut core, 1, MONO, vec![0.25; 4]);
    add_sink(&mut core, 1, MONO);
    core.add_processor(ProcessorId(1), Box::new(Doubler::new()), spec(STEREO, 4))
        .expect("a fresh effect id");

    let error = core
        .set_routes(&[through(RouteId(1), SourceId(1), ProcessorId(1), SinkId(1))])
        .expect_err("a stereo effect cannot process a mono route");

    assert!(matches!(error, RouterError::ProcessorFormatMismatch { .. }));
}

#[test]
fn an_effect_prepared_for_shorter_blocks_than_the_router_uses_is_refused() {
    let mut core = core();
    add_source(&mut core, 1, MONO, vec![0.25; 4]);
    add_sink(&mut core, 1, MONO);
    core.add_processor(ProcessorId(1), Box::new(Doubler::new()), spec(MONO, 2))
        .expect("a fresh effect id");

    let error = core
        .set_routes(&[through(RouteId(1), SourceId(1), ProcessorId(1), SinkId(1))])
        .expect_err("an effect must agree to the router's block size");

    assert!(matches!(error, RouterError::ProcessorBlockTooSmall { .. }));
}

#[test]
fn an_effect_still_used_by_a_route_cannot_be_removed_out_from_under_it() {
    let mut core = core();
    add_source(&mut core, 1, MONO, vec![0.25; 4]);
    add_sink(&mut core, 1, MONO);
    core.add_processor(ProcessorId(1), Box::new(Doubler::new()), spec(MONO, 4))
        .expect("a fresh effect id");
    core.set_routes(&[through(RouteId(1), SourceId(1), ProcessorId(1), SinkId(1))])
        .expect("a valid route");

    assert!(matches!(
        core.remove_processor(ProcessorId(1)),
        Err(RouterError::StillRouted(id)) if id == RouteId(1)
    ));
}

// ------------------------------------------------------------ transactions

#[test]
fn a_rejected_route_table_leaves_the_previous_one_running() {
    let mut core = core();
    add_source(&mut core, 1, MONO, vec![0.5; 4]);
    add_source(&mut core, 2, MONO, vec![0.5; 4]);
    let captured = add_sink(&mut core, 1, MONO);
    core.set_routes(&[RouteSpec::direct(RouteId(1), SourceId(1), SinkId(1))])
        .expect("a valid route");

    let error = core
        .set_routes(&[
            RouteSpec::direct(RouteId(1), SourceId(1), SinkId(1)),
            RouteSpec::direct(RouteId(2), SourceId(2), SinkId(99)),
        ])
        .expect_err("sink 99 does not exist");
    assert_eq!(error, RouterError::UnknownSink(SinkId(99)));

    core.process();

    // Nothing was applied in part: the working route is untouched, which is
    // the rollback a reroute depends on.
    assert_eq!(core.route_ids(), vec![RouteId(1)]);
    assert_eq!(recorded(&captured), vec![0.5; 4]);
}

#[test]
fn a_source_cannot_feed_two_routes_at_once() {
    let mut core = core();
    add_source(&mut core, 1, MONO, vec![0.5; 4]);
    add_sink(&mut core, 1, MONO);
    add_sink(&mut core, 2, MONO);

    let error = core
        .set_routes(&[
            RouteSpec::direct(RouteId(1), SourceId(1), SinkId(1)),
            RouteSpec::direct(RouteId(2), SourceId(1), SinkId(2)),
        ])
        .expect_err("one source, one route: fan-out belongs to the route");

    // Fan-out is expressed as several destinations on one route, so that the
    // source is pulled exactly once per block.
    assert_eq!(error, RouterError::SourceInUse(SourceId(1)));
}

#[test]
fn a_route_with_no_destination_is_refused() {
    let mut core = core();
    add_source(&mut core, 1, MONO, vec![0.5; 4]);

    let error = core
        .set_routes(&[fan_out(RouteId(1), SourceId(1), Vec::new())])
        .expect_err("a route that reaches nowhere is not a route");

    assert_eq!(error, RouterError::RouteWithoutDestination(RouteId(1)));
}

#[test]
fn the_same_destination_twice_on_one_route_is_refused() {
    let mut core = core();
    add_source(&mut core, 1, MONO, vec![0.5; 4]);
    add_sink(&mut core, 1, MONO);

    let error = core
        .set_routes(&[fan_out(
            RouteId(1),
            SourceId(1),
            vec![
                DestinationSpec::new(SinkId(1)),
                DestinationSpec::new(SinkId(1)),
            ],
        )])
        .expect_err("a duplicate destination would double the audio");

    assert_eq!(error, RouterError::DuplicateDestination(SinkId(1)));
}

#[test]
fn two_routes_cannot_claim_the_same_identity() {
    let mut core = core();
    add_source(&mut core, 1, MONO, vec![0.5; 4]);
    add_source(&mut core, 2, MONO, vec![0.5; 4]);
    add_sink(&mut core, 1, MONO);
    add_sink(&mut core, 2, MONO);

    let error = core
        .set_routes(&[
            RouteSpec::direct(RouteId(1), SourceId(1), SinkId(1)),
            RouteSpec::direct(RouteId(1), SourceId(2), SinkId(2)),
        ])
        .expect_err("an ambiguous route id would make undo unaddressable");

    assert_eq!(error, RouterError::DuplicateRoute(RouteId(1)));
}

#[test]
fn registering_the_same_id_twice_is_refused() {
    let mut core = core();
    add_source(&mut core, 1, MONO, vec![0.5; 4]);
    assert_eq!(
        core.add_source(
            SourceId(1),
            Box::new(BufferSource::looping(MONO, vec![0.0]))
        ),
        Err(RouterError::DuplicateSource(SourceId(1)))
    );
}

#[test]
fn a_device_with_no_usable_geometry_is_refused_at_registration() {
    let mut core = core();
    let broken = AudioFormat::new(48_000, 0);
    let (sink, _captured) = CaptureSink::new(broken);
    assert_eq!(
        core.add_sink(SinkId(1), Box::new(sink) as Box<dyn AudioSink>),
        Err(RouterError::InvalidFormat {
            sample_rate: 48_000,
            channels: 0,
        })
    );
}

#[test]
fn a_source_still_used_by_a_route_cannot_be_removed_out_from_under_it() {
    let mut core = core();
    add_source(&mut core, 1, MONO, vec![0.5; 4]);
    add_sink(&mut core, 1, MONO);
    core.set_routes(&[RouteSpec::direct(RouteId(1), SourceId(1), SinkId(1))])
        .expect("a valid route");

    assert!(matches!(
        core.remove_source(SourceId(1)),
        Err(RouterError::StillRouted(RouteId(1)))
    ));

    // Clearing the table releases it, and the device comes back to the
    // caller so it is torn down on the caller's thread.
    core.set_routes(&[]).expect("an empty table is valid");
    assert!(core.remove_source(SourceId(1)).is_ok());
}

// ------------------------------------------------------------------ health

#[test]
fn a_lost_source_stops_its_route_and_is_named_in_the_report() {
    let mut core = core();
    core.add_source(SourceId(1), Box::new(LostSource::new(MONO)))
        .expect("a fresh id");
    let captured = add_sink(&mut core, 1, MONO);
    core.set_routes(&[RouteSpec::direct(RouteId(1), SourceId(1), SinkId(1))])
        .expect("a valid route");

    let report = core.process();

    assert_eq!(report.lost, vec![RouteId(1)]);
    // A dead device must not be presented as a quiet one.
    assert!(recorded(&captured).is_empty());
    assert_eq!(
        core.metrics(RouteId(1)).expect("the route exists").fault,
        RouteFault::DeviceLost
    );
}

#[test]
fn a_lost_destination_stops_its_route_and_is_named_in_the_report() {
    let mut core = core();
    add_source(&mut core, 1, MONO, vec![0.5; 4]);
    let (sink, _captured) = CaptureSink::lost(MONO);
    core.add_sink(SinkId(1), Box::new(sink))
        .expect("a fresh id");
    core.set_routes(&[RouteSpec::direct(RouteId(1), SourceId(1), SinkId(1))])
        .expect("a valid route");

    let report = core.process();

    assert_eq!(report.lost, vec![RouteId(1)]);
    assert_eq!(
        core.metrics(RouteId(1)).expect("the route exists").fault,
        RouteFault::DeviceLost
    );
}

#[test]
fn a_starved_source_contributes_silence_rather_than_the_previous_block() {
    let mut core = core();
    // Four frames of audio, then nothing: the second block has no input.
    core.add_source(
        SourceId(1),
        Box::new(BufferSource::new(MONO, vec![0.5, 0.5, 0.5, 0.5])),
    )
    .expect("a fresh id");
    let captured = add_sink(&mut core, 1, MONO);
    core.set_routes(&[RouteSpec::direct(RouteId(1), SourceId(1), SinkId(1))])
        .expect("a valid route");

    core.process();
    core.process();

    // Nothing new was written, rather than the first block being replayed as
    // if it were fresh audio.
    assert_eq!(recorded(&captured), vec![0.5; 4]);
    let metrics = core.metrics(RouteId(1)).expect("the route exists");
    assert_eq!(metrics.source_underruns, 4);
    assert_eq!(metrics.fault, RouteFault::SourceStarved);
}

#[test]
fn a_destination_that_cannot_keep_up_is_counted_rather_than_ignored() {
    let mut core = core();
    add_source(&mut core, 1, MONO, vec![0.5; 4]);
    let (sink, captured) = CaptureSink::throttled(MONO, 1);
    core.add_sink(SinkId(1), Box::new(sink))
        .expect("a fresh id");
    core.set_routes(&[RouteSpec::direct(RouteId(1), SourceId(1), SinkId(1))])
        .expect("a valid route");

    core.process();

    assert_eq!(recorded(&captured), vec![0.5]);
    let metrics = core.metrics(RouteId(1)).expect("the route exists");
    assert_eq!(metrics.sink_overruns, 3);
    assert_eq!(metrics.fault, RouteFault::SinkBackedUp);
}

#[test]
fn a_fault_clears_once_the_route_is_healthy_again() {
    let mut core = core();
    core.add_source(
        SourceId(1),
        Box::new(BufferSource::new(MONO, vec![0.5, 0.5])),
    )
    .expect("a fresh id");
    add_sink(&mut core, 1, MONO);
    core.set_routes(&[RouteSpec::direct(RouteId(1), SourceId(1), SinkId(1))])
        .expect("a valid route");

    core.process();
    assert_eq!(
        core.metrics(RouteId(1)).expect("the route exists").fault,
        RouteFault::SourceStarved
    );

    // Refill the source by replacing it, then run a full block.
    core.set_routes(&[]).expect("an empty table is valid");
    let _ = core.remove_source(SourceId(1));
    add_source(&mut core, 1, MONO, vec![0.5; 4]);
    core.set_routes(&[RouteSpec::direct(RouteId(1), SourceId(1), SinkId(1))])
        .expect("a valid route");
    core.process();

    assert_eq!(
        core.metrics(RouteId(1)).expect("the route exists").fault,
        RouteFault::None
    );
}

#[test]
fn resetting_a_route_is_recorded_as_a_discontinuity_and_a_restart() {
    let mut core = core();
    add_source(&mut core, 1, MONO, vec![0.5; 4]);
    add_sink(&mut core, 1, MONO);
    core.set_routes(&[RouteSpec::direct(RouteId(1), SourceId(1), SinkId(1))])
        .expect("a valid route");

    core.reset_route(RouteId(1)).expect("the route exists");

    let metrics = core.metrics(RouteId(1)).expect("the route exists");
    // A route that keeps needing this should be visible, not merely quiet.
    assert_eq!(metrics.discontinuities, 1);
    assert_eq!(metrics.restarts, 1);
}

#[test]
fn resetting_a_route_that_does_not_exist_says_so() {
    let mut core = core();
    assert_eq!(
        core.reset_route(RouteId(7)),
        Err(RouterError::UnknownRoute(RouteId(7)))
    );
}

// ----------------------------------------------------------------- meters

#[test]
fn a_route_meters_what_its_destinations_receive_including_rms() {
    let mut core = core();
    add_source(&mut core, 1, MONO, vec![1.0, -1.0, 1.0, -1.0]);
    add_sink(&mut core, 1, MONO);
    core.set_routes(&[RouteSpec::direct(RouteId(1), SourceId(1), SinkId(1))])
        .expect("a valid route");

    core.process();

    let reading = core.meter(RouteId(1)).expect("the route exists");
    assert!(reading.available);
    assert_eq!(reading.peak, 1.0);
    // The reading Core Audio's peak-only meter cannot provide.
    assert!((reading.rms - 1.0).abs() < 1e-6);
}

#[test]
fn a_source_meters_what_the_device_produced_not_what_the_route_did_to_it() {
    let mut core = core();
    add_source(&mut core, 1, MONO, vec![0.25; 4]);
    add_sink(&mut core, 1, MONO);
    core.add_processor(ProcessorId(1), Box::new(Doubler::new()), spec(MONO, 4))
        .expect("a fresh effect id");
    core.set_routes(&[RouteSpec {
        gain: 2.0,
        ..through(RouteId(1), SourceId(1), ProcessorId(1), SinkId(1))
    }])
    .expect("a valid route");

    core.process();

    // The device's node shows what the device produced. PipeWire meters a
    // port with the audio that port carries, and a microphone's level must
    // not jump because something downstream added gain.
    let source = core.source_meter(RouteId(1)).expect("the route exists");
    assert_eq!(source.peak, 0.25);
    assert!((source.rms - 0.25).abs() < 1e-6);
    // The branch meter is the other reading: 0.25, doubled by route gain,
    // doubled again by the effect.
    assert_eq!(core.meter(RouteId(1)).expect("the route exists").peak, 1.0);
}

#[test]
fn a_retired_routes_source_level_stops_being_reported() {
    let mut core = core();
    add_source(&mut core, 1, MONO, vec![0.5; 4]);
    add_sink(&mut core, 1, MONO);
    core.set_routes(&[RouteSpec::direct(RouteId(1), SourceId(1), SinkId(1))])
        .expect("a valid route");
    core.process();
    assert!(core.source_meter(RouteId(1)).expect("routed").available);

    core.set_routes(&[]).expect("an empty table is valid");

    assert!(!core.source_meter(RouteId(1)).expect("remembered").available);
    core.forget_retired_routes();
    assert!(core.source_meter(RouteId(1)).is_none());
}

#[test]
fn the_meter_reads_after_the_effect_chain_not_before_it() {
    let mut core = core();
    add_source(&mut core, 1, MONO, vec![0.25; 4]);
    add_sink(&mut core, 1, MONO);
    core.add_processor(ProcessorId(1), Box::new(Doubler::new()), spec(MONO, 4))
        .expect("a fresh effect id");
    core.set_routes(&[through(RouteId(1), SourceId(1), ProcessorId(1), SinkId(1))])
        .expect("a valid route");

    core.process();

    // Post-effect is the cross-platform definition §12.3 asks to fix; a
    // pre-effect reading here would say 0.25.
    assert_eq!(core.meter(RouteId(1)).expect("the route exists").peak, 0.5);
}

#[test]
fn meters_survive_a_route_table_replacement_that_keeps_the_route() {
    let mut core = core();
    add_source(&mut core, 1, MONO, vec![0.5; 4]);
    add_sink(&mut core, 1, MONO);
    add_sink(&mut core, 2, MONO);
    core.set_routes(&[RouteSpec::direct(RouteId(1), SourceId(1), SinkId(1))])
        .expect("a valid route");
    core.process();

    core.set_routes(&[RouteSpec::direct(RouteId(1), SourceId(1), SinkId(2))])
        .expect("a valid route");

    // A reroute must not blank the level bar or reset the dropout counts
    // that were about to explain why the user rerouted.
    assert!(core.meter(RouteId(1)).expect("the route exists").available);
    assert!(core.metrics(RouteId(1)).is_some());
}

#[test]
fn a_retired_route_stops_reporting_a_level_it_no_longer_carries() {
    let mut core = core();
    add_source(&mut core, 1, MONO, vec![0.5; 4]);
    add_sink(&mut core, 1, MONO);
    core.set_routes(&[RouteSpec::direct(RouteId(1), SourceId(1), SinkId(1))])
        .expect("a valid route");
    core.process();

    core.set_routes(&[]).expect("an empty table is valid");

    assert!(!core.meter(RouteId(1)).expect("still remembered").available);
    core.forget_retired_routes();
    assert!(core.meter(RouteId(1)).is_none());
}

#[test]
fn the_frame_clock_advances_by_one_block_per_cycle() {
    let mut core = core();
    assert_eq!(core.frame_clock(), 0);
    core.process();
    core.process();
    assert_eq!(core.frame_clock(), 8);
}
