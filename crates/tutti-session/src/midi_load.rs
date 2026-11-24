//! A MIDI file as a value: [`Midi::load_midi`] returns a [`LoadMidi`] that,
//! when the app awaits it, reads the file and resolves to a [`LoadedMidi`], each
//! track already a clip value.
//!
//! The reading, the conversion of every track's events to a [`ClipEvents`], and
//! the tempo map and meter of its timeline are all the awaited load's; the
//! control thread publishes the finished values ([`Midi::play_track`]). A
//! metrical file's ticks become beats over its division, so no sample rate
//! enters and the load never converts to frames. A file in SMPTE time counts
//! seconds, not beats: its events are placed over the session's tempo map as last
//! published when [`Midi::load_midi`] was called ([`LoadMidi`]).
//!
//! What the read repaired ([`ReadRepair`]) and what the conversion left out
//! (an [`Unplayed`]) is the loaded value's [`report`](LoadedMidi::report),
//! never a log line.

use std::future::IntoFuture;
use std::sync::Arc;

use tutti_buffers::Float;
use tutti_midi_file::{
    ClipFile, MidiFile, ReadRepair, RefusedTimeSignature, SmfEvent, SmfTimeline, StandardMidiFile,
    Tick, Timebase, TimelineDrop, Track,
};
use tutti_midi_runtime::{ClipEvents, TimedMidiEvent};
use tutti_platform::{Asset, WeakExecutor};
use tutti_tempo::TempoMap;
use tutti_timeline::ProjectMeter;
use tutti_ump::{TicksPerQuarterNote, Ump, Unknown};
use tutti_units::Beat;

use crate::LoadError;
use crate::Midi;
use crate::unblocked::Unblocked;

/// A MIDI file, an SMF or a MIDI 2.0 Clip File, to read and convert to clip
/// values, built by [`Midi::load_midi`]. It is `Send`, and it is a [`Future`] through
/// [`IntoFuture`]: awaiting it reads the file and converts each track as a task
/// on the session's executor, and resolves to a [`LoadedMidi`]. There is no
/// blocking form.
///
/// A file in SMPTE time (SMF 1.0 section 2.1: ticks are fractions of a second
/// of `fps * ticks per frame`, 29.97 being 30000/1001) has no beats of its own,
/// and its Set Tempo events move no tick. Its events keep their seconds, and a
/// clip is placed in beats over **the app's tempo base when
/// [`Midi::load_midi`] was called**, a staged write included and a followed
/// source's steps not (the live tempo is not baked into stored beats): an
/// event at `s` seconds lands on `tempo.beat_at_seconds(s)`. Each track's
/// [`tempo`](LoadedTrack::tempo) is that map and its
/// [`meter`](LoadedTrack::meter) the app's meter base, so the clip plays at the
/// same times in the session as in the file for as long as the map stays. Unlike
/// a metrical file's clip, an SMPTE clip does not follow later tempo changes in
/// absolute time: it stays on the beats it was placed on.
///
/// It holds the asset, the app's tempo and meter bases, and a weak handle to
/// the session's executor, and nothing else of the session. Dropping it
/// unawaited reads nothing.
#[derive(Debug)]
#[must_use = "a load does nothing unless it is awaited"]
pub struct LoadMidi {
    asset: Asset,
    executor: WeakExecutor,
    /// The session's tempo map and meter at the call, which an SMPTE file's
    /// seconds are placed over.
    tempo: Arc<TempoMap>,
    meter: ProjectMeter,
}

impl LoadMidi {
    /// Reads the file and converts each track to a clip value. The cost is a
    /// full read of the file.
    fn run(self) -> Result<LoadedMidi, LoadError> {
        MidiFile::read(self.asset)
            .map(|file| convert(file, &self.tempo, &self.meter))
            .map_err(|error| LoadError::Midi(Arc::new(error)))
    }
}

impl IntoFuture for LoadMidi {
    type Output = Result<LoadedMidi, LoadError>;
    type IntoFuture = LoadingMidi;

