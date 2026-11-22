//! `PlaybackSlot` — a [`Voice`] plus its resident time-stretch processor, and
//! the read that turns the pair into audio: a block at a time into planar
//! lanes (`render_into`), on the block's transport.
//!
//! This is where the two source tiers and the two stretch states meet: four
//! combinations, each of which has to agree with the others about how much
//! source one output sample costs. They are easy to get subtly wrong, so the
//! reasoning is kept inline at each fork.

use std::ops::Range;

use crate::lanes::{BlockScratch, Lanes, accumulate, audible_gain};
use crate::nonempty;

use super::clock::BlockClock;
use super::interp::Gate;
use super::memory_source::VoiceWindow;
use super::stretched::{
    self, Cursor, DiskFeed, Feed, Input, MemoryFeed, PRIME_SPEED, Stretch, Vocoders,
};
use super::types::{Playback, SlotId, Voice, VoiceSource};
use crate::SamplerFloat;
use tutti_buffers::ChannelLayout;
use tutti_dsp::PhaseVocoder;
use tutti_units::{Beat, Bpm, Cents, ReadRate, SamplePosition, StretchFactor};

/// A [`Voice`] plus the resident time-stretch DSP processor, and the read that
/// turns the pair into audio.
///
/// The processor is the DSP object rather than a config record, so it stays on
/// the slot alongside the engine sample rate it is tuned to; the control
/// *intent* — factor, pitch, gain, direction, loop — lives in `voice.play`, and
/// the two are kept in step by [`set_stretch`](Self::set_stretch) writing both.
///
/// Shared verbatim by [`VoicePool`](super::pool::VoicePool)'s slot vector and by
/// [`VoiceNode`](super::node::VoiceNode), which holds exactly one — so the per-voice read has one
/// definition and a fix in it is a fix in both.
///
/// # Not a `VoiceSlot`
///
/// `tutti-polysynth` has a `VoiceSlot`, and it is a different kind of thing: an
/// *allocator* slot carrying note identity plus an Idle/Active/Releasing/Stolen
/// lifecycle, which is what its stealing policy scores over. This type carries
/// no identity and no such state — it is a playback descriptor, and everything
/// about which slot is live is decided by the owner. Sharing the name across the
/// two crates invited reading one crate's stealing rules into the other's, so
/// the name says which job this is.
pub(crate) struct PlaybackSlot<F: SamplerFloat> {
    /// Addresses this slot for every command after the add. `SlotId(0)` on a
    /// `VoiceNode`, whose single voice has nothing to disambiguate.
    pub(crate) id: SlotId,
    /// The source (in-memory or streaming) plus the control intent recorded for
    /// it. Which tier is live is the [`VoiceSource`] arm; the read below forks
    /// on that enum at each call site rather than through a trait, so a
    /// tier-conditional difference stays visible.
    ///
    /// Boxed as `VoiceCommand::AddVoice` carries it, so the pool's drain moves
    /// the box in rather than moving the `Voice` out of it — which would free
    /// the box on the audio thread. A slot leaves the pool through the
    /// retirement channel, box and all.
    pub(crate) voice: Box<Voice<F>>,
    /// The phase vocoder, present once the voice has been asked to stretch or
    /// transpose: `None` reads the source dry, the common case. From then on it
    /// always runs, at unity too, so the voice's sound does not change with its stretch. It owns
    /// no copy of the audio source: the read feeds it the source frames it asks for (see
    /// `stretched`).
    ///
    /// Built on the control thread, never in the callback. A vocoder is built
    /// by `VoicePoolHandle::send` when a `VoiceCommand::AddVoice` or a
    /// `VoiceCommand::UpdateStretch` asks for stretching (or by a standalone
    /// `VoiceNode`'s constructor), and the pool's drain in `process` only
    /// moves it in; a redundant one goes back through the retirement channel
    /// rather than being dropped on the audio thread. Boxed so that moving it
    /// is a pointer move that frees nothing.
    ///
    /// The vocoder's controls are read from `voice.play.stretch` and
    /// `voice.play.pitch` at each block, so a later change goes through
    /// [`PlaybackSlot::set_stretch`], which writes those.
    pub(crate) stretch: Option<Box<Vocoders<F>>>,
    /// Where the vocoder's next input frame is read in the source, once the
    /// voice is seated (see `stretched`).
    pub(crate) cursor: Option<Cursor>,
    /// Where the spare vocoder's next input frame is read, while it primes for a loop
    /// wrap ahead (see `stretched`).
    pub(crate) spare_cursor: Option<Cursor>,
    /// The seats the voice has had: the generation of the current one.
    pub(crate) seats: u64,
    /// The spare's, which nothing reads (a memory voice's read does not use a seat's
    /// generation).
    pub(crate) spare_seats: u64,
    /// Width the vocoder must be built at, remembered so a later
    /// materialisation matches the reader rather than defaulting.
    pub(crate) channels: ChannelLayout,
}

