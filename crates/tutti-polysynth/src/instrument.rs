//! [`NoteInstrument`]: MIDI notes played on any voice that renders a note, in a graph.
//!
//! The node holds a [`VoiceTable`] of [`InstrumentVoice`]s and the allocator that commands
//! them, so polyphony, stealing, pedals, channel controllers, bends and tunings follow the
//! [`VoiceConfig`] it is built with. The voices here sound the `tutti-dsp` resonators: a
//! struck [`ModalVoice`], a plucked [`KarplusVoice`], a [`WaveguideVoice`] string and an
//! [`AdditiveVoice`] of sine partials. Each is a mono voice, heard on both outputs.
//!
//! The node has stereo outputs and one event input, and the master volume as its param.
//! Every voice is built when the node is, so a note allocates nothing on the audio thread.

mod additive;
mod karplus;
mod modal;
mod note;
mod waveguide;

pub use additive::{AdditiveVoice, AdditiveVoiceConfig};
pub use karplus::{KarplusVoice, KarplusVoiceConfig};
pub use modal::{ModalVoice, ModalVoiceConfig};
pub use note::Strike;
pub use waveguide::{WaveguideVoice, WaveguideVoiceConfig};

use core::fmt::Debug;
use core::marker::PhantomData;

use tutti_buffers::{ChannelTopology, Float};
use tutti_graph::{Chase, Cx, EventKind, InEvents, Io, Node, Prepare, Shape, Status};
use tutti_midi_protocol::{SysexDeviceId, UniversalReader, channel_voice};
use tutti_ump::{MidiChannel, MidiGroup, NoteNumber, NoteVelocity, Ump};
use tutti_units::{Amplitude, Real, SampleRate, Samples, Semitones, Tail, UnitParam};
use tutti_voicing::{NoteVoice, Patch, VoiceAllocator, VoiceConfig, VoiceSpec, VoiceTable};

/// A note voice a [`NoteInstrument`] renders: made from its config at a rate, summed into a
/// block a span at a time.
pub trait InstrumentVoice: NoteVoice<Context = ()> + Clone + Debug + Send + 'static {
    /// The float the voice renders in.
    type Float: Float;

    /// What the voice is made from.
    type Config: Clone + Debug + Send + 'static;

    /// An idle voice for `rate`. Allocates: it is called when the instrument is built and when
    /// the graph's rate is not the one it was built for.
    fn new(config: &Self::Config, rate: SampleRate) -> Self;

    /// The channel the voice's note is on.
    fn channel(&self) -> MidiChannel;

    /// Adds the note's next `out.len()` frames to `out`, with `bend`, its channel's pitch, on
    /// it. No allocation.
    fn render(&mut self, bend: Semitones, out: &mut [Self::Float]);

    /// Whether the note has ended and the voice may be reused.
    fn is_finished(&self) -> bool;
}

/// Every note is one voice.
struct OneVoice;

impl Patch for OneVoice {
    fn voices(
        &self,
        _: MidiChannel,
        _: NoteNumber,
        _: NoteVelocity,
        spec: &mut dyn FnMut(VoiceSpec),
    ) {
        spec(VoiceSpec {
            group: None,
            scope: 0,
            tag: 0,
        });
    }
}

tutti_graph::declare_params!([V: InstrumentVoice] NoteInstrument<V>,
    InstrumentConfig<C: Clone>, InstrumentCells<C: Clone> {
    required { voice: C, rules: VoiceConfig }
    params { Volume => gain: Amplitude { whole } = Amplitude::UNITY }
    options { group: MidiGroup = MidiGroup::FIRST, device: SysexDeviceId = SysexDeviceId::DEFAULT }
});

/// A polyphonic instrument of `V`'s voices, played by MIDI.
///
/// Built from an [`InstrumentConfig`] (the voice's config and the [`VoiceConfig`] that sets
/// its polyphony and steal rule), it is a graph node with no audio input, a stereo output and
/// one MIDI event input, each event applied on its frame. MIDI 1.0 and 2.0 channel voice
/// messages and the Universal System Exclusive of its Group are read; the master volume is its
/// `Volume` param. A stolen voice is cut at once. It is an instrument for
/// `Session::add_instrument`.
///
/// ```
/// use std::num::NonZeroUsize;
/// use tutti_graph::{Prepare, Renderer};
/// use tutti_buffers::MaxBlock;
/// use tutti_polysynth::{InstrumentConfig, KarplusVoice, KarplusVoiceConfig, NoteInstrument};
/// use tutti_units::{SampleRate, Samples};
/// use tutti_voicing::VoiceConfig;
///
/// let instrument = NoteInstrument::<KarplusVoice<f32>>::new(InstrumentConfig::new(
///     KarplusVoiceConfig::new(),
///     VoiceConfig::poly(NonZeroUsize::new(8).unwrap()),
/// ));
/// let prepare = Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(64)));
/// let (mut renderer, ..) = Renderer::alone(instrument, prepare);
/// assert_eq!(renderer.render(64).len(), 2);
/// ```
#[derive(Clone, Debug)]
pub struct NoteInstrument<V: InstrumentVoice> {
    cells: InstrumentCells<V::Config>,
    voice: V::Config,
    rate: SampleRate,
    voices: VoiceTable<V>,
    universal: UniversalReader,
    group: MidiGroup,
    /// The last finite volume the node read.
    last_gain: Amplitude,
    /// The gains the last span ended at, left and right: each span ramps from them.
    applied: Option<[Amplitude; 2]>,
    float: PhantomData<fn() -> V::Float>,
}