    fn into_future(self) -> LoadingMidi {
        Unblocked::new(self.executor.clone(), self, Self::run)
    }
}

/// A MIDI file load being awaited ([`LoadMidi`]): resolves to the
/// [`LoadedMidi`] once the file is read and converted.
///
/// A [`Unblocked`]: its first poll starts the work as a task on the session's executor, and
/// `Session::update` is not needed.
///
/// # Errors
///
/// The output is [`LoadError::Midi`] for a file that cannot be read,
/// [`LoadError::Panicked`] when the parser panicked, and
/// [`LoadError::ShutDown`] when the session is gone.
///
/// # Panics
///
/// When polled after it resolved.
pub type LoadingMidi = Unblocked<LoadMidi, LoadedMidi, LoadError>;

/// Why a [`LoadedMidi`] has no [`LoadedTrack`] `i`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TrackError {
    /// The file has no such track.
    #[error("the file has no track {index}: it has {tracks}")]
    NoSuch {
        /// The index asked for.
        index: usize,
        /// How many tracks the file has.
        tracks: usize,
    },
}

/// Why a [`LoadedMidi`] has no one tempo map and meter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum NoCommonTimelineError {
    /// The tracks are independent sequences (format 2): each keeps its own
    /// ([`LoadedTrack::tempo`], [`LoadedTrack::meter`]).
    #[error("the file's tracks are independent sequences, each with its own timeline")]
    PerTrack,
}

/// One track of a [`LoadedMidi`] as the values a session plays and follows.
#[derive(Clone, Debug)]
pub struct LoadedTrack {
    /// The track's events in beats, ready to play ([`Midi::play_track`]).
    pub events: Arc<ClipEvents>,
    /// The tempo map the track plays on: the file's one map in a Clip File or
    /// a format 0 or 1 file, the sequence's own in a format 2 file, and the
    /// session's at the load in a file in SMPTE time. It is the file's data:
    /// the project adopts it only through an explicit session call
    /// ([`LoadedMidi::tempo`]).
    pub tempo: Arc<TempoMap>,
    /// The meter its Time Signature events declare, bars counted from beat 0;
    /// 4/4 where the track declares none; the session's in a file in SMPTE
    /// time. Like the tempo, it is the file's data and adopted only by an
    /// explicit session call.
    pub meter: Arc<ProjectMeter>,
}

/// Something of a file that a load does not play, listed in
/// [`LoadReport::unplayed`]: each variant is one kind of file's own account of
/// what it left out.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Unplayed {
    /// A Standard MIDI File's event that has no packet
    /// ([`SmfTimeline::dropped`](tutti_midi_file::SmfTimeline::dropped)).
    Smf {
        /// The track the event is in.
        track: usize,
        /// Where the event lands.
        beat: Beat,
        /// The event.
        event: SmfEvent,
        /// Why it has no packet.
        why: TimelineDrop,
    },
    /// A Clip File packet no definition covers
    /// ([`ClipTimeline::unknown`](tutti_midi_file::ClipTimeline::unknown)): it
    /// is no message, so there is nothing to play.
    Clip {
        /// Where the packet lands.
        beat: Beat,
        /// The packet.
        packet: Unknown,
    },
    /// A Set Profile On message of a Clip File's configuration header (M2-116
    /// sections 6.2 and 6.2.1.1): the load does not run the MIDI-CI
    /// transaction that configures the receiver for it. The rest of the
    /// header's configuration is the clip's own [`ClipEvents::header`]: the
    /// clip player sends it where the clip starts, and what the file states of
    /// its tempo and meter is not here:
    /// [`ClipHeader::setup_messages`](tutti_midi_file::ClipHeader::setup_messages)
    /// names the rule, and a Set Time Signature the meter refuses is in
    /// [`LoadReport::refused_time_signatures`].
    Header {
        /// Where the message is, on the header's own clock.
        tick: Tick,
        /// The message.
        packet: Ump,
    },
}