impl<F: SamplerFloat> PlaybackSlot<F> {
    /// Build a slot at an explicit width.
    ///
    /// # Width is explicit at every call site
    ///
    /// There is no stereo-defaulting `new`, because both callers (the reader's
    /// drain and `VoiceNode`) know their own width and a default here would
    /// silently mismatch it.
    ///
    /// # The vocoder is not built here
    ///
    /// It is `None` until the slot
    /// actually needs it, for two reasons:
    ///
    /// - Most voices never stretch. A vocoder is one FFT setup plus its
    ///   input, overlap-add and output rings *per channel* (about 60 KB × N),
    ///   so building one for every voice spent that on the common case for
    ///   nothing.
    /// - This constructor is reachable from the audio thread.
    ///   `VoiceCommand::AddVoice` is drained by `drain_commands`, which runs from
    ///   the node's `process`, so eager construction meant adding a voice mid-playback
    ///   allocated in the callback.
    ///
    /// # Who builds one, when the voice needs it
    ///
    /// A slot that arrives already needing stretch (non-unity `play.stretch` /
    /// `play.pitch`) gets its vocoder built on the control thread, before the
    /// audio thread ever sees the slot. The two owners do that differently, and
    /// the difference is not cosmetic:
    ///
    /// - `VoicePool` takes a pre-built vocoder from the sender — see
    ///   `VoicePoolHandle::prepare`, which fills in `VoiceCommand::AddVoice`'s
    ///   `stretch` field. It has to: `AddVoice` is drained in the callback, so the
    ///   pool cannot build one at the point it learns it needs one.
    /// - `VoiceNode` builds its own, in `VoiceNode::with_channels`, because
    ///   that constructor *is* control-thread code. Nothing prepares a vocoder for
    ///   a node, and nothing needs to.
    ///
    /// # Turning stretch on later is a pool-only capability
    ///
    /// `VoiceCommand::UpdateStretch` reaches `set_stretch` below through the
    /// pool's drain; a `VoiceNodeHandle` publishes a placement and nothing
    /// else (it has no command queue), so there is no way to send a node this
    /// command. A node's pitch is therefore fixed at construction, and a
    /// host that wants to change it respawns the voice, which is what the
    /// hosts we know of do.
    ///
    /// If that gap is ever closed, the node would adopt a vocoder built by its
    /// handle the way the pool's drain does: moved in, with the surplus retired
    /// for the control thread to free.
    pub(crate) fn with_channels(
        id: SlotId,
        voice: Box<Voice<F>>,
        channels: impl Into<ChannelLayout>,
    ) -> Self {
        let channels = nonempty(channels.into());
        Self {
            id,
            voice,
            stretch: None,
            cursor: None,
            spare_cursor: None,
            seats: 0,
            spare_seats: 0,
            channels,
        }
    }

