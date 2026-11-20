//! A MIDI file as a job: [`Midi::load_midi`] reads the file at a path on a
//! worker and lands a [`LoadedMidi`], each track already a clip value.
//!
//! The reading, the conversion of every track's events to a [`ClipEvents`], and
//! the tempo map and meter of its timeline are all the worker's; the control
//! thread receives finished values and publishes them
//! ([`Midi::play_track`]). A file's ticks become beats over its division, so no
//! sample rate enters and the job never converts to frames.
//!
//! What the read repaired ([`Normalized`]) and what the conversion left out
//! (a [`DroppedEvent`]) is the landed value's [`report`](LoadedMidi::report),
//! never a log line.

use std::sync::Arc;

use tutti_buffers::Float;
use tutti_midi_file::{
    FileEvent, MidiFile, Normalized, ReadError, RefusedTimeSignature, Timebase, TimelineDrop, Track,
};
use tutti_midi_runtime::{ClipEvents, TimedMidiEvent};
use tutti_platform::Asset;
use tutti_tempo::TempoMap;
use tutti_transport::ProjectMeter;
use tutti_ump::TicksPerQuarterNote;
use tutti_units::Beat;

use crate::job::job_kind;
use crate::{Midi, WaitError, WorkerError};

job_kind! {
    /// A MIDI file read and converted to clip values ([`Midi::load_midi`]); the
    /// job lands a [`LoadedMidi`].
    pub LoadMidi => LoadedMidi, LoadMidiError
}

/// Why a [`LoadMidi`] job ended without a [`LoadedMidi`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LoadMidiError {
    /// The file could not be read.
    #[error("MIDI file: {0}")]
    Read(#[from] ReadError),
    /// The worker did not finish: its work panicked, or the spawner dropped it.
    #[error(transparent)]
    Worker(#[from] WorkerError),
    /// The wait could not run ([`Job::wait`](crate::Job::wait)).
    #[error(transparent)]
    Wait(#[from] WaitError),
}

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
    /// The file's ticks are fractions of a second: it has no beats to place a
    /// clip on, and a file in SMPTE time is not converted to clips.
    #[error("the file is in SMPTE time: it has no beats, and is not converted to clips")]
    SmpteTiming,
}

/// Why a [`LoadedMidi`] has no one tempo map and meter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum NoCommonTimelineError {
    /// The tracks are independent sequences (format 2): each keeps its own
    /// ([`LoadedTrack::tempo`], [`LoadedTrack::meter`]).
    #[error("the file's tracks are independent sequences, each with its own timeline")]
    PerTrack,
    /// The file's ticks are fractions of a second: it has no tempo map or
    /// meter.
    #[error("the file is in SMPTE time: it has no tempo map or meter")]
    SmpteTiming,
}

/// One track of a [`LoadedMidi`] as the values a session plays and follows.
#[derive(Clone, Debug)]
pub struct LoadedTrack {
    /// The track's events in beats, ready to play ([`Midi::play_track`]).
    pub events: Arc<ClipEvents>,
    /// The tempo map the track plays on: the file's one map in a format 0 or 1
    /// file, the sequence's own in a format 2 file.
    pub tempo: Arc<TempoMap>,
    /// The meter its Time Signature events declare, bars counted from beat 0;
    /// 4/4 where the track declares none.
    pub meter: Arc<ProjectMeter>,
}

/// An event of a track's timeline that has no packet, and so is not played
/// ([`Timeline::dropped`](tutti_midi_file::Timeline::dropped)).
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct DroppedEvent {
    /// The track the event is in.
    pub track: usize,
    /// Where the event lands.
    pub beat: Beat,
    /// The event.
    pub event: FileEvent,
    /// Why it has no packet.
    pub why: TimelineDrop,
}

/// What loading a file changed or left out: the app's to show or to act on.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct LoadReport {
    /// What the read repaired so that the file can be written
    /// ([`MidiFile::normalized`]).
    pub normalized: Vec<Normalized>,
    /// The events the conversion to clips left out, in track order.
    pub dropped: Vec<DroppedEvent>,
    /// The Time Signature events no meter holds, so no change of meter was
    /// made for them, each with its sequence's track (format 2), or `None`
    /// for the file's one timeline.
    pub refused_time_signatures: Vec<(Option<usize>, RefusedTimeSignature)>,
}

impl LoadReport {
    /// Whether the file was read as it is and every event is in a clip.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.normalized.is_empty()
            && self.dropped.is_empty()
            && self.refused_time_signatures.is_empty()
    }
}

/// What a loaded file's tracks share.
#[derive(Debug)]
enum Timeline {
    /// One tempo map and meter: a format 0 or 1 file's.
    Common(Arc<TempoMap>, Arc<ProjectMeter>),
    /// Format 2: each track has its own tempo map and meter.
    PerTrack,
    /// A file in SMPTE time: no beats, so no tempo map or meter.
    Smpte,
}