/// What loading a file changed or left out: the app's to show or to act on.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct LoadReport {
    /// What the read repaired so that the file can be written
    /// ([`StandardMidiFile::repairs`]).
    pub repairs: Vec<ReadRepair>,
    /// What the load does not play. For a Standard MIDI File, its events that
    /// have no packet, in track order. For a Clip File, the Set Profile On
    /// messages of its header, in header order, then the packets of its
    /// sequence that are no message, in sequence order.
    pub unplayed: Vec<Unplayed>,
    /// The Time Signature events no meter holds, so no change of meter was
    /// made for them, each with its sequence's track (format 2), or `None`
    /// for the file's one timeline.
    pub refused_time_signatures: Vec<(Option<usize>, RefusedTimeSignature)>,
}

impl LoadReport {
    /// Whether the file was read as it is, every event is in a clip, and no
    /// Set Profile On message of a Clip File's header went unplayed.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.repairs.is_empty()
            && self.unplayed.is_empty()
            && self.refused_time_signatures.is_empty()
    }
}

/// What a loaded file's tracks share.
#[derive(Debug)]
enum Timeline {
    /// One tempo map and meter: a Clip File's, or a format 0 or 1 file's.
    Common(Arc<TempoMap>, Arc<ProjectMeter>),
    /// Format 2: each track has its own tempo map and meter.
    PerTrack,
}

/// What a [`LoadMidi`] returns: the file as read, and each track as a
/// [`LoadedTrack`].
#[derive(Debug)]
pub struct LoadedMidi {
    file: MidiFile,
    tracks: Vec<LoadedTrack>,
    timeline: Timeline,
    report: LoadReport,
}

impl LoadedMidi {
    /// The file as read, of the kind its bytes were.
    #[must_use]
    pub fn file(&self) -> &MidiFile {
        &self.file
    }

    /// Track `i`, in the file's track order.
    ///
    /// # Errors
    ///
    /// [`TrackError::NoSuch`] when the file has no track `i`.
    pub fn track(&self, i: usize) -> Result<&LoadedTrack, TrackError> {
        self.tracks.get(i).ok_or(TrackError::NoSuch {
            index: i,
            tracks: self.tracks.len(),
        })
    }

    /// The tempo map every track shares: a Clip File's, or a format 0 or 1
    /// file's.
    ///
    /// A file's tempo track is the file's data, not the project's: the project
    /// adopts it only through an explicit session call. Loading, and playing
    /// a track, leave the session's tempo as it was.
    ///
    /// ```
    /// use tutti_midi_file::{Layout, LeadTrack, Meta, SmfDivision, StandardMidiFile, Tick, Track};
    /// use tutti_session::Session;
    /// use tutti_units::{Beat, Bpm, SampleRate};
    ///
    /// // A file whose tempo track says 100 beats a minute.
    /// let track = Track::new().meta(Tick(0), Meta::tempo(Bpm::of(100.0)).unwrap());
    /// let file = StandardMidiFile::new(
    ///     SmfDivision::per_quarter(480),
    ///     Layout::Format0(LeadTrack::new(track)),
    /// )?;
    /// let mut bytes = Vec::new();
    /// file.write(&mut bytes)?;
    ///
    /// let mut session = Session::builder().build_detached(SampleRate::SR_48K)?;
    /// let loaded = tutti_fixtures::block_on(session.midi().load_midi(bytes))?;
    /// let map = loaded.tempo()?.clone();
    /// assert_eq!(map.tempo_at(Beat::new(0.0)), Bpm::of(100.0));
    ///
    /// // The load left the session's tempo as it was ...
    /// let tempo = session.transport_handle().tempo();
    /// assert_ne!(session.get(tempo)?.tempo_at(Beat::new(0.0)), Bpm::of(100.0));
    /// // ... and the project takes the file's by setting it.
    /// session.set(tempo.to(map))?;
    /// assert_eq!(session.get(tempo)?.tempo_at(Beat::new(0.0)), Bpm::of(100.0));
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`NoCommonTimelineError::PerTrack`] for a format 2 file, whose tracks
    /// each keep their own ([`LoadedTrack::tempo`]).
    pub fn tempo(&self) -> Result<&Arc<TempoMap>, NoCommonTimelineError> {
        self.common().map(|(tempo, _)| tempo)
    }

    /// The meter every track shares: a Clip File's, or a format 0 or 1 file's.
    ///
    /// Like the tempo map ([`tempo`](Self::tempo)), a file's meter is the
    /// file's data, and the project adopts it only through an explicit session
    /// call (`session.transport_handle().meter()`).
    ///
    /// # Errors
    ///
    /// As [`tempo`](Self::tempo).
    pub fn meter(&self) -> Result<&Arc<ProjectMeter>, NoCommonTimelineError> {
        self.common().map(|(_, meter)| meter)
    }

    fn common(&self) -> Result<(&Arc<TempoMap>, &Arc<ProjectMeter>), NoCommonTimelineError> {
        match &self.timeline {
            Timeline::Common(tempo, meter) => Ok((tempo, meter)),
            Timeline::PerTrack => Err(NoCommonTimelineError::PerTrack),
        }
    }

    /// What the read repaired and the conversion left out.
    #[must_use]
    pub fn report(&self) -> &LoadReport {
        &self.report
    }
}

