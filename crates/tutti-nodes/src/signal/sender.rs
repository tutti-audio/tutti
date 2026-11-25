//! [`SourceNode`]: the one node that sends a [`ControlSource`]'s legs to
//! control lanes.

use tutti_buffers::ChannelLayout;
use tutti_buffers::Float;
use tutti_graph::{
    Cost, Cx, EventKind, InEvents, Io, Node, Offset, OutEvents, ParamEvent, ParamSegment, Prepare,
    Shape, Status,
};
use tutti_modulation::{ControlSource, Leg, SourceId};
use tutti_units::{Frame, Samples};

use super::PlayWalk;

/// Events a source node sends per block, at most. Past it the node first
/// thins its grid (doubling the stride: a 4096-frame block's grid is 128
/// points), never a corner; past it with no grid left to thin it sends what
/// fits and re-sends where it stands at the next block's first frame.
pub const SOURCE_EVENT_CAPACITY: std::num::NonZeroU32 = std::num::NonZeroU32::new(256).unwrap();

/// How a control source's node is wired: its cost, whether it hears MIDI on
/// its event input, and how it reads its audio input.
pub trait SourcePorts: ControlSource {
    /// Its cost class.
    const COST: Cost;
    /// It hears MIDI on its event input (input 0, which every source node
    /// has), through [`ControlSource::heard`].
    const MIDI_IN: bool;

    /// Frames `from..to` of its audio input, in order, before the stops on
    /// `to`. A source with no audio input reads nothing.
    fn hear<F: Float>(&mut self, _io: &Io<'_, F>, _from: usize, _to: usize) {}
}

/// The leg last sent (or meant to be sent, when it was refused), and the
/// stream frame it was sent on.
#[derive(Clone, Copy, Debug)]
struct Flight {
    segment: ParamSegment,
    at: u64,
}

impl Flight {
    fn since(&self, now: u64) -> Samples {
        Samples::new(usize::try_from(now.saturating_sub(self.at)).unwrap_or(usize::MAX))
    }

    /// Its value at stream frame `now`.
    fn value(&self, now: u64) -> f64 {
        self.segment.raw_value_after(self.since(now))
    }

    /// It has arrived by `now`: it holds `to` from here on.
    fn arrived(&self, now: u64) -> bool {
        self.since(now) >= self.segment.arrives_after()
    }
}

/// A **control source's node**: walks the graph's play clock (`Cx::play_at`,
/// the node's input time) and sends what its [`ControlSource`] produces as
/// segments addressed `ParamAddr::Id(source.get())`, for every control lane that
/// reads `source`.
///
/// - **Its stops**, in this order on one frame: a play start (the source's
///   `start`), a corner the source named, a point of its [grid], and each
///   MIDI message or [`EventKind::Resync`] on its input. A fresh node is idle
///   already, so a play start on its first frame is none.
/// - **One leg per frame**: a later one on the same frame replaces it.
/// - **Silence is not sent twice**: a leg from 0 to 0, where what it sent
///   has arrived at 0, is not sent (nor before it has sent anything: a lane
///   reads an input that has sent nothing as 0).
/// - **Overflow** (more than [`SOURCE_EVENT_CAPACITY`] in a block): first its
///   grid's stride doubles until the block fits (a block past 8192 frames for
///   the control grid), never a corner; then what still does not fit is
///   dropped, and it re-sends where it stands, resumed, at the next block's
///   first frame.
/// - **A resync** ([`EventKind::Resync`] on its input) re-sends where it
///   stands, resumed, on the event's frame: for a lane that newly reads it.
///
/// [grid]: ControlSource::GRID
#[derive(Clone, Debug)]
pub struct SourceNode<S: ControlSource> {
    source: S,
    id: SourceId,
    /// Its audio input's width ([`ChannelLayout::EMPTY`]: none).
    input: ChannelLayout,
    flight: Option<Flight>,
    /// Re-send where it stands at the next block's first frame: the last
    /// block refused events.
    owed: bool,
    /// The play clock's epoch at the last frame read; `None` while fresh.
    epoch: Option<u32>,
    /// Frames since its last reset: the clock its flight is timed on.
    position: u64,
    /// Its event input: [`EventKind::Resync`], and MIDI when its source hears it.
    events_in: InEvents,
    /// Its segment output.
    out: OutEvents,
}

impl<S: ControlSource> SourceNode<S> {
    /// The source id its segments are addressed to.
    pub fn source(&self) -> SourceId {
        self.id
    }

    fn segment(&self, leg: Leg) -> ParamSegment {
        ParamSegment::by_id(self.id.get(), leg.piece.from, leg.piece.to, Samples::new(0))
            .law(leg.piece.law)
            .resumed(Samples::new(0), leg.frames)
    }

