//! The unit as a graph node: MIDI arrives on an
//! event input, on its frame (to the voices' 8-frame chunk, see
//! [`SoundFontUnit`]'s "How an event's offset is honoured").
//!
//! It follows its graph's rate: `prepare` rebuilds the synthesizer at the
//! prepared rate (control thread, where that may allocate), keeping the
//! preset.

use tutti_buffers::{ChannelLayout, Float};
use tutti_dsp::SimdLanes;
use tutti_graph::{
    Chase, ConstantMask, Cx, IntoNode, Io, Node, NodeParts, Prepare, Resolution, Shape,
    SilenceMask, Status,
};

use crate::SoundFontUnit;
use crate::player::{CHASE_MESSAGES, Plane, RATES};

impl<F: Float + SimdLanes> Node<F> for SoundFontUnit<F> {
    /// No audio in, five out (dry left and right, chorus send left and right,
    /// reverb send), one MIDI event input, placed to the 8-frame chunk the
    /// voices render in, and the tail of the longest release.
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::from_count(Plane::COUNT as u16))
            .with_events(1, 0)
            .with_event_resolution(Resolution::Frames(crate::SYNTH_BLOCK_FRAMES as u32))
            // Sample playback and filters per voice: measured 76 ns
            // per frame and channel with one note held, 140 with eight,
            // against `SvfFilterNode`'s 7.2.
            // The default, stated.
            .with_cost(tutti_graph::Cost::Heavy)
            // The longest a voice rings after its key is up: an export that
            // ends at the last note-off extends by this, and keeps the
            // release. The effects after the unit declare their own.
            .with_tail(self.shape_tail())
            // The rates the voices are built for: a graph refuses another
            // (`Graph::set_sample_rate`, a fork) instead of playing at the
            // wrong pitch.
            .with_rates(RATES)
            // The state it hands the unit that replaces it (`chase`).
            .with_chase(CHASE_MESSAGES as u32)
    }

    /// Re-rate to the prepared rate (keeping the preset), and size the
    /// render scratch to the largest block. Control thread.
    ///
    /// A rate within [`shape`](Node::shape)'s rates (16 to 192 kHz) is the unit's rate, rounded to
    /// whole hertz. A graph never hands another through a re-rate or a fork: both refuse it. A
    /// node added at a rate outside them still reaches `prepare` until the graph refuses the
    /// insertion too; the unit then keeps the rate it has (44.1 kHz until prepared) and so plays
    /// at the wrong pitch.
    fn prepare(&mut self, p: &Prepare) {
        let rate = p.sample_rate();
        if rate.get().round() != self.sample_rate.get().round()
            && let Some(unit) = self.rerated(rate)
        {
            *self = unit;
        }
        let frames = p.max_block().get();
        for plane in &mut self.planes {
            if plane.len() < frames {
                plane.resize(frames, F::ZERO);
            }
        }
    }

    /// What the block did: [`Status::Idle`] when no voice is left to sound and
    /// nothing was written; else [`Status::Masked`] naming the planes no voice
    /// sent anything to (exact zeros, written), and [`Status::Modified`] when
    /// every plane has something in it.
    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        // No clamp: `prepare` sized the scratch to the prepared `MaxBlock`,
        // and the graph never hands a node a longer block.
        let size = io.frames();
        self.render_events(io.sub_blocks(0));
        let heard = self.take_heard();
        if self.is_idle() && !heard.has_any() {
            return Status::Idle;
        }
        let (_, mut outputs) = io.split();
        let mut silent = SilenceMask::NONE;
        for (c, (out, plane)) in outputs.iter_mut().zip(&self.planes).enumerate() {
            out[..size].copy_from_slice(&plane[..size]);
            if !heard.has(Plane::ALL[c]) {
                silent = silent.with(c);
            }
        }
        if silent == SilenceMask::NONE {
            Status::Modified
        } else {
            Status::Masked {
                silent,
                constant: ConstantMask::NONE,
            }
        }
    }

    fn reset(&mut self) {
        self.reset_player();
    }

    /// Hands the unit that replaces this one every channel's program, bank,
    /// effect sends, controllers, pedals, pitch wheel and registered
    /// controllers, as MIDI 2.0 messages in the unit's Group: what a performer
    /// set survives a swap of font. A channel left as a fresh unit has it
    /// sends nothing, so a successor built on another program keeps it. The
    /// sounding notes are not carried (nothing re-strikes a held key), nor the
    /// per-note controllers or the key a portamento glides from.
    fn chase(&self, out: &mut Chase<'_>) {
        self.player.chase(|ump| {
            out.push(ump);
        });
    }
}

/// The unit, inserted as a graph node: its fork is a graph node too, which
/// follows its render's rate (see the `fork` module docs). No controls: the
/// unit has no param a host sets while it plays (its preset is MIDI's).
impl<F: Float + SimdLanes> IntoNode<F> for SoundFontUnit<F> {
    type Controls = ();

    fn into_parts(self) -> NodeParts<(), F> {
        let fork = self.fork_template();
        NodeParts {
            node: Box::new(self),
            controls: (),
            fork: Some(Box::new(fork)),
        }
    }
}

/// A SoundFont unit is an instrument: MIDI on event input 0, its dry pair on
/// outputs 0 and 1. The other three outputs are the effect sends.
impl<F: Float + SimdLanes> tutti_graph::Instrument<F> for SoundFontUnit<F> {
    fn main_outputs(&self) -> [u16; 2] {
        [Plane::DryLeft as u16, Plane::DryRight as u16]
    }
}