    /// Drop every sample of audio this slot is holding mid-flight.
    ///
    /// Called from two places, which is why it is one function: `Node::reset`
    /// (a graph-level reset) and a transport jump (a seek or a loop wrap,
    /// on its frame: [`render_into`](Self::render_into)). They must clear exactly the same state — a seek that
    /// flushed less than a reset would leave a subset of the stale audio behind,
    /// which sounds like an intermittent artifact rather than a bug.
    ///
    /// The vocoder is the state that matters. A placed source re-derives
    /// its position from the playhead every frame, so it is correct the instant
    /// the playhead moves; the vocoder's rings and per-bin phase accumulators
    /// are not. The voice is unseated, and the next sounding frame seats it where
    /// the playhead now is, which resets the vocoder.
    ///
    /// RT-safe: it clears cursors.
    pub(crate) fn flush_playhead_state(&mut self) {
        self.voice.source.flush();
        self.cursor = None;
        self.spare_cursor = None;
    }

    /// A jump on its frame: a loop wrap (`wrapped`) goes on with the spare vocoder if it
    /// has been priming for it (`stretched`), and anything else flushes.
    fn jumped(&mut self, wrapped: bool) {
        if wrapped && self.swap_in_spare() {
            self.voice.source.flush();
        } else {
            self.flush_playhead_state();
        }
    }

    /// Makes the spare vocoder, with the cursor it has primed to, the voice's, and
    /// resets the one it replaces. `false` when the spare is not priming.
    fn swap_in_spare(&mut self) -> bool {
        let (Some(vocoders), Some(next)) = (&mut self.stretch, self.spare_cursor.take()) else {
            return false;
        };
        let Some(spare) = &mut vocoders.spare else {
            return false;
        };
        std::mem::swap(&mut vocoders.live, &mut **spare);
        self.cursor = Some(next);
        true
    }

    /// Adopts `incoming` as the spare vocoder (`Some`), or takes the spare out (`None`),
    /// and returns what is left over for the caller to retire: the arrival when the slot
    /// has no vocoder or already has a spare, else the spare taken out. Taking it out
    /// unseats it. Allocation-free and free-free.
    pub(crate) fn set_spare(
        &mut self,
        incoming: Option<Box<PhaseVocoder<F>>>,
    ) -> Option<Box<PhaseVocoder<F>>> {
        let Some(vocoders) = &mut self.stretch else {
            return incoming;
        };
        match incoming {
            Some(spare) if vocoders.spare.is_none() => {
                vocoders.spare = Some(spare);
                None
            }
            Some(surplus) => Some(surplus),
            None => {
                self.spare_cursor = None;
                vocoders.spare.take()
            }
        }
    }

    /// Whether the slot carries a spare vocoder.
    pub(crate) fn has_spare(&self) -> bool {
        self.stretch.as_ref().is_some_and(|v| v.spare.is_some())
    }

    /// Update the stretch factors — the only entry point for mutating them.
    /// Lock-free: writes `voice.play`, which the vocoder's controls are read
    /// from at the next block.
    ///
    /// `incoming` adopts a vocoder built by the sender, for the case where this
    /// slot has none and the new values ask for one — a voice spawned at
    /// unity/zero correctly got `stretch: None` from `AddVoice`, so turning
    /// stretching on later has to bring its own. Passing `None` when one is
    /// already resident is the common path; an `incoming` that
    /// arrives redundantly is handed back rather than swapped in, so a live vocoder
    /// never loses its phase mid-note.
    ///
    /// Allocation-free and free-free, so it is safe on the audio-thread
    /// command drain: adopting is a move, and a redundant arrival is *returned*
    /// rather than dropped — see the caller in `VoicePool::drain_commands`,
    /// which retires it for control-thread release. Dropping a vocoder
    /// here would free its rings in the callback.
    ///
    /// Until a vocoder arrives the slot reads dry, which is why the hot paths
    /// gate on `stretch.is_some()` and not on the intent.
    pub(crate) fn set_stretch(
        &mut self,
        stretch_factor: StretchFactor,
        pitch_cents: Cents,
        incoming: Option<Box<Vocoders<F>>>,
    ) -> Option<Box<Vocoders<F>>> {
        self.voice.play.stretch = stretch_factor;
        self.voice.play.pitch = pitch_cents;
        match (self.stretch.is_some(), incoming) {
            // Nothing resident and the sender sent one: adopt it. This is the
            // case that makes turning stretch on mid-flight audible at all.
            (false, Some(vocoder)) => {
                self.stretch = Some(vocoder);
                None
            }
            // Already have one — hand the spare back unused.
            (true, Some(vocoder)) => Some(vocoder),
            (_, None) => None,
        }
    }