impl<F: Float> Midi<'_, F> {
    /// A [`LoadMidi`] of the Standard MIDI File or Clip File `asset` (a path, or bytes);
    /// nothing is read until the app awaits it. A file that cannot be read fails with its
    /// [`tutti_midi_file::ReadError`]. The value holds the session's weak executor and nothing
    /// else of the session.
    pub fn load_midi(&self, asset: impl Into<Asset>) -> LoadMidi {
        LoadMidi {
            asset: asset.into(),
            executor: self.executor.clone(),
            tempo: self.bases.tempo.get(),
            meter: ProjectMeter::new((*self.bases.meter.get()).clone()),
        }
    }
}

/// The clips, timeline values and report of `file`; a file in SMPTE time is
/// placed over `session_tempo`, with `session_meter` as its meter.
fn convert(
    file: MidiFile,
    session_tempo: &Arc<TempoMap>,
    session_meter: &ProjectMeter,
) -> LoadedMidi {
    let mut report = LoadReport::default();
    let (tracks, timeline) = match &file {
        MidiFile::Standard(smf) => convert_smf(smf, &mut report, session_tempo, session_meter),
        MidiFile::Clip(clip) => convert_clip(clip, &mut report),
    };
    LoadedMidi {
        file,
        tracks,
        timeline,
        report,
    }
}

