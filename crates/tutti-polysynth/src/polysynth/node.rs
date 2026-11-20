//! The synth as a graph node: MIDI arrives
//! on an event input port, on its frame, from whatever feeds it — a
//! `MidiClipNode` (`tutti-midi-runtime`), an arpeggiator, a
//! hardware source — in the same block it was written.
//!
//! A keyboard reaches it the same way: through an event inlet
//! (`tutti_graph::EventInletNode`) wired to its input. Driven by hand (a test, a bench), its events are handed to it
//! the same way, through `tutti_graph::contract::{drive_input, Direct}`.
//!
//! Its controls are its [`ParamSet`]: the master volume and, with a unison
//! engine, the unison detune and stereo spread, by `UnitParam`. It is a
//! [`ParamNode`], inserted through [`tutti_graph::param_parts`], so a fork
//! starts from the values last set through the set (see `src/fork.rs`).

use tutti_buffers::BusTopology;
use tutti_graph::{
    Chase, Cx, IntoNode, Io, Node, NodeParts, ParamNode, ParamSet, Prepare, Shape, Status,
};
use tutti_units::UnitParam;
use tutti_units::{Amplitude, CentsSpan, Spread};
use tutti_voicing::VoiceAllocator;

use super::PolySynth;
use crate::lane::SynthFloat;

// The params a `PolySynth` lets the graph modulate, in port order: master
// volume and, with a unison engine, detune and stereo spread. Each is
// block-rate in the synth (read once per block), so a driven port is read the
// same way: its value on the block's last frame.
tutti_graph::declare_params!([F: SynthFloat] PolySynth<F>, n {
    Volume => Amplitude = &n.master_volume,
    Detune => CentsSpan = n.unison.as_ref().map(|u| u.detune()),
    StereoSpread => Spread = n.unison.as_ref().map(|u| u.spread()),
});

/// The most oscillator lanes (`polyphony` × unison sub-voices) a synth
/// may render and still declare [`Cost::Moderate`](tutti_graph::Cost).
///
/// The voice bank renders each SIMD group of lanes that holds a sounding
/// voice, so at full polyphony the synth costs what its lane count does.
/// Measured with a saw, every voice held:
/// 18–21 ns per frame and channel at 8 lanes, 74 at 32 (65 at 8 voices ×
/// 4 unison), about 2.2 more per lane, against `SvfFilterNode`'s 7.2
/// (`Moderate`'s reference) and `Heavy`'s 256. 16 lanes come to about
/// 38 ns, under the two classes' geometric middle (45); 20 would be over.
pub(super) const MODERATE_LANES: usize = 16;

impl<F: SynthFloat> PolySynth<F> {
    /// What the synth declares to the cost model: `Moderate` up to
    /// [`MODERATE_LANES`] oscillator lanes, `Heavy` past them.
    ///
    /// The editor reads it at every insert and re-prepare. The lane count
    /// is fixed when the synth is built, so every read agrees.
    pub(crate) fn cost(&self) -> tutti_graph::Cost {
        if self.config.polyphony.get() * usize::from(self.unison_voice_count().get())
            <= MODERATE_LANES
        {
            tutti_graph::Cost::Moderate
        } else {
            tutti_graph::Cost::Heavy
        }
    }
}

impl<F: SynthFloat> Node<F> for PolySynth<F> {
    /// No audio in, stereo out, one MIDI event input; the release as its
    /// tail. Events land on their frame ([`Resolution::Sample`](tutti_graph::Resolution::Sample)).
    fn shape(&self) -> Shape {
        // The unison engine is built with the synth or never (`&mut`
        // setters only), so every read of the shape declares the same ports.
        Shape::source(BusTopology::STEREO)
            .with_events(1, 0)
            .with_tail(self.release_tail())
            .with_cost(self.cost())
            .with_param_addrs(self.declared_ports().as_slice())
            // The channel state it hands the synth that replaces it (`chase`).
            .with_chase(VoiceAllocator::CHASE_MESSAGES as u32)
    }

    fn prepare(&mut self, p: &Prepare) {
        self.set_rate(p.sample_rate());
    }

    /// Always [`Status::Modified`]: the synth sounds after its input's last
    /// event (its release), so the executor must never park it.
    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        let frames = io.frames();
        if frames == 0 {
            return Status::Modified;
        }
        let driven = [
            self.declared_param::<{ UnitParam::Detune as u16 }, _>(&io)
                .last_frame(),
            self.declared_param::<{ UnitParam::StereoSpread as u16 }, _>(&io)
                .last_frame(),
        ];
        if let Some(unison) = &mut self.unison {
            unison.sync_from_atomics(driven);
        }
        let blocks = io.sub_blocks(0);
        let volume = self
            .declared_param::<{ UnitParam::Volume as u16 }, _>(&io)
            .last_frame()
            .map_or_else(
                || {
                    let cell = self.master_volume.load();
                    if cell.get().is_finite() {
                        self.last_volume = cell;
                    }
                    self.last_volume
                },
                |lane| Amplitude::new(lane.into()),
            );
        let volume = Self::clamp_volume(volume);
        let (_, mut outputs) = io.split();
        let mut channels = outputs.iter_mut();
        let (Some(left), right) = (channels.next(), channels.next()) else {
            return Status::Modified;
        };
        match right {
            Some(right) => self.render_events(blocks, volume, &mut |i, l, r| {
                left[i] = l;
                right[i] = r;
            }),
            None => self.render_events(blocks, volume, &mut |i, l, _| left[i] = l),
        }
        Status::Modified
    }

    fn reset(&mut self) {
        self.reset_voices();
    }

    /// Hands the instrument that replaces this one every channel's
    /// controllers, pedals, pitch wheel, pressure and registered controllers,
    /// as MIDI 2.0 messages in the synth's Group (`VoiceAllocator::chase`): what a
    /// performer set survives a swap, whatever the successor is. The sounding
    /// notes are not carried (nothing re-strikes a held key), nor the
    /// per-note controllers or the key a portamento glides from. A synth has
    /// no program or bank.
    fn chase(&self, out: &mut Chase<'_>) {
        self.allocator.chase(self.config.group, |ump| {
            out.push(ump);
        });
    }

    fn param_base(&self, k: usize) -> Option<f64> {
        self.declared_base(k)
    }
}

impl<F: SynthFloat> ParamNode<F> for PolySynth<F> {
    /// `Volume` (the master gain, as a linear amplitude), and — only when
    /// this synth has a unison engine — `Detune` (cents) and `StereoSpread`
    /// (0..1): the cells the synth reads once per block, which a host
    /// writes (and a control lane takes as its knob).
    fn param_set(&self) -> ParamSet {
        self.declared_set()
    }

    /// See the `fork` module docs (`src/fork.rs`).
    fn fork_fresh(&self) -> Self {
        self.fork_instance()
    }
}

/// The synth, inserted with its [`ParamSet`] as its controls and a
/// fork that starts from the values last set through it
/// ([`tutti_graph::param_parts`]; see the `fork` module docs for what a fork
/// carries).
impl<F: SynthFloat> IntoNode<F> for PolySynth<F> {
    type Controls = ParamSet;

    fn into_parts(self) -> NodeParts<ParamSet, F> {
        tutti_graph::param_parts(self)
    }
}

/// The synth is an instrument in any width it renders in: MIDI on event input 0, its
/// stereo pair on outputs 0 and 1.
impl<F: SynthFloat> tutti_graph::Instrument<F> for PolySynth<F> {}

#[cfg(test)]
mod tests;