    /// Accumulate this slot's contribution to block frames `range` (at most
    /// [`BlockScratch::frames`], the owner's prepared `max_block`) into `out`
    /// (one slice per channel, block-long),
    /// a block at a time: the voice renders into `scratch`'s planar lanes
    /// ([`render_lanes`](Self::render_lanes)), and each lane is added into its
    /// output channel by one vectorised [`accumulate`]. Shared by the mixer
    /// loop and the standalone [`VoiceNode`](super::node::VoiceNode), so the
    /// per-voice read has one definition.
    ///
    /// A jump flushes on its frame. Where the playhead jumps inside the
    /// range (a seek, a loop wrap: a run of the block that starts with
    /// [`jump`](super::clock::Run::jump)), the read stops there, the slot's
    /// buffered state is flushed ([`flush_playhead_state`](Self::flush_playhead_state)),
    /// and it goes on from the jump: a vocoder never plays the old
    /// region's material over the new one.
    ///
    /// Audio-thread safe: allocation-free (the lanes are the owner's,
    /// built on the control thread), and the tier fork is a [`VoiceSource`]
    /// match rather than a virtual call.
    ///
    /// `width` is a plain `usize`, deliberately not a [`ChannelLayout`]: it is
    /// an already-intersected clamp that callers compute as
    /// `slot width ∧ output channels ∧ MAX_CHANNELS`, not a declaration
    /// of how many channels anything *has*.
    #[inline]
    pub(crate) fn render_into(
        &mut self,
        clock: &BlockClock<'_>,
        range: Range<usize>,
        width: usize,
        scratch: &mut BlockScratch<F>,
        out: &mut [&mut [F]],
    ) {
        let n = width.min(out.len());
        debug_assert!(
            range.len() <= scratch.frames(),
            "a piece longer than the scratch"
        );
        let mut from = range.start;
        // Most blocks hold no jump, and then no range of them does. A stretching voice
        // reads a piece a run at a time as well: where the transport stops or starts
        // inside the block it primes in the stopped run, as it would in a block of its
        // own, so what it sounds does not depend on how the output was split.
        if clock.jumps() || self.stretch.is_some() {
            for run in clock.runs_in(range.clone()) {
                if run.jump {
                    if run.start > from {
                        self.render_piece(clock, from..run.start, n, scratch, out);
                        from = run.start;
                    }
                    self.jumped(run.wrap.is_some_and(|wrap| wrap.ago == 0));
                } else if self.stretch.is_some() && run.start > from {
                    self.render_piece(clock, from..run.start, n, scratch, out);
                    from = run.start;
                }
            }
        }
        self.render_piece(clock, from..range.end, n, scratch, out);
    }

    /// [`render_lanes`](Self::render_lanes) for `range`, added into `out`.
    fn render_piece(
        &mut self,
        clock: &BlockClock<'_>,
        range: Range<usize>,
        n: usize,
        scratch: &mut BlockScratch<F>,
        out: &mut [&mut [F]],
    ) {
        if range.is_empty() {
            return;
        }
        let frames = range.len();
        match self.render_lanes(clock, range.clone(), n, scratch) {
            Piece::Silent => {}
            Piece::Ready => {
                for (c, lane) in scratch.out.lanes().iter().enumerate().take(n) {
                    accumulate(&mut out[c][range.clone()], &lane[..frames]);
                }
            }
        }
    }

