//! The measuring probe: activate a plugin and hold its
//! claimed tail and latency against what it renders.
//!
//! [`measure_at`] is one experiment at one configuration, over any
//! [`Processor`]; [`measure`] loads a plugin by its descriptor, runs it at each
//! of [`CONFIGS`] and keeps the one record [`Measured::merge`] makes of them.
//!
//! The experiment, in order:
//!
//! 1. **Silence in.** Render [`SILENCE`] of nothing (no input, no note). If the
//!    output never falls below the floor ([`PROBE_FLOOR`]) the plugin makes its
//!    own sound (hiss, a noise generator, an oscillator): the verdict is
//!    `NeverSettles` with the level it held, and nothing else is judged, since
//!    a tail measured against a floor the plugin cannot meet is a lie about
//!    the plugin.
//! 2. **Reset**, so the stimulus meets a plugin with no history.
//! 3. **Stimulus.** An effect gets an impulse of [`IMPULSE`] on every input
//!    channel at frame 0, then silence. An instrument (no audio input) gets a
//!    note-on at frame 0 and a note-off [`NOTE_LENGTH`] later.
//! 4. **Render until it settles**: until the output has stayed below the floor
//!    for [`SETTLE`] after its last frame above it (and after the note-off), or
//!    the render budget ([`BUDGET_MARGIN`] past `MAX_NODE_TAIL`, in time) is spent.
//!
//! The figures: the first frame above the floor is the latency (an onset, so a
//! disagreement with the report is recorded and never corrected); the last is
//! the tail, counted from the impulse or the note-off and less the reported
//! latency, since a tail excludes latency (doc 013). The impulse is 0.1 and not
//! 1.0 because a limiter or a saturator answers a full-scale impulse unlike
//! program material, so a figure is a lower bound.

use std::path::Path;
use std::time::{Duration, Instant};

use tutti_graph::{MAX_NODE_TAIL, Offset, UmpAt};
use tutti_plugin_types::{
    Controller, Deadline, InProcess, LatencyVerdict, Load, MainThreadMarker, MaxBlock,
    MeasureFailure, Measured, NotChecked, PROBE_FLOOR, PluginDescriptor, PluginError, PluginIo,
    PluginTail, Prepare, ProbeConfig, ProcessContext, ProcessOutput, Processor, StateHash,
    TailVerdict, Verdict, channel_block,
};
use tutti_ump::{
    Address, Attribute, Message, MidiChannel, MidiGroup, NoteNumber, NoteVelocity, Ump, Voice2,
};
use tutti_units::{Amplitude, Db, SampleRate, Samples, Seconds};

/// The impulse an effect is given, on every input channel (0.1, not 1.0, for the nonlinear
/// plugins). **The probe measures at this
/// level**: a figure is of that stimulus, not a bound for a nonlinear plugin (a
/// gate or an expander may never open at 0.1, which reads as `Silent`).
pub const IMPULSE: Amplitude = Amplitude(0.1);

/// How long silence is rendered before the stimulus: what decides
/// `NeverSettles`.
pub const SILENCE: Seconds = Seconds(1.0);

/// How long the output must stay below the floor, after its last frame above
/// it, for the plugin to count as settled.
pub const SETTLE: Seconds = Seconds(1.0);

/// What the stimulus render runs past [`tutti_graph::MAX_NODE_TAIL`]: **the render
/// budget is `MAX_NODE_TAIL` plus this**, a time converted at the configuration's
/// rate, so it follows the graph's bound at every rate and the two cannot drift. A
/// plugin still above the floor when the budget runs out has rung past the bound, so:
///
/// - an **effect** is judged by its lower bound: the last frame above the floor,
///   which is beyond `MAX_NODE_TAIL`, so it reads as `TailLonger` (against any
///   claim) and `Processing::ceiling` declares it `Unbounded`;
/// - an **instrument** is `NotChecked(NeverReachedFloor)`: a held note has no
///   lower bound worth reporting.
pub const BUDGET_MARGIN: Seconds = Seconds(5.0);

/// How long one plugin may render at one configuration before it loses its row
/// ([`MeasureFailure::TimedOut`]): a plugin slower than real time over the
/// render budget. The probe checks it between blocks, so a plugin that never
/// returns from one is the host's deadline for the whole probe. A render is also
/// bounded by what is left of the probe's run budget (see [`measure`]), so the
/// sum over a file's plugins never passes the host's deadline.
pub const RENDER_DEADLINE: Duration = Duration::from_secs(60);

/// How long an instrument's note is held before its note-off.
pub const NOTE_LENGTH: Seconds = Seconds(0.1);

/// The key an instrument is played (middle C).
pub const NOTE: u8 = 60;

/// The note's velocity as a MIDI 2.0 value (0x8000 is the midpoint,
/// M2-115 section 3).
const VELOCITY: u16 = 0x8000;