/// The clips and timeline of a Standard MIDI File, its repairs and dropped
/// events added to `report`.
fn convert_smf(
    file: &StandardMidiFile,
    report: &mut LoadReport,
    session_tempo: &Arc<TempoMap>,
    session_meter: &ProjectMeter,
) -> (Vec<LoadedTrack>, Timeline) {
    report.repairs = file.repairs().to_vec();
    let mut tracks = Vec::new();
    let timeline = match file.timebase() {
        Timebase::Metrical(view) => {
            let tempo = Arc::clone(view.tempo_map());
            let meter = Arc::new(ProjectMeter::new(view.meter_map()));
            report.refused_time_signatures.extend(
                view.refused_time_signatures()
                    .into_iter()
                    .map(|refused| (None, refused)),
            );
            for (index, track) in file.tracks().iter().enumerate() {
                tracks.push(LoadedTrack {
                    events: clip_of(report, index, track, view.ticks_per_quarter()),
                    tempo: Arc::clone(&tempo),
                    meter: Arc::clone(&meter),
                });
            }
            Timeline::Common(tempo, meter)
        }
        Timebase::MetricalSequences(sequences) => {
            for (index, sequence) in sequences.sequences().enumerate() {
                report.refused_time_signatures.extend(
                    sequence
                        .refused_time_signatures()
                        .into_iter()
                        .map(|refused| (Some(index), refused)),
                );
                tracks.push(LoadedTrack {
                    events: clip_of(
                        report,
                        index,
                        sequence.track(),
                        sequences.ticks_per_quarter(),
                    ),
                    tempo: Arc::clone(sequence.tempo_map()),
                    meter: Arc::new(ProjectMeter::new(sequence.meter_map())),
                });
            }
            Timeline::PerTrack
        }
        Timebase::Smpte(view) => {
            let meter = Arc::new(session_meter.clone());
            for (index, timeline) in view.timelines(session_tempo).enumerate() {
                tracks.push(LoadedTrack {
                    events: clip_from(report, index, timeline),
                    tempo: Arc::clone(session_tempo),
                    meter: Arc::clone(&meter),
                });
            }
            Timeline::Common(Arc::clone(session_tempo), meter)
        }
        Timebase::SmpteSequences(sequences) => {
            for (index, sequence) in sequences.sequences().enumerate() {
                tracks.push(LoadedTrack {
                    events: clip_from(report, index, sequence.timeline(session_tempo)),
                    tempo: Arc::clone(session_tempo),
                    meter: Arc::new(session_meter.clone()),
                });
            }
            Timeline::PerTrack
        }
    };
    (tracks, timeline)
}

/// The one clip and the timeline of a Clip File: its sequence is the clip and
/// its header's setup messages the clip's header; each Set Profile On message,
/// which a load does not deliver, is listed in `report`, ahead of the packets
/// no definition covers.
fn convert_clip(file: &ClipFile, report: &mut LoadReport) -> (Vec<LoadedTrack>, Timeline) {
    let tempo = Arc::clone(file.tempo_map());
    let meter = Arc::new(ProjectMeter::new(file.meter_map()));
    report.refused_time_signatures.extend(
        file.refused_time_signatures()
            .into_iter()
            .map(|refused| (None, refused)),
    );
    report.unplayed.extend(
        file.header()
            .profile_messages()
            .iter()
            .map(|&(tick, packet)| Unplayed::Header { tick, packet }),
    );
    let timeline = file.timeline();
    report.unplayed.extend(
        timeline
            .unknown
            .into_iter()
            .map(|(beat, packet)| Unplayed::Clip { beat, packet }),
    );
    let ticks_per_quarter = file.ticks_per_quarter();
    let setup = file
        .header()
        .setup_messages()
        .filter_map(|(tick, packet)| TimedMidiEvent::new(tick.beats(ticks_per_quarter), packet));
    let track = LoadedTrack {
        events: Arc::new(clip_events(timeline.packets).header(setup)),
        tempo: Arc::clone(&tempo),
        meter: Arc::clone(&meter),
    };
    (vec![track], Timeline::Common(tempo, meter))
}

/// `track`'s events as a clip, its dropped events added to `report`.
fn clip_of(
    report: &mut LoadReport,
    index: usize,
    track: &Track,
    ticks_per_quarter: TicksPerQuarterNote,
) -> Arc<ClipEvents> {
    clip_from(report, index, track.timeline(ticks_per_quarter))
}

/// `timeline` as a clip, its dropped events added to `report` as track `index`'s.
fn clip_from(report: &mut LoadReport, index: usize, timeline: SmfTimeline) -> Arc<ClipEvents> {
    report.unplayed.extend(
        timeline
            .dropped
            .into_iter()
            .map(|(beat, event, why)| Unplayed::Smf {
                track: index,
                beat,
                event,
                why,
            }),
    );
    Arc::new(clip_events(timeline.packets))
}

/// `packets` as a clip. A timeline's beats are finite, which is all a clip
/// event asks.
fn clip_events(packets: Vec<(Beat, Ump)>) -> ClipEvents {
    packets
        .into_iter()
        .filter_map(|(beat, packet)| TimedMidiEvent::new(beat, packet))
        .collect()
}