    /// Render block frames `range` (at most [`BlockScratch::frames`]) of this voice,
    /// `n` channels wide, into frames `0..range.len()` of `scratch.out`:
    /// every frame of each of the `n` lanes is written, silence as zero,
    /// unless the piece is [`Piece::Silent`]. `scratch.raw` and `scratch.reads`
    /// are the vocoder's input, when the voice stretches.
    ///
    /// Forks on the tier and the stretch, and reads what each tier reads:
    /// see the comments at each arm for the rule each keeps.
    pub(crate) fn render_lanes(
        &mut self,
        clock: &BlockClock<'_>,
        range: Range<usize>,
        n: usize,
        scratch: &mut BlockScratch<F>,
    ) -> Piece {
        let frames = range.len();
        let play = self.voice.play;
        let direction = play.direction;
        let gain = audible_gain::<F>(play.gain);
        let BlockScratch {
            out,
            raw,
            prime,
            positions,
            reads,
            gather,
        } = scratch;
        let positions = &mut positions[..frames];
        let longest = reads.len();
        // A resident vocoder always runs, at unity too, so the voice does not change
        // with its stretch; a voice without one reads dry.
        //
        // Its seat primes over several blocks (see `stretched`): a seek, a loop wrap or
        // the first block of a window sounds up to a few blocks late, and the voice is
        // silent meanwhile. The vocoder's output is passed on as it is: a non-finite
        // source sample does not latch in it (the vocoder's own invariant), so no
        // reset follows a block that wrote one.
        let (stretch, mut spare) = match self.stretch.as_deref_mut() {
            Some(Vocoders { live, spare }) => {
                live.retarget(&stretched::controls(&play));
                if let Some(spare) = spare.as_deref_mut() {
                    spare.retarget(&stretched::controls(&play));
                }
                (
                    Some(Stretch {
                        consumption: live.read_rate(),
                        vocoder: live,
                        cursor: &mut self.cursor,
                        seats: &mut self.seats,
                    }),
                    spare.as_deref_mut().map(|spare| Stretch {
                        consumption: spare.read_rate(),
                        vocoder: spare,
                        cursor: &mut self.spare_cursor,
                        seats: &mut self.spare_seats,
                    }),
                )
            }
            None => (None, None),
        };
        match (&mut self.voice.source, stretch) {
            (VoiceSource::Memory(sampler), Some(mut stretch)) if sampler.is_placed() => {
                // The transport seats the read: the gate's position at the
                // stretch's rate, where the voice first sounds. From the seat
                // the vocoder takes its input at a cursor of its own, one
                // source step (`read_rate`: varispeed and `src_ratio`) per
                // input frame, so a change of the stretch moves nothing.
                let window = sampler.window();
                let gate = sampler.gate(stretch.consumption);
                let sounds =
                    sampler.placed_positions(clock, range.clone(), stretch.consumption, positions);
                let mut feed = MemoryFeed {
                    sampler,
                    direction,
                    gain,
                    n,
                    gather,
                };
                let mut input = Input { raw, reads, n };
                let ahead = Ahead {
                    clock,
                    from: range.start,
                    frames,
                    longest,
                    window,
                    gate,
                };
                let in_reach =
                    ahead.prime(&mut stretch, spare.as_mut(), &mut feed, &mut input, prime);
                if !sounds {
                    // Any other silence unseats.
                    if !in_reach {
                        stretch.unseat();
                    }
                    return Piece::Silent;
                }
                stretched::render(stretch, &mut feed, positions, out, prime, &mut input);
                Piece::Ready
            }
            (VoiceSource::Disk(reader), Some(mut stretch)) => {
                // The stream sizes its fills from the rate the voice consumes
                // the file at, which the vocoder's `1 / stretch` sets: it is
                // published per block, and a voice returned to unity
                // publishes unity again.
                reader.set_stretch_rate(stretch.consumption);
                let sounds = reader.seat_positions(clock, range.clone(), positions);
                let ahead = Ahead {
                    clock,
                    from: range.start,
                    frames,
                    longest,
                    window: Some(reader.window()),
                    gate: Some(reader.gate()),
                };
                let mut feed = DiskFeed { reader, n, gather };
                let mut input = Input { raw, reads, n };
                // A disk voice primes ahead of its window and where the transport
                // stands as a memory voice does, from the ring; a loop wrap is a seek
                // to it (no spare primes: the ring holds what the stream's window
                // follows, not the loop's start).
                let in_reach = ahead.prime(&mut stretch, None, &mut feed, &mut input, prime);
                if !sounds {
                    if !in_reach {
                        stretch.unseat();
                        feed.reader.idle();
                    }
                    return Piece::Silent;
                }
                stretched::render(stretch, &mut feed, positions, out, prime, &mut input);
                Piece::Ready
            }
            // A free-running voice steps its own cursor (the transport is not
            // its clock), unstretched: stretch is refused where it is set
            // ([`SendError::FreeRunning`]), and a loop is ignored in reverse.
            (VoiceSource::Memory(sampler), _) if !sampler.is_placed() => {
                sampler.render_free(direction, frames, n, out.lanes_mut())
            }
            (VoiceSource::Memory(sampler), _) => {
                // Seated at the gate's origin (`window_rate`, varispeed
                // alone, in the wave's own frames) and stepped by `read_rate`
                // (varispeed and `src_ratio`), per frame: see
                // `MemorySource::placed_positions`. Stepping by the gate's
                // rate read a 24 kHz file on a 48 kHz clock one file frame per
                // output frame, then jumped back at every block.
                sampler.render_placed(
                    clock,
                    range,
                    direction,
                    gain,
                    n,
                    positions,
                    gather,
                    out.lanes_mut(),
                    raw.lanes_mut(),
                )
            }
            (VoiceSource::Disk(reader), _) => {
                // Unity, every block: the stretch rate belongs to the vocoder
                // rather than to the voice, so a voice whose vocoder was
                // removed must publish unity or its stream keeps reading at
                // the old factor. The reader applies its own gain (the
                // stream's control cell).
                reader.set_stretch_rate(ReadRate::UNITY);
                if reader.render_lanes(clock, range, out, n, positions, gather) {
                    Piece::Ready
                } else {
                    // No frame has a position: the reader's frames are +0.0
                    // (its gain never scales a silent frame), and its state
                    // is as the zeros left it.
                    Piece::Silent
                }
            }
        }
    }
}