/// The configurations the probe measures at, in order: a
/// figure can depend on the rate and the block.
pub const CONFIGS: [(SampleRate, MaxBlock); 2] = [
    (SampleRate::SR_48K, MaxBlock::of(Samples(512))),
    (SampleRate::SR_44K1, MaxBlock::of(Samples(64))),
];

/// What the experiment needs to know about the plugin it runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Subject {
    /// Input channels, all buses.
    pub inputs: usize,
    /// Output channels, all buses.
    pub outputs: usize,
    /// Whether the stimulus is a note, and the tail is measured from its
    /// note-off: the plugin has no audio input.
    pub instrument: bool,
    /// The tail the plugin reports.
    pub tail: PluginTail,
    /// The latency the plugin reports.
    pub latency: Samples,
}

/// One block's buffers, rendered through a processor.
struct Render<'p, P> {
    processor: &'p mut P,
    block: usize,
    ins: Vec<Vec<f32>>,
    outs: Vec<Vec<f32>>,
    out: ProcessOutput,
}

impl<'p, P: Processor> Render<'p, P> {
    fn new(processor: &'p mut P, subject: Subject, block: MaxBlock) -> Self {
        let block = block.get();
        Self {
            processor,
            block,
            ins: vec![vec![0.0; block]; subject.inputs],
            outs: vec![vec![0.0; block]; subject.outputs],
            out: ProcessOutput::default(),
        }
    }

    /// One block of `block` frames: the inputs as they stand and `events`.
    fn run(&mut self, events: &[UmpAt]) -> Result<(), PluginError> {
        for ch in &mut self.outs {
            ch.fill(0.0);
        }
        self.out.clear();
        let ins: Vec<&[f32]> = self.ins.iter().map(Vec::as_slice).collect();
        let mut outs: Vec<&mut [f32]> = self.outs.iter_mut().map(Vec::as_mut_slice).collect();
        let ctx = ProcessContext::new().midi(events);
        let mut io = PluginIo {
            buffer: channel_block(Samples(self.block), &ins, &mut outs).expect("equal channels"),
            ctx: &ctx,
            out: &mut self.out,
        };
        self.processor.process(&mut io)
    }

    /// The first frame of the block above `floor` on any output channel.
    fn first_above(&self, floor: f32) -> Option<usize> {
        (0..self.block).find(|&i| self.outs.iter().any(|ch| above(ch[i], floor)))
    }

    /// The last frame of the block above `floor` on any output channel.
    fn last_above(&self, floor: f32) -> Option<usize> {
        (0..self.block)
            .rev()
            .find(|&i| self.outs.iter().any(|ch| above(ch[i], floor)))
    }

    /// The block's peak over every output channel; a NaN is an infinite peak,
    /// since a plugin that writes one is not quiet.
    fn peak(&self) -> f32 {
        self.outs
            .iter()
            .flatten()
            .map(|v| if v.is_nan() { f32::INFINITY } else { v.abs() })
            .fold(0.0, f32::max)
    }
}

/// Whether `x` is above the floor. Not `x.abs() > floor`: a NaN is above it.
fn above(x: f32, floor: f32) -> bool {
    x.is_nan() || x.abs() > floor
}

fn note(on: bool, offset: usize, block: usize) -> UmpAt {
    let address = Address {
        group: MidiGroup::FIRST,
        channel: MidiChannel::of(0),
    };
    let (note, velocity, attribute) = (
        NoteNumber::of(NOTE),
        NoteVelocity::new(if on { VELOCITY } else { 0 }),
        Attribute::None,
    );
    let voice = if on {
        Voice2::NoteOn {
            note,
            velocity,
            attribute,
        }
    } else {
        Voice2::NoteOff {
            note,
            velocity,
            attribute,
        }
    };
    let offset = Offset::new(Samples(offset), Samples(block)).expect("an offset inside its block");
    (offset, Ump::from(Message::Midi2(address, voice)))
}

/// One experiment on an activated plugin, at the configuration `config` it was
/// activated at, with no deadline.
///
/// # Errors
///
/// [`MeasureFailure::Render`] for a block the plugin failed to render.
pub fn measure_at<P: Processor>(
    processor: &mut P,
    subject: Subject,
    config: ProbeConfig,
) -> Result<Measured, MeasureFailure> {
    measure_within(processor, subject, config, Deadline::never())
}