    /// The value it has sent, at stream frame `now` (0 before any).
    fn level(&self, now: u64) -> f64 {
        self.flight.map_or(0.0, |f| f.value(now))
    }

    /// Where it stands at stream frame `now`, as the leg in flight resumed
    /// there (the same trajectory, so a lane reading it hears no change), or
    /// 0 before any.
    fn standing(&self, now: u64) -> ParamSegment {
        match self.flight {
            Some(f) => f
                .segment
                .resumed(f.segment.position() + f.since(now), f.segment.length()),
            None => self.segment(Leg::hold(0.0)),
        }
    }

    /// Whether `leg` is a 0 where 0 is held at `now`: silence is not sent
    /// twice. (Any other constant is sent: a lane may be new, or have
    /// thinned, and a held level is what a reader resynchronizes on.)
    fn silent(&self, leg: &Leg, now: u64) -> bool {
        let zero = 0.0f64.to_bits();
        if leg.piece.from.to_bits() != zero || leg.piece.to.to_bits() != zero {
            return false;
        }
        self.flight
            .is_none_or(|f| f.arrived(now) && f.value(now).to_bits() == zero)
    }
}

/// A block's output: the segment waiting on its frame, so a later one on the
/// same frame replaces it, written once the walk moves past it.
struct Out {
    pending: Option<(usize, ParamSegment)>,
    len: Samples,
    refused: bool,
    port: OutEvents,
}

impl Out {
    fn put<F: Float>(&mut self, io: &mut Io<'_, F>, i: usize, seg: ParamSegment) {
        if self.pending.is_some_and(|(p, _)| p != i) {
            self.flush(io);
        }
        self.pending = Some((i, seg));
    }

    fn flush<F: Float>(&mut self, io: &mut Io<'_, F>) {
        if let Some((i, seg)) = self.pending.take() {
            let pushed = Offset::new(Samples::new(i), self.len).is_some_and(|at| {
                io.event_out(self.port)
                    .push_param(ParamEvent::segment(at, seg))
                    .is_ok()
            });
            self.refused |= !pushed;
        }
    }
}

impl<S: SourcePorts> SourceNode<S> {
    /// A node sending `source`'s legs as `id`, reading `input`-wide audio.
    pub(crate) fn with_source(id: SourceId, source: S, input: ChannelLayout) -> Self {
        let shape = Self::declared(input);
        Self {
            source,
            id,
            input,
            flight: None,
            owed: false,
            epoch: None,
            position: 0,
            events_in: shape.event_input(0).expect("declared one event input"),
            out: shape.event_output(0).expect("declared one event output"),
        }
    }

    /// Its audio input (if any) in, no audio out; one event input
    /// ([`EventKind::Resync`], and MIDI if its source hears it), and one event
    /// output (the segments).
    fn declared(input: ChannelLayout) -> Shape {
        let shape = if input == ChannelLayout::EMPTY {
            Shape::EMPTY
        } else {
            Shape::sink_unassigned(input)
        };
        shape
            .events(1, 1)
            .param_events()
            .event_capacity(SOURCE_EVENT_CAPACITY)
            .cost(S::COST)
            .no_note_events()
    }

    fn send<F: Float>(&mut self, out: &mut Out, io: &mut Io<'_, F>, i: usize, seg: ParamSegment) {
        self.flight = Some(Flight {
            segment: seg,
            at: self.position + i as u64,
        });
        out.put(io, i, seg);
    }

    fn send_leg<F: Float>(
        &mut self,
        out: &mut Out,
        io: &mut Io<'_, F>,
        i: usize,
        leg: Option<Leg>,
    ) {
        if let Some(leg) = leg
            && !self.silent(&leg, self.position + i as u64)
        {
            let seg = self.segment(leg);
            self.send(out, io, i, seg);
        }
    }