/// What a stretching voice knows of the transport ahead of a piece: where its window sits,
/// the gate that places it, and the piece.
struct Ahead<'a> {
    clock: &'a BlockClock<'a>,
    /// The piece's first block frame, and its length.
    from: usize,
    frames: usize,
    /// The longest piece the scratch holds.
    longest: usize,
    window: Option<VoiceWindow>,
    gate: Option<Gate>,
}

impl Ahead<'_> {
    /// Primes the voice ahead of its first sounding frame (see `stretched`): where the
    /// transport stands inside the window, or ahead of a window that opens soon, so the
    /// first frame lands on its frame; and the spare, for a loop wrap ahead of a window
    /// that covers the loop's start. Whether a window is ahead of the voice or the transport
    /// stands in it (priming ran or not), which the voice's silence must not unseat.
    fn prime<F: SamplerFloat>(
        &self,
        stretch: &mut Stretch<'_, F>,
        spare: Option<&mut Stretch<'_, F>>,
        feed: &mut impl Feed<F>,
        input: &mut Input<'_, F>,
        prime: &mut Lanes<F>,
    ) -> bool {
        let Self {
            clock,
            from,
            frames,
            longest,
            window,
            gate,
        } = *self;
        // A stopped transport standing inside the window: where the voice seats.
        let standing = window.and_then(|w| standing_in(clock, from, w));
        let standing_origin =
            standing.and_then(|(beat, tempo)| Some((beat, gate?.origin(beat, tempo, Some(clock)))));
        let ahead = window.and_then(|w| frames_until(clock, from, w.start));
        let lead = stretched::PRIME.get().div_ceil(PRIME_SPEED);
        if let Some((beat, origin)) = standing_origin {
            stretched::preroll(stretch, feed, origin, Some(beat), 0, frames, input, prime);
        } else if let Some(ahead) = ahead
            && (stretch.cursor.is_some() || ahead <= lead + longest)
        {
            stretched::preroll(
                stretch,
                feed,
                SamplePosition::new(0.0),
                None,
                ahead,
                frames,
                input,
                prime,
            );
        }
        // A loop wrap ahead of a voice whose window covers the loop's start: the spare
        // vocoder primes for it and takes over at the wrap.
        let wrap = window.and_then(|w| wrap_ahead(clock, from, w));
        if let (Some(spare), Some((to_wrap, loop_start, tempo)), Some(gate)) = (spare, wrap, gate)
            && (spare.cursor.is_some() || to_wrap <= lead + longest)
        {
            let origin = gate.origin(loop_start, tempo, Some(clock));
            stretched::preroll(spare, feed, origin, None, to_wrap, frames, input, prime);
        }
        ahead.is_some() || standing_origin.is_some()
    }
}