impl<V: InstrumentVoice> NoteInstrument<V> {
    /// The rate the voices are built for until the graph prepares the node: 44.1 kHz.
    const INITIAL_RATE: SampleRate = SampleRate::SR_44K1;

    const PORTS: Shape = Shape::source(ChannelTopology::STEREO).events(1, 0);
    const EVENTS: InEvents = Self::PORTS
        .event_input(0)
        .expect("declared one event input");

    /// An instrument of `config.rules.polyphony` idle voices. Allocates.
    #[must_use]
    pub fn new(config: InstrumentConfig<V::Config>) -> Self {
        let rate = Self::INITIAL_RATE;
        let voices = VoiceTable::new(config.rules, || V::new(&config.voice, rate));
        Self {
            cells: InstrumentCells::from_config(&config),
            rate,
            voices,
            universal: UniversalReader::new(config.group, config.device),
            group: config.group,
            last_gain: config.gain,
            applied: None,
            voice: config.voice,
            float: PhantomData,
        }
    }

    /// How many voices are sounding, those in their release among them: a meter an app reads
    /// for a voice-count display.
    #[must_use]
    pub fn sounding(&self) -> usize {
        self.voices.iter().count()
    }

    /// Reads one packet: the Group's Universal System Exclusive, then its channel voice message.
    fn apply(&mut self, ump: Ump) {
        if let Some(heard) = self.universal.push(ump) {
            self.voices.universal(heard, &());
        }
        if let Some((address, message)) = channel_voice(ump)
            && address.group == self.group
        {
            self.voices.step(address.channel, message, &OneVoice, &());
        }
    }

    /// Renders one span of the mix into `left` and `right`: the voices summed, then the volume
    /// and the master gain ramped across it.
    fn render_span(&mut self, left: &mut [V::Float], right: &mut [V::Float], volume: Amplitude) {
        left.fill(V::Float::ZERO);
        self.voices.retain(|voice, allocator| {
            voice.render(allocator.channel_pitch(voice.channel()), left);
            !voice.is_finished()
        });
        right.copy_from_slice(left);
        let master = self.voices.allocator().master_gain();
        let to = [
            Amplitude::new(volume.get() * master.left.get()),
            Amplitude::new(volume.get() * master.right.get()),
        ];
        let from = self.applied.unwrap_or(to);
        ramp(left, from[0], to[0]);
        ramp(right, from[1], to[1]);
        self.applied = Some(to);
    }
}

/// Scales `out` by a gain that moves in a line from `from` to `to` across it.
fn ramp<F: Float>(out: &mut [F], from: Amplitude, to: Amplitude) {
    let (from, to) = (F::from_unit(from), F::from_unit(to));
    let n = Samples::new(out.len());
    for (k, y) in out.iter_mut().enumerate() {
        *y *= tutti_graph::ramp_in(None, from, to, Samples::new(k), n);
    }
}

impl<V: InstrumentVoice> Node<V::Float> for NoteInstrument<V> {
    /// No audio in, stereo out, one MIDI event input. Held notes ring as long as their keys
    /// are down, so the tail is unbounded.
    fn shape(&self) -> Shape {
        Self::PORTS
            .tail(Tail::Unbounded)
            .param_ports(self.declared_ports())
            .chase(VoiceAllocator::CHASE_MESSAGES as u32)
    }

    fn prepare(&mut self, p: &Prepare) {
        let rate = p.sample_rate();
        if rate != self.rate {
            self.rate = rate;
            let config = &self.voice;
            self.voices = self.voices.fork(|| V::new(config, rate));
        }
    }

    /// Always [`Status::Modified`]: a note sounds after its input's last event. The volume lane
    /// is read at the span's last frame and ramped across each span.
    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_, V::Float>) -> Status {
        if io.frames() == 0 {
            return Status::Modified;
        }
        let volume = match self
            .declared_param::<{ UnitParam::Volume as u16 }, _>(&io)
            .last_frame()
        {
            Some(lane) => Amplitude::new(lane),
            None => {
                let cell = self.cells.gain.load();
                if cell.get().is_finite() {
                    self.last_gain = cell;
                }
                self.last_gain
            }
        }
        .max(Amplitude::SILENT);
        let blocks = io.sub_blocks(Self::EVENTS);
        let (_, mut outputs) = io.split();
        let mut channels = outputs.iter_mut();
        let (Some(left), Some(right)) = (channels.next(), channels.next()) else {
            return Status::Modified;
        };
        for (range, events) in blocks {
            for event in events {
                if let EventKind::Midi(ump) = event.kind {
                    self.apply(ump);
                }
            }
            self.render_span(&mut left[range.clone()], &mut right[range], volume);
        }
        Status::Modified
    }

    /// Silences every voice and forgets the notes and pedals; the channels' controllers stay.
    fn reset(&mut self) {
        self.voices.clear();
        self.voices.forget_notes();
        self.applied = None;
    }

    /// Hands the instrument that replaces this one every channel's controllers, pedals, wheel
    /// and registered controllers, as MIDI 2.0 messages in its Group.
    fn chase(&self, out: &mut Chase<'_>) {
        self.voices.allocator().chase(self.group, |ump| {
            out.push(ump);
        });
    }

    fn param_base(&self, k: usize) -> Option<f64> {
        self.declared_base(k)
    }
}

/// A [`NoteInstrument`] is an instrument in the float its voices render in: MIDI on event
/// input 0, its stereo pair on outputs 0 and 1.
impl<V: InstrumentVoice> tutti_graph::InstrumentNode<V::Float> for NoteInstrument<V> {}