/// [`measure_at`] that stops at `deadline`, read from the clock before each block.
///
/// # Errors
///
/// [`MeasureFailure::Render`] for a block the plugin failed to render,
/// [`MeasureFailure::TimedOut`] if `deadline` passed first.
pub fn measure_within<P: Processor>(
    processor: &mut P,
    subject: Subject,
    config: ProbeConfig,
    deadline: Deadline,
) -> Result<Measured, MeasureFailure> {
    let rendering = |e: PluginError| MeasureFailure::Render {
        rate: config.rate,
        block: config.block,
        reason: e.to_string(),
    };
    let timed_out = || MeasureFailure::TimedOut {
        rate: config.rate,
        block: config.block,
    };
    // The render is `f32`: the floor narrows once.
    let floor = config.floor.to_amplitude().get() as f32;
    let frames = |t: Seconds| t.to_samples_ceil(config.rate).get();
    let mut run = Render::new(processor, subject, config.block);
    let block = run.block;

    // 1. Silence in: does the plugin settle at all?
    let mut held = f32::INFINITY;
    for _ in 0..frames(SILENCE).div_ceil(block) {
        if deadline.passed(Instant::now()) {
            return Err(timed_out());
        }
        run.run(&[]).map_err(rendering)?;
        held = held.min(run.peak());
    }
    if held > floor {
        let level = Amplitude(f64::from(held)).to_db();
        run.processor.stop();
        return Ok(unsettled(config, level));
    }

    // 2. A plugin with no history.
    run.processor.reset();

    // 3 and 4. The stimulus, rendered until it settles.
    let judge_tail = matches!(subject.tail, PluginTail::None | PluginTail::Finite(_));
    let (settle, budget) = (frames(SETTLE), frames(MAX_NODE_TAIL + BUDGET_MARGIN));
    let note_off = subject.instrument.then(|| frames(NOTE_LENGTH));
    // The earliest frame the stimulus can be heard at: the settle window starts
    // here, never before, so a plugin that reports more latency than `SETTLE`
    // is not judged settled before its output has begun.
    let heard_from = note_off.unwrap_or(0) + subject.latency.get();
    let mut onset = None;
    let mut last = None;
    let mut at = 0usize;
    loop {
        for ch in &mut run.ins {
            ch.fill(0.0);
        }
        let mut events = Vec::new();
        if subject.instrument {
            // An instrument is played; whatever audio input it has hears silence.
            if at == 0 {
                events.push(note(true, 0, block));
            }
            if let Some(off) = note_off.filter(|off| (at..at + block).contains(off)) {
                events.push(note(false, off - at, block));
            }
        } else if at == 0 {
            for ch in &mut run.ins {
                // The probe renders in `f32`: the impulse narrows once.
                ch[0] = IMPULSE.get() as f32;
            }
        }
        if deadline.passed(Instant::now()) {
            return Err(timed_out());
        }
        run.run(&events).map_err(rendering)?;
        if onset.is_none() {
            onset = run.first_above(floor).map(|i| at + i);
        }
        if let Some(i) = run.last_above(floor) {
            last = Some(at + i);
        }
        at += block;
        let quiet_from = last.unwrap_or(0).max(heard_from);
        let done = if judge_tail {
            at >= budget || at.saturating_sub(quiet_from) >= settle
        } else {
            // The tail is not judged: all that is wanted is the onset.
            onset.is_some() || at >= heard_from + settle
        };
        if done {
            let settled = at.saturating_sub(quiet_from) >= settle;
            run.processor.stop();
            return Ok(judge(subject, config, onset, last, note_off, settled));
        }
    }
}

/// The record of a plugin that never fell below the floor with silence in.
fn unsettled(config: ProbeConfig, level: Db) -> Measured {
    Measured {
        tail: None,
        latency: None,
        config,
        verdict: Verdict {
            tail: TailVerdict::NeverSettles { level },
            latency: LatencyVerdict::NeverSettles { level },
        },
    }
}

/// What the figures say of the claims. Pure.
fn judge(
    subject: Subject,
    config: ProbeConfig,
    onset: Option<usize>,
    last: Option<usize>,
    note_off: Option<usize>,
    settled: bool,
) -> Measured {
    let latency_figure = if subject.instrument {
        None
    } else {
        onset.map(Samples)
    };
    let latency = match (subject.instrument, latency_figure) {
        (true, _) => LatencyVerdict::NotChecked(NotChecked::Instrument),
        (false, None) => LatencyVerdict::NotChecked(NotChecked::Silent),
        (false, Some(m)) if m == subject.latency => LatencyVerdict::Honest,
        (false, Some(m)) => LatencyVerdict::LatencyDiffers {
            reported: subject.latency,
            measured: m,
        },
    };

    // The tail, from the impulse (frame 0) or the note-off, less the latency the
    // plugin reports: a tail excludes latency.
    let from = note_off.unwrap_or(0);
    let figure = last
        .map(|l| Samples(l.saturating_sub(from)).remaining_after(subject.latency))
        .unwrap_or(Samples::ZERO);
    let (tail, tail_verdict) = match subject.tail {
        PluginTail::Unbounded => (None, TailVerdict::NotChecked(NotChecked::UnboundedClaim)),
        PluginTail::Unknown => (None, TailVerdict::NotChecked(NotChecked::UnknownClaim)),
        // Nothing rose above the floor: there is no ring to measure, which is
        // not the same as a ring of none.
        _ if last.is_none() => (None, TailVerdict::NotChecked(NotChecked::Silent)),
        // A note that is still sounding at the end of the budget has no figure.
        _ if subject.instrument && !settled => {
            (None, TailVerdict::NotChecked(NotChecked::NeverReachedFloor))
        }
        claim => {
            let reported = claim.samples().unwrap_or(Samples::ZERO);
            let verdict = if figure > reported {
                TailVerdict::TailLonger {
                    reported,
                    measured: figure,
                }
            } else {
                TailVerdict::Honest
            };
            (Some(figure), verdict)
        }
    };
    Measured {
        tail,
        latency: latency_figure,
        config,
        verdict: Verdict {
            tail: tail_verdict,
            latency,
        },
    }
}