/// The frames from block frame `from` until the playhead reaches `start`, while the
/// transport rolls (`None` when it does not, or `start` is not ahead), counted through the
/// project's tempo map ([`BlockClock::frames_between`]).
fn frames_until(clock: &BlockClock<'_>, from: usize, start: Beat) -> Option<usize> {
    let run = clock.runs_in(from..from + 1).next()?;
    if !run.rolling() {
        return None;
    }
    let beat = run.beat_at(from.max(run.start));
    (start > beat).then(|| {
        clock
            .frames_between(beat, start, run.tempo(), run.sample_rate())
            .get()
    })
}

/// The beat and tempo of a stopped transport at block frame `from`, when it stands
/// inside `window`: a voice there seats on the spot and primes for play from it.
fn standing_in(clock: &BlockClock<'_>, from: usize, window: VoiceWindow) -> Option<(Beat, Bpm)> {
    let run = clock.runs_in(from..from + 1).next()?;
    if run.rolling() {
        return None;
    }
    let beat = run.beat_at(from.max(run.start));
    let before_end = window.duration.is_none_or(|d| beat < window.start + d);
    (beat >= window.start && before_end).then_some((beat, run.tempo()))
}

/// The frames from block frame `from` to the end of the loop the rolling transport plays
/// over, the loop's start beat and the tempo, when `window` covers the loop's start:
/// what a voice needs to prime for the wrap, counted through the project's tempo map
/// ([`BlockClock::frames_between`]).
fn wrap_ahead(
    clock: &BlockClock<'_>,
    from: usize,
    window: VoiceWindow,
) -> Option<(usize, Beat, Bpm)> {
    let region = clock.looping()?;
    let run = clock.runs_in(from..from + 1).next()?;
    if !run.rolling() {
        return None;
    }
    let beat = run.beat_at(from.max(run.start));
    let (start, end) = (region.start(), region.end());
    (window_covers(window, start) && beat < end).then(|| {
        let frames = clock.frames_between(beat, end, run.tempo(), run.sample_rate());
        (frames.get(), start, run.tempo())
    })
}

/// Whether `window` covers `beat`: the test a voice's wrap is primed for under, and a
/// pool's handle sends it a spare by.
pub(crate) fn window_covers(window: VoiceWindow, beat: Beat) -> bool {
    window.start <= beat && window.duration.is_none_or(|d| beat < window.start + d)
}