    /// The grid's stride for this block: the source's, doubled while the
    /// block's grid points and `reserve` other stops would not fit.
    fn stride(&self, cx: &Cx<'_>, walk: &PlayWalk, len: usize, reserve: usize) -> Option<u64> {
        let mut stride = u64::from(S::GRID?.stride());
        let room = (SOURCE_EVENT_CAPACITY.get() as usize).saturating_sub(reserve);
        while grid_points(cx, walk, len, self.epoch, stride) > room && stride < len as u64 {
            stride *= 2;
        }
        Some(stride)
    }
}

/// Grid points of `stride` in a block: frames whose play count is a multiple.
fn grid_points(cx: &Cx<'_>, walk: &PlayWalk, len: usize, prev: Option<u32>, stride: u64) -> usize {
    let mut n = 0;
    let mut starts = walk.starts(cx, len, prev).peekable();
    let mut s = 0;
    while s < len {
        if starts.peek() == Some(&s) {
            starts.next();
        }
        let e = starts.peek().copied().unwrap_or(len);
        let p = walk.at(cx, s).play.get();
        let first = p.div_ceil(stride) * stride - p;
        let span = (e - s) as u64;
        if first < span {
            n += ((span - first - 1) / stride + 1) as usize;
        }
        s = e;
    }
    n
}

impl<F: Float, S: SourcePorts> Node<F> for SourceNode<S> {
    /// Its audio input (if any) in, no audio out; one event input
    /// ([`EventKind::Resync`], and MIDI if its source hears it), and one event
    /// output (the segments).
    fn shape(&self) -> Shape {
        Self::declared(self.input)
    }

    fn prepare(&mut self, prepare: &Prepare) {
        self.source.prepare(prepare.sample_rate());
    }

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        let len = io.frames();
        if len == 0 {
            return Status::Modified;
        }
        let walk = PlayWalk::new(cx, len);
        let events = io.events(self.events_in).as_slice();
        let n_starts = walk.starts(cx, len, self.epoch).count();
        // Every other stop a block can hold: the owed re-send, the starts,
        // and per event its own leg and the corner it may set.
        let reserve = 2 + n_starts + 2 * events.len();
        let stride = self.stride(cx, &walk, len, reserve);
        let mut out = Out {
            pending: None,
            len: cx.env.block_len,
            refused: false,
            port: self.out,
        };
        if std::mem::take(&mut self.owed) {
            // Refused last block: where it stands now.
            let seg = self.standing(self.position);
            self.send(&mut out, &mut io, 0, seg);
        }
        let mut starts = walk.starts(cx, len, self.epoch).peekable();
        let (mut ev, mut heard, mut s) = (0usize, 0usize, 0usize);
        while s < len {
            let start = starts.peek() == Some(&s);
            if start {
                starts.next();
            }
            let e = starts.peek().copied().unwrap_or(len);
            let reading = walk.at(cx, s);
            let base = reading.play.get();
            // The play frame of offset `i` in this epoch's span.
            let play = |i: usize| Frame::new(base + (i - s) as u64);
            if start {
                let leg = self.source.start();
                self.send_leg(&mut out, &mut io, s, leg);
            }
            // The first frame not yet searched for a stop.
            let mut q = s;
            loop {
                let corner = self
                    .source
                    .next_corner()
                    .map(|c| (s + c.get().saturating_sub(base) as usize).max(q))
                    .filter(|&i| i < e);
                let grid = stride
                    .map(|st| {
                        let p = base + (q - s) as u64;
                        q + (p.div_ceil(st) * st - p) as usize
                    })
                    .filter(|&i| i < e);
                let note = events.get(ev).map(|x| x.offset.index()).filter(|&i| i < e);
                let Some(p) = [corner, grid, note].into_iter().flatten().min() else {
                    break;
                };
                self.source.hear(&io, heard, p);
                heard = p;
                if corner == Some(p) {
                    let leg = self.source.corner(play(p));
                    self.send_leg(&mut out, &mut io, p, leg);
                }
                if let (true, Some(st)) = (grid == Some(p), stride) {
                    let leg = self.source.grid(play(p), Samples::new(st as usize));
                    self.send_leg(&mut out, &mut io, p, leg);
                }
                while let Some(x) = events.get(ev).filter(|x| x.offset.index() == p) {
                    match x.kind {
                        EventKind::Midi(ump) if S::MIDI_IN => {
                            let level = self.level(self.position + p as u64);
                            let leg = self.source.heard(play(p), level, ump);
                            self.send_leg(&mut out, &mut io, p, leg);
                        }
                        // A lane newly reads it: where it stands, resumed.
                        EventKind::Resync => {
                            let seg = self.standing(self.position + p as u64);
                            self.send(&mut out, &mut io, p, seg);
                        }
                        _ => {}
                    }
                    ev += 1;
                }
                q = p + 1;
            }
            self.source.hear(&io, heard, e);
            heard = e;
            s = e;
        }
        out.flush(&mut io);
        self.owed |= out.refused;
        self.epoch = Some(walk.at(cx, len - 1).epoch);
        self.position += len as u64;
        Status::Modified
    }

    fn reset(&mut self) {
        let _ = self.source.start();
        self.flight = None;
        self.owed = false;
        self.epoch = None;
        self.position = 0;
    }
}