/// The landing of a [`LoadMidi`] job: the file as read, and each track as a
/// [`LoadedTrack`].
#[derive(Debug)]
pub struct LoadedMidi {
    file: MidiFile,
    tracks: Vec<LoadedTrack>,
    timeline: Timeline,
    report: LoadReport,
}

impl LoadedMidi {
    /// The file as read.
    #[must_use]
    pub fn file(&self) -> &MidiFile {
        &self.file
    }

    /// Track `i`, in the file's track order.
    ///
    /// # Errors
    ///
    /// [`TrackError::NoSuch`] when the file has no track `i`;
    /// [`TrackError::SmpteTiming`] when it is in SMPTE time, which is not
    /// converted to clips.
    pub fn track(&self, i: usize) -> Result<&LoadedTrack, TrackError> {
        if matches!(self.timeline, Timeline::Smpte) {
            return Err(TrackError::SmpteTiming);
        }
        self.tracks.get(i).ok_or(TrackError::NoSuch {
            index: i,
            tracks: self.tracks.len(),
        })
    }

    /// The tempo map every track shares: a format 0 or 1 file's.
    ///
    /// # Errors
    ///
    /// [`NoCommonTimelineError::PerTrack`] for a format 2 file, whose tracks
    /// each keep their own ([`LoadedTrack::tempo`]);
    /// [`NoCommonTimelineError::SmpteTiming`] for a file in SMPTE time.
    pub fn tempo(&self) -> Result<&Arc<TempoMap>, NoCommonTimelineError> {
        self.common().map(|(tempo, _)| tempo)
    }

    /// The meter every track shares: a format 0 or 1 file's.
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
            Timeline::Smpte => Err(NoCommonTimelineError::SmpteTiming),
        }
    }

    /// What the read repaired and the conversion left out.
    #[must_use]
    pub fn report(&self) -> &LoadReport {
        &self.report
    }
}

impl<F: Float> Midi<'_, F> {
    /// Reads the MIDI file `asset` (a path, or bytes) on the session's spawner and converts each
    /// track to a clip value there. Returns at once; the [`LoadedMidi`] lands
    /// in a later [`update`](crate::Session::update) as a
    /// [`SessionEvent::JobDone`](crate::SessionEvent::JobDone), or
    /// [`Job::wait`](crate::Job::wait) returns it. A file that cannot be read
    /// lands [`LoadMidiError::Read`]. **Dropping the [`Job`](crate::Job)
    /// discards the result.**
    ///
    /// A file in SMPTE time has seconds and no beats: it lands with its
    /// [`file`](LoadedMidi::file) and report, and [`TrackError::SmpteTiming`]
    /// for every track.
    pub fn load_midi(&mut self, asset: impl Into<Asset>) -> crate::Job<LoadMidi> {
        let asset = asset.into();
        self.jobs.submit_job(
            move |_| load(&asset),
            |_, done, ticket| {
                ticket.land(match done {
                    Ok(loaded) => loaded,
                    Err(failed) => Err(failed.into()),
                });
            },
        )
    }
}

/// Reads `asset` and builds its [`LoadedMidi`].
fn load(asset: &Asset) -> Result<LoadedMidi, LoadMidiError> {
    Ok(convert(MidiFile::read(asset.clone())?))
}

/// The clips, timeline values and report of `file`.
fn convert(file: MidiFile) -> LoadedMidi {
    let mut report = LoadReport {
        normalized: file.normalized().to_vec(),
        ..LoadReport::default()
    };
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
                    events: clip_of(&mut report, index, track, view.ticks_per_quarter()),
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
                        &mut report,
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
        Timebase::Smpte(_) | Timebase::SmpteSequences(_) => Timeline::Smpte,
    };
    LoadedMidi {
        file,
        tracks,
        timeline,
        report,
    }
}

/// `track`'s events as a clip, its dropped events added to `report`.
fn clip_of(
    report: &mut LoadReport,
    index: usize,
    track: &Track,
    ticks_per_quarter: TicksPerQuarterNote,
) -> Arc<ClipEvents> {
    let timeline = track.timeline(ticks_per_quarter);
    report.dropped.extend(
        timeline
            .dropped
            .into_iter()
            .map(|(beat, event, why)| DroppedEvent {
                track: index,
                beat,
                event,
                why,
            }),
    );
    // A timeline's beats are finite, which is all a clip event asks.
    Arc::new(
        timeline
            .packets
            .into_iter()
            .filter_map(|(beat, packet)| TimedMidiEvent::new(beat, packet))
            .collect(),
    )
}