/// What [`PlaybackSlot::render_lanes`] left for its piece.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Piece {
    /// Every frame of the piece is a zero the mix would add (±0.0), so it
    /// adds nothing: the lanes were not written, and the read was not done.
    ///
    /// Skipping the add is exact. The owner's mix starts at +0.0 and
    /// only adds, and under round-to-nearest a sum is -0.0 only when both
    /// operands are, so no mixed sample is ever -0.0; `x + ±0.0` is `x` for
    /// every other `x`, and a NaN there stays the quiet NaN arithmetic made.
    Silent,
    /// The piece's frames are in the lanes, ready to add.
    Ready,
}

/// Whether a [`Playback`] record asks for stretching: the sender builds a
/// vocoder for a voice that does, and a voice that has one runs it from then
/// on.
#[inline]
pub(crate) fn stretch_wanted(play: &Playback) -> bool {
    stretch_values_want_filter(play.stretch, play.pitch)
}

/// The same question asked of a loose factor/pitch pair, for
/// `VoiceCommand::UpdateStretch` —
/// which carries the two values but no `Playback` to wrap them in.
///
/// [`stretch_wanted`] delegates here so the thresholds exist once.
#[inline]
pub(crate) fn stretch_values_want_filter(stretch: StretchFactor, pitch: Cents) -> bool {
    (stretch.get() - 1.0).abs() > 0.001 || pitch.get().abs() > 0.5
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::clock::Clock;
    use std::sync::Arc;
    use tutti_graph::{Env, PlayClock, TransportChanges, TransportPosition};
    use tutti_music::LoopRange;
    use tutti_tempo::{TempoMap, TempoPoint, Transition};
    use tutti_units::{Frame, SampleRate, Samples};

    /// What a voice counts to a beat ahead goes through the tempo map: the playhead at beat
    /// 1.3 and 60 BPM, a step to 240 BPM at beat 1.38, a loop that ends at beat 1.5 and a
    /// clip that starts at beat 1.45. The frames are the map's closed form, piecewise at
    /// each tempo: 0.08 beat at 60 BPM is 0.08 s and 0.12 beat at 240 BPM is 0.03 s, 3 840
    /// and 1 440 frames at 48 kHz, 5 280 to the loop's end; to 1.45, 3 840 and 840, 4 680.
    /// At the playhead's own tempo they would be 9 600 and 7 200.
    #[test]
    fn what_a_voice_counts_ahead_goes_through_the_tempo_map() {
        let map = TempoMap::new([
            TempoPoint {
                beat: Beat::new(0.0),
                tempo: Bpm::of(60.0),
                transition: Transition::Step,
            },
            TempoPoint {
                beat: Beat::new(1.38),
                tempo: Bpm::of(240.0),
                transition: Transition::Step,
            },
        ])
        .expect("a map");
        let looping = LoopRange::new(Beat::new(0.25), Beat::new(1.5)).expect("a loop");
        let env = Env {
            frame: Frame::new(0),
            sample_rate: SampleRate::SR_48K,
            block_len: Samples::new(64),
            transport: TransportPosition::new(true, Bpm::of(60.0), Beat::new(1.3), Some(looping)),
            changes: TransportChanges::NONE,
            play: PlayClock::ZERO,
        };
        let map = Arc::new(map);
        let mut clock = Clock::new();
        let block = clock.observe_with(&env, Some(&map));
        let window = VoiceWindow::from(Beat::new(0.0));
        let (to_wrap, start, _) = wrap_ahead(&block, 0, window).expect("a wrap ahead");
        assert_eq!(start, Beat::new(0.25));
        assert!(to_wrap.abs_diff(5_280) <= 1, "to the wrap: {to_wrap}");
        let to_clip = frames_until(&block, 0, Beat::new(1.45)).expect("a start ahead");
        assert!(to_clip.abs_diff(4_680) <= 1, "to the clip: {to_clip}");
    }
}
