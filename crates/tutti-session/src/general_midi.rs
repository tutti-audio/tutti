//! A General MIDI setup of a SoundFont unit: the unit, then its chorus and
//! reverb, then the sum of the three.
//!
//! A [`SoundFontUnit`] has no effect inside it. It has five outputs (the dry
//! pair, the chorus send pair and the reverb send), and a synthesizer with the
//! sound of a General MIDI one needs what its sends were for. [`Session::add_general_midi`]
//! wires them: the chorus is a `ModDelayNode` with
//! [`ModDelayConfig::general_midi_chorus`] (wet only), the reverb a
//! [`FreeverbNode`], the graph sums the dry pair and both effects' pairs into
//! one stereo mix (each of the fader's two inputs has three sources), and a
//! [`BusStripNode`] after the sum is the setup's fader, whose output is [`output`](GeneralMidi::output). The sends are
//! post-fader, as a mixer channel's are: the strip's gain and its
//! [`NodeMode`](tutti_graph::NodeMode) act on the whole mix, effects included.
//! Nothing else is needed to hear it; nothing here is required either: a host
//! that wants a different reverb, two units into one chorus or no chorus wires
//! the unit's outputs itself, through [`Graph::connect`](crate::Graph::connect).

use tutti_buffers::{BusTopology, Float};
use tutti_dsp::{Lerp, SimdLanes};
use tutti_nodes::{
    BusStripConfig, BusStripNode, FreeverbConfig, FreeverbNode, ModDelayConfig, ModDelayNode,
};
use tutti_soundfont::SoundFontUnit;
use tutti_units::Amplitude;

use crate::{InstrumentHandle, Key, OutputPort, ParamTarget, RemoveError, Session, SessionNode};

/// A SoundFont unit wired to its chorus and reverb, and what they sum to.
///
/// What [`Session::add_general_midi`] adds. The mix is `out()`'s ports 0 and 1
/// (left and right). It is not wired to the graph's outputs: that is the host's
/// (`Graph::connect` into `MainOutput`), since a second setup must not take the first's
/// place.
///
/// The [`instrument`](Self::instrument) is the unit's handle as any
/// instrument has it: MIDI is sent through it, and its gain is the **dry
/// level** (a strip on the dry pair only, before the sum). The setup's own
/// fader, [`gain`](Self::gain), is after the sum and so takes the
/// effects with it; so does muting it, which is the graph's, not a method
/// here: `session.set(gm.output().mode().to(NodeMode::Muted))`. A send's level is the font's
/// and the channel's (CC 91 and CC 93), not a gain's.
#[derive(Debug)]
pub struct GeneralMidi<F: Float + SimdLanes + Lerp> {
    instrument: InstrumentHandle<SoundFontUnit<F>>,
    chorus: Key<ModDelayNode<F>>,
    reverb: Key<FreeverbNode<F>>,
    strip: Key<BusStripNode>,
}

impl<F: Float + SimdLanes + Lerp> GeneralMidi<F> {
    /// The unit, played through its handle.
    #[must_use]
    pub fn instrument(&self) -> &InstrumentHandle<SoundFontUnit<F>> {
        &self.instrument
    }

    /// The unit's handle, to send with ([`InstrumentHandle::keys_mut`]).
    pub fn instrument_mut(&mut self) -> &mut InstrumentHandle<SoundFontUnit<F>> {
        &mut self.instrument
    }

    /// The chorus: the unit's chorus send in, the wet chorus out.
    #[must_use]
    pub fn chorus(&self) -> Key<ModDelayNode<F>> {
        self.chorus
    }

    /// The reverb: the unit's reverb send in, the wet reverb out.
    #[must_use]
    pub fn reverb(&self) -> Key<FreeverbNode<F>> {
        self.reverb
    }

    /// The mix after the fader: dry pair, chorus and reverb summed, on ports 0
    /// (left) and 1 (right).
    #[must_use]
    pub fn output(&self) -> Key<BusStripNode> {
        self.strip
    }

    /// The gain of the whole mix, a linear amplitude: the dry pair and both
    /// effects, the fader's volume. `session.set(gm.gain().to(Amplitude::new(0.5)))`
    /// writes it.
    #[must_use]
    pub fn gain(&self) -> ParamTarget<Amplitude> {
        self.strip.volume()
    }

    /// Takes the whole setup out of the graph: the unit with its queue and
    /// strip, the chorus, the reverb and the fader. Every part that is there
    /// goes.
    ///
    /// # Errors
    ///
    /// [`RemoveError::NoSuchNode`] naming the first part that was not in the
    /// graph (a part was removed by hand).
    pub fn remove<K: crate::AppKey, C: crate::Clock<Float = F>>(
        self,
        session: &mut Session<K, C>,
    ) -> Result<(), RemoveError> {
        let node = self.instrument.node().erase();
        let mut first = session.remove_instrument(self.instrument);
        for node in [
            node,
            self.chorus.erase(),
            self.reverb.erase(),
            self.strip.erase(),
        ] {
            let removed = session.remove(node).map(|_| ());
            first = first.and(removed);
        }
        first
    }
}

impl<K: crate::AppKey, C: crate::Clock> Session<K, C>
where
    C::Float: SimdLanes + Lerp,
    ModDelayNode<C::Float>: SessionNode<C::Float>,
    FreeverbNode<C::Float>: SessionNode<C::Float>,
{
    /// Adds `unit` with a General MIDI chorus and reverb, and a fader that
    /// sums the three.
    ///
    /// The wiring is the unit's dry pair (through the handle's strip), its
    /// chorus send pair through a wet-only
    /// [`ModDelayConfig::general_midi_chorus`], and its reverb send through a
    /// [`FreeverbNode`], each effect's stereo pair a source of the fader's
    /// left and right inputs beside the dry pair's: the graph sums them. Each effect declares its
    /// own tail, so an export that ends at the last note-off keeps the release
    /// and then the room's ring-out.
    ///
    /// # Errors
    ///
    /// [`Error::NotInstrument`](crate::Error::NotInstrument) if the unit cannot
    /// be played as an instrument; none for a `SoundFontUnit`, which has an
    /// event input and five outputs.
    pub fn add_general_midi(
        &mut self,
        unit: SoundFontUnit<C::Float>,
    ) -> Result<GeneralMidi<C::Float>, crate::Error> {
        let instrument = self.add_instrument_unwired(unit)?;
        let unit = instrument.node().erase();
        let graph = self.graph();
        let chorus = graph.add(ModDelayNode::<C::Float>::from(
            ModDelayConfig::general_midi_chorus(BusTopology::STEREO),
        ));
        let reverb = graph.add(FreeverbNode::<C::Float>::from(
            FreeverbConfig::general_midi(),
        ));
        let strip = graph.add(BusStripNode::from(BusStripConfig::new(BusTopology::STEREO)));
        // The unit's outputs: dry L/R, chorus send L/R, reverb send.
        graph.connect(OutputPort::new(unit, 2), chorus.input(0))?;
        graph.connect(OutputPort::new(unit, 3), chorus.input(1))?;
        graph.connect(OutputPort::new(unit, 4), reverb.input(0))?;
        // The fader's inputs sum the dry pair, the chorus and the reverb,
        // each stereo, left on port 0 and right on port 1.
        let dry = instrument.output().erase();
        for (channel, port) in [(0, 0), (1, 1)] {
            for node in [dry, chorus.erase(), reverb.erase()] {
                graph.connect(OutputPort::new(node, channel), strip.input(port))?;
            }
        }
        Ok(GeneralMidi {
            instrument,
            chorus,
            reverb,
            strip,
        })
    }
}