/// Loads the plugin `descriptor` names from the file at `path` as format `L`,
/// puts it in `state` if one is given (the bytes its own `save_state` wrote; the
/// record's [`ProbeConfig::state`] hashes them), measures it at each of
/// [`CONFIGS`], each within [`RENDER_DEADLINE`] and within `run` (the deadline of
/// the probe's whole run, which the plugins of a file share), and returns the one
/// record they make ([`Measured::merge`]). A plugin that finds `run` passed times
/// out on its own row, so one slow plugin never costs the file's others their
/// rows nor the file a kill by the host.
///
/// A plugin that crashes or hangs here takes the probe process with it, which
/// the host that spawned it sees as it sees a crash in the load.
///
/// # Errors
///
/// A [`MeasureFailure`] says why this plugin has no measurement: it would not
/// load, take `state` ([`MeasureFailure::StateRefused`]: the caller asked for
/// that state, so the probe refuses the file), save its state, activate, render
/// or deactivate, it has no audio output, or a render passed its deadline.
pub fn measure<L: InProcess>(
    main: MainThreadMarker,
    path: &Path,
    descriptor: &PluginDescriptor,
    state: Option<&[u8]>,
    run: Deadline,
) -> Result<Measured, MeasureFailure> {
    // The plugin closes on this thread when `loaded` drops, so the collector it is
    // given never receives work.
    let (_collector, work) = tutti_plugin_types::MainWork::new();
    let mut loaded = L::load_in_process(main, &work, path, &Load::new().plugin(&descriptor.id))
        .map_err(|e| MeasureFailure::Load {
            reason: e.to_string(),
        })?;
    if let Some(bytes) = state {
        loaded
            .load_state(main, bytes, tutti_plugin_types::StateUse::Project)
            .map_err(|e| MeasureFailure::StateRefused {
                reason: e.to_string(),
            })?;
    }
    // The hash names the state the figure is of: a plugin that cannot say what
    // state it is in has no state to record, and empty bytes would name another.
    let saved = loaded
        .save_state(main, tutti_plugin_types::StateUse::Project)
        .map_err(|e| MeasureFailure::SaveState {
            reason: e.to_string(),
        })?;
    let state = StateHash::of(&saved);
    let wiring = loaded.wiring(main);
    if wiring.total_outputs() == 0 {
        return Err(MeasureFailure::NoAudioOutput);
    }
    // An instrument is a plugin with no audio input, whatever its descriptor's
    // features say (the shared plugin's says both): what decides the stimulus is
    // whether an impulse has anywhere to go.
    let instrument = wiring.total_inputs() == 0;
    let mut record: Option<Measured> = None;
    for (rate, block) in CONFIGS {
        let (mut processor, controller) = loaded
            .activate(main, Prepare::new(rate, block))
            .map_err(|refused| MeasureFailure::Activate {
                rate,
                block,
                reason: refused.reason.to_string(),
            })?;
        let subject = Subject {
            inputs: wiring.total_inputs(),
            outputs: wiring.total_outputs(),
            instrument,
            tail: controller.tail(main),
            latency: controller.latency(main),
        };
        let config = ProbeConfig {
            rate,
            block,
            floor: PROBE_FLOOR,
            state,
        };
        let deadline = Deadline::after(Instant::now(), RENDER_DEADLINE).sooner(run);
        let measured = measure_within(&mut processor, subject, config, deadline)?;
        record = Some(match record {
            Some(first) => first.merge(measured),
            None => measured,
        });
        loaded =
            L::deactivate(main, processor, controller).map_err(|e| MeasureFailure::Deactivate {
                rate,
                block,
                reason: e.reason.to_string(),
            })?;
    }
    // `CONFIGS` is not empty, so a record was made.
    record.ok_or(MeasureFailure::NoAudioOutput)
}

#[cfg(test)]
mod tests;
